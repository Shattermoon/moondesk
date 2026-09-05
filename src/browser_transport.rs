use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};

use crate::process_runner::{SpawnedProcess, spawn_owned_program};
use crate::state::SharedState;

const DEVTOOLS_PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_DIAGNOSTIC_LINES: usize = 100;
const MAX_DIAGNOSTIC_CHARS: usize = 2_048;
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

pub struct BrowserMcpTransport {
    process: Mutex<SpawnedProcess>,
    stdin: Mutex<BufWriter<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
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
            stdin: Mutex::new(BufWriter::new(stdin)),
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
            .notify("notifications/initialized", json!({}))
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
        self.pending.lock().await.insert(id, tx);

        if let Err(error) = self.write_message(&request).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        let response = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                self.alive.store(false, Ordering::Release);
                return Err(BrowserTransportError::Disconnected(
                    "chrome-devtools-mcp response channel closed".to_string(),
                ));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(BrowserTransportError::Timeout);
            }
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

    async fn notify(&self, method: &str, params: Value) -> Result<(), BrowserTransportError> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn write_message(&self, message: &Value) -> Result<(), BrowserTransportError> {
        let encoded = serde_json::to_string(message).map_err(|error| {
            BrowserTransportError::Protocol(format!(
                "Could not encode browser MCP request: {error}"
            ))
        })?;
        let mut stdin = self.stdin.lock().await;
        if let Err(error) = async {
            stdin.write_all(encoded.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        }
        .await
        {
            self.alive.store(false, Ordering::Release);
            return Err(BrowserTransportError::Disconnected(format!(
                "Could not write to chrome-devtools-mcp: {error}"
            )));
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        if !self.alive.swap(false, Ordering::AcqRel) {
            // Even if stdout already observed EOF, retain deterministic process-tree cleanup.
        }
        self.pending.lock().await.clear();
        let _ = self.stdin.lock().await.shutdown().await;
        let mut process = self.process.lock().await;
        process.terminate_tree().await;
        let _ = tokio::time::timeout(SHUTDOWN_WAIT, process.wait()).await;
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    alive: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(message);
                }
            }
        }
    }
    alive.store(false, Ordering::Release);
    pending.lock().await.clear();
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, state: Option<SharedState>) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let mut reported = 0usize;
    let mut suppression_reported = false;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
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
                    if trimmed.chars().count() > MAX_DIAGNOSTIC_CHARS {
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
    }
}

fn npx_program() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_uses_pinned_devtools_protocol() {
        assert_eq!(DEVTOOLS_PROTOCOL_VERSION, "2025-03-26");
    }
}
