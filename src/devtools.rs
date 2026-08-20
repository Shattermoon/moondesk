use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::browser::DetectedBrowser;
use crate::state::{ServerUiEvent, SharedState, UiEventSender};

const CHROME_DEVTOOLS_MCP_VERSION: &str = "1.7.0";
const MAX_DEVTOOLS_DIAGNOSTIC_LINES: usize = 100;
const MAX_DEVTOOLS_DIAGNOSTIC_CHARS: usize = 2_048;
const DEVTOOLS_RESTART_COOLDOWN: Duration = Duration::from_secs(10);
const DEVTOOLS_RESTART_INIT_TIMEOUT: Duration = Duration::from_secs(30);

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

fn restart_cooldown_remaining(last_failure: Option<Instant>, now: Instant) -> Option<Duration> {
    let failed_at = last_failure?;
    let elapsed = now.saturating_duration_since(failed_at);
    (elapsed < DEVTOOLS_RESTART_COOLDOWN).then(|| DEVTOOLS_RESTART_COOLDOWN.saturating_sub(elapsed))
}

/// A running chrome-devtools-mcp child process with stdin/stdout JSON-RPC bridge.
pub struct DevtoolsBridge {
    child: Mutex<Child>,
    stdin: Mutex<tokio::io::BufWriter<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<std::collections::HashMap<Value, tokio::sync::oneshot::Sender<Value>>>>,
    initialization: Arc<InitializationState>,
    initialization_lock: Mutex<()>,
    next_request_id: AtomicU64,
    alive: Arc<AtomicBool>,
}

pub struct DevtoolsManager {
    selected_browser: Option<DetectedBrowser>,
    ui_events: UiEventSender,
    state: SharedState,
    bridge: Mutex<Option<Arc<DevtoolsBridge>>>,
    restart_lock: Mutex<()>,
    initialization_request: Mutex<Option<Value>>,
    last_restart_failure: Mutex<Option<Instant>>,
    shutting_down: AtomicBool,
    generation: Arc<AtomicU64>,
}

impl DevtoolsBridge {
    /// Spawn the tested chrome-devtools-mcp version and set up the stdio bridge.
    async fn start(
        selected_browser: Option<&DetectedBrowser>,
        ui_events: UiEventSender,
        state: SharedState,
        generation_counter: Arc<AtomicU64>,
        generation: u64,
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
        let alive = Arc::new(AtomicBool::new(true));
        let bridge = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            initialization: initialization.clone(),
            initialization_lock: Mutex::new(()),
            next_request_id: AtomicU64::new(1),
            alive: alive.clone(),
        });

        // Spawn stdout reader task. EOF means the child is no longer usable: clear
        // pending requests so their receivers fail immediately and update the TUI.
        let pending_clone = pending;
        let stdout_state = state;
        let stdout_alive = alive;
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
            stdout_alive.store(false, Ordering::Release);
            initialization.reset();
            pending_clone.lock().await.clear();
            if generation_counter.load(Ordering::Acquire) == generation {
                stdout_state.lock().await.devtools_running = false;
            }
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
            self.alive.store(false, Ordering::Release);
            self.initialization.reset();
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
                self.alive.store(false, Ordering::Release);
                self.initialization.reset();
                self.pending.lock().await.remove(&internal_id);
                return Err("Response channel closed".into());
            }
            Err(_) => {
                self.alive.store(false, Ordering::Release);
                self.initialization.reset();
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
                    .map_err(|e| format!("stdin write: {e}"))?;
                stdin.flush().await.map_err(|e| format!("stdin flush: {e}"))
            }
            .await
        };
        if write_result.is_err() {
            self.alive.store(false, Ordering::Release);
            self.initialization.reset();
        }
        write_result
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Stop the shared bridge explicitly so MoonDesk never leaves the npx child
    /// alive after the host exits.
    pub async fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        self.initialization.reset();
        self.pending.lock().await.clear();
        let _ = self.stdin.lock().await.shutdown().await;
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    }
}

impl DevtoolsManager {
    pub async fn start(
        selected_browser: Option<&DetectedBrowser>,
        ui_events: UiEventSender,
        state: SharedState,
    ) -> Result<Arc<Self>, String> {
        let manager = Arc::new(Self {
            selected_browser: selected_browser.cloned(),
            ui_events,
            state,
            bridge: Mutex::new(None),
            restart_lock: Mutex::new(()),
            initialization_request: Mutex::new(None),
            last_restart_failure: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            generation: Arc::new(AtomicU64::new(0)),
        });
        let bridge = manager.spawn_bridge().await?;
        *manager.bridge.lock().await = Some(bridge);
        Ok(manager)
    }

    async fn spawn_bridge(&self) -> Result<Arc<DevtoolsBridge>, String> {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        DevtoolsBridge::start(
            self.selected_browser.as_ref(),
            self.ui_events.clone(),
            self.state.clone(),
            self.generation.clone(),
            generation,
        )
        .await
    }

    async fn bridge_or_restart(&self) -> Result<Arc<DevtoolsBridge>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("chrome-devtools-mcp is shutting down".to_string());
        }
        if let Some(bridge) = self.bridge.lock().await.clone()
            && bridge.is_alive()
        {
            return Ok(bridge);
        }

        let _restart_guard = self.restart_lock.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("chrome-devtools-mcp is shutting down".to_string());
        }
        if let Some(bridge) = self.bridge.lock().await.clone()
            && bridge.is_alive()
        {
            return Ok(bridge);
        }
        if let Some(remaining) =
            restart_cooldown_remaining(*self.last_restart_failure.lock().await, Instant::now())
        {
            return Err(format!(
                "chrome-devtools-mcp restart is cooling down after a failure; retry in about {} seconds",
                remaining.as_secs().max(1)
            ));
        }

        let previous = self.bridge.lock().await.take();
        if let Some(previous) = previous {
            previous.stop().await;
        }

        match self.spawn_bridge().await {
            Ok(bridge) => {
                if let Some(init_req) = self.initialization_request.lock().await.clone() {
                    let init_result = tokio::time::timeout(
                        DEVTOOLS_RESTART_INIT_TIMEOUT,
                        bridge.ensure_initialized(&init_req),
                    )
                    .await;
                    let init_error = match init_result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error),
                        Err(_) => Some(format!(
                            "timed out after {} seconds",
                            DEVTOOLS_RESTART_INIT_TIMEOUT.as_secs()
                        )),
                    };
                    if let Some(error) = init_error {
                        *self.last_restart_failure.lock().await = Some(Instant::now());
                        bridge.stop().await;
                        let mut app = self.state.lock().await;
                        app.devtools_running = false;
                        app.log(
                            "ERROR",
                            format!("chrome-devtools-mcp restart initialization failed: {error}"),
                        );
                        return Err(format!(
                            "chrome-devtools-mcp restart initialization failed: {error}"
                        ));
                    }
                }
                *self.last_restart_failure.lock().await = None;
                *self.bridge.lock().await = Some(bridge.clone());
                let mut app = self.state.lock().await;
                app.devtools_running = true;
                app.log(
                    "INFO",
                    "chrome-devtools-mcp restarted after child exit".into(),
                );
                Ok(bridge)
            }
            Err(error) => {
                *self.last_restart_failure.lock().await = Some(Instant::now());
                let mut app = self.state.lock().await;
                app.devtools_running = false;
                app.log(
                    "ERROR",
                    format!("chrome-devtools-mcp restart failed: {error}"),
                );
                Err(error)
            }
        }
    }

    fn recover_after_failure(
        &self,
        bridge: &Arc<DevtoolsBridge>,
        original_error: String,
    ) -> String {
        if bridge.is_alive() || self.shutting_down.load(Ordering::Acquire) {
            return original_error;
        }
        format!(
            "{original_error}; chrome-devtools-mcp stopped unexpectedly; the next browser request will attempt a restart"
        )
    }

    pub async fn ensure_initialized(&self, init_req: &Value) -> Result<(), String> {
        let bridge = self.bridge_or_restart().await?;
        match bridge.ensure_initialized(init_req).await {
            Ok(()) => {
                *self.initialization_request.lock().await = Some(init_req.clone());
                Ok(())
            }
            Err(error) if !bridge.is_alive() && !self.shutting_down.load(Ordering::Acquire) => {
                let replacement = self.bridge_or_restart().await.map_err(|restart_error| {
                    format!(
                        "{error}; chrome-devtools-mcp restart failed during initialization: {restart_error}"
                    )
                })?;
                let retry_result = tokio::time::timeout(
                    DEVTOOLS_RESTART_INIT_TIMEOUT,
                    replacement.ensure_initialized(init_req),
                )
                .await;
                match retry_result {
                    Ok(Ok(())) => {
                        *self.last_restart_failure.lock().await = None;
                        *self.initialization_request.lock().await = Some(init_req.clone());
                        Ok(())
                    }
                    Ok(Err(retry_error)) => {
                        *self.last_restart_failure.lock().await = Some(Instant::now());
                        replacement.stop().await;
                        self.state.lock().await.devtools_running = false;
                        Err(format!(
                            "{error}; chrome-devtools-mcp restarted but initialization retry failed: {retry_error}"
                        ))
                    }
                    Err(_) => {
                        *self.last_restart_failure.lock().await = Some(Instant::now());
                        replacement.stop().await;
                        self.state.lock().await.devtools_running = false;
                        Err(format!(
                            "{error}; chrome-devtools-mcp restarted but initialization retry timed out after {} seconds",
                            DEVTOOLS_RESTART_INIT_TIMEOUT.as_secs()
                        ))
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    pub async fn request(&self, req: &Value) -> Result<Value, String> {
        let bridge = self.bridge_or_restart().await?;
        match bridge.request(req).await {
            Ok(response) => Ok(response),
            Err(error) => Err(self.recover_after_failure(&bridge, error)),
        }
    }

    pub async fn stop(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(bridge) = self.bridge.lock().await.take() {
            bridge.stop().await;
        }
        self.state.lock().await.devtools_running = false;
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

    #[test]
    fn restart_cooldown_blocks_only_the_configured_failure_window() {
        let failed_at = Instant::now();
        assert_eq!(
            restart_cooldown_remaining(Some(failed_at), failed_at + Duration::from_secs(4)),
            Some(Duration::from_secs(6))
        );
        assert_eq!(
            restart_cooldown_remaining(Some(failed_at), failed_at + DEVTOOLS_RESTART_COOLDOWN),
            None
        );
        assert_eq!(restart_cooldown_remaining(None, failed_at), None);
    }
}
