use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::browser::DetectedBrowser;
use crate::state::{ServerUiEvent, SharedState, UiEventSender};

const CHROME_DEVTOOLS_MCP_VERSION: &str = "1.7.0";
const MAX_DEVTOOLS_DIAGNOSTIC_LINES: usize = 100;
const MAX_DEVTOOLS_DIAGNOSTIC_CHARS: usize = 2_048;

#[derive(Default)]
struct InitializationState {
    initialized: AtomicBool,
}

impl InitializationState {
    fn needs_initialization(&self) -> bool {
        !self.initialized.load(Ordering::Acquire)
    }

    fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    fn reset(&self) {
        self.initialized.store(false, Ordering::Release);
    }
}

fn internal_request_id(sequence: u64) -> Value {
    Value::String(format!(
        "moondesk-devtools-{}-{sequence}",
        std::process::id()
    ))
}

/// A running chrome-devtools-mcp child process with stdin/stdout JSON-RPC bridge.
pub struct DevtoolsBridge {
    child: Mutex<Child>,
    stdin: Mutex<tokio::io::BufWriter<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<std::collections::HashMap<Value, tokio::sync::oneshot::Sender<Value>>>>,
    initialization: Arc<InitializationState>,
    initialization_lock: Mutex<()>,
    next_request_id: AtomicU64,
}

impl DevtoolsBridge {
    /// Spawn the tested chrome-devtools-mcp version and set up the stdio bridge.
    pub async fn start(
        selected_browser: Option<&DetectedBrowser>,
        ui_events: UiEventSender,
        state: SharedState,
    ) -> Result<Arc<Self>, String> {
        let mut command = Command::new("npx");
        let package = format!("chrome-devtools-mcp@{CHROME_DEVTOOLS_MCP_VERSION}");
        command.args(["-y", package.as_str()]);

        if let Some(browser) = selected_browser {
            if browser.remote_debug_active {
                if let Some(target) = browser.remote_debug_target.as_deref() {
                    if target == "pipe" {
                        command.args(["--executablePath", &browser.path]);
                    } else {
                        command.args(["--browserUrl", &format!("http://{target}")]);
                    }
                } else {
                    command.args(["--executablePath", &browser.path]);
                }
            } else {
                command.args(["--executablePath", &browser.path]);
            }
        }

        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn chrome-devtools-mcp: {e}"))?;

        let child_stdin = child.stdin.take().ok_or("No stdin")?;
        let child_stdout = child.stdout.take().ok_or("No stdout")?;
        let child_stderr = child.stderr.take().ok_or("No stderr")?;

        let stdin = tokio::io::BufWriter::new(child_stdin);
        let pending: Arc<
            Mutex<std::collections::HashMap<Value, tokio::sync::oneshot::Sender<Value>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let initialization = Arc::new(InitializationState::default());
        let bridge = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            initialization: initialization.clone(),
            initialization_lock: Mutex::new(()),
            next_request_id: AtomicU64::new(1),
        });

        // Spawn stdout reader task. EOF means the child is no longer usable: clear
        // pending requests so their receivers fail immediately and update the TUI.
        let pending_clone = pending;
        let stdout_state = state;
        tokio::spawn(async move {
            let mut reader = BufReader::new(child_stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(msg) = serde_json::from_str::<Value>(trimmed)
                            && let Some(id) = msg.get("id").cloned()
                        {
                            let mut map = pending_clone.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            initialization.reset();
            pending_clone.lock().await.clear();
            stdout_state.lock().await.devtools_running = false;
        });

        // Drain stderr so diagnostics cannot block the child. Surface a bounded
        // number of bounded lines in the TUI and keep reading after that.
        tokio::spawn(async move {
            let mut reader = BufReader::new(child_stderr);
            let mut line = String::new();
            let mut reported = 0usize;
            let mut suppression_reported = false;
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if reported < MAX_DEVTOOLS_DIAGNOSTIC_LINES {
                            let mut message = trimmed
                                .chars()
                                .take(MAX_DEVTOOLS_DIAGNOSTIC_CHARS)
                                .collect::<String>();
                            if trimmed.chars().count() > MAX_DEVTOOLS_DIAGNOSTIC_CHARS {
                                message.push('…');
                            }
                            let _ = ui_events.send(ServerUiEvent::Log {
                                workspace_id: None,
                                level: "WARN",
                                message: format!("chrome-devtools-mcp: {message}"),
                            });
                            reported += 1;
                        } else if !suppression_reported {
                            let _ = ui_events.send(ServerUiEvent::Log {
                                workspace_id: None,
                                level: "WARN",
                                message:
                                    "chrome-devtools-mcp: additional stderr diagnostics suppressed"
                                        .to_string(),
                            });
                            suppression_reported = true;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(bridge)
    }

    /// Initialize the shared chrome-devtools-mcp child at most once while it is alive.
    /// The initialization mutex only serializes the handshake itself; normal tool
    /// requests do not hold it and can wait for independent responses concurrently.
    pub async fn ensure_initialized(&self, init_req: &Value) -> Result<(), String> {
        if !self.initialization.needs_initialization() {
            return Ok(());
        }
        let _guard = self.initialization_lock.lock().await;
        if !self.initialization.needs_initialization() {
            return Ok(());
        }
        self.request(init_req).await?;
        self.notify(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await?;
        self.initialization.mark_initialized();
        Ok(())
    }

    fn next_internal_request_id(&self) -> Value {
        let sequence = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        internal_request_id(sequence)
    }

    /// Send a JSON-RPC request and wait for the response. Client-provided IDs are
    /// remapped so different ChatGPT workspace connectors can safely reuse the same
    /// JSON-RPC correlation ID without colliding in the shared DevTools child.
    pub async fn request(&self, req: &Value) -> Result<Value, String> {
        let mut outbound = req.clone();
        let original_id = req.get("id").cloned();
        let pending = if original_id.is_some() {
            let internal_id = self.next_internal_request_id();
            let object = outbound
                .as_object_mut()
                .ok_or_else(|| "DevTools JSON-RPC request must be an object".to_string())?;
            object.insert("id".to_string(), internal_id.clone());
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.pending.lock().await.insert(internal_id.clone(), tx);
            Some((internal_id, rx))
        } else {
            None
        };

        let line = serde_json::to_string(&outbound).map_err(|e| e.to_string())?;
        let write_result = {
            let mut stdin = self.stdin.lock().await;
            async {
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| format!("stdin write: {e}"))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| format!("stdin write newline: {e}"))?;
                stdin.flush().await.map_err(|e| format!("stdin flush: {e}"))
            }
            .await
        };

        if let Err(error) = write_result {
            if let Some((id, _)) = &pending {
                self.pending.lock().await.remove(id);
            }
            return Err(error);
        }

        let Some((internal_id, rx)) = pending else {
            return Ok(Value::Null);
        };
        let mut response = match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&internal_id);
                return Err("Response channel closed".into());
            }
            Err(_) => {
                self.pending.lock().await.remove(&internal_id);
                return Err("Request timed out (120s)".into());
            }
        };
        if let Some(original_id) = original_id
            && let Some(object) = response.as_object_mut()
        {
            object.insert("id".to_string(), original_id);
        }
        Ok(response)
    }

    /// Send a notification (no id, no response expected).
    pub async fn notify(&self, req: &Value) -> Result<(), String> {
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("stdin write: {e}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("stdin write: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("stdin flush: {e}"))?;
        Ok(())
    }

    /// Stop the shared bridge explicitly so MoonDesk never leaves the npx child
    /// alive after the host exits.
    pub async fn stop(&self) {
        self.initialization.reset();
        self.pending.lock().await.clear();
        let _ = self.stdin.lock().await.shutdown().await;
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_state_allows_one_handshake_until_reset() {
        let state = InitializationState::default();
        let mut handshakes = 0;
        for _ in 0..3 {
            if state.needs_initialization() {
                handshakes += 1;
                state.mark_initialized();
            }
        }
        assert_eq!(handshakes, 1);
        state.reset();
        assert!(state.needs_initialization());
    }

    #[test]
    fn internal_devtools_request_ids_are_unique_and_do_not_reuse_client_ids() {
        assert_ne!(internal_request_id(1), internal_request_id(2));
        assert_ne!(internal_request_id(1), Value::String("1".into()));
    }
}
