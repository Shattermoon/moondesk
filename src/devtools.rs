use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc::UnboundedSender};

use crate::browser::DetectedBrowser;
use crate::state::ServerUiEvent;

const CHROME_DEVTOOLS_MCP_VERSION: &str = "1.7.0";
const MAX_DEVTOOLS_DIAGNOSTIC_LINES: usize = 100;
const MAX_DEVTOOLS_DIAGNOSTIC_CHARS: usize = 2_048;

/// A running chrome-devtools-mcp child process with stdin/stdout JSON-RPC bridge.
pub struct DevtoolsBridge {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::io::BufWriter<tokio::process::ChildStdin>,
    pending: Arc<Mutex<std::collections::HashMap<Value, tokio::sync::oneshot::Sender<Value>>>>,
}

impl DevtoolsBridge {
    /// Spawn the tested chrome-devtools-mcp version and set up the stdio bridge.
    pub async fn start(
        selected_browser: Option<&DetectedBrowser>,
        ui_events: UnboundedSender<ServerUiEvent>,
    ) -> Result<Arc<Mutex<Self>>, String> {
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

        let bridge = Arc::new(Mutex::new(Self {
            child,
            stdin,
            pending: pending.clone(),
        }));

        // Spawn stdout reader task. EOF means the child is no longer usable: clear
        // pending requests so their receivers fail immediately and update the TUI.
        let pending_clone = pending;
        let stdout_ui_events = ui_events.clone();
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
            pending_clone.lock().await.clear();
            let _ = stdout_ui_events.send(ServerUiEvent::SetDevtoolsRunning(false));
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

    /// Send a JSON-RPC request and wait for the response.
    pub async fn request(&mut self, req: &Value) -> Result<Value, String> {
        // Register the response channel *before* writing the request. A fast child
        // can answer immediately after flush; registering afterwards creates a race
        // where the reader discards a valid response and the caller waits 120s.
        let pending = if let Some(id) = req.get("id").cloned() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.pending.lock().await.insert(id.clone(), tx);
            Some((id, rx))
        } else {
            None
        };

        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let write_result = async {
            self.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("stdin write: {e}"))?;
            self.stdin
                .write_all(b"\n")
                .await
                .map_err(|e| format!("stdin write newline: {e}"))?;
            self.stdin
                .flush()
                .await
                .map_err(|e| format!("stdin flush: {e}"))
        }
        .await;

        if let Err(error) = write_result {
            if let Some((id, _)) = &pending {
                self.pending.lock().await.remove(id);
            }
            return Err(error);
        }

        let Some((id, rx)) = pending else {
            return Ok(Value::Null);
        };
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err("Response channel closed".into())
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("Request timed out (120s)".into())
            }
        }
    }

    /// Send a notification (no id, no response expected).
    pub async fn notify(&mut self, req: &Value) -> Result<(), String> {
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("stdin write: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("stdin write: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("stdin flush: {e}"))?;
        Ok(())
    }

    /// Kill the child process.
    #[allow(dead_code)]
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}
