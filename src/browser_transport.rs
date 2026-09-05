use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};

use crate::process_runner::{SpawnedProcess, spawn_owned_program};
use crate::state::SharedState;

const DEVTOOLS_PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_MCP_PROTOCOL_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIAGNOSTIC_LINES: usize = 100;
const MAX_DIAGNOSTIC_CHARS: usize = 2_048;
const MAX_DIAGNOSTIC_LINE_BYTES: usize = MAX_DIAGNOSTIC_CHARS * 4;
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub enum BrowserTransportError {
    Timeout,
    Disconnected(String),
    Protocol(String),
}

impl fmt::Display for BrowserTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "browser MCP request timed out"),
            Self::Disconnected(message) | Self::Protocol(message) => f.write_str(message),
        }
    }
}

type PendingBrowserResponse = oneshot::Sender<Result<Value, BrowserTransportError>>;

pub struct BrowserMcpTransport {
    process: Mutex<SpawnedProcess>,
    stdin: Mutex<Option<BufWriter<tokio::process::ChildStdin>>>,
    pending: Arc<Mutex<HashMap<u64, PendingBrowserResponse>>>,
    next_request_id: AtomicU64,
    alive: Arc<AtomicBool>,
}

impl BrowserMcpTransport {
    pub async fn start(
        workspace_root: &str,
        package_version: &str,
        server_args: &[String],
        state: Option<SharedState>,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<Self>, BrowserTransportError> {
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }

        let package = format!("chrome-devtools-mcp@{package_version}");
        let mut command = Command::new(npx_program());
        command
            .args(["-y", "-p", package.as_str(), "chrome-devtools-mcp"])
            .args(server_args)
            .env("CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS", "1")
            .current_dir(workspace_root);

        let mut process = spawn_owned_program(command).map_err(|error| {
            BrowserTransportError::Disconnected(format!(
                "Failed to start pinned chrome-devtools-mcp: {error}"
            ))
        })?;
        let stdin = process.take_stdin().ok_or_else(|| {
            BrowserTransportError::Disconnected(
                "chrome-devtools-mcp did not expose stdin".to_string(),
            )
        })?;
        let stdout = process.take_stdout().ok_or_else(|| {
            BrowserTransportError::Disconnected(
                "chrome-devtools-mcp did not expose stdout".to_string(),
            )
        })?;
        let stderr = process.take_stderr().ok_or_else(|| {
            BrowserTransportError::Disconnected(
                "chrome-devtools-mcp did not expose stderr".to_string(),
            )
        })?;

        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let transport = Arc::new(Self {
            process: Mutex::new(process),
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            pending: pending.clone(),
            next_request_id: AtomicU64::new(1),
            alive: alive.clone(),
        });

        tokio::spawn(read_stdout(stdout, pending, alive.clone()));
        tokio::spawn(drain_stderr(stderr, state));

        let initialize = json!({
            "protocolVersion": DEVTOOLS_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "moondesk-browser-runtime",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        if let Err(error) = transport.request("initialize", initialize, deadline).await {
            transport.shutdown().await;
            return Err(error);
        }
        if let Err(error) = transport
            .notify("notifications/initialized", json!({}), deadline)
            .await
        {
            transport.shutdown().await;
            return Err(error);
        }
        Ok(transport)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    #[cfg(all(test, windows))]
    pub async fn pid(&self) -> Option<u32> {
        self.process.lock().await.pid()
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
            deadline,
        )
        .await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        if !self.is_alive() {
            return Err(BrowserTransportError::Disconnected(
                "chrome-devtools-mcp is not running".to_string(),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout_at(deadline, self.pending.lock())
            .await
            .map_err(|_| BrowserTransportError::Timeout)?
            .insert(id, tx);

        if let Err(error) = self.write_message(&request, deadline).await {
            if let Ok(mut pending) = tokio::time::timeout_at(deadline, self.pending.lock()).await {
                pending.remove(&id);
            }
            return Err(error);
        }

        let response = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => {
                self.alive.store(false, Ordering::Release);
                return Err(BrowserTransportError::Disconnected(
                    "chrome-devtools-mcp response channel closed".to_string(),
                ));
            }
            Err(_) => return Err(BrowserTransportError::Timeout),
        };

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown chrome-devtools-mcp error");
            return Err(BrowserTransportError::Protocol(format!(
                "chrome-devtools-mcp error {code}: {message}"
            )));
        }
        response.get("result").cloned().ok_or_else(|| {
            BrowserTransportError::Protocol(
                "chrome-devtools-mcp returned a response without result or error".to_string(),
            )
        })
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        self.write_message(
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
            deadline,
        )
        .await
    }

    async fn write_message(
        &self,
        message: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        let encoded = serde_json::to_string(message).map_err(|error| {
            BrowserTransportError::Protocol(format!(
                "Could not encode browser MCP request: {error}"
            ))
        })?;
        let write = async {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "chrome-devtools-mcp stdin is closed",
                )
            })?;
            stdin.write_all(encoded.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        match tokio::time::timeout_at(deadline, write).await {
            Err(_) => {
                self.alive.store(false, Ordering::Release);
                Err(BrowserTransportError::Timeout)
            }
            Ok(Err(error)) => {
                self.alive.store(false, Ordering::Release);
                Err(BrowserTransportError::Disconnected(format!(
                    "Could not write to chrome-devtools-mcp: {error}"
                )))
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);

        // Terminate the owned process tree first. A request may be blocked while writing to a live
        // child that stopped reading stdin; killing the child makes that write fail instead of
        // allowing shutdown to wait behind the buffered-stdin mutex/flush indefinitely.
        {
            let mut process = self.process.lock().await;
            process.terminate_tree().await;
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, process.wait()).await;
        }

        self.pending.lock().await.clear();
        let _ = tokio::time::timeout(SHUTDOWN_WAIT, async {
            let mut stdin = self.stdin.lock().await;
            stdin.take();
        })
        .await;
    }
}

enum ProtocolFrameRead {
    Eof,
    Frame(Vec<u8>),
    Overflow,
}

async fn read_protocol_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<ProtocolFrameRead> {
    let mut frame = Vec::with_capacity(max_bytes.min(64 * 1024));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(ProtocolFrameRead::Eof)
            } else {
                Ok(ProtocolFrameRead::Frame(frame))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let take = newline + 1;
            if frame.len().saturating_add(take) > max_bytes {
                return Ok(ProtocolFrameRead::Overflow);
            }
            frame.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(ProtocolFrameRead::Frame(frame));
        }
        if frame.len().saturating_add(available.len()) > max_bytes {
            return Ok(ProtocolFrameRead::Overflow);
        }
        let take = available.len();
        frame.extend_from_slice(available);
        reader.consume(take);
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<u64, PendingBrowserResponse>>>,
    alive: Arc<AtomicBool>,
) {
    read_stdout_with_limit(stdout, pending, alive, MAX_MCP_PROTOCOL_FRAME_BYTES).await;
}

async fn read_stdout_with_limit<R: AsyncRead + Unpin>(
    stdout: R,
    pending: Arc<Mutex<HashMap<u64, PendingBrowserResponse>>>,
    alive: Arc<AtomicBool>,
    max_frame_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    let failure = loop {
        let frame = match read_protocol_frame(&mut reader, max_frame_bytes).await {
            Ok(ProtocolFrameRead::Frame(frame)) => frame,
            Ok(ProtocolFrameRead::Eof) => {
                break "chrome-devtools-mcp stdout closed".to_string();
            }
            Ok(ProtocolFrameRead::Overflow) => {
                break format!(
                    "chrome-devtools-mcp response exceeded MoonDesk's {max_frame_bytes}-byte protocol frame limit; use pagination or a file-output option for large results"
                );
            }
            Err(error) => {
                break format!("Could not read chrome-devtools-mcp stdout: {error}");
            }
        };
        if frame.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let message = match serde_json::from_slice::<Value>(&frame) {
            Ok(message) => message,
            Err(error) => {
                // stdout is reserved for MCP JSON-RPC. A malformed frame makes stream alignment
                // untrustworthy, so fail closed and let the owner invalidate the process tree.
                break format!("chrome-devtools-mcp emitted invalid JSON-RPC: {error}");
            }
        };
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(Ok(message));
        }
    };
    alive.store(false, Ordering::Release);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(BrowserTransportError::Disconnected(failure.clone())));
    }
}

async fn read_bounded_diagnostic_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut truncated = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() && !truncated {
                Ok(None)
            } else {
                Ok(Some((line, truncated)))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let content = &available[..newline];
            let remaining = max_bytes.saturating_sub(line.len());
            let copy = remaining.min(content.len());
            line.extend_from_slice(&content[..copy]);
            truncated |= copy < content.len();
            reader.consume(newline + 1);
            return Ok(Some((line, truncated)));
        }

        let remaining = max_bytes.saturating_sub(line.len());
        let copy = remaining.min(available.len());
        line.extend_from_slice(&available[..copy]);
        truncated |= copy < available.len();
        let take = available.len();
        reader.consume(take);
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, state: Option<SharedState>) {
    let mut reader = BufReader::new(stderr);
    let mut reported = 0usize;
    let mut suppression_reported = false;
    loop {
        let Some((line, ingest_truncated)) =
            read_bounded_diagnostic_line(&mut reader, MAX_DIAGNOSTIC_LINE_BYTES)
                .await
                .ok()
                .flatten()
        else {
            break;
        };
        let decoded = String::from_utf8_lossy(&line);
        let trimmed = decoded.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(state) = state.as_ref() else {
            continue;
        };
        if reported < MAX_DIAGNOSTIC_LINES {
            let mut message = trimmed
                .chars()
                .take(MAX_DIAGNOSTIC_CHARS)
                .collect::<String>();
            if ingest_truncated || trimmed.chars().count() > MAX_DIAGNOSTIC_CHARS {
                message.push_str(" ...[truncated]");
            }
            state
                .lock()
                .await
                .log("WARN", format!("chrome-devtools-mcp: {message}"));
            reported += 1;
        } else if !suppression_reported {
            state.lock().await.log(
                "WARN",
                "chrome-devtools-mcp: additional stderr diagnostics suppressed".to_string(),
            );
            suppression_reported = true;
        }
    }
}

fn npx_program() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_reading_child_command() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30"]);
            command
        }
    }

    #[test]
    fn transport_uses_pinned_devtools_protocol() {
        assert_eq!(DEVTOOLS_PROTOCOL_VERSION, "2025-03-26");
    }

    #[tokio::test]
    async fn request_write_respects_deadline_when_child_stops_reading_stdin() {
        let mut process = spawn_owned_program(non_reading_child_command())
            .expect("spawn non-reading transport child");
        let stdin = process.take_stdin().expect("test child stdin");
        drop(process.take_stdout());
        drop(process.take_stderr());
        let transport = BrowserMcpTransport {
            process: Mutex::new(process),
            stdin: Mutex::new(Some(BufWriter::new(stdin))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
        };
        let message = json!({"payload": "x".repeat(512 * 1024)});
        let started = tokio::time::Instant::now();
        let deadline = started + std::time::Duration::from_millis(200);
        let error = transport
            .write_message(&message, deadline)
            .await
            .expect_err("blocked stdin write must hit the caller deadline");
        assert!(matches!(error, BrowserTransportError::Timeout));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "blocked request write exceeded its bounded deadline cleanup window"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), transport.shutdown())
            .await
            .expect("shutdown must not wait behind the stuck buffered stdin writer");
    }

    #[tokio::test]
    async fn oversized_protocol_frame_invalidates_reader_without_unbounded_allocation() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(1, tx);
        let task = tokio::spawn(read_stdout_with_limit(
            reader,
            pending.clone(),
            alive.clone(),
            128,
        ));
        let frame = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"value\":\"{}\"}}}}\n",
            "x".repeat(256)
        );
        let _ = writer.write_all(frame.as_bytes()).await;
        drop(writer);
        task.await.expect("bounded stdout reader task");
        assert!(!alive.load(Ordering::Acquire));
        assert!(pending.lock().await.is_empty());
        let error = rx
            .await
            .expect("oversized frame should complete the pending channel")
            .expect_err("oversized protocol frame must fail its pending response");
        assert!(
            matches!(error, BrowserTransportError::Disconnected(message) if message.contains("protocol frame limit"))
        );
    }

    #[tokio::test]
    async fn stderr_ingestion_discards_oversized_line_tail_without_growing_buffer() {
        let (mut writer, reader) = tokio::io::duplex(512);
        writer
            .write_all(format!("{}\n", "x".repeat(256)).as_bytes())
            .await
            .expect("write oversized diagnostic line");
        drop(writer);
        let mut reader = BufReader::new(reader);
        let (line, truncated) = read_bounded_diagnostic_line(&mut reader, 32)
            .await
            .expect("read bounded diagnostic")
            .expect("diagnostic line");
        assert_eq!(line.len(), 32);
        assert!(truncated);
    }
}
