use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::process_runner::{SpawnedProcess, spawn_owned_program};
use crate::state::{BrowserPresentation, SharedState};

const DEVTOOLS_ACTIVE_PORT_FILE: &str = "DevToolsActivePort";
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const MAX_EVENT_HISTORY: usize = 5_000;
const DEFAULT_EVENT_PAGE_SIZE: usize = 50;
const MAX_EVENT_PAGE_SIZE: usize = 200;
const MAX_TRACE_BYTES: usize = 256 * 1024 * 1024;
const MAX_HEAP_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_VIEWPORT_WIDTH: u64 = 1_280;
const DEFAULT_VIEWPORT_HEIGHT: u64 = 800;

type BrowserWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type BrowserWebSocketWriter = SplitSink<BrowserWebSocket, Message>;
type PendingCdpResponse = oneshot::Sender<Result<Value, BrowserTransportError>>;

#[derive(Debug)]
pub enum BrowserTransportError {
    Timeout,
    Disconnected(String),
    Protocol(String),
}

impl fmt::Display for BrowserTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "browser CDP request timed out"),
            Self::Disconnected(message) | Self::Protocol(message) => f.write_str(message),
        }
    }
}

#[derive(Clone, Debug)]
struct CdpEvent {
    id: u64,
    session_id: Option<String>,
    method: String,
    params: Value,
}

#[derive(Debug)]
struct CdpEventCapture {
    session_id: Option<String>,
    methods: HashSet<String>,
    max_bytes: usize,
    captured_bytes: usize,
    overflowed: bool,
    events: Vec<CdpEvent>,
}

struct CdpConnection {
    writer: Mutex<BrowserWebSocketWriter>,
    pending: Arc<Mutex<HashMap<u64, PendingCdpResponse>>>,
    next_request_id: AtomicU64,
    next_capture_id: AtomicU64,
    alive: Arc<AtomicBool>,
    events: Arc<Mutex<VecDeque<CdpEvent>>>,
    captures: Arc<Mutex<HashMap<u64, CdpEventCapture>>>,
}

impl CdpConnection {
    async fn connect(
        websocket_url: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<Self>, BrowserTransportError> {
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }
        let connect = connect_async(websocket_url);
        let (socket, _) = tokio::time::timeout_at(deadline, connect)
            .await
            .map_err(|_| BrowserTransportError::Timeout)?
            .map_err(|error| {
                BrowserTransportError::Disconnected(format!(
                    "Could not connect to MoonDesk Chromium DevTools endpoint: {error}"
                ))
            })?;
        let (writer, reader) = socket.split();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let captures = Arc::new(Mutex::new(HashMap::new()));
        let next_event_id = Arc::new(AtomicU64::new(1));
        let connection = Arc::new(Self {
            writer: Mutex::new(writer),
            pending: pending.clone(),
            next_request_id: AtomicU64::new(1),
            next_capture_id: AtomicU64::new(1),
            alive: alive.clone(),
            events: events.clone(),
            captures: captures.clone(),
        });
        tokio::spawn(read_cdp_messages(
            reader,
            pending,
            alive,
            events,
            captures,
            next_event_id,
        ));
        Ok(connection)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        if !self.is_alive() {
            return Err(BrowserTransportError::Disconnected(
                "MoonDesk Chromium DevTools connection is not running".to_string(),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_string());
        }

        let (tx, rx) = oneshot::channel();
        tokio::time::timeout_at(deadline, self.pending.lock())
            .await
            .map_err(|_| BrowserTransportError::Timeout)?
            .insert(id, tx);

        let encoded = serde_json::to_string(&request).map_err(|error| {
            BrowserTransportError::Protocol(format!(
                "Could not encode browser CDP request: {error}"
            ))
        })?;
        let write_result = tokio::time::timeout_at(deadline, async {
            let mut writer = self.writer.lock().await;
            writer.send(Message::Text(encoded.into())).await
        })
        .await;
        match write_result {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                self.alive.store(false, Ordering::Release);
                return Err(BrowserTransportError::Timeout);
            }
            Ok(Err(error)) => {
                self.pending.lock().await.remove(&id);
                self.alive.store(false, Ordering::Release);
                return Err(BrowserTransportError::Disconnected(format!(
                    "Could not write browser CDP request: {error}"
                )));
            }
            Ok(Ok(())) => {}
        }

        let response = match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => {
                self.alive.store(false, Ordering::Release);
                return Err(BrowserTransportError::Disconnected(
                    "MoonDesk Chromium CDP response channel closed".to_string(),
                ));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(BrowserTransportError::Timeout);
            }
        };

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32_000);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown Chrome DevTools Protocol error");
            return Err(BrowserTransportError::Protocol(format!(
                "Chrome DevTools Protocol error {code}: {message}"
            )));
        }
        response.get("result").cloned().ok_or_else(|| {
            BrowserTransportError::Protocol(
                "Chrome DevTools Protocol returned a response without result or error".to_string(),
            )
        })
    }

    async fn events_for_session(&self, session_id: &str) -> Vec<CdpEvent> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session_id))
            .cloned()
            .collect()
    }

    async fn start_event_capture(
        &self,
        session_id: Option<&str>,
        methods: &[&str],
        max_bytes: usize,
    ) -> u64 {
        let id = self.next_capture_id.fetch_add(1, Ordering::Relaxed);
        self.captures.lock().await.insert(
            id,
            CdpEventCapture {
                session_id: session_id.map(str::to_string),
                methods: methods.iter().map(|method| (*method).to_string()).collect(),
                max_bytes,
                captured_bytes: 0,
                overflowed: false,
                events: Vec::new(),
            },
        );
        id
    }

    async fn capture_has_method(&self, id: u64, method: &str) -> bool {
        self.captures
            .lock()
            .await
            .get(&id)
            .is_some_and(|capture| capture.events.iter().any(|event| event.method == method))
    }

    async fn wait_for_captured_method(
        &self,
        id: u64,
        method: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        loop {
            if self.capture_has_method(id, method).await {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserTransportError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn finish_event_capture(
        &self,
        id: u64,
    ) -> Result<CdpEventCapture, BrowserTransportError> {
        self.captures.lock().await.remove(&id).ok_or_else(|| {
            BrowserTransportError::Protocol(format!(
                "MoonDesk CDP event capture {id} was not found"
            ))
        })
    }

    async fn cancel_event_capture(&self, id: u64) {
        self.captures.lock().await.remove(&id);
    }
}

async fn read_cdp_messages(
    mut reader: SplitStream<BrowserWebSocket>,
    pending: Arc<Mutex<HashMap<u64, PendingCdpResponse>>>,
    alive: Arc<AtomicBool>,
    events: Arc<Mutex<VecDeque<CdpEvent>>>,
    captures: Arc<Mutex<HashMap<u64, CdpEventCapture>>>,
    next_event_id: Arc<AtomicU64>,
) {
    let failure = loop {
        let Some(message) = reader.next().await else {
            break "MoonDesk Chromium DevTools WebSocket closed".to_string();
        };
        let message = match message {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(error) => break format!("Browser CDP emitted invalid UTF-8: {error}"),
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Close(_)) => {
                break "MoonDesk Chromium closed the DevTools connection".to_string();
            }
            Ok(Message::Frame(_)) => continue,
            Err(error) => break format!("Could not read browser CDP response: {error}"),
        };
        let value: Value = match serde_json::from_str(&message) {
            Ok(value) => value,
            Err(error) => break format!("Browser CDP emitted invalid JSON: {error}"),
        };
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            if let Some(tx) = pending.lock().await.remove(&id) {
                let _ = tx.send(Ok(value));
            }
            continue;
        }
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            continue;
        };
        let event = CdpEvent {
            id: next_event_id.fetch_add(1, Ordering::Relaxed),
            session_id: value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or_else(|| json!({})),
        };
        {
            let mut active_captures = captures.lock().await;
            for capture in active_captures.values_mut() {
                if !capture.methods.contains(&event.method)
                    || (capture.overflowed && event.method != "Tracing.tracingComplete")
                    || capture
                        .session_id
                        .as_deref()
                        .is_some_and(|session_id| event.session_id.as_deref() != Some(session_id))
                {
                    continue;
                }
                let encoded_bytes = serde_json::to_vec(&event.params)
                    .map_or(0, |encoded| encoded.len())
                    .saturating_add(event.method.len())
                    .saturating_add(event.session_id.as_deref().map_or(0, str::len));
                if capture.captured_bytes.saturating_add(encoded_bytes) > capture.max_bytes {
                    capture.overflowed = true;
                    continue;
                }
                capture.captured_bytes = capture.captured_bytes.saturating_add(encoded_bytes);
                capture.events.push(event.clone());
            }
        }
        {
            let mut history = events.lock().await;
            history.push_back(event.clone());
            while history.len() > MAX_EVENT_HISTORY {
                history.pop_front();
            }
        }
    };

    alive.store(false, Ordering::Release);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(BrowserTransportError::Disconnected(failure.clone())));
    }
}

#[derive(Clone, Debug)]
struct CdpElement {
    backend_node_id: u64,
}

#[derive(Clone, Debug)]
struct CdpPage {
    target_id: String,
    context_id: String,
    context_name: String,
    session_id: Option<String>,
    url: String,
    title: String,
    snapshot_generation: u64,
    elements: HashMap<String, CdpElement>,
}

#[derive(Clone, Debug)]
struct CdpTraceRecording {
    capture_id: u64,
}

#[derive(Default)]
struct CdpBrowserState {
    contexts_by_name: HashMap<String, String>,
    context_names_by_id: HashMap<String, String>,
    pages_by_id: BTreeMap<u64, CdpPage>,
    target_to_page_id: HashMap<String, u64>,
    next_page_id: u64,
    selected_page: Option<u64>,
}

impl CdpBrowserState {
    fn next_page_id(&mut self) -> u64 {
        if self.next_page_id == 0 {
            self.next_page_id = 1;
        }
        let id = self.next_page_id;
        self.next_page_id = self.next_page_id.saturating_add(1);
        id
    }
}

pub struct BrowserCdpTransport {
    process: Mutex<SpawnedProcess>,
    runtime_cwd: PathBuf,
    presentation: BrowserPresentation,
    connection: Arc<CdpConnection>,
    state: Mutex<CdpBrowserState>,
    trace: Mutex<Option<CdpTraceRecording>>,
    browser_name: String,
    alive: Arc<AtomicBool>,
}

impl BrowserCdpTransport {
    pub async fn start(
        presentation: BrowserPresentation,
        app_state: Option<SharedState>,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<Self>, BrowserTransportError> {
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }

        let (browser_path, browser_name) = resolve_browser_executable().await.map_err(|error| {
            BrowserTransportError::Disconnected(format!(
                "Could not resolve MoonDesk's Chromium browser: {error}"
            ))
        })?;
        let runtime_cwd =
            create_private_runtime_dir("moondesk-browser-runtime").map_err(|error| {
                BrowserTransportError::Disconnected(format!(
                    "Failed to create MoonDesk browser runtime directory: {error}"
                ))
            })?;
        let profile_dir = runtime_cwd.join("profile");
        if let Err(error) = std::fs::create_dir_all(&profile_dir) {
            let _ = std::fs::remove_dir_all(&runtime_cwd);
            return Err(BrowserTransportError::Disconnected(format!(
                "Failed to create MoonDesk browser profile: {error}"
            )));
        }
        set_private_dir_permissions(&profile_dir);

        let mut command = Command::new(&browser_path);
        command
            .arg("--remote-debugging-port=0")
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--metrics-recording-only",
                "--disable-default-apps",
                "--disable-features=Translate",
                "--disable-breakpad",
            ]);
        if presentation.is_headless() {
            command
                .arg("--headless=new")
                .arg("--no-startup-window")
                .arg(format!(
                    "--window-size={DEFAULT_VIEWPORT_WIDTH},{DEFAULT_VIEWPORT_HEIGHT}"
                ));
        } else {
            command.arg("about:blank");
        }
        command.current_dir(&runtime_cwd);

        let mut process = match spawn_owned_program(command) {
            Ok(process) => process,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&runtime_cwd);
                return Err(BrowserTransportError::Disconnected(format!(
                    "Failed to start MoonDesk Chromium at {}: {error}",
                    browser_path.display()
                )));
            }
        };

        if let Some(stdout) = process.take_stdout() {
            tokio::spawn(drain_browser_stream(stdout, app_state.clone(), "stdout"));
        }
        if let Some(stderr) = process.take_stderr() {
            tokio::spawn(drain_browser_stream(stderr, app_state, "stderr"));
        }

        let websocket_url = match wait_for_devtools_endpoint(&profile_dir, deadline).await {
            Ok(url) => url,
            Err(error) => {
                process.terminate_tree().await;
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, process.wait()).await;
                let _ = std::fs::remove_dir_all(&runtime_cwd);
                return Err(error);
            }
        };
        let connection = match CdpConnection::connect(&websocket_url, deadline).await {
            Ok(connection) => connection,
            Err(error) => {
                process.terminate_tree().await;
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, process.wait()).await;
                let _ = std::fs::remove_dir_all(&runtime_cwd);
                return Err(error);
            }
        };
        let alive = Arc::new(AtomicBool::new(true));
        let transport = Arc::new(Self {
            process: Mutex::new(process),
            runtime_cwd,
            presentation,
            connection,
            state: Mutex::new(CdpBrowserState::default()),
            trace: Mutex::new(None),
            browser_name,
            alive,
        });

        // Headless Chromium can run without a default-context page. Headed Chromium on Windows
        // needs its startup window to remain alive or an immediate Target.createTarget in an
        // isolated BrowserContext can fail with "Failed to open a new tab". The default-context
        // target is never admitted to MoonDesk's managed page registry, so retaining it in visible
        // mode does not grant any caller tab authority.
        if presentation.is_headless() {
            transport.close_unowned_startup_pages(deadline).await?;
        }
        Ok(transport)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.connection.is_alive()
    }

    pub fn browser_name(&self) -> &str {
        &self.browser_name
    }

    pub async fn managed_context_names(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .contexts_by_name
            .keys()
            .cloned()
            .collect()
    }

    pub async fn browser_contexts_match_managed(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<bool, BrowserTransportError> {
        let managed_context_ids = {
            let state = self.state.lock().await;
            state
                .context_names_by_id
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
        let contexts = self
            .connection
            .call("Target.getBrowserContexts", json!({}), None, deadline)
            .await?;
        let actual_context_ids = contexts
            .get("browserContextIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();
        Ok(actual_context_ids == managed_context_ids)
    }

    pub async fn has_unmanaged_page_targets(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<bool, BrowserTransportError> {
        let managed_context_ids = {
            let state = self.state.lock().await;
            state
                .context_names_by_id
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
        let settle_deadline = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(750),
        );
        loop {
            let targets = self
                .connection
                .call("Target.getTargets", json!({}), None, deadline)
                .await?;
            let unmanaged_pages = targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|target| {
                    target.get("type").and_then(Value::as_str) == Some("page")
                        && !target
                            .get("browserContextId")
                            .and_then(Value::as_str)
                            .is_some_and(|context_id| managed_context_ids.contains(context_id))
                })
                .collect::<Vec<_>>();
            if unmanaged_pages.is_empty() {
                return Ok(false);
            }
            if tokio::time::Instant::now() >= settle_deadline {
                return Ok(true);
            }
            tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
        }
    }

    #[cfg(all(test, windows))]
    pub async fn pid(&self) -> Option<u32> {
        self.process.lock().await.pid()
    }

    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);
        {
            let mut process = self.process.lock().await;
            process.terminate_tree().await;
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, process.wait()).await;
        }
        let _ = std::fs::remove_dir_all(&self.runtime_cwd);
    }

    pub async fn dispose_context(
        &self,
        context_name: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        let context_id = {
            let state = self.state.lock().await;
            state.contexts_by_name.get(context_name).cloned()
        };
        let Some(context_id) = context_id else {
            return Ok(());
        };
        self.connection
            .call(
                "Target.disposeBrowserContext",
                json!({ "browserContextId": context_id }),
                None,
                deadline,
            )
            .await?;
        let mut state = self.state.lock().await;
        state.contexts_by_name.remove(context_name);
        state.context_names_by_id.remove(&context_id);
        let removed = state
            .pages_by_id
            .iter()
            .filter_map(|(page_id, page)| (page.context_id == context_id).then_some(*page_id))
            .collect::<Vec<_>>();
        for page_id in removed {
            if let Some(page) = state.pages_by_id.remove(&page_id) {
                state.target_to_page_id.remove(&page.target_id);
            }
            if state.selected_page == Some(page_id) {
                state.selected_page = None;
            }
        }
        Ok(())
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        if !self.is_alive() {
            return Err(BrowserTransportError::Disconnected(
                "MoonDesk Chromium browser is not running".to_string(),
            ));
        }
        let result = match name {
            "list_pages" => self.list_pages(deadline).await,
            "new_page" => self.new_page(&arguments, deadline).await,
            "select_page" => self.select_page(&arguments, deadline).await,
            "close_page" => self.close_page(&arguments, deadline).await,
            "navigate_page" => self.navigate_page(&arguments, deadline).await,
            "take_snapshot" => self.take_snapshot(&arguments, deadline).await,
            "click" => self.click_element(&arguments, deadline).await,
            "click_at" => self.click_at(&arguments, deadline).await,
            "fill" => self.fill_element(&arguments, deadline).await,
            "hover" => self.hover_element(&arguments, deadline).await,
            "drag" => self.drag_element(&arguments, deadline).await,
            "evaluate_script" => self.evaluate_script(&arguments, deadline).await,
            "upload_file" => self.upload_file(&arguments, deadline).await,
            "press_key" => self.press_key(&arguments, deadline).await,
            "type_text" => self.type_text(&arguments, deadline).await,
            "scroll" => self.scroll(&arguments, deadline).await,
            "resize_page" => self.resize_page(&arguments, deadline).await,
            "emulate" => self.emulate(&arguments, deadline).await,
            "take_screenshot" => self.take_screenshot(&arguments, deadline).await,
            "wait_for" => self.wait_for(&arguments, deadline).await,
            "handle_dialog" => self.handle_dialog(&arguments, deadline).await,
            "list_console_messages" => self.list_console_messages(&arguments, deadline).await,
            "get_console_message" => self.get_console_message(&arguments, deadline).await,
            "list_network_requests" => self.list_network_requests(&arguments, deadline).await,
            "get_network_request" => self.get_network_request(&arguments, deadline).await,
            "performance_start_trace" => self.performance_start_trace(&arguments, deadline).await,
            "performance_stop_trace" => self.performance_stop_trace(&arguments, deadline).await,
            "take_heapsnapshot" => self.take_heapsnapshot(&arguments, deadline).await,
            other => Ok(tool_error(format!(
                "Browser command '{other}' is not implemented by MoonDesk's native CDP engine"
            ))),
        };
        match result {
            Ok(value) => Ok(value),
            Err(BrowserTransportError::Protocol(error)) => Ok(tool_error(error)),
            Err(error) => Err(error),
        }
    }

    async fn close_unowned_startup_pages(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        let settle_deadline = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(750),
        );
        loop {
            let targets = self
                .connection
                .call("Target.getTargets", json!({}), None, deadline)
                .await?;
            let page_targets = targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|target| target.get("type").and_then(Value::as_str) == Some("page"))
                .collect::<Vec<_>>();
            if page_targets.is_empty() {
                return Ok(());
            }

            // Chrome for Testing can launch its initial about:blank inside a generated
            // BrowserContext. Closing only that target leaves the context alive and Chromium can
            // later recreate a blank page inside it, which would become foreign browser-global
            // state. Dispose startup BrowserContexts themselves; only fall back to closing the
            // target when Chromium reports the default context without an ID.
            let startup_context_ids = page_targets
                .iter()
                .filter_map(|target| target.get("browserContextId").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<std::collections::HashSet<_>>();
            let default_context_target_ids = page_targets
                .iter()
                .filter(|target| {
                    target
                        .get("browserContextId")
                        .and_then(Value::as_str)
                        .is_none()
                })
                .filter_map(|target| target.get("targetId").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>();

            for context_id in startup_context_ids {
                let _ = self
                    .connection
                    .call(
                        "Target.disposeBrowserContext",
                        json!({ "browserContextId": context_id }),
                        None,
                        deadline,
                    )
                    .await;
            }
            for target_id in default_context_target_ids {
                let _ = self
                    .connection
                    .call(
                        "Target.closeTarget",
                        json!({ "targetId": target_id }),
                        None,
                        deadline,
                    )
                    .await;
            }

            if tokio::time::Instant::now() >= settle_deadline {
                return Err(BrowserTransportError::Protocol(
                    "Chromium retained an unowned startup page after MoonDesk attempted to retire its startup BrowserContext"
                        .to_string(),
                ));
            }
            tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
        }
    }

    async fn ensure_context(
        &self,
        context_name: &str,
        deadline: tokio::time::Instant,
    ) -> Result<String, BrowserTransportError> {
        if let Some(context_id) = self
            .state
            .lock()
            .await
            .contexts_by_name
            .get(context_name)
            .cloned()
        {
            return Ok(context_id);
        }
        let result = self
            .connection
            .call(
                "Target.createBrowserContext",
                json!({ "disposeOnDetach": true }),
                None,
                deadline,
            )
            .await?;
        let context_id = required_str(&result, "browserContextId")?.to_string();
        let mut state = self.state.lock().await;
        if let Some(existing) = state.contexts_by_name.get(context_name) {
            let existing = existing.clone();
            drop(state);
            let _ = self
                .connection
                .call(
                    "Target.disposeBrowserContext",
                    json!({ "browserContextId": context_id }),
                    None,
                    deadline,
                )
                .await;
            return Ok(existing);
        }
        state
            .contexts_by_name
            .insert(context_name.to_string(), context_id.clone());
        state
            .context_names_by_id
            .insert(context_id.clone(), context_name.to_string());
        Ok(context_id)
    }

    async fn refresh_pages(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<Value>, BrowserTransportError> {
        let managed_context_ids = {
            let state = self.state.lock().await;
            state
                .context_names_by_id
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
        let settle_deadline = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(750),
        );
        let targets = loop {
            let result = self
                .connection
                .call("Target.getTargets", json!({}), None, deadline)
                .await?;
            let targets = result
                .get("targetInfos")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let has_transient_page = targets.iter().any(|target| {
                target.get("type").and_then(Value::as_str) == Some("page")
                    && target
                        .get("browserContextId")
                        .and_then(Value::as_str)
                        .is_some_and(|context_id| managed_context_ids.contains(context_id))
                    && target
                        .get("url")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
            });
            if !has_transient_page || tokio::time::Instant::now() >= settle_deadline {
                break targets;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };

        let mut state = self.state.lock().await;
        let mut alive_targets = std::collections::HashSet::new();
        for target in targets {
            if target.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let Some(target_id) = target.get("targetId").and_then(Value::as_str) else {
                continue;
            };
            let Some(context_id) = target.get("browserContextId").and_then(Value::as_str) else {
                continue;
            };
            let Some(context_name) = state.context_names_by_id.get(context_id).cloned() else {
                continue;
            };
            alive_targets.insert(target_id.to_string());
            let url = target
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let title = target
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(page_id) = state.target_to_page_id.get(target_id).copied() {
                if let Some(page) = state.pages_by_id.get_mut(&page_id) {
                    page.url = url;
                    page.title = title;
                }
                continue;
            }
            let page_id = state.next_page_id();
            state
                .target_to_page_id
                .insert(target_id.to_string(), page_id);
            state.pages_by_id.insert(
                page_id,
                CdpPage {
                    target_id: target_id.to_string(),
                    context_id: context_id.to_string(),
                    context_name,
                    session_id: None,
                    url,
                    title,
                    snapshot_generation: 0,
                    elements: HashMap::new(),
                },
            );
        }

        let vanished = state
            .target_to_page_id
            .keys()
            .filter(|target_id| !alive_targets.contains(*target_id))
            .cloned()
            .collect::<Vec<_>>();
        for target_id in vanished {
            if let Some(page_id) = state.target_to_page_id.remove(&target_id) {
                state.pages_by_id.remove(&page_id);
                if state.selected_page == Some(page_id) {
                    state.selected_page = None;
                }
            }
        }
        Ok(page_values(&state))
    }

    async fn list_pages(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let pages = self.refresh_pages(deadline).await?;
        Ok(pages_result(pages))
    }

    async fn new_page(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let url = required_str(arguments, "url")?;
        let context_name = required_str(arguments, "isolatedContext")?;
        let background = arguments
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context_id = self.ensure_context(context_name, deadline).await?;
        let created = self
            .connection
            .call(
                "Target.createTarget",
                json!({
                    "url": url,
                    "browserContextId": context_id,
                    "background": background,
                }),
                None,
                deadline,
            )
            .await?;
        let target_id = required_str(&created, "targetId")?.to_string();
        let pages = self.refresh_pages(deadline).await?;
        let page_id = self
            .state
            .lock()
            .await
            .target_to_page_id
            .get(&target_id)
            .copied();
        if let Some(page_id) = page_id {
            let mut state = self.state.lock().await;
            if !background || state.selected_page.is_none() {
                state.selected_page = Some(page_id);
            }
        }
        if !background && let Some(page_id) = page_id {
            let session_id = self.ensure_page_session(page_id, deadline).await?;
            let _ = self
                .connection
                .call("Page.bringToFront", json!({}), Some(&session_id), deadline)
                .await;
        }
        Ok(pages_result(pages))
    }

    async fn select_page(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = required_u64(arguments, "pageId")?;
        let bring_to_front = arguments
            .get("bringToFront")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.page_target(page_id).await?;
        self.state.lock().await.selected_page = Some(page_id);
        if bring_to_front {
            let session_id = self.ensure_page_session(page_id, deadline).await?;
            self.connection
                .call("Page.bringToFront", json!({}), Some(&session_id), deadline)
                .await?;
        }
        let pages = self.refresh_pages(deadline).await?;
        Ok(pages_result(pages))
    }

    async fn close_page(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = required_u64(arguments, "pageId")?;
        let target_id = self.page_target(page_id).await?;
        self.connection
            .call(
                "Target.closeTarget",
                json!({ "targetId": target_id }),
                None,
                deadline,
            )
            .await?;
        {
            let mut state = self.state.lock().await;
            if let Some(page) = state.pages_by_id.remove(&page_id) {
                state.target_to_page_id.remove(&page.target_id);
            }
            if state.selected_page == Some(page_id) {
                state.selected_page = state.pages_by_id.keys().next().copied();
            }
        }
        let pages = self.refresh_pages(deadline).await?;
        Ok(pages_result(pages))
    }

    async fn page_target(&self, page_id: u64) -> Result<String, BrowserTransportError> {
        self.state
            .lock()
            .await
            .pages_by_id
            .get(&page_id)
            .map(|page| page.target_id.clone())
            .ok_or_else(|| {
                BrowserTransportError::Protocol(format!("Browser page {page_id} was not found"))
            })
    }

    async fn ensure_page_session(
        &self,
        page_id: u64,
        deadline: tokio::time::Instant,
    ) -> Result<String, BrowserTransportError> {
        let (target_id, existing) = {
            let state = self.state.lock().await;
            let page = state.pages_by_id.get(&page_id).ok_or_else(|| {
                BrowserTransportError::Protocol(format!("Browser page {page_id} was not found"))
            })?;
            (page.target_id.clone(), page.session_id.clone())
        };
        if let Some(session_id) = existing {
            return Ok(session_id);
        }

        let attached = self
            .connection
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
                deadline,
            )
            .await?;
        let session_id = required_str(&attached, "sessionId")?.to_string();

        for domain in [
            "Page.enable",
            "Runtime.enable",
            "DOM.enable",
            "Network.enable",
        ] {
            self.connection
                .call(domain, json!({}), Some(&session_id), deadline)
                .await?;
        }
        let _ = self
            .connection
            .call("Log.enable", json!({}), Some(&session_id), deadline)
            .await;
        if self.presentation.is_headless() {
            self.connection
                .call(
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "width": DEFAULT_VIEWPORT_WIDTH,
                        "height": DEFAULT_VIEWPORT_HEIGHT,
                        "deviceScaleFactor": 1,
                        "mobile": false,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }

        let mut state = self.state.lock().await;
        let page = state.pages_by_id.get_mut(&page_id).ok_or_else(|| {
            BrowserTransportError::Protocol(format!(
                "Browser page {page_id} disappeared while attaching"
            ))
        })?;
        if page.target_id != target_id {
            return Err(BrowserTransportError::Protocol(format!(
                "Browser page {page_id} changed target while attaching"
            )));
        }
        if let Some(existing) = &page.session_id {
            return Ok(existing.clone());
        }
        page.session_id = Some(session_id.clone());
        Ok(session_id)
    }

    async fn page_id_from_arguments(
        &self,
        arguments: &Value,
    ) -> Result<u64, BrowserTransportError> {
        let page_id = required_u64(arguments, "pageId")?;
        self.page_target(page_id).await?;
        Ok(page_id)
    }

    async fn navigate_page(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let navigation_type = arguments
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("url");
        if navigation_type != "url" && arguments.get("initScript").is_some() {
            return Ok(tool_error(
                "initScript is only supported for URL navigation".to_string(),
            ));
        }
        let init_script_id =
            if let Some(script) = arguments.get("initScript").and_then(Value::as_str) {
                let added = self
                    .connection
                    .call(
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({ "source": script }),
                        Some(&session_id),
                        deadline,
                    )
                    .await?;
                Some(required_str(&added, "identifier")?.to_string())
            } else {
                None
            };
        match navigation_type {
            "url" => {
                let url = required_str(arguments, "url")?;
                let result = self
                    .connection
                    .call(
                        "Page.navigate",
                        json!({ "url": url }),
                        Some(&session_id),
                        deadline,
                    )
                    .await?;
                if let Some(error) = result.get("errorText").and_then(Value::as_str) {
                    if let Some(identifier) = init_script_id.as_deref() {
                        let _ = self
                            .connection
                            .call(
                                "Page.removeScriptToEvaluateOnNewDocument",
                                json!({ "identifier": identifier }),
                                Some(&session_id),
                                deadline,
                            )
                            .await;
                    }
                    return Ok(tool_error(format!("Navigation failed: {error}")));
                }
            }
            "reload" => {
                self.connection
                    .call(
                        "Page.reload",
                        json!({
                            "ignoreCache": arguments
                                .get("ignoreCache")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        }),
                        Some(&session_id),
                        deadline,
                    )
                    .await?;
            }
            "back" | "forward" => {
                let history = self
                    .connection
                    .call(
                        "Page.getNavigationHistory",
                        json!({}),
                        Some(&session_id),
                        deadline,
                    )
                    .await?;
                let current = history
                    .get("currentIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let target_index = if arguments.get("type").and_then(Value::as_str) == Some("back")
                {
                    current - 1
                } else {
                    current + 1
                };
                let entries = history
                    .get("entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let entry = entries.get(usize::try_from(target_index).unwrap_or(usize::MAX));
                let Some(entry_id) = entry
                    .and_then(|entry| entry.get("id"))
                    .and_then(Value::as_i64)
                else {
                    return Ok(tool_error(
                        "No navigation history entry is available".to_string(),
                    ));
                };
                self.connection
                    .call(
                        "Page.navigateToHistoryEntry",
                        json!({ "entryId": entry_id }),
                        Some(&session_id),
                        deadline,
                    )
                    .await?;
            }
            other => {
                return Ok(tool_error(format!(
                    "Unsupported navigation action '{other}'"
                )));
            }
        }
        self.wait_document_ready(&session_id, deadline).await?;
        if let Some(identifier) = init_script_id.as_deref() {
            self.connection
                .call(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({ "identifier": identifier }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        let pages = self.refresh_pages(deadline).await?;
        Ok(pages_result(pages))
    }

    async fn wait_document_ready(
        &self,
        session_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserTransportError::Timeout);
            }
            let result = self
                .connection
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": "document.readyState",
                        "returnByValue": true,
                    }),
                    Some(session_id),
                    deadline,
                )
                .await?;
            let ready = result
                .pointer("/result/value")
                .and_then(Value::as_str)
                .is_some_and(|value| matches!(value, "interactive" | "complete"));
            if ready {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    async fn take_snapshot(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let verbose = arguments
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = self
            .connection
            .call(
                "Accessibility.getFullAXTree",
                json!({}),
                Some(&session_id),
                deadline,
            )
            .await?;
        let nodes = result
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let location = self
            .evaluate_value(
                &session_id,
                "({url: location.href, title: document.title})",
                deadline,
            )
            .await
            .unwrap_or_else(|_| json!({}));

        let mut state = self.state.lock().await;
        let page = state.pages_by_id.get_mut(&page_id).ok_or_else(|| {
            BrowserTransportError::Protocol(format!("Browser page {page_id} was not found"))
        })?;
        page.snapshot_generation = page.snapshot_generation.saturating_add(1);
        let generation = page.snapshot_generation;
        page.elements.clear();
        let mut lines = Vec::new();
        if let Some(url) = location.get("url").and_then(Value::as_str) {
            lines.push(format!("url=\"{}\"", escape_snapshot_text(url)));
        }
        if let Some(title) = location
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            lines.push(format!("title=\"{}\"", escape_snapshot_text(title)));
        }

        for node in nodes {
            if node.get("ignored").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let role = ax_value_string(node.get("role")).unwrap_or("unknown");
            let name = ax_value_string(node.get("name")).unwrap_or("");
            let value = ax_value_string(node.get("value"));
            let backend_node_id = node.get("backendDOMNodeId").and_then(Value::as_u64);
            let mut line = String::new();
            if let Some(backend_node_id) = backend_node_id {
                let uid = format!("{generation}-{backend_node_id}");
                page.elements
                    .insert(uid.clone(), CdpElement { backend_node_id });
                line.push_str(&format!("uid={uid} "));
            }
            line.push_str(role);
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", escape_snapshot_text(name)));
            }
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                line.push_str(&format!(" value=\"{}\"", escape_snapshot_text(value)));
            }
            if verbose
                && let Some(description) = ax_value_string(node.get("description"))
                    .filter(|description| !description.is_empty())
            {
                line.push_str(&format!(
                    " description=\"{}\"",
                    escape_snapshot_text(description)
                ));
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
        drop(state);

        let snapshot = lines.join("\n");
        if let Some(path) = arguments.get("filePath").and_then(Value::as_str) {
            std::fs::write(path, snapshot.as_bytes()).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not write browser snapshot to {path}: {error}"
                ))
            })?;
            Ok(tool_success_text(format!("Saved to {path}.")))
        } else {
            Ok(tool_success_text(snapshot))
        }
    }

    async fn element_backend_node(
        &self,
        page_id: u64,
        uid: &str,
    ) -> Result<u64, BrowserTransportError> {
        self.state
            .lock()
            .await
            .pages_by_id
            .get(&page_id)
            .and_then(|page| page.elements.get(uid))
            .map(|element| element.backend_node_id)
            .ok_or_else(|| {
                BrowserTransportError::Protocol(format!(
                    "Element uid {uid} was not found. Take a fresh browser snapshot and retry."
                ))
            })
    }

    async fn resolve_element_object(
        &self,
        page_id: u64,
        uid: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(String, String), BrowserTransportError> {
        let backend_node_id = self.element_backend_node(page_id, uid).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let resolved = self
            .connection
            .call(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let object_id = resolved
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BrowserTransportError::Protocol(format!(
                    "Element uid {uid} could not be resolved in the page"
                ))
            })?
            .to_string();
        Ok((session_id, object_id))
    }

    async fn element_center(
        &self,
        page_id: u64,
        uid: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(f64, f64), BrowserTransportError> {
        let backend_node_id = self.element_backend_node(page_id, uid).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        if let Ok(resolved) = self
            .connection
            .call(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
                Some(&session_id),
                deadline,
            )
            .await
            && let Some(object_id) = resolved.pointer("/object/objectId").and_then(Value::as_str)
        {
            let _ = self
                .connection
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function(){ this.scrollIntoView({block:'center', inline:'center'}); }",
                        "returnByValue": true,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await;
        }
        let model = self
            .connection
            .call(
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_node_id }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let quad = model
            .pointer("/model/border")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BrowserTransportError::Protocol(format!(
                    "Element uid {uid} does not have clickable layout geometry"
                ))
            })?;
        quad_center(quad).ok_or_else(|| {
            BrowserTransportError::Protocol(format!(
                "Element uid {uid} returned invalid layout geometry"
            ))
        })
    }

    async fn dispatch_click(
        &self,
        session_id: &str,
        x: f64,
        y: f64,
        click_count: u64,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y }),
                Some(session_id),
                deadline,
            )
            .await?;
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 1,
                    "clickCount": click_count,
                }),
                Some(session_id),
                deadline,
            )
            .await?;
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 0,
                    "clickCount": click_count,
                }),
                Some(session_id),
                deadline,
            )
            .await?;
        Ok(())
    }

    async fn click_element(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let uid = required_str(arguments, "uid")?;
        let (x, y) = self.element_center(page_id, uid, deadline).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let click_count = if arguments
            .get("dblClick")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            2
        } else {
            1
        };
        self.dispatch_click(&session_id, x, y, click_count, deadline)
            .await?;
        let mut result = tool_success_text(format!("Clicked element {uid}."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn click_at(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let x = required_f64(arguments, "x")?;
        let y = required_f64(arguments, "y")?;
        let click_count = if arguments
            .get("dblClick")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            2
        } else {
            1
        };
        self.dispatch_click(&session_id, x, y, click_count, deadline)
            .await?;
        let mut result = tool_success_text(format!("Clicked at ({x}, {y})."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn focus_element(
        &self,
        page_id: u64,
        uid: &str,
        select: bool,
        deadline: tokio::time::Instant,
    ) -> Result<String, BrowserTransportError> {
        let (session_id, object_id) = self.resolve_element_object(page_id, uid, deadline).await?;
        let function = if select {
            "function(){ this.focus(); if (typeof this.select === 'function') this.select(); }"
        } else {
            "function(){ this.focus(); }"
        };
        let result = self
            .connection
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": function,
                    "returnByValue": true,
                }),
                Some(&session_id),
                deadline,
            )
            .await;
        let _ = self
            .connection
            .call(
                "Runtime.releaseObject",
                json!({ "objectId": object_id }),
                Some(&session_id),
                deadline,
            )
            .await;
        result?;
        Ok(session_id)
    }

    async fn fill_element(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let uid = required_str(arguments, "uid")?;
        let value = required_str(arguments, "value")?;
        let (session_id, object_id) = self.resolve_element_object(page_id, uid, deadline).await?;
        let metadata = self
            .connection
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(){ return { tag: (this.tagName || '').toLowerCase(), type: (this.type || '').toLowerCase(), role: (this.getAttribute && this.getAttribute('role') || '').toLowerCase(), checked: !!this.checked, ariaChecked: this.getAttribute && this.getAttribute('aria-checked') }; }",
                    "returnByValue": true,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let element = metadata
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null);
        let tag = element
            .get("tag")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input_type = element
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let role = element
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let operation = if tag == "select" {
            let selected = self
                .connection
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function(value){ const options = Array.from(this.options || []); const option = options.find((item) => item.value === value) || options.find((item) => (item.textContent || '').trim() === value); if (!option) return { ok: false, error: 'No matching option' }; const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')?.set; if (setter) setter.call(this, option.value); else this.value = option.value; this.dispatchEvent(new Event('input', { bubbles: true })); this.dispatchEvent(new Event('change', { bubbles: true })); return { ok: true, value: this.value }; }",
                        "arguments": [{ "value": value }],
                        "returnByValue": true,
                        "userGesture": true,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            let selected = selected
                .pointer("/result/value")
                .cloned()
                .unwrap_or(Value::Null);
            if selected.get("ok").and_then(Value::as_bool) != Some(true) {
                Ok(tool_error(format!(
                    "Select element {uid} has no option matching '{value}'"
                )))
            } else {
                Ok(tool_success_text(format!("Filled element {uid}.")))
            }
        } else if input_type == "checkbox"
            || input_type == "radio"
            || matches!(role, "checkbox" | "switch" | "radio")
        {
            let desired = match value.to_ascii_lowercase().as_str() {
                "true" => true,
                "false" if input_type != "radio" && role != "radio" => false,
                "false" => {
                    let _ = self
                        .connection
                        .call(
                            "Runtime.releaseObject",
                            json!({ "objectId": object_id }),
                            Some(&session_id),
                            deadline,
                        )
                        .await;
                    return Ok(tool_error(format!(
                        "Radio element {uid} only accepts the value 'true'"
                    )));
                }
                _ => {
                    let _ = self
                        .connection
                        .call(
                            "Runtime.releaseObject",
                            json!({ "objectId": object_id }),
                            Some(&session_id),
                            deadline,
                        )
                        .await;
                    return Ok(tool_error(format!(
                        "Boolean element {uid} requires value 'true' or 'false'"
                    )));
                }
            };
            let toggled = self
                .connection
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function(desired){ const aria = this.getAttribute && this.getAttribute('aria-checked'); const before = typeof this.checked === 'boolean' ? this.checked : aria === 'true'; if (before !== desired && typeof this.click === 'function') this.click(); const afterAria = this.getAttribute && this.getAttribute('aria-checked'); const after = typeof this.checked === 'boolean' ? this.checked : afterAria === 'true'; return { ok: after === desired, checked: after }; }",
                        "arguments": [{ "value": desired }],
                        "returnByValue": true,
                        "userGesture": true,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            let toggled = toggled
                .pointer("/result/value")
                .cloned()
                .unwrap_or(Value::Null);
            if toggled.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(tool_success_text(format!("Filled element {uid}.")))
            } else {
                Ok(tool_error(format!(
                    "Element {uid} could not be changed to {desired}"
                )))
            }
        } else {
            let _ = self
                .connection
                .call(
                    "Runtime.releaseObject",
                    json!({ "objectId": object_id.clone() }),
                    Some(&session_id),
                    deadline,
                )
                .await;
            let session_id = self.focus_element(page_id, uid, true, deadline).await?;
            self.connection
                .call(
                    "Input.insertText",
                    json!({ "text": value }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            Ok(tool_success_text(format!("Filled element {uid}.")))
        };

        let _ = self
            .connection
            .call(
                "Runtime.releaseObject",
                json!({ "objectId": object_id }),
                Some(&session_id),
                deadline,
            )
            .await;

        let mut result = operation?;
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn hover_element(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let uid = required_str(arguments, "uid")?;
        let (x, y) = self.element_center(page_id, uid, deadline).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let mut result = tool_success_text(format!("Hovered element {uid}."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn drag_element(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let from_uid = required_str(arguments, "from_uid")?;
        let to_uid = required_str(arguments, "to_uid")?;
        let (from_x, from_y) = self.element_center(page_id, from_uid, deadline).await?;
        let (to_x, to_y) = self.element_center(page_id, to_uid, deadline).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": from_x, "y": from_y }),
                Some(&session_id),
                deadline,
            )
            .await?;
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": from_x,
                    "y": from_y,
                    "button": "left",
                    "buttons": 1,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        for step in 1..=4 {
            let t = f64::from(step) / 4.0;
            let x = from_x + ((to_x - from_x) * t);
            let y = from_y + ((to_y - from_y) * t);
            self.connection
                .call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": "mouseMoved",
                        "x": x,
                        "y": y,
                        "button": "left",
                        "buttons": 1,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": to_x,
                    "y": to_y,
                    "button": "left",
                    "buttons": 0,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let mut result = tool_success_text(format!("Dragged {from_uid} to {to_uid}."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn evaluate_value(
        &self,
        session_id: &str,
        expression: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let result = self
            .connection
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
                Some(session_id),
                deadline,
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("JavaScript evaluation failed");
            return Err(BrowserTransportError::Protocol(text.to_string()));
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn evaluate_script(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let function = required_str(arguments, "function")?;
        let element_uids = arguments
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let value = if element_uids.is_empty() {
            let expression = format!("({function})()");
            self.evaluate_value(&session_id, &expression, deadline)
                .await?
        } else {
            let mut object_ids = Vec::with_capacity(element_uids.len());
            for (index, uid) in element_uids.iter().enumerate() {
                let Some(uid) = uid.as_str() else {
                    for object_id in &object_ids {
                        let _ = self
                            .connection
                            .call(
                                "Runtime.releaseObject",
                                json!({ "objectId": object_id }),
                                Some(&session_id),
                                deadline,
                            )
                            .await;
                    }
                    return Err(BrowserTransportError::Protocol(format!(
                        "evaluate_script args[{index}] must be an element uid"
                    )));
                };
                match self.resolve_element_object(page_id, uid, deadline).await {
                    Ok((_, object_id)) => object_ids.push(object_id),
                    Err(error) => {
                        for object_id in &object_ids {
                            let _ = self
                                .connection
                                .call(
                                    "Runtime.releaseObject",
                                    json!({ "objectId": object_id }),
                                    Some(&session_id),
                                    deadline,
                                )
                                .await;
                        }
                        return Err(error);
                    }
                }
            }

            let call_arguments = object_ids
                .iter()
                .map(|object_id| json!({ "objectId": object_id }))
                .collect::<Vec<_>>();
            let function_declaration =
                format!("function(...args){{ return ({function})(...args); }}");
            let called = self
                .connection
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_ids[0],
                        "functionDeclaration": function_declaration,
                        "arguments": call_arguments,
                        "awaitPromise": true,
                        "returnByValue": true,
                        "userGesture": true,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await;
            for object_id in &object_ids {
                let _ = self
                    .connection
                    .call(
                        "Runtime.releaseObject",
                        json!({ "objectId": object_id }),
                        Some(&session_id),
                        deadline,
                    )
                    .await;
            }
            let called = called?;
            if let Some(exception) = called.get("exceptionDetails") {
                let message = exception
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("JavaScript evaluation failed");
                return Err(BrowserTransportError::Protocol(message.to_string()));
            }
            called
                .pointer("/result/value")
                .cloned()
                .unwrap_or(Value::Null)
        };

        let text = serde_json::to_string(&value).map_err(|error| {
            BrowserTransportError::Protocol(format!("Could not encode JavaScript result: {error}"))
        })?;
        if let Some(path) = arguments.get("filePath").and_then(Value::as_str) {
            std::fs::write(path, text.as_bytes()).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not write JavaScript result to {path}: {error}"
                ))
            })?;
            Ok(tool_success_text(format!("Saved to {path}.")))
        } else {
            let message = format!("Script ran on page and returned:\n```json\n{text}\n```");
            Ok(tool_success_structured(
                message.clone(),
                json!({ "message": message }),
            ))
        }
    }

    async fn upload_file(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let uid = required_str(arguments, "uid")?;
        let file_path = required_str(arguments, "filePath")?;
        let backend_node_id = self.element_backend_node(page_id, uid).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        self.connection
            .call(
                "DOM.setFileInputFiles",
                json!({
                    "files": [file_path],
                    "backendNodeId": backend_node_id,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let mut result = tool_success_text(format!("Uploaded file to element {uid}."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn type_text(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let text = required_str(arguments, "text")?;
        self.connection
            .call(
                "Input.insertText",
                json!({ "text": text }),
                Some(&session_id),
                deadline,
            )
            .await?;
        if let Some(key) = arguments.get("submitKey").and_then(Value::as_str) {
            self.dispatch_key(&session_id, key, deadline).await?;
        }
        Ok(tool_success_text("Typed text.".to_string()))
    }

    async fn press_key(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let key = required_str(arguments, "key")?;
        self.dispatch_key(&session_id, key, deadline).await?;
        let mut result = tool_success_text(format!("Pressed {key}."));
        if arguments
            .get("includeSnapshot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let snapshot = self.take_snapshot(arguments, deadline).await?;
            append_tool_content(&mut result, &snapshot);
        }
        Ok(result)
    }

    async fn dispatch_key(
        &self,
        session_id: &str,
        raw_key: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), BrowserTransportError> {
        let parsed = ParsedKey::parse(raw_key)?;
        let mut down = json!({
            "type": "rawKeyDown",
            "key": parsed.key,
            "code": parsed.code,
            "modifiers": parsed.modifiers,
        });
        if let Some(code) = parsed.windows_virtual_key_code {
            down["windowsVirtualKeyCode"] = Value::from(code);
            down["nativeVirtualKeyCode"] = Value::from(code);
        }
        if let Some(text) = parsed.text {
            down["text"] = Value::String(text.clone());
            down["unmodifiedText"] = Value::String(text);
        }
        self.connection
            .call("Input.dispatchKeyEvent", down, Some(session_id), deadline)
            .await?;
        self.connection
            .call(
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "key": parsed.key,
                    "code": parsed.code,
                    "modifiers": parsed.modifiers,
                    "windowsVirtualKeyCode": parsed.windows_virtual_key_code.unwrap_or(0),
                    "nativeVirtualKeyCode": parsed.windows_virtual_key_code.unwrap_or(0),
                }),
                Some(session_id),
                deadline,
            )
            .await?;
        Ok(())
    }

    async fn scroll(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let delta_x = required_f64(arguments, "deltaX")?;
        let delta_y = required_f64(arguments, "deltaY")?;
        let metrics = self
            .evaluate_value(
                &session_id,
                "({width: innerWidth, height: innerHeight, x: window.scrollX, y: window.scrollY})",
                deadline,
            )
            .await
            .unwrap_or_else(|_| json!({}));
        let x = metrics
            .get("width")
            .and_then(Value::as_f64)
            .map(|width| width / 2.0)
            .unwrap_or(0.0);
        let y = metrics
            .get("height")
            .and_then(Value::as_f64)
            .map(|height| height / 2.0)
            .unwrap_or(0.0);
        self.connection
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": x,
                    "y": y,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        let initial_x = metrics.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let initial_y = metrics.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let settle_deadline = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(250),
        );
        let mut position = json!({ "x": initial_x, "y": initial_y });
        loop {
            if tokio::time::Instant::now() >= settle_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
            position = self
                .evaluate_value(
                    &session_id,
                    "({x: window.scrollX, y: window.scrollY})",
                    deadline,
                )
                .await?;
            let current_x = position
                .get("x")
                .and_then(Value::as_f64)
                .unwrap_or(initial_x);
            let current_y = position
                .get("y")
                .and_then(Value::as_f64)
                .unwrap_or(initial_y);
            if current_x != initial_x || current_y != initial_y {
                break;
            }
        }
        Ok(tool_success_structured(
            "Scrolled the page.".to_string(),
            json!({ "scroll": position }),
        ))
    }

    async fn resize_page(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let width = required_f64(arguments, "width")?.round();
        let height = required_f64(arguments, "height")?.round();
        self.connection
            .call(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                }),
                Some(&session_id),
                deadline,
            )
            .await?;
        Ok(tool_success_text(format!(
            "Viewport resized to {width}x{height}."
        )))
    }

    async fn emulate(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;

        if let Some(viewport) = arguments.get("viewport").and_then(Value::as_str) {
            let parsed = ParsedViewport::parse(viewport)?;
            let orientation = if parsed.landscape {
                json!({ "type": "landscapePrimary", "angle": 90 })
            } else {
                json!({ "type": "portraitPrimary", "angle": 0 })
            };
            self.connection
                .call(
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "width": parsed.width,
                        "height": parsed.height,
                        "deviceScaleFactor": parsed.dpr,
                        "mobile": parsed.mobile,
                        "screenOrientation": orientation,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            self.connection
                .call(
                    "Emulation.setTouchEmulationEnabled",
                    json!({
                        "enabled": parsed.touch,
                        "maxTouchPoints": if parsed.touch { 5 } else { 1 },
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(color_scheme) = arguments.get("colorScheme").and_then(Value::as_str) {
            let features = if color_scheme == "auto" {
                Vec::<Value>::new()
            } else {
                vec![json!({ "name": "prefers-color-scheme", "value": color_scheme })]
            };
            self.connection
                .call(
                    "Emulation.setEmulatedMedia",
                    json!({ "features": features }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(user_agent) = arguments.get("userAgent").and_then(Value::as_str) {
            self.connection
                .call(
                    "Network.setUserAgentOverride",
                    json!({ "userAgent": user_agent }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(geolocation) = arguments.get("geolocation").and_then(Value::as_str) {
            let mut parts = geolocation.split(',').map(str::trim);
            let latitude = parts.next().and_then(|value| value.parse::<f64>().ok());
            let longitude = parts.next().and_then(|value| value.parse::<f64>().ok());
            let (Some(latitude), Some(longitude)) = (latitude, longitude) else {
                return Ok(tool_error(
                    "geolocation must be formatted as latitude,longitude".to_string(),
                ));
            };
            self.connection
                .call(
                    "Emulation.setGeolocationOverride",
                    json!({
                        "latitude": latitude,
                        "longitude": longitude,
                        "accuracy": 1,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(headers) = arguments.get("extraHttpHeaders").and_then(Value::as_str) {
            let parsed: Value = serde_json::from_str(headers).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "extraHttpHeaders must be a JSON object: {error}"
                ))
            })?;
            if !parsed.is_object() {
                return Ok(tool_error(
                    "extraHttpHeaders must be a JSON object".to_string(),
                ));
            }
            self.connection
                .call(
                    "Network.setExtraHTTPHeaders",
                    json!({ "headers": parsed }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(network) = arguments.get("networkConditions").and_then(Value::as_str) {
            let (offline, latency, down, up) = network_profile(network);
            self.connection
                .call(
                    "Network.emulateNetworkConditions",
                    json!({
                        "offline": offline,
                        "latency": latency,
                        "downloadThroughput": down,
                        "uploadThroughput": up,
                    }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        if let Some(rate) = arguments.get("cpuThrottlingRate").and_then(Value::as_f64) {
            self.connection
                .call(
                    "Emulation.setCPUThrottlingRate",
                    json!({ "rate": rate }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
        }
        Ok(tool_success_text("Browser emulation updated.".to_string()))
    }

    async fn take_screenshot(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let format = arguments
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("png");
        let mut capture = Map::new();
        capture.insert("format".to_string(), Value::String(format.to_string()));
        capture.insert("fromSurface".to_string(), Value::Bool(true));
        capture.insert("captureBeyondViewport".to_string(), Value::Bool(false));
        if let Some(quality) = arguments.get("quality").and_then(Value::as_f64)
            && matches!(format, "jpeg" | "webp")
        {
            capture.insert("quality".to_string(), Value::from(quality.round() as i64));
        }

        if let Some(uid) = arguments.get("uid").and_then(Value::as_str) {
            let backend_node_id = self.element_backend_node(page_id, uid).await?;
            let model = self
                .connection
                .call(
                    "DOM.getBoxModel",
                    json!({ "backendNodeId": backend_node_id }),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            let quad = model
                .pointer("/model/border")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    BrowserTransportError::Protocol(format!(
                        "Element uid {uid} does not have screenshot geometry"
                    ))
                })?;
            let (x, y, width, height) = quad_bounds(quad).ok_or_else(|| {
                BrowserTransportError::Protocol(format!(
                    "Element uid {uid} returned invalid screenshot geometry"
                ))
            })?;
            capture.insert(
                "clip".to_string(),
                json!({ "x": x, "y": y, "width": width, "height": height, "scale": 1 }),
            );
        } else if arguments
            .get("fullPage")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            capture.insert("captureBeyondViewport".to_string(), Value::Bool(true));
            let metrics = self
                .connection
                .call(
                    "Page.getLayoutMetrics",
                    json!({}),
                    Some(&session_id),
                    deadline,
                )
                .await?;
            if let Some(size) = metrics.get("cssContentSize") {
                let x = size.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = size.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                let width = size.get("width").and_then(Value::as_f64).unwrap_or(1.0);
                let height = size.get("height").and_then(Value::as_f64).unwrap_or(1.0);
                capture.insert(
                    "clip".to_string(),
                    json!({ "x": x, "y": y, "width": width, "height": height, "scale": 1 }),
                );
            }
        }

        let result = self
            .connection
            .call(
                "Page.captureScreenshot",
                Value::Object(capture),
                Some(&session_id),
                deadline,
            )
            .await?;
        let data = required_str(&result, "data")?.to_string();
        if let Some(path) = arguments.get("filePath").and_then(Value::as_str) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|error| {
                    BrowserTransportError::Protocol(format!(
                        "Could not decode browser screenshot: {error}"
                    ))
                })?;
            std::fs::write(path, bytes).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not write browser screenshot to {path}: {error}"
                ))
            })?;
            Ok(tool_success_text(format!("Saved to {path}.")))
        } else {
            let mime_type = match format {
                "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                _ => "image/png",
            };
            Ok(json!({
                "isError": false,
                "content": [{
                    "type": "image",
                    "data": data,
                    "mimeType": mime_type,
                }]
            }))
        }
    }

    async fn wait_for(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let texts = arguments
            .get("text")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BrowserTransportError::Protocol("wait_for requires text values".to_string())
            })?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if texts.is_empty() {
            return Ok(tool_error(
                "wait_for requires at least one text value".to_string(),
            ));
        }
        let requested_timeout_ms = arguments
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let wait_deadline = if requested_timeout_ms == 0 {
            deadline
        } else {
            std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_millis(requested_timeout_ms),
            )
        };
        loop {
            if tokio::time::Instant::now() >= wait_deadline {
                return Ok(tool_error(format!(
                    "Timed out after waiting {requested_timeout_ms}ms for requested text"
                )));
            }
            let expression = format!(
                "(() => {{ const text = document.body ? document.body.innerText : ''; return {}; }})()",
                texts
                    .iter()
                    .map(|text| format!(
                        "text.includes({})",
                        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
                    ))
                    .collect::<Vec<_>>()
                    .join(" || ")
            );
            let visible = match self
                .evaluate_value(&session_id, &expression, wait_deadline)
                .await
            {
                Ok(value) => value.as_bool().unwrap_or(false),
                // An explicit wait timeout is a page-level outcome, not proof that the shared
                // browser transport is unhealthy. Keep Chromium alive when the WebSocket still is.
                Err(BrowserTransportError::Timeout)
                    if requested_timeout_ms != 0 && self.connection.is_alive() =>
                {
                    return Ok(tool_error(format!(
                        "Timed out after waiting {requested_timeout_ms}ms for requested text"
                    )));
                }
                Err(error) => return Err(error),
            };
            if visible {
                return self.take_snapshot(arguments, deadline).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn handle_dialog(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("accept");
        let mut params = json!({ "accept": action != "dismiss" });
        if let Some(prompt_text) = arguments.get("promptText").and_then(Value::as_str) {
            params["promptText"] = Value::String(prompt_text.to_string());
        }
        self.connection
            .call(
                "Page.handleJavaScriptDialog",
                params,
                Some(&session_id),
                deadline,
            )
            .await?;
        Ok(tool_success_text(format!("Dialog {action}ed.")))
    }

    async fn list_console_messages(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let type_filter = browser_string_filter(arguments, "types");
        let events = self.connection.events_for_session(&session_id).await;
        let mut messages = Vec::new();
        for event in events {
            let entry = match event.method.as_str() {
                "Runtime.consoleAPICalled" => {
                    let level = event
                        .params
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("log");
                    let text = event
                        .params
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|args| {
                            args.iter()
                                .filter_map(|arg| {
                                    arg.get("value").map(value_to_compact_text).or_else(|| {
                                        arg.get("description")
                                            .and_then(Value::as_str)
                                            .map(str::to_string)
                                    })
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    Some((level.to_string(), text))
                }
                "Log.entryAdded" => {
                    let entry = event.params.get("entry").unwrap_or(&Value::Null);
                    let level = entry
                        .get("level")
                        .and_then(Value::as_str)
                        .unwrap_or("info")
                        .to_string();
                    let text = entry
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((level, text))
                }
                _ => None,
            };
            let Some((level, text)) = entry else {
                continue;
            };
            if !type_filter.is_empty() && !type_filter.contains(&level.to_ascii_lowercase()) {
                continue;
            }
            messages.push(json!({
                "id": event.id,
                "type": level,
                "text": text,
            }));
        }

        let (page_idx, page_size, start, end) = browser_pagination(arguments, messages.len())?;
        let page = &messages[start..end];
        let text = page
            .iter()
            .map(|message| {
                format!(
                    "{}: [{}] {}",
                    message.get("id").and_then(Value::as_u64).unwrap_or(0),
                    message.get("type").and_then(Value::as_str).unwrap_or("log"),
                    message
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(tool_success_structured(
            text,
            json!({
                "messages": page,
                "total": messages.len(),
                "pageIdx": page_idx,
                "pageSize": page_size,
            }),
        ))
    }

    async fn get_console_message(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let requested = arguments
            .get("msgid")
            .and_then(Value::as_u64)
            .or_else(|| arguments.get("messageId").and_then(Value::as_u64))
            .ok_or_else(|| {
                BrowserTransportError::Protocol(
                    "get_console_message requires a numeric message id".to_string(),
                )
            })?;
        let event = self
            .connection
            .events_for_session(&session_id)
            .await
            .into_iter()
            .find(|event| {
                event.id == requested
                    && matches!(
                        event.method.as_str(),
                        "Runtime.consoleAPICalled" | "Log.entryAdded"
                    )
            });
        let Some(event) = event else {
            return Ok(tool_error(format!(
                "Console message {requested} was not found"
            )));
        };
        Ok(tool_success_text(value_to_compact_text(&event.params)))
    }

    async fn list_network_requests(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let resource_filter = browser_string_filter(arguments, "resourceTypes");
        let events = self.connection.events_for_session(&session_id).await;
        let mut seen = HashMap::<String, Value>::new();
        for event in events {
            if event.method != "Network.requestWillBeSent" {
                continue;
            }
            let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                continue;
            };
            let request = event.params.get("request").unwrap_or(&Value::Null);
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_string();
            let url = request
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let resource_type = event
                .params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("Other")
                .to_string();
            if !resource_filter.is_empty()
                && !resource_filter.contains(&resource_type.to_ascii_lowercase())
            {
                continue;
            }
            seen.insert(
                request_id.to_string(),
                json!({
                    "id": event.id,
                    "requestId": request_id,
                    "method": method,
                    "url": url,
                    "resourceType": resource_type,
                }),
            );
        }
        let mut requests = seen.into_values().collect::<Vec<_>>();
        requests.sort_by_key(|request| request.get("id").and_then(Value::as_u64).unwrap_or(0));

        let (page_idx, page_size, start, end) = browser_pagination(arguments, requests.len())?;
        let page = &requests[start..end];
        let text = page
            .iter()
            .map(|request| {
                format!(
                    "{}: {} {} [{}] requestId={}",
                    request.get("id").and_then(Value::as_u64).unwrap_or(0),
                    request
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or("GET"),
                    request
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    request
                        .get("resourceType")
                        .and_then(Value::as_str)
                        .unwrap_or("Other"),
                    request
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(tool_success_structured(
            text,
            json!({
                "requests": page,
                "total": requests.len(),
                "pageIdx": page_idx,
                "pageSize": page_size,
            }),
        ))
    }

    async fn get_network_request(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let requested = arguments
            .get("reqid")
            .and_then(Value::as_u64)
            .or_else(|| arguments.get("requestId").and_then(Value::as_u64))
            .ok_or_else(|| {
                BrowserTransportError::Protocol(
                    "get_network_request requires a numeric request id".to_string(),
                )
            })?;
        let event = self
            .connection
            .events_for_session(&session_id)
            .await
            .into_iter()
            .find(|event| event.id == requested && event.method == "Network.requestWillBeSent");
        let Some(event) = event else {
            return Ok(tool_error(format!(
                "Network request {requested} was not found"
            )));
        };
        Ok(tool_success_text(value_to_compact_text(&event.params)))
    }

    async fn performance_start_trace(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        if self.trace.lock().await.is_some() {
            return Ok(tool_error(
                "A MoonDesk performance trace is already active".to_string(),
            ));
        }

        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let capture_id = self
            .connection
            .start_event_capture(
                None,
                &["Tracing.dataCollected", "Tracing.tracingComplete"],
                MAX_TRACE_BYTES,
            )
            .await;
        if let Err(error) = self
            .connection
            .call(
                "Tracing.start",
                json!({
                    "categories": "devtools.timeline,v8.execute,blink.user_timing,loading,disabled-by-default-devtools.timeline",
                    "options": "sampling-frequency=10000",
                    "transferMode": "ReportEvents",
                }),
                None,
                deadline,
            )
            .await
        {
            self.connection.cancel_event_capture(capture_id).await;
            return Err(error);
        }
        *self.trace.lock().await = Some(CdpTraceRecording { capture_id });

        let reload = arguments
            .get("reload")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let auto_stop = arguments
            .get("autoStop")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if reload {
            if let Err(error) = self
                .connection
                .call(
                    "Page.reload",
                    json!({ "ignoreCache": false }),
                    Some(&session_id),
                    deadline,
                )
                .await
            {
                let _ = self
                    .connection
                    .call("Tracing.end", json!({}), None, deadline)
                    .await;
                self.connection.cancel_event_capture(capture_id).await;
                *self.trace.lock().await = None;
                return Err(error);
            }
            if let Err(error) = self.wait_document_ready(&session_id, deadline).await {
                let _ = self
                    .connection
                    .call("Tracing.end", json!({}), None, deadline)
                    .await;
                self.connection.cancel_event_capture(capture_id).await;
                *self.trace.lock().await = None;
                return Err(error);
            }
        }

        if auto_stop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            self.finish_performance_trace(
                arguments.get("filePath").and_then(Value::as_str),
                deadline,
            )
            .await
        } else {
            Ok(tool_success_text(
                "MoonDesk performance trace started.".to_string(),
            ))
        }
    }

    async fn performance_stop_trace(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        self.page_id_from_arguments(arguments).await?;
        self.finish_performance_trace(arguments.get("filePath").and_then(Value::as_str), deadline)
            .await
    }

    async fn finish_performance_trace(
        &self,
        file_path: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let recording = self.trace.lock().await.clone();
        let Some(recording) = recording else {
            return Ok(tool_error(
                "No MoonDesk performance trace is currently active".to_string(),
            ));
        };

        self.connection
            .call("Tracing.end", json!({}), None, deadline)
            .await?;
        self.connection
            .wait_for_captured_method(recording.capture_id, "Tracing.tracingComplete", deadline)
            .await?;
        let mut capture = self
            .connection
            .finish_event_capture(recording.capture_id)
            .await?;
        *self.trace.lock().await = None;

        if capture.overflowed {
            return Ok(tool_success_structured(
                format!(
                    "Performance trace stopped, but captured data exceeded MoonDesk's {} MiB safety limit and was discarded.",
                    MAX_TRACE_BYTES / (1024 * 1024)
                ),
                json!({
                    "traceStopped": true,
                    "captureOverflowed": true,
                    "maxBytes": MAX_TRACE_BYTES,
                }),
            ));
        }

        let mut trace_events = Vec::new();
        for event in &mut capture.events {
            if event.method != "Tracing.dataCollected" {
                continue;
            }
            if let Some(values) = event.params.get_mut("value").and_then(Value::as_array_mut) {
                trace_events.append(values);
            }
        }

        let event_count = trace_events.len();
        if let Some(file_path) = file_path {
            let trace = json!({
                "traceEvents": trace_events,
                "metadata": {
                    "source": "MoonDesk native CDP",
                }
            });
            let mut file = std::fs::File::create(file_path).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not create performance trace at {file_path}: {error}"
                ))
            })?;
            serde_json::to_writer(&mut file, &trace).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not encode performance trace: {error}"
                ))
            })?;
            file.sync_all().map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not sync performance trace at {file_path}: {error}"
                ))
            })?;
            Ok(tool_success_text(format!("Saved to {file_path}.")))
        } else {
            Ok(tool_success_structured(
                format!("Captured {event_count} native CDP performance trace events."),
                json!({ "eventCount": event_count }),
            ))
        }
    }

    async fn take_heapsnapshot(
        &self,
        arguments: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value, BrowserTransportError> {
        let page_id = self.page_id_from_arguments(arguments).await?;
        let session_id = self.ensure_page_session(page_id, deadline).await?;
        let file_path = required_str(arguments, "filePath")?;

        self.connection
            .call(
                "HeapProfiler.enable",
                json!({}),
                Some(&session_id),
                deadline,
            )
            .await?;
        let capture_id = self
            .connection
            .start_event_capture(
                Some(&session_id),
                &["HeapProfiler.addHeapSnapshotChunk"],
                MAX_HEAP_SNAPSHOT_BYTES,
            )
            .await;
        if let Err(error) = self
            .connection
            .call(
                "HeapProfiler.takeHeapSnapshot",
                json!({
                    "reportProgress": false,
                    "captureNumericValue": true,
                }),
                Some(&session_id),
                deadline,
            )
            .await
        {
            self.connection.cancel_event_capture(capture_id).await;
            return Err(error);
        }

        let capture = self.connection.finish_event_capture(capture_id).await?;
        if capture.overflowed {
            return Ok(tool_error(format!(
                "Heap snapshot exceeded MoonDesk's {} MiB safety limit",
                MAX_HEAP_SNAPSHOT_BYTES / (1024 * 1024)
            )));
        }

        let mut file = std::fs::File::create(file_path).map_err(|error| {
            BrowserTransportError::Protocol(format!(
                "Could not create heap snapshot at {file_path}: {error}"
            ))
        })?;
        let mut wrote_chunk = false;
        for event in capture.events {
            let Some(chunk) = event.params.get("chunk").and_then(Value::as_str) else {
                continue;
            };
            std::io::Write::write_all(&mut file, chunk.as_bytes()).map_err(|error| {
                BrowserTransportError::Protocol(format!(
                    "Could not write heap snapshot to {file_path}: {error}"
                ))
            })?;
            wrote_chunk = true;
        }
        if !wrote_chunk {
            let _ = std::fs::remove_file(file_path);
            return Ok(tool_error(
                "Chrome completed the heap snapshot without emitting snapshot chunks".to_string(),
            ));
        }
        file.sync_all().map_err(|error| {
            BrowserTransportError::Protocol(format!(
                "Could not sync heap snapshot at {file_path}: {error}"
            ))
        })?;
        Ok(tool_success_text(format!("Saved to {file_path}.")))
    }
}

async fn wait_for_devtools_endpoint(
    profile_dir: &Path,
    deadline: tokio::time::Instant,
) -> Result<String, BrowserTransportError> {
    let path = profile_dir.join(DEVTOOLS_ACTIVE_PORT_FILE);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserTransportError::Timeout);
        }
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let mut lines = content.lines();
                let Some(port) = lines
                    .next()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                else {
                    tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
                    continue;
                };
                let Some(browser_path) = lines
                    .next()
                    .map(str::trim)
                    .filter(|value| value.starts_with("/devtools/browser/"))
                else {
                    tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
                    continue;
                };
                let parsed_port = port.parse::<u16>().map_err(|_| {
                    BrowserTransportError::Protocol(format!(
                        "Chromium wrote invalid DevTools port '{port}'"
                    ))
                })?;
                return Ok(format!("ws://127.0.0.1:{parsed_port}{browser_path}"));
            }
            Err(error) if devtools_endpoint_read_error_is_transient(&error) => {
                tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
            }
            Err(error) => {
                return Err(BrowserTransportError::Disconnected(format!(
                    "Could not read Chromium DevTools endpoint {}: {error}",
                    path.display()
                )));
            }
        }
    }
}

fn devtools_endpoint_read_error_is_transient(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

async fn drain_browser_stream<R>(
    mut stream: R,
    state: Option<SharedState>,
    stream_name: &'static str,
) where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes).await;
    if bytes.is_empty() {
        return;
    }
    if let Some(state) = state {
        let text = String::from_utf8_lossy(&bytes);
        let diagnostic = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(8)
            .collect::<Vec<_>>()
            .join(" | ");
        if !diagnostic.is_empty() {
            state.lock().await.log(
                "DEBUG",
                format!("MoonDesk Chromium {stream_name}: {diagnostic}"),
            );
        }
    }
}

fn create_private_runtime_dir(prefix: &str) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&path)?;
    set_private_dir_permissions(&path);
    Ok(path)
}

fn set_private_dir_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

async fn resolve_browser_executable() -> Result<(PathBuf, String), String> {
    if let Some(path) = std::env::var_os("MOONDESK_BROWSER_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        if path.is_file() {
            return Ok((path, "MoonDesk browser override".to_string()));
        }
        return Err(format!(
            "MOONDESK_BROWSER_PATH points to a missing browser executable: {}",
            path.display()
        ));
    }

    if let Some(browser) = crate::browser_manager::installed_browser()? {
        return Ok((
            browser.executable,
            format!("MoonDesk Chromium {}", browser.version),
        ));
    }

    let browser = crate::browser_manager::ensure_browser().await?;
    Ok((
        browser.executable,
        format!("MoonDesk Chromium {}", browser.version),
    ))
}

fn pages_result(pages: Vec<Value>) -> Value {
    let mut lines = vec!["## Pages".to_string()];
    for page in &pages {
        let id = page.get("id").and_then(Value::as_u64).unwrap_or(0);
        let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
        let title = page
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let selected = page
            .get("selected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let label = title.map_or_else(|| url.to_string(), |title| format!("{title} ({url})"));
        lines.push(format!(
            "{id}: {label}{}",
            if selected { " [selected]" } else { "" }
        ));
    }
    tool_success_structured(lines.join("\n"), json!({ "pages": pages }))
}

fn page_values(state: &CdpBrowserState) -> Vec<Value> {
    state
        .pages_by_id
        .iter()
        .map(|(page_id, page)| {
            json!({
                "id": page_id,
                "url": page.url,
                "title": page.title,
                "selected": state.selected_page == Some(*page_id),
                "isolatedContext": page.context_name,
            })
        })
        .collect()
}

fn tool_success_text(text: String) -> Value {
    json!({
        "isError": false,
        "content": [{ "type": "text", "text": text }],
    })
}

fn tool_success_structured(text: String, structured: Value) -> Value {
    json!({
        "isError": false,
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    })
}

fn tool_error(message: String) -> Value {
    json!({
        "isError": true,
        "content": [{ "type": "text", "text": message }],
    })
}

fn append_tool_content(target: &mut Value, source: &Value) {
    let additions = source
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(content) = target.get_mut("content").and_then(Value::as_array_mut) {
        content.extend(additions);
    }
}

fn required_str<'a>(value: &'a Value, name: &str) -> Result<&'a str, BrowserTransportError> {
    value.get(name).and_then(Value::as_str).ok_or_else(|| {
        BrowserTransportError::Protocol(format!("Missing or invalid browser parameter '{name}'"))
    })
}

fn required_u64(value: &Value, name: &str) -> Result<u64, BrowserTransportError> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get(name)
                .and_then(Value::as_f64)
                .filter(|number| number.is_finite() && number.fract() == 0.0 && *number >= 0.0)
                .map(|number| number as u64)
        })
        .ok_or_else(|| {
            BrowserTransportError::Protocol(format!(
                "Missing or invalid browser parameter '{name}'"
            ))
        })
}

fn required_f64(value: &Value, name: &str) -> Result<f64, BrowserTransportError> {
    value.get(name).and_then(Value::as_f64).ok_or_else(|| {
        BrowserTransportError::Protocol(format!("Missing or invalid browser parameter '{name}'"))
    })
}

fn ax_value_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
}

fn escape_snapshot_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn quad_center(quad: &[Value]) -> Option<(f64, f64)> {
    let (x, y, width, height) = quad_bounds(quad)?;
    Some((x + (width / 2.0), y + (height / 2.0)))
}

fn quad_bounds(quad: &[Value]) -> Option<(f64, f64, f64, f64)> {
    if quad.len() < 8 {
        return None;
    }
    let xs = [0usize, 2, 4, 6]
        .into_iter()
        .filter_map(|index| quad.get(index).and_then(Value::as_f64))
        .collect::<Vec<_>>();
    let ys = [1usize, 3, 5, 7]
        .into_iter()
        .filter_map(|index| quad.get(index).and_then(Value::as_f64))
        .collect::<Vec<_>>();
    if xs.len() != 4 || ys.len() != 4 {
        return None;
    }
    let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let width = max_x - min_x;
    let height = max_y - min_y;
    (width > 0.0 && height > 0.0).then_some((min_x, min_y, width, height))
}

fn browser_string_filter(arguments: &Value, name: &str) -> HashSet<String> {
    arguments
        .get(name)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn browser_pagination(
    arguments: &Value,
    total: usize,
) -> Result<(usize, usize, usize, usize), BrowserTransportError> {
    let page_size = arguments
        .get("pageSize")
        .and_then(Value::as_u64)
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
        .unwrap_or(DEFAULT_EVENT_PAGE_SIZE);
    if page_size == 0 || page_size > MAX_EVENT_PAGE_SIZE {
        return Err(BrowserTransportError::Protocol(format!(
            "pageSize must be between 1 and {MAX_EVENT_PAGE_SIZE}"
        )));
    }
    let page_idx = arguments
        .get("pageIdx")
        .and_then(Value::as_u64)
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
        .unwrap_or(0);
    let start = page_idx.saturating_mul(page_size).min(total);
    let end = start.saturating_add(page_size).min(total);
    Ok((page_idx, page_size, start, end))
}

fn value_to_compact_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

fn network_profile(name: &str) -> (bool, f64, f64, f64) {
    match name {
        "Offline" => (true, 0.0, 0.0, 0.0),
        "Slow 3G" => (false, 2_000.0, 50_000.0, 50_000.0),
        "Fast 3G" => (false, 562.5, 180_000.0, 84_375.0),
        "Slow 4G" => (false, 170.0, 562_500.0, 562_500.0),
        "Fast 4G" => (false, 20.0, 2_500_000.0, 1_250_000.0),
        _ => (false, 0.0, -1.0, -1.0),
    }
}

struct ParsedViewport {
    width: u64,
    height: u64,
    dpr: f64,
    mobile: bool,
    touch: bool,
    landscape: bool,
}

impl ParsedViewport {
    fn parse(raw: &str) -> Result<Self, BrowserTransportError> {
        let mut parts = raw.split(',');
        let dimensions = parts.next().unwrap_or_default();
        let mut dimensions = dimensions.split('x');
        let width = dimensions
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| BrowserTransportError::Protocol(format!("Invalid viewport '{raw}'")))?;
        let height = dimensions
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| BrowserTransportError::Protocol(format!("Invalid viewport '{raw}'")))?;
        let dpr = dimensions
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(1.0);
        if !dpr.is_finite() || dpr <= 0.0 {
            return Err(BrowserTransportError::Protocol(format!(
                "Invalid viewport device scale factor in '{raw}'"
            )));
        }
        let flags = parts.collect::<Vec<_>>();
        Ok(Self {
            width,
            height,
            dpr,
            mobile: flags.contains(&"mobile"),
            touch: flags.contains(&"touch"),
            landscape: flags.contains(&"landscape"),
        })
    }
}

struct ParsedKey {
    key: String,
    code: String,
    modifiers: u64,
    windows_virtual_key_code: Option<u64>,
    text: Option<String>,
}

impl ParsedKey {
    fn parse(raw: &str) -> Result<Self, BrowserTransportError> {
        let parts = raw
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let Some(last) = parts.last().copied() else {
            return Err(BrowserTransportError::Protocol(
                "Keyboard key must not be empty".to_string(),
            ));
        };
        let mut modifiers = 0u64;
        for modifier in &parts[..parts.len().saturating_sub(1)] {
            match modifier.to_ascii_lowercase().as_str() {
                "alt" | "option" => modifiers |= 1,
                "ctrl" | "control" => modifiers |= 2,
                "meta" | "cmd" | "command" | "super" => modifiers |= 4,
                "shift" => modifiers |= 8,
                other => {
                    return Err(BrowserTransportError::Protocol(format!(
                        "Unsupported keyboard modifier '{other}'"
                    )));
                }
            }
        }

        let normalized = match last.to_ascii_lowercase().as_str() {
            "enter" | "return" => ("Enter", "Enter", Some(13), None),
            "tab" => ("Tab", "Tab", Some(9), None),
            "escape" | "esc" => ("Escape", "Escape", Some(27), None),
            "backspace" => ("Backspace", "Backspace", Some(8), None),
            "delete" | "del" => ("Delete", "Delete", Some(46), None),
            "arrowup" | "up" => ("ArrowUp", "ArrowUp", Some(38), None),
            "arrowdown" | "down" => ("ArrowDown", "ArrowDown", Some(40), None),
            "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", Some(37), None),
            "arrowright" | "right" => ("ArrowRight", "ArrowRight", Some(39), None),
            "home" => ("Home", "Home", Some(36), None),
            "end" => ("End", "End", Some(35), None),
            "pageup" => ("PageUp", "PageUp", Some(33), None),
            "pagedown" => ("PageDown", "PageDown", Some(34), None),
            "space" => (" ", "Space", Some(32), Some(" ".to_string())),
            value if value.chars().count() == 1 => {
                let text = last.to_string();
                let code = last
                    .chars()
                    .next()
                    .map(|ch| {
                        if ch.is_ascii_alphabetic() {
                            format!("Key{}", ch.to_ascii_uppercase())
                        } else if ch.is_ascii_digit() {
                            format!("Digit{ch}")
                        } else {
                            last.to_string()
                        }
                    })
                    .unwrap_or_else(|| last.to_string());
                let key = last.to_string();
                let vk = last
                    .chars()
                    .next()
                    .filter(|ch| ch.is_ascii())
                    .map(|ch| u64::from(ch.to_ascii_uppercase() as u8));
                return Ok(Self {
                    key,
                    code,
                    modifiers,
                    windows_virtual_key_code: vk,
                    text: (modifiers == 0 || modifiers == 8).then_some(text),
                });
            }
            _ => (last, last, None, None),
        };
        Ok(Self {
            key: normalized.0.to_string(),
            code: normalized.1.to_string(),
            modifiers,
            windows_virtual_key_code: normalized.2,
            text: normalized.3,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_parser_keeps_agent_emulation_flags() {
        let viewport = ParsedViewport::parse("390x844x3,mobile,touch,landscape")
            .unwrap_or_else(|error| panic!("parse viewport: {error}"));
        assert_eq!(viewport.width, 390);
        assert_eq!(viewport.height, 844);
        assert_eq!(viewport.dpr, 3.0);
        assert!(viewport.mobile);
        assert!(viewport.touch);
        assert!(viewport.landscape);
    }

    #[test]
    fn key_parser_maps_modifiers_without_shell_semantics() {
        let key =
            ParsedKey::parse("CTRL+SHIFT+A").unwrap_or_else(|error| panic!("parse key: {error}"));
        assert_eq!(key.key, "A");
        assert_eq!(key.code, "KeyA");
        assert_eq!(key.modifiers, 10);
    }

    #[test]
    fn devtools_endpoint_missing_file_is_transient() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(devtools_endpoint_read_error_is_transient(&error));
    }

    #[cfg(windows)]
    #[test]
    fn devtools_endpoint_windows_sharing_violation_is_transient() {
        for code in [32, 33] {
            let error = std::io::Error::from_raw_os_error(code);
            assert!(devtools_endpoint_read_error_is_transient(&error));
        }
        let error = std::io::Error::from_raw_os_error(5);
        assert!(!devtools_endpoint_read_error_is_transient(&error));
    }
}
