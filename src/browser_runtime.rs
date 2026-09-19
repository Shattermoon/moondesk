use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::browser_cdp::{BrowserCdpTransport, BrowserTransportError};
pub use crate::browser_contract::canonical_browser_flag_name;
use crate::browser_contract::{
    BrowserOutputFormat, ParsedBrowserInvocation, browser_command_help,
    browser_structured_arguments_to_cli, parse_browser_cli_invocation,
};
use crate::state::{BrowserPresentation, Mode, SharedState};
use crate::workspaces::WorkspaceId;

pub const DEFAULT_BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_BROWSER_TIMEOUT_MS: u64 = 120_000;
pub const MAX_BROWSER_COMMAND_BYTES: usize = 128;
pub const MAX_BROWSER_ARGS: usize = 64;
pub const MAX_BROWSER_ARG_BYTES: usize = 8 * 1024;
pub const MAX_BROWSER_CONTROL_BODY_BYTES: usize = 128 * 1024;
const MAX_CAPTURED_OUTPUT_BYTES: usize = 256 * 1024;
const BROWSER_PRESENTATION_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const BROWSER_WORKSPACE_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct BrowserCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub restarted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserPresentationChange {
    Unchanged,
    Updated,
    UpdatedAndSessionClosed,
    RequiresRestart,
    Busy,
}

impl BrowserCommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    pub fn failure_details(&self) -> String {
        [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum BrowserCallerIdentity {
    OpenAi {
        subject_digest: Option<[u8; 32]>,
        session_digest: [u8; 32],
    },
    LocalCli,
    WorkspaceFallback,
    Standalone,
}

fn browser_identity_digest(label: &str, value: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"moondesk-browser-identity-v1\0");
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    hasher.finalize().into()
}

fn browser_workspace_context_name(workspace_key: &str) -> String {
    let digest = browser_identity_digest("workspace", workspace_key);
    let mut suffix = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        let _ = write!(&mut suffix, "{byte:02x}");
    }
    format!("moondesk-ws-{suffix}")
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BrowserSessionKey {
    workspace_key: String,
    caller: BrowserCallerIdentity,
}

impl BrowserSessionKey {
    pub fn openai(workspace_id: &WorkspaceId, subject: Option<&str>, session: &str) -> Self {
        Self {
            workspace_key: workspace_id.as_str().to_string(),
            caller: BrowserCallerIdentity::OpenAi {
                subject_digest: subject.map(|value| browser_identity_digest("subject", value)),
                session_digest: browser_identity_digest("session", session),
            },
        }
    }

    pub fn local_cli(workspace_id: &WorkspaceId) -> Self {
        Self {
            workspace_key: workspace_id.as_str().to_string(),
            caller: BrowserCallerIdentity::LocalCli,
        }
    }

    pub fn workspace_fallback(workspace_id: &WorkspaceId) -> Self {
        Self {
            workspace_key: workspace_id.as_str().to_string(),
            caller: BrowserCallerIdentity::WorkspaceFallback,
        }
    }

    fn standalone(workspace_root: &str) -> Self {
        Self {
            workspace_key: workspace_root.to_string(),
            caller: BrowserCallerIdentity::Standalone,
        }
    }

    fn workspace_context_name(&self) -> String {
        browser_workspace_context_name(&self.workspace_key)
    }

    fn belongs_to_workspace(&self, workspace_id: &WorkspaceId) -> bool {
        self.workspace_key == workspace_id.as_str()
    }
}

#[derive(Clone)]
struct BrowserOwnedPage {
    upstream_id: u64,
    url: String,
    title: String,
}

struct BrowserLogicalSession {
    pages: BTreeMap<u64, BrowserOwnedPage>,
    active_page: Option<u64>,
    next_page_id: u64,
}

impl BrowserLogicalSession {
    fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            active_page: None,
            next_page_id: 1,
        }
    }
}

#[derive(Clone)]
struct BrowserPageLease {
    owner: BrowserSessionKey,
    upstream_page_id: u64,
}

#[derive(Default)]
struct BrowserRoutingState {
    sessions: HashMap<BrowserSessionKey, BrowserLogicalSession>,
    upstream_owners: HashMap<u64, (BrowserSessionKey, u64)>,
    active_trace: Option<BrowserPageLease>,
}

impl BrowserRoutingState {
    fn clear(&mut self) {
        self.sessions.clear();
        self.upstream_owners.clear();
        self.active_trace = None;
    }
}

#[derive(Default)]
struct BrowserRuntimeState {
    transport: Option<Arc<BrowserCdpTransport>>,
    has_started: bool,
    generation: u64,
    routing: BrowserRoutingState,
}

#[derive(Clone, Debug)]
struct UpstreamPageInfo {
    id: u64,
    url: String,
    title: String,
    selected: bool,
    isolated_context: Option<String>,
}

/// Shared, lazy native CDP browser runtime owned directly by the MoonDesk host.
///
/// Constructing this value never launches Chromium. The first browser operation starts one
/// MoonDesk-owned Chromium process and connects to its browser-level DevTools WebSocket directly.
/// Workspaces get isolated BrowserContexts inside that Chromium process, while each MCP
/// conversation and local CLI caller owns a logical page set routed by MoonDesk.
pub struct BrowserRuntime {
    state: Option<SharedState>,
    runtime: Mutex<BrowserRuntimeState>,
    operation: Mutex<()>,
}

fn external_browser_input_files_allowed(mode: Option<Mode>) -> bool {
    match mode {
        Some(mode) => mode.computer_enabled(),
        None => true,
    }
}

impl BrowserRuntime {
    pub fn new(state: SharedState) -> Self {
        Self::with_optional_state(Some(state))
    }

    /// Create a runtime without TUI state for command-help and isolated integration tests.
    /// Normal `moondesk browser` actions are lightweight clients to the running MoonDesk host.
    pub fn standalone() -> Self {
        Self::with_optional_state(None)
    }

    fn with_optional_state(state: Option<SharedState>) -> Self {
        Self {
            state,
            runtime: Mutex::new(BrowserRuntimeState::default()),
            operation: Mutex::new(()),
        }
    }

    pub async fn run(
        &self,
        workspace_root: &str,
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        let session = BrowserSessionKey::standalone(workspace_root);
        self.run_for_session(&session, workspace_root, command, args, timeout)
            .await
    }

    pub async fn run_for_session(
        &self,
        session: &BrowserSessionKey,
        workspace_root: &str,
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        self.run_internal(session, workspace_root, command, args, timeout, None)
            .await
    }

    pub(crate) async fn run_managed_temp_output_for_session(
        &self,
        session: &BrowserSessionKey,
        workspace_root: &str,
        command: &str,
        args: &[String],
        managed_temp_output: &Path,
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        if command.trim() != "take_screenshot" || !managed_browser_temp_output(managed_temp_output)
        {
            return Err(
                "Managed browser output is reserved for MoonDesk's internal view_page screenshot"
                    .to_string(),
            );
        }
        self.run_internal(
            session,
            workspace_root,
            command,
            args,
            timeout,
            Some(managed_temp_output),
        )
        .await
    }

    pub(crate) async fn fill_form_for_session(
        &self,
        session: &BrowserSessionKey,
        workspace_root: &str,
        elements: &[(String, String)],
        include_snapshot: bool,
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        if elements.is_empty() {
            return Err("fill_form requires at least one element".to_string());
        }
        let calls = elements
            .iter()
            .enumerate()
            .map(|(index, (uid, value))| {
                let args = browser_structured_arguments_to_cli(
                    "fill",
                    &serde_json::json!({
                        "uid": uid,
                        "value": value,
                        "includeSnapshot": include_snapshot && index + 1 == elements.len(),
                    }),
                )?;
                Ok(("fill".to_string(), args))
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.run_serialized_fill_calls(session, workspace_root, &calls, timeout)
            .await
    }

    pub(crate) async fn wait_for_text_for_session(
        &self,
        session: &BrowserSessionKey,
        _workspace_root: &str,
        texts: &[String],
        timeout_ms: Option<u64>,
    ) -> Result<BrowserCommandOutput, String> {
        if texts.is_empty() {
            return Err("wait_for requires at least one text value".to_string());
        }
        let requested_timeout = timeout_ms.unwrap_or(0);
        // This is only MoonDesk's queue/transport safety ceiling. The connector timeout itself is
        // forwarded unchanged below, including omitted/zero values, so pinned v1.7 keeps ownership
        // of its actual waiter default and CPU-throttling semantics.
        let operation_timeout = if requested_timeout == 0 {
            DEFAULT_BROWSER_COMMAND_TIMEOUT
        } else {
            Duration::from_millis(requested_timeout).saturating_add(Duration::from_secs(10))
        };
        let deadline = tokio::time::Instant::now() + operation_timeout;
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(operation_timeout))?;
        let (transport, restarted) = self.ensure_transport(deadline).await?;
        let page_id = self
            .ensure_active_upstream_page(session, &transport, deadline, operation_timeout)
            .await?;

        let mut arguments = serde_json::json!({ "text": texts, "pageId": page_id });
        if let Some(timeout_ms) = timeout_ms {
            arguments["timeout"] = Value::from(timeout_ms);
        }
        let result = self
            .call_transport_tool(
                &transport,
                "wait_for",
                arguments,
                deadline,
                operation_timeout,
            )
            .await?;
        let parsed = parse_browser_cli_invocation("take_snapshot", &[])?;
        browser_output_from_result(result, parsed, restarted)
    }

    pub(crate) async fn scroll_for_session(
        &self,
        session: &BrowserSessionKey,
        delta_x: f64,
        delta_y: f64,
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        if timeout.is_zero() {
            return Err("Browser command timeout must be at least 1 ms".to_string());
        }
        if !delta_x.is_finite() || !delta_y.is_finite() || (delta_x == 0.0 && delta_y == 0.0) {
            return Err("Browser scroll requires finite non-zero delta_x or delta_y".to_string());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(timeout))?;
        let (transport, restarted) = self.ensure_transport(deadline).await?;
        let page_id = self
            .ensure_active_upstream_page(session, &transport, deadline, timeout)
            .await?;
        let result = self
            .call_transport_tool(
                &transport,
                "scroll",
                serde_json::json!({
                    "pageId": page_id,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                }),
                deadline,
                timeout,
            )
            .await?;
        let parsed = ParsedBrowserInvocation {
            arguments: Map::new(),
            output_format: BrowserOutputFormat::Json,
        };
        browser_output_from_result(result, parsed, restarted)
    }

    async fn run_serialized_fill_calls(
        &self,
        session: &BrowserSessionKey,
        _workspace_root: &str,
        calls: &[(String, Vec<String>)],
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        if timeout.is_zero() {
            return Err("Browser command timeout must be at least 1 ms".to_string());
        }
        let parsed_calls = calls
            .iter()
            .map(|(command, args)| {
                parse_browser_cli_invocation(command, args).map(|parsed| (command.as_str(), parsed))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let deadline = tokio::time::Instant::now() + timeout;
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(timeout))?;
        let (transport, restarted) = self.ensure_transport(deadline).await?;
        let page_id = self
            .ensure_active_upstream_page(session, &transport, deadline, timeout)
            .await?;
        let mut last_output = BrowserCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            restarted,
        };
        for (index, (command, parsed)) in parsed_calls.into_iter().enumerate() {
            let mut arguments = parsed.arguments.clone();
            arguments.insert("pageId".to_string(), Value::from(page_id));
            let result = self
                .call_transport_tool(
                    &transport,
                    command,
                    Value::Object(arguments),
                    deadline,
                    timeout,
                )
                .await?;
            let mut output = browser_output_from_result(result, parsed, restarted)?;
            if !output.success() {
                let diagnostic =
                    format!("fill_form element {index} failed after {index} completed element(s)");
                output.stderr = if output.stderr.trim().is_empty() {
                    diagnostic
                } else {
                    format!("{diagnostic}\n{}", output.stderr)
                };
                return Ok(output);
            }
            last_output = output;
        }
        Ok(last_output)
    }

    async fn run_internal(
        &self,
        session: &BrowserSessionKey,
        workspace_root: &str,
        command: &str,
        args: &[String],
        timeout: Duration,
        managed_temp_output: Option<&Path>,
    ) -> Result<BrowserCommandOutput, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Browser command cannot be empty".to_string());
        }
        if browser_service_command(command) {
            return Err(
                "MoonDesk owns the browser lifecycle; use a browser operation such as list_pages, new_page, take_snapshot, click, fill, resize_page, or evaluate_script instead"
                    .to_string(),
            );
        }
        if timeout.is_zero() {
            return Err("Browser command timeout must be at least 1 ms".to_string());
        }
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
        {
            return run_cli_help(command);
        }

        let deadline = tokio::time::Instant::now() + timeout;
        // Browser-only mode intentionally keeps local file inputs workspace-scoped. Both mode
        // already exposes explicit absolute-file reads and the unrestricted developer shell, so an
        // explicitly addressed external file may be staged for browser input there. Standalone CLI
        // calls are initiated directly by the local user and follow the same permissive input rule.
        let mode = if let Some(state) = &self.state {
            Some(state.lock().await.mode)
        } else {
            None
        };
        let allow_external_absolute_input_files = external_browser_input_files_allowed(mode);
        let prepare_workspace = workspace_root.to_string();
        let prepare_command = command.to_string();
        let prepare_args = args.to_vec();
        let prepare_managed = managed_temp_output.map(Path::to_path_buf);
        let prepare_task = tokio::task::spawn_blocking(move || {
            prepare_browser_invocation_with_managed_temp_deadline(
                &prepare_workspace,
                &prepare_command,
                &prepare_args,
                allow_external_absolute_input_files,
                prepare_managed.as_deref(),
                Some(deadline),
            )
        });
        let mut prepared = tokio::time::timeout_at(deadline, prepare_task)
            .await
            .map_err(|_| total_timeout_message(timeout))?
            .map_err(|error| format!("Browser staging task failed: {error}"))??;
        let parsed = parse_browser_cli_invocation(command, &prepared.args)?;

        // MoonDesk owns browser authority. This lock serializes logical-page reconciliation and
        // every page-scoped call is routed to the exact conversation-owned CDP target/session;
        // Chromium's visible selected tab is never caller authority.
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(timeout))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(total_timeout_message(timeout));
        }

        let (transport, restarted) = self.ensure_transport(deadline).await?;
        let mut arguments = parsed.arguments.clone();
        let mut result = match command {
            "list_pages" => {
                self.ensure_active_upstream_page(session, &transport, deadline, timeout)
                    .await?;
                let result = self
                    .list_upstream_pages_result(&transport, deadline, timeout)
                    .await?;
                let pages = upstream_pages_from_result(&result)?;
                self.ensure_global_recording_pages_present(&transport, &pages)
                    .await?;
                self.reconcile_pages(session, &pages, None).await;
                result
            }
            "new_page" => {
                if arguments.contains_key("isolatedContext") {
                    return Err(
                        "MoonDesk owns browser workspace isolation; isolatedContext cannot be supplied by callers"
                            .to_string(),
                    );
                }
                let before = self
                    .list_upstream_pages(&transport, deadline, timeout)
                    .await?;
                self.ensure_global_recording_pages_present(&transport, &before)
                    .await?;
                let before_ids = upstream_page_ids(&before);
                arguments.insert(
                    "isolatedContext".to_string(),
                    Value::String(session.workspace_context_name()),
                );
                let result = self
                    .call_transport_tool(
                        &transport,
                        command,
                        Value::Object(arguments),
                        deadline,
                        timeout,
                    )
                    .await?;
                if !browser_result_is_error(&result) {
                    let pages = self
                        .pages_from_result_or_list(&transport, &result, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    let claim_ids = new_upstream_page_ids(&before_ids, &pages);
                    self.reconcile_pages(session, &pages, Some(&claim_ids))
                        .await;
                }
                result
            }
            "select_page" => {
                let logical_page_id = browser_requested_page_id(&arguments)?;
                let upstream_page_id = self
                    .owned_upstream_page_id(session, logical_page_id)
                    .await?;
                arguments.insert("pageId".to_string(), Value::from(upstream_page_id));
                let result = self
                    .call_transport_tool(
                        &transport,
                        command,
                        Value::Object(arguments),
                        deadline,
                        timeout,
                    )
                    .await?;
                if !browser_result_is_error(&result) {
                    self.set_active_logical_page(session, logical_page_id).await;
                    let pages = self
                        .pages_from_result_or_list(&transport, &result, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    self.reconcile_pages(session, &pages, None).await;
                }
                result
            }
            "close_page" => {
                let logical_page_id = browser_requested_page_id(&arguments)?;
                let upstream_page_id = self
                    .owned_upstream_page_id(session, logical_page_id)
                    .await?;
                self.ensure_page_can_close(session, upstream_page_id)
                    .await?;
                self.select_surviving_upstream_page_before_close(
                    &transport,
                    upstream_page_id,
                    deadline,
                    timeout,
                )
                .await?;
                arguments.insert("pageId".to_string(), Value::from(upstream_page_id));
                let result = self
                    .call_transport_tool(
                        &transport,
                        command,
                        Value::Object(arguments),
                        deadline,
                        timeout,
                    )
                    .await?;
                if !browser_result_is_error(&result) {
                    let pages = self
                        .pages_from_result_or_list(&transport, &result, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    self.reconcile_pages(session, &pages, None).await;
                }
                result
            }
            _ if browser_page_scoped_command(command) => {
                let mut page_id = self
                    .ensure_active_upstream_page(session, &transport, deadline, timeout)
                    .await?;
                if command == "performance_stop_trace" {
                    let pages = self
                        .list_upstream_pages(&transport, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    page_id = self.performance_trace_page(session).await?;
                }
                let before_ids = if browser_command_may_open_or_close_pages(command) {
                    let pages = self
                        .list_upstream_pages(&transport, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    Some(upstream_page_ids(&pages))
                } else {
                    None
                };
                if command == "evaluate_script" && arguments.contains_key("serviceWorkerId") {
                    return Err(
                        "evaluate_script serviceWorkerId targeting is disabled in MoonDesk's shared Chromium runtime because raw service-worker IDs are not scoped to the caller's logical browser session"
                            .to_string(),
                    );
                }
                arguments.insert("pageId".to_string(), Value::from(page_id));
                let trace_auto_stop = if command == "performance_start_trace" {
                    self.begin_performance_trace(session, page_id).await?;
                    Some(
                        arguments
                            .get("autoStop")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                    )
                } else {
                    None
                };
                let result = match self
                    .call_transport_tool(
                        &transport,
                        command,
                        Value::Object(arguments),
                        deadline,
                        timeout,
                    )
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        if trace_auto_stop.is_some() {
                            self.finish_performance_trace(session).await;
                        }
                        return Err(error);
                    }
                };
                let browser_error = browser_result_is_error(&result);
                if let Some(auto_stop) = trace_auto_stop {
                    if browser_error || auto_stop {
                        self.finish_performance_trace(session).await;
                    }
                } else if command == "performance_stop_trace" && !browser_error {
                    self.finish_performance_trace(session).await;
                }
                if !browser_error && let Some(before_ids) = before_ids {
                    let pages = self
                        .list_upstream_pages(&transport, deadline, timeout)
                        .await?;
                    self.ensure_global_recording_pages_present(&transport, &pages)
                        .await?;
                    let claim_ids = new_upstream_page_ids(&before_ids, &pages);
                    self.reconcile_pages(session, &pages, Some(&claim_ids))
                        .await;
                }
                result
            }
            _ => {
                return Err(format!(
                    "Browser command '{command}' is not classified for MoonDesk's shared session router"
                ));
            }
        };

        self.sanitize_page_result_for_session(session, &mut result)
            .await;
        let mut output = browser_output_from_result(result, parsed, restarted)?;
        if output.success() {
            if tokio::time::Instant::now() >= deadline {
                self.invalidate_transport(&transport, "browser output deadline expired")
                    .await;
                return Err(total_timeout_message(timeout));
            }
            let commit_task = tokio::task::spawn_blocking(move || {
                let result = prepared.commit_outputs(Some(deadline));
                (prepared, result)
            });
            let (returned, commit_result) = commit_task
                .await
                .map_err(|error| format!("Browser output publication task failed: {error}"))?;
            prepared = returned;
            if let Err(error) = commit_result {
                if browser_deadline_expired(Some(deadline)) {
                    self.invalidate_transport(
                        &transport,
                        "browser output publication exceeded its deadline",
                    )
                    .await;
                    return Err(total_timeout_message(timeout));
                }
                return Err(error);
            }
        }
        prepared.rewrite_output_paths(&mut output);
        Ok(output)
    }

    async fn ensure_active_upstream_page(
        &self,
        session: &BrowserSessionKey,
        transport: &Arc<BrowserCdpTransport>,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<u64, String> {
        if let Some(page_id) = self.active_upstream_page_id(session).await {
            return Ok(page_id);
        }

        let existing = self
            .list_upstream_pages(transport, deadline, timeout)
            .await?;
        self.ensure_global_recording_pages_present(transport, &existing)
            .await?;
        self.reconcile_pages(session, &existing, None).await;
        if let Some(page_id) = self.active_upstream_page_id(session).await {
            return Ok(page_id);
        }

        let before_ids = upstream_page_ids(&existing);
        let created = self
            .call_transport_tool(
                transport,
                "new_page",
                serde_json::json!({
                    "url": "about:blank",
                    "background": true,
                    "isolatedContext": session.workspace_context_name(),
                }),
                deadline,
                timeout,
            )
            .await?;
        if browser_result_is_error(&created) {
            return Err(format!(
                "Could not create an isolated page for this browser session: {}",
                browser_result_text(&created)
            ));
        }
        let pages = self
            .pages_from_result_or_list(transport, &created, deadline, timeout)
            .await?;
        self.ensure_global_recording_pages_present(transport, &pages)
            .await?;
        let claim_ids = new_upstream_page_ids(&before_ids, &pages);
        self.reconcile_pages(session, &pages, Some(&claim_ids))
            .await;
        self.active_upstream_page_id(session).await.ok_or_else(|| {
            "MoonDesk created a browser page but could not bind it to this browser session"
                .to_string()
        })
    }

    async fn active_upstream_page_id(&self, session: &BrowserSessionKey) -> Option<u64> {
        let mut runtime = self.runtime.lock().await;
        let logical = runtime.routing.sessions.get_mut(session)?;
        let active = logical.active_page?;
        logical.pages.get(&active).map(|page| page.upstream_id)
    }

    async fn owned_upstream_page_id(
        &self,
        session: &BrowserSessionKey,
        logical_page_id: u64,
    ) -> Result<u64, String> {
        let mut runtime = self.runtime.lock().await;
        let logical = runtime.routing.sessions.get_mut(session).ok_or_else(|| {
            format!("Browser page {logical_page_id} does not belong to this session")
        })?;
        logical
            .pages
            .get(&logical_page_id)
            .map(|page| page.upstream_id)
            .ok_or_else(|| {
                format!("Browser page {logical_page_id} does not belong to this session")
            })
    }

    async fn set_active_logical_page(&self, session: &BrowserSessionKey, logical_page_id: u64) {
        let mut runtime = self.runtime.lock().await;
        if let Some(logical) = runtime.routing.sessions.get_mut(session)
            && logical.pages.contains_key(&logical_page_id)
        {
            logical.active_page = Some(logical_page_id);
        }
    }

    async fn missing_global_recording_page(
        &self,
        pages: &[UpstreamPageInfo],
    ) -> Option<&'static str> {
        let existing = pages.iter().map(|page| page.id).collect::<HashSet<_>>();
        let runtime = self.runtime.lock().await;
        if runtime
            .routing
            .active_trace
            .as_ref()
            .is_some_and(|active| !existing.contains(&active.upstream_page_id))
        {
            return Some("performance trace");
        }
        None
    }

    async fn ensure_global_recording_pages_present(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        pages: &[UpstreamPageInfo],
    ) -> Result<(), String> {
        let Some(kind) = self.missing_global_recording_page(pages).await else {
            return Ok(());
        };
        self.invalidate_transport(
            transport,
            "a page owning shared browser recording state disappeared",
        )
        .await;
        Err(format!(
            "The page owning the active {kind} disappeared. MoonDesk reset the shared browser runtime so recording state cannot leak across sessions; re-establish the page and start the recording again."
        ))
    }

    async fn ensure_page_can_close(
        &self,
        session: &BrowserSessionKey,
        upstream_page_id: u64,
    ) -> Result<(), String> {
        let runtime = self.runtime.lock().await;
        if runtime.routing.active_trace.as_ref().is_some_and(|active| {
            active.owner == *session && active.upstream_page_id == upstream_page_id
        }) {
            return Err(
                "Stop the active performance trace before closing its browser page".to_string(),
            );
        }
        Ok(())
    }

    async fn select_surviving_upstream_page_before_close(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        target_page_id: u64,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<(), String> {
        let pages = self
            .list_upstream_pages(transport, deadline, timeout)
            .await?;
        let Some(survivor_page_id) = browser_close_survivor_page_id(&pages, target_page_id) else {
            return Ok(());
        };
        let selected = self
            .call_transport_tool(
                transport,
                "select_page",
                serde_json::json!({
                    "pageId": survivor_page_id,
                    "bringToFront": false,
                }),
                deadline,
                timeout,
            )
            .await?;
        if browser_result_is_error(&selected) {
            return Err(format!(
                "Could not select MoonDesk's surviving browser page before close: {}",
                browser_result_text(&selected)
            ));
        }
        Ok(())
    }

    async fn begin_performance_trace(
        &self,
        session: &BrowserSessionKey,
        upstream_page_id: u64,
    ) -> Result<(), String> {
        let mut runtime = self.runtime.lock().await;
        if let Some(active) = runtime.routing.active_trace.as_ref() {
            return Err(if active.owner == *session {
                "This browser session already owns the active performance trace; stop it before starting another"
                    .to_string()
            } else {
                "Another browser session currently owns the shared Chromium performance trace; retry after it stops"
                    .to_string()
            });
        }
        runtime.routing.active_trace = Some(BrowserPageLease {
            owner: session.clone(),
            upstream_page_id,
        });
        Ok(())
    }

    async fn performance_trace_page(&self, session: &BrowserSessionKey) -> Result<u64, String> {
        let runtime = self.runtime.lock().await;
        match runtime.routing.active_trace.as_ref() {
            Some(active) if active.owner == *session => Ok(active.upstream_page_id),
            Some(_) => Err(
                "Another browser session owns the active performance trace; this session cannot stop it"
                    .to_string(),
            ),
            None => Err("This browser session has no active performance trace".to_string()),
        }
    }

    async fn finish_performance_trace(&self, session: &BrowserSessionKey) {
        let mut runtime = self.runtime.lock().await;
        if runtime
            .routing
            .active_trace
            .as_ref()
            .is_some_and(|active| active.owner == *session)
        {
            runtime.routing.active_trace = None;
        }
    }

    async fn list_upstream_pages_result(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<Value, String> {
        let result = self
            .call_transport_tool(
                transport,
                "list_pages",
                serde_json::json!({}),
                deadline,
                timeout,
            )
            .await?;
        if browser_result_is_error(&result) {
            return Err(format!(
                "Could not inspect browser pages: {}",
                browser_result_text(&result)
            ));
        }
        Ok(result)
    }

    async fn list_upstream_pages(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<Vec<UpstreamPageInfo>, String> {
        let result = self
            .list_upstream_pages_result(transport, deadline, timeout)
            .await?;
        upstream_pages_from_result(&result)
    }

    async fn pages_from_result_or_list(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        result: &Value,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<Vec<UpstreamPageInfo>, String> {
        match upstream_pages_from_result(result) {
            Ok(pages) => Ok(pages),
            Err(_) => self.list_upstream_pages(transport, deadline, timeout).await,
        }
    }

    async fn reconcile_pages(
        &self,
        acting_session: &BrowserSessionKey,
        pages: &[UpstreamPageInfo],
        claim_upstream_ids: Option<&HashSet<u64>>,
    ) {
        let all_page_ids = pages
            .iter()
            .map(|page| page.id)
            .collect::<std::collections::HashSet<_>>();
        let workspace_context = acting_session.workspace_context_name();
        let mut runtime = self.runtime.lock().await;

        let vanished = runtime
            .routing
            .upstream_owners
            .keys()
            .copied()
            .filter(|page_id| !all_page_ids.contains(page_id))
            .collect::<Vec<_>>();
        for upstream_id in vanished {
            if let Some((owner, logical_id)) = runtime.routing.upstream_owners.remove(&upstream_id)
                && let Some(logical) = runtime.routing.sessions.get_mut(&owner)
            {
                logical.pages.remove(&logical_id);
                if logical.active_page == Some(logical_id) {
                    logical.active_page = logical.pages.keys().next().copied();
                }
            }
        }

        for page in pages {
            if let Some((owner, logical_id)) =
                runtime.routing.upstream_owners.get(&page.id).cloned()
            {
                if let Some(logical) = runtime.routing.sessions.get_mut(&owner)
                    && let Some(owned) = logical.pages.get_mut(&logical_id)
                {
                    owned.url = page.url.clone();
                    owned.title = page.title.clone();
                }
                continue;
            }
            if !claim_upstream_ids.is_some_and(|ids| ids.contains(&page.id))
                || page.isolated_context.as_deref() != Some(workspace_context.as_str())
            {
                continue;
            }

            let logical = runtime
                .routing
                .sessions
                .entry(acting_session.clone())
                .or_insert_with(BrowserLogicalSession::new);
            let logical_id = logical.next_page_id;
            logical.next_page_id = logical.next_page_id.saturating_add(1);
            logical.pages.insert(
                logical_id,
                BrowserOwnedPage {
                    upstream_id: page.id,
                    url: page.url.clone(),
                    title: page.title.clone(),
                },
            );
            if logical.active_page.is_none() || page.selected {
                logical.active_page = Some(logical_id);
            }
            runtime
                .routing
                .upstream_owners
                .insert(page.id, (acting_session.clone(), logical_id));
        }

        if let Some(logical) = runtime.routing.sessions.get_mut(acting_session)
            && logical.active_page.is_none()
        {
            logical.active_page = logical.pages.keys().next().copied();
        }
    }

    pub(crate) async fn session_pages(&self, session: &BrowserSessionKey) -> Vec<Value> {
        let runtime = self.runtime.lock().await;
        let Some(logical) = runtime.routing.sessions.get(session) else {
            return Vec::new();
        };
        logical
            .pages
            .iter()
            .map(|(logical_id, page)| {
                serde_json::json!({
                    "id": logical_id,
                    "url": page.url,
                    "title": page.title,
                    "selected": logical.active_page == Some(*logical_id),
                })
            })
            .collect()
    }

    async fn sanitize_page_result_for_session(
        &self,
        session: &BrowserSessionKey,
        result: &mut Value,
    ) {
        let safe_pages = self.session_pages(session).await;
        let had_structured_pages = result
            .pointer("/structuredContent/pages")
            .and_then(Value::as_array)
            .is_some();
        if had_structured_pages
            && let Some(structured) = result
                .get_mut("structuredContent")
                .and_then(Value::as_object_mut)
        {
            structured.insert("pages".to_string(), Value::Array(safe_pages.clone()));
            structured.remove("extensionPages");
        }

        let safe_text = browser_safe_pages_markdown(&safe_pages);
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            for item in content {
                let Some(text) = item.get_mut("text") else {
                    continue;
                };
                let Some(original) = text.as_str() else {
                    continue;
                };
                *text = Value::String(rewrite_browser_page_sections(original, &safe_text));
            }
        }
    }

    async fn call_transport_tool(
        &self,
        transport: &Arc<BrowserCdpTransport>,
        command: &str,
        arguments: Value,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<Value, String> {
        match transport.call_tool(command, arguments, deadline).await {
            Ok(result) => Ok(result),
            Err(BrowserTransportError::Timeout) => {
                self.invalidate_transport(transport, "browser operation timed out")
                    .await;
                Err(total_timeout_message(timeout))
            }
            Err(BrowserTransportError::Disconnected(error)) => {
                self.invalidate_transport(transport, "browser runtime disconnected")
                    .await;
                Err(format!(
                    "Browser runtime was lost before the operation completed: {error}. The session was invalidated; retry from a fresh page/snapshot."
                ))
            }
            Err(BrowserTransportError::Protocol(error)) => Err(error),
        }
    }

    async fn ensure_transport(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(Arc<BrowserCdpTransport>, bool), String> {
        let (stale, has_started) = {
            let mut runtime = self.runtime.lock().await;
            if let Some(transport) = runtime.transport.as_ref()
                && transport.is_alive()
            {
                return Ok((transport.clone(), false));
            }
            (runtime.transport.take(), runtime.has_started)
        };
        if let Some(stale) = stale {
            stale.shutdown().await;
        }

        let presentation = if let Some(state) = &self.state {
            state.lock().await.browser_presentation
        } else {
            BrowserPresentation::default()
        };
        let transport = BrowserCdpTransport::start(presentation, self.state.clone(), deadline)
            .await
            .map_err(|error| match error {
                BrowserTransportError::Timeout => {
                    "Browser runtime startup exhausted the caller's total timeout".to_string()
                }
                other => format!("Could not start isolated MoonDesk browser runtime: {other}"),
            })?;
        let browser_name = transport.browser_name().to_string();

        let generation = {
            let mut runtime = self.runtime.lock().await;
            runtime.transport = Some(transport.clone());
            runtime.has_started = true;
            runtime.generation = runtime.generation.saturating_add(1);
            runtime.routing.clear();
            runtime.generation
        };
        if let Some(state) = &self.state {
            let mut app = state.lock().await;
            app.browser_runtime_running = true;
            app.log(
                "INFO",
                format!(
                    "Shared agent browser runtime generation {generation} started lazily with {browser_name} Â· {}",
                    presentation.label()
                ),
            );
        }
        Ok((transport, has_started))
    }

    async fn invalidate_transport(&self, expected: &Arc<BrowserCdpTransport>, reason: &str) {
        let transport = {
            let mut runtime = self.runtime.lock().await;
            let matches = runtime
                .transport
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected));
            let transport = matches.then(|| runtime.transport.take()).flatten();
            if transport.is_some() {
                runtime.routing.clear();
            }
            transport
        };
        if let Some(transport) = transport {
            transport.shutdown().await;
        }
        if let Some(state) = &self.state {
            let mut app = state.lock().await;
            app.browser_runtime_running = false;
            app.log(
                "WARN",
                format!("MoonDesk invalidated the owned browser runtime: {reason}"),
            );
        }
    }

    /// Change whether the isolated agent browser is headless or visible.
    ///
    /// Presentation is a Chromium process-startup choice. Hold the same operation lock used by
    /// browser actions while tearing down the current transport and publishing the new setting so
    /// another request cannot race in and restart Chromium with the old presentation.
    /// A live session is never discarded unless `allow_restart` is true.
    pub async fn set_presentation(
        &self,
        presentation: BrowserPresentation,
        allow_restart: bool,
    ) -> BrowserPresentationChange {
        let Ok(_operation) =
            tokio::time::timeout(BROWSER_PRESENTATION_LOCK_TIMEOUT, self.operation.lock()).await
        else {
            return BrowserPresentationChange::Busy;
        };
        let Some(state) = &self.state else {
            return BrowserPresentationChange::Unchanged;
        };
        let current = state.lock().await.browser_presentation;
        if current == presentation {
            return BrowserPresentationChange::Unchanged;
        }

        let has_live_session = self
            .runtime
            .lock()
            .await
            .transport
            .as_ref()
            .is_some_and(|transport| transport.is_alive());
        if has_live_session && !allow_restart {
            return BrowserPresentationChange::RequiresRestart;
        }

        let transport = {
            let mut runtime = self.runtime.lock().await;
            let transport = runtime.transport.take();
            if transport.is_some() {
                runtime.routing.clear();
            }
            transport
        };
        if let Some(transport) = transport {
            transport.shutdown().await;
        }

        let mut app = state.lock().await;
        app.browser_presentation = presentation;
        app.browser_runtime_running = false;
        app.mark_config_dirty();
        app.log(
            "INFO",
            format!(
                "Agent browser display changed to {}{}",
                presentation.label(),
                if has_live_session {
                    "; previous isolated browser session closed"
                } else {
                    ""
                }
            ),
        );
        if has_live_session {
            BrowserPresentationChange::UpdatedAndSessionClosed
        } else {
            BrowserPresentationChange::Updated
        }
    }

    pub async fn ensure_started(&self, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(timeout))?;
        self.ensure_transport(deadline).await.map(|_| ())
    }

    #[cfg(all(test, windows))]
    async fn transport_pid(&self) -> Option<u32> {
        let transport = self.runtime.lock().await.transport.clone()?;
        transport.pid().await
    }

    /// Retire all logical sessions and pages owned by one workspace without disturbing other
    /// workspaces sharing the host Chromium process. If upstream cleanup cannot be completed,
    /// invalidate the shared runtime so removed-workspace state cannot remain reachable.
    pub async fn release_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), String> {
        let _operation = self.operation.lock().await;
        let (transport, trace_page, mut page_ids) = {
            let runtime = self.runtime.lock().await;
            let transport = runtime
                .transport
                .as_ref()
                .filter(|transport| transport.is_alive())
                .cloned();
            let trace_page = runtime
                .routing
                .active_trace
                .as_ref()
                .filter(|active| active.owner.belongs_to_workspace(workspace_id))
                .map(|active| active.upstream_page_id);
            let page_ids = runtime
                .routing
                .upstream_owners
                .iter()
                .filter_map(|(page_id, (owner, _))| {
                    owner.belongs_to_workspace(workspace_id).then_some(*page_id)
                })
                .collect::<Vec<_>>();
            (transport, trace_page, page_ids)
        };
        page_ids.sort_unstable();

        let mut cleanup_error = None;
        if let Some(transport) = transport.as_ref() {
            let deadline = tokio::time::Instant::now() + BROWSER_WORKSPACE_RELEASE_TIMEOUT;
            if let Some(page_id) = trace_page {
                match self
                    .call_transport_tool(
                        transport,
                        "performance_stop_trace",
                        serde_json::json!({ "pageId": page_id }),
                        deadline,
                        BROWSER_WORKSPACE_RELEASE_TIMEOUT,
                    )
                    .await
                {
                    Ok(result) if browser_result_is_error(&result) => {
                        cleanup_error = Some(format!(
                            "could not stop workspace performance trace: {}",
                            browser_result_text(&result)
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        cleanup_error = Some(format!(
                            "could not stop workspace performance trace: {error}"
                        ));
                    }
                }
            }
            if cleanup_error.is_none() {
                for page_id in page_ids {
                    if let Err(error) = self
                        .select_surviving_upstream_page_before_close(
                            transport,
                            page_id,
                            deadline,
                            BROWSER_WORKSPACE_RELEASE_TIMEOUT,
                        )
                        .await
                    {
                        cleanup_error = Some(format!(
                            "could not prepare removed-workspace browser page {page_id} for close: {error}"
                        ));
                        break;
                    }
                    match self
                        .call_transport_tool(
                            transport,
                            "close_page",
                            serde_json::json!({ "pageId": page_id }),
                            deadline,
                            BROWSER_WORKSPACE_RELEASE_TIMEOUT,
                        )
                        .await
                    {
                        Ok(result) if browser_result_is_error(&result) => {
                            cleanup_error = Some(format!(
                                "could not close removed-workspace browser page {page_id}: {}",
                                browser_result_text(&result)
                            ));
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            cleanup_error = Some(format!(
                                "could not close removed-workspace browser page {page_id}: {error}"
                            ));
                            break;
                        }
                    }
                }
            }
            if cleanup_error.is_none() {
                let context_name = browser_workspace_context_name(workspace_id.as_str());
                if let Err(error) = transport.dispose_context(&context_name, deadline).await {
                    cleanup_error = Some(format!(
                        "could not dispose removed-workspace browser context: {error}"
                    ));
                }
            }
        }

        if let Some(error) = cleanup_error {
            if let Some(transport) = transport {
                self.invalidate_transport(
                    &transport,
                    "workspace browser cleanup could not be completed safely",
                )
                .await;
            }
            return Err(error);
        }

        let mut runtime = self.runtime.lock().await;
        runtime
            .routing
            .sessions
            .retain(|owner, _| !owner.belongs_to_workspace(workspace_id));
        runtime
            .routing
            .upstream_owners
            .retain(|_, (owner, _)| !owner.belongs_to_workspace(workspace_id));
        if runtime
            .routing
            .active_trace
            .as_ref()
            .is_some_and(|active| active.owner.belongs_to_workspace(workspace_id))
        {
            runtime.routing.active_trace = None;
        }
        Ok(())
    }

    /// Return the configured presentation for MoonDesk's one host-owned agent browser.
    pub(crate) async fn presentation(&self) -> BrowserPresentation {
        if let Some(state) = &self.state {
            state.lock().await.browser_presentation
        } else {
            BrowserPresentation::default()
        }
    }

    /// Return whether MoonDesk currently owns a live browser transport.
    pub async fn is_running(&self) -> bool {
        self.runtime
            .lock()
            .await
            .transport
            .as_ref()
            .is_some_and(|transport| transport.is_alive())
    }

    pub async fn stop(&self) {
        let _operation = self.operation.lock().await;
        let transport = {
            let mut runtime = self.runtime.lock().await;
            let transport = runtime.transport.take();
            runtime.routing.clear();
            transport
        };
        if let Some(transport) = transport {
            transport.shutdown().await;
        }
        if let Some(state) = &self.state {
            state.lock().await.browser_runtime_running = false;
        }
    }
}

fn browser_result_is_error(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
}

fn browser_requested_page_id(arguments: &serde_json::Map<String, Value>) -> Result<u64, String> {
    let Some(value) = arguments.get("pageId") else {
        return Err("Browser pageId is required".to_string());
    };
    let page_id = value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|value| value.is_finite() && value.fract() == 0.0 && *value >= 0.0)
                .map(|value| value as u64)
        })
        .filter(|value| *value > 0)
        .ok_or_else(|| "Browser pageId must be a positive integer".to_string())?;
    Ok(page_id)
}

fn upstream_pages_from_result(result: &Value) -> Result<Vec<UpstreamPageInfo>, String> {
    let pages = result
        .pointer("/structuredContent/pages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "MoonDesk's native browser engine did not return structured page metadata; the browser contract may have changed"
                .to_string()
        })?;
    pages
        .iter()
        .map(|page| {
            let id = page
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "Browser page metadata did not contain a numeric id".to_string())?;
            Ok(UpstreamPageInfo {
                id,
                url: page
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: page
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                selected: page
                    .get("selected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                isolated_context: page
                    .get("isolatedContext")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn upstream_page_ids(pages: &[UpstreamPageInfo]) -> HashSet<u64> {
    pages.iter().map(|page| page.id).collect()
}

fn new_upstream_page_ids(before: &HashSet<u64>, after: &[UpstreamPageInfo]) -> HashSet<u64> {
    after
        .iter()
        .map(|page| page.id)
        .filter(|page_id| !before.contains(page_id))
        .collect()
}

fn browser_close_survivor_page_id(pages: &[UpstreamPageInfo], target_page_id: u64) -> Option<u64> {
    if !pages
        .iter()
        .any(|page| page.id == target_page_id && page.selected)
    {
        return None;
    }
    pages
        .iter()
        .find(|page| page.id != target_page_id && page.isolated_context.is_none())
        .or_else(|| pages.iter().find(|page| page.id != target_page_id))
        .map(|page| page.id)
}

#[cfg(test)]
fn browser_logical_page_control_command(command: &str) -> bool {
    matches!(
        command,
        "list_pages" | "new_page" | "select_page" | "close_page"
    )
}

fn browser_page_scoped_command(command: &str) -> bool {
    matches!(
        command,
        "click"
            | "click_at"
            | "drag"
            | "emulate"
            | "evaluate_script"
            | "fill"
            | "get_console_message"
            | "get_network_request"
            | "handle_dialog"
            | "hover"
            | "list_console_messages"
            | "list_network_requests"
            | "navigate_page"
            | "performance_start_trace"
            | "performance_stop_trace"
            | "press_key"
            | "resize_page"
            | "scroll"
            | "take_heapsnapshot"
            | "take_screenshot"
            | "take_snapshot"
            | "type_text"
            | "upload_file"
    )
}

fn browser_command_may_open_or_close_pages(command: &str) -> bool {
    matches!(
        command,
        "click" | "click_at" | "evaluate_script" | "navigate_page" | "press_key"
    )
}

fn browser_safe_pages_markdown(pages: &[Value]) -> String {
    let mut lines = vec!["## Pages".to_string()];
    for page in pages {
        let id = page.get("id").and_then(Value::as_u64).unwrap_or(0);
        let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
        let title = page
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty());
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
    lines.join("\n")
}

fn rewrite_browser_page_sections(text: &str, safe_pages: &str) -> String {
    let mut output = Vec::new();
    let mut lines = text.lines().peekable();
    let mut replaced_pages = false;
    while let Some(line) = lines.next() {
        if line.trim() == "## Pages" {
            if !replaced_pages {
                output.extend(safe_pages.lines().map(str::to_string));
                replaced_pages = true;
            }
            while let Some(next) = lines.peek() {
                if next.starts_with("## ") {
                    break;
                }
                lines.next();
            }
            continue;
        }
        if line.trim() == "## Extension Pages" {
            while let Some(next) = lines.peek() {
                if next.starts_with("## ") {
                    break;
                }
                lines.next();
            }
            continue;
        }
        if line.starts_with("Note: the previously selected page ")
            || line
                .starts_with("Note: the browser was restarted or reconnected since the last call.")
        {
            continue;
        }
        output.push(line.to_string());
    }
    output.join("\n")
}

fn browser_result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| item.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
fn browser_page_listing_text(text: &str) -> Vec<(u64, bool)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let selected = line.ends_with(" [selected]");
            let line = line.strip_suffix(" [selected]").unwrap_or(line);
            let (page_id, _) = line.split_once(':')?;
            page_id
                .trim()
                .parse::<u64>()
                .ok()
                .map(|page_id| (page_id, selected))
        })
        .collect()
}

#[cfg(test)]
fn browser_page_listing(result: &Value) -> Vec<(u64, bool)> {
    browser_page_listing_text(&browser_result_text(result))
}

fn browser_output_from_result(
    result: Value,
    parsed: ParsedBrowserInvocation,
    restarted: bool,
) -> Result<BrowserCommandOutput, String> {
    use base64::Engine as _;

    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let output_format = parsed.output_format;
    let stdout = if is_error {
        serde_json::to_string(result.get("content").unwrap_or(&Value::Null))
            .map_err(|error| format!("Could not encode browser tool error: {error}"))?
    } else {
        let mut chunks = Vec::new();
        let mut images = Vec::new();
        for item in result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        chunks.push(text.to_string());
                    }
                }
                Some("image") => {
                    let data = item.get("data").and_then(Value::as_str).ok_or_else(|| {
                        "Browser image response did not contain base64 data".to_string()
                    })?;
                    let mime_type = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    let extension = match mime_type {
                        "image/jpeg" | "image/jpg" => "jpeg",
                        "image/webp" => "webp",
                        _ => "png",
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|error| {
                            format!("Could not decode browser image response: {error}")
                        })?;
                    let path = std::env::temp_dir().join(format!(
                        "moondesk-browser-image-{}.{}",
                        uuid::Uuid::new_v4().simple(),
                        extension
                    ));
                    let mut options = std::fs::OpenOptions::new();
                    options.create_new(true).write(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    if let Err(error) = options
                        .open(&path)
                        .and_then(|mut file| file.write_all(&bytes))
                    {
                        let _ = std::fs::remove_file(&path);
                        return Err(format!(
                            "Could not write browser image response to {}: {error}",
                            path.display()
                        ));
                    }
                    images.push(serde_json::json!({
                        "filePath": path.to_string_lossy(),
                        "mimeType": mime_type,
                    }));
                    chunks.push(format!("Saved to {}.", path.display()));
                }
                Some(other) => {
                    return Err(format!(
                        "Unsupported browser response content type '{other}'"
                    ));
                }
                None => {}
            }
        }

        match output_format {
            BrowserOutputFormat::Json => {
                if let Some(structured) = result.get("structuredContent").and_then(Value::as_object)
                {
                    let mut structured = structured.clone();
                    if !images.is_empty() {
                        structured.insert("images".to_string(), Value::Array(images));
                    }
                    serde_json::to_string(&Value::Object(structured))
                        .map_err(|error| format!("Could not encode browser JSON output: {error}"))?
                } else {
                    serde_json::to_string(&chunks)
                        .map_err(|error| format!("Could not encode browser JSON output: {error}"))?
                }
            }
            BrowserOutputFormat::Markdown => chunks.join(" "),
        }
    };

    Ok(BrowserCommandOutput {
        stdout: match output_format {
            BrowserOutputFormat::Json => bounded_json_text(&stdout),
            BrowserOutputFormat::Markdown => bounded_text(&stdout),
        },
        stderr: String::new(),
        exit_code: if is_error { 1 } else { 0 },
        restarted,
    })
}

fn total_timeout_message(timeout: Duration) -> String {
    format!(
        "Browser command timed out after {} ms total; the owned browser runtime was invalidated before serialization was released",
        timeout.as_millis()
    )
}

fn run_cli_help(command_name: &str) -> Result<BrowserCommandOutput, String> {
    Ok(BrowserCommandOutput {
        stdout: bounded_text(&browser_command_help(command_name)?),
        stderr: String::new(),
        exit_code: 0,
        restarted: false,
    })
}

fn bounded_text(text: &str) -> String {
    bounded_output(text.as_bytes())
}

fn bounded_json_text(text: &str) -> String {
    if text.len() <= MAX_CAPTURED_OUTPUT_BYTES {
        return text.to_string();
    }
    serde_json::json!({
        "_moondesk": {
            "truncated": true,
            "originalBytes": text.len(),
            "limitBytes": MAX_CAPTURED_OUTPUT_BYTES,
            "message": "Inline browser JSON exceeded MoonDesk's output limit; use command-specific pagination or a file-output option for the full result"
        }
    })
    .to_string()
}

pub fn browser_service_command(command: &str) -> bool {
    matches!(command.trim(), "start" | "status" | "stop")
}

pub fn validate_browser_request_bounds(
    command: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<(), String> {
    let command = command.trim();
    if command.is_empty() || command.len() > MAX_BROWSER_COMMAND_BYTES {
        return Err(format!(
            "command must be between 1 and {MAX_BROWSER_COMMAND_BYTES} bytes"
        ));
    }
    if args.len() > MAX_BROWSER_ARGS {
        return Err(format!(
            "args may contain at most {MAX_BROWSER_ARGS} values"
        ));
    }
    if let Some((index, _)) = args
        .iter()
        .enumerate()
        .find(|(_, arg)| arg.len() > MAX_BROWSER_ARG_BYTES)
    {
        return Err(format!(
            "args[{index}] exceeds the {MAX_BROWSER_ARG_BYTES}-byte limit"
        ));
    }
    if !(1..=MAX_BROWSER_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "timeout must be between 1 and {MAX_BROWSER_TIMEOUT_MS} ms"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserPathKind {
    InputFile,
    OutputFile,
}

#[derive(Clone, Debug)]
struct BrowserPathRewrite {
    staged: PathBuf,
    visible: PathBuf,
}

#[derive(Clone, Debug)]
struct BrowserOutputStage {
    stage_dir: PathBuf,
    staged_requested_path: PathBuf,
    destination: PathBuf,
    kind: BrowserPathKind,
}

#[derive(Debug, Default)]
struct BrowserStagingRoot {
    path: Option<PathBuf>,
}

impl Drop for BrowserStagingRoot {
    fn drop(&mut self) {
        if let Some(root) = self.path.take() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[derive(Debug)]
struct PreparedBrowserInvocation {
    args: Vec<String>,
    workspace_root: PathBuf,
    _staging_root: BrowserStagingRoot,
    outputs: Vec<BrowserOutputStage>,
    rewrites: Vec<BrowserPathRewrite>,
}

impl PreparedBrowserInvocation {
    fn commit_outputs(&mut self, deadline: Option<tokio::time::Instant>) -> Result<(), String> {
        let outputs = self.outputs.clone();
        for output in outputs {
            match output.kind {
                BrowserPathKind::OutputFile => {
                    let actual_staged = resolve_staged_output_file(&output)?;
                    let actual_destination = destination_for_staged_output(
                        &self.workspace_root,
                        &output,
                        &actual_staged,
                    )?;
                    publish_browser_output_file(&actual_staged, &actual_destination, deadline)?;
                    self.rewrites.push(BrowserPathRewrite {
                        staged: actual_staged,
                        visible: actual_destination,
                    });
                }
                BrowserPathKind::InputFile => {
                    return Err(
                        "Internal browser staging error: input registered as output".to_string()
                    );
                }
            }
        }
        Ok(())
    }

    fn rewrite_output_paths(&self, output: &mut BrowserCommandOutput) {
        for rewrite in &self.rewrites {
            let staged = rewrite.staged.to_string_lossy();
            let visible = rewrite.visible.to_string_lossy();
            output.stdout = output.stdout.replace(staged.as_ref(), visible.as_ref());
            output.stderr = output.stderr.replace(staged.as_ref(), visible.as_ref());
        }
    }
}

fn browser_path_flag_kind(command: &str, flag: &str) -> Option<BrowserPathKind> {
    match (command, flag) {
        ("evaluate_script", "filepath")
        | ("performance_start_trace", "filepath")
        | ("performance_stop_trace", "filepath")
        | ("take_screenshot", "filepath")
        | ("take_snapshot", "filepath")
        | ("take_heapsnapshot", "filepath") => Some(BrowserPathKind::OutputFile),
        ("upload_file", "filepath") => Some(BrowserPathKind::InputFile),
        _ => None,
    }
}

fn positional_browser_paths(command: &str) -> &'static [(usize, BrowserPathKind, &'static str)] {
    use BrowserPathKind::{InputFile, OutputFile};
    match command {
        "take_heapsnapshot" => &[(0, OutputFile, "filepath")],
        "upload_file" => &[(1, InputFile, "filepath")],
        _ => &[],
    }
}

fn managed_browser_temp_output(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    parent == std::env::temp_dir()
        && name.starts_with("moondesk-view-page-")
        && name.ends_with(".jpeg")
}

fn path_within(root: &Path, candidate: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn validate_workspace_input(
    workspace_root: &Path,
    raw_path: &str,
    allow_external_absolute_input_files: bool,
) -> Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err("Browser file path cannot be empty".to_string());
    }
    let raw = PathBuf::from(raw_path);
    let explicitly_absolute = raw.is_absolute();
    let candidate = if explicitly_absolute {
        raw
    } else {
        workspace_root.join(raw)
    };
    let metadata = std::fs::symlink_metadata(&candidate).map_err(|error| {
        format!(
            "Could not inspect browser input path {}: {error}",
            candidate.display()
        )
    })?;
    if metadata_is_link_like(&metadata) {
        return Err(format!(
            "Browser input path may not be a symlink or reparse point: {}",
            candidate.display()
        ));
    }
    let canonical = candidate.canonicalize().map_err(|error| {
        format!(
            "Could not resolve browser input path {}: {error}",
            candidate.display()
        )
    })?;
    let canonical = crate::command::normalize_windows_verbatim_path(canonical);
    if !canonical.is_file() {
        return Err(format!(
            "Browser input path has the wrong type: {}",
            candidate.display()
        ));
    }
    // Relative browser inputs remain workspace-scoped. Explicit absolute files follow the same
    // local-read contract as MoonDesk's read/vision tools: if the user can read the regular file,
    // MoonDesk may stage a private copy for the isolated browser.
    let external_absolute_file = allow_external_absolute_input_files && explicitly_absolute;
    if !path_within(workspace_root, &canonical) && !external_absolute_file {
        return Err(format!(
            "Browser path is outside the active workspace: {}",
            candidate.display()
        ));
    }
    Ok(canonical)
}

fn validate_workspace_output_destination(
    workspace_root: &Path,
    requested: &Path,
) -> Result<PathBuf, String> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace_root.join(requested)
    };
    if candidate.exists() {
        let metadata = std::fs::symlink_metadata(&candidate).map_err(|error| {
            format!(
                "Could not inspect browser output path {}: {error}",
                candidate.display()
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "Browser output path may not be a symlink or reparse point: {}",
                candidate.display()
            ));
        }
        let canonical = candidate.canonicalize().map_err(|error| {
            format!(
                "Could not resolve browser output path {}: {error}",
                candidate.display()
            )
        })?;
        let canonical = crate::command::normalize_windows_verbatim_path(canonical);
        if !canonical.is_file() {
            return Err(format!(
                "Browser output path has the wrong type: {}",
                candidate.display()
            ));
        }
        if !path_within(workspace_root, &canonical) {
            return Err(format!(
                "Browser output path is outside the active workspace: {}",
                candidate.display()
            ));
        }
        return Ok(canonical);
    }

    let parent = candidate
        .parent()
        .ok_or_else(|| format!("Browser output path has no parent: {}", candidate.display()))?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        format!(
            "Browser output parent must already exist ({}): {error}",
            parent.display()
        )
    })?;
    let canonical_parent = crate::command::normalize_windows_verbatim_path(canonical_parent);
    if !path_within(workspace_root, &canonical_parent) {
        return Err(format!(
            "Browser output path is outside the active workspace: {}",
            candidate.display()
        ));
    }
    let name = candidate
        .file_name()
        .ok_or_else(|| format!("Browser output path is invalid: {}", candidate.display()))?;
    Ok(canonical_parent.join(name))
}

fn create_private_browser_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

fn ensure_browser_staging_root(staging_root: &mut BrowserStagingRoot) -> Result<PathBuf, String> {
    if let Some(root) = staging_root.path.as_ref() {
        return Ok(root.clone());
    }
    let root = std::env::temp_dir().join(format!(
        "moondesk-browser-files-{}",
        uuid::Uuid::new_v4().simple()
    ));
    create_private_browser_directory(&root).map_err(|error| {
        format!(
            "Could not create temporary browser file staging directory {}: {error}",
            root.display()
        )
    })?;
    staging_root.path = Some(root.clone());
    Ok(root)
}

fn browser_deadline_expired(deadline: Option<tokio::time::Instant>) -> bool {
    deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
}

fn ensure_browser_deadline(deadline: Option<tokio::time::Instant>) -> Result<(), String> {
    if browser_deadline_expired(deadline) {
        return Err("Browser command total timeout budget was exhausted during filesystem staging/publication".to_string());
    }
    Ok(())
}

fn copy_browser_file_with_deadline(
    source: &Path,
    destination: &Path,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), String> {
    ensure_browser_deadline(deadline)?;
    let mut input = std::fs::File::open(source)
        .map_err(|error| format!("Could not open browser file {}: {error}", source.display()))?;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination).map_err(|error| {
        format!(
            "Could not create browser file copy destination {}: {error}",
            destination.display()
        )
    })?;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        ensure_browser_deadline(deadline)?;
        let read = input.read(&mut buffer).map_err(|error| {
            format!("Could not read browser file {}: {error}", source.display())
        })?;
        if read == 0 {
            break;
        }
        ensure_browser_deadline(deadline)?;
        output.write_all(&buffer[..read]).map_err(|error| {
            format!(
                "Could not write browser file copy {}: {error}",
                destination.display()
            )
        })?;
    }
    output.flush().map_err(|error| {
        format!(
            "Could not flush browser file copy {}: {error}",
            destination.display()
        )
    })?;
    if let Ok(permissions) = std::fs::metadata(source).map(|metadata| metadata.permissions()) {
        let _ = std::fs::set_permissions(destination, permissions);
    }
    ensure_browser_deadline(deadline)
}

#[cfg(not(windows))]
fn replace_browser_output_file(temp_path: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(temp_path, destination)
}

#[cfg(windows)]
fn replace_browser_output_file(temp_path: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temp_wide = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn publish_browser_output_file(
    source: &Path,
    destination: &Path,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), String> {
    ensure_browser_deadline(deadline)?;
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "Browser output path has no parent: {}",
            destination.display()
        )
    })?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    let temp_path = parent.join(format!(
        ".{name}.moondesk-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        copy_browser_file_with_deadline(source, &temp_path, deadline)?;
        ensure_browser_deadline(deadline)?;
        replace_browser_output_file(&temp_path, destination).map_err(|error| {
            format!(
                "Could not publish browser output to {}: {error}",
                destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn resolve_staged_output_file(output: &BrowserOutputStage) -> Result<PathBuf, String> {
    if output.staged_requested_path.is_file() {
        return Ok(output.staged_requested_path.clone());
    }
    let files = std::fs::read_dir(&output.stage_dir)
        .map_err(|error| {
            format!(
                "Could not inspect temporary browser output directory {}: {error}",
                output.stage_dir.display()
            )
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_file())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();
    match files.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(format!(
            "Browser command completed without producing the requested output in {}",
            output.stage_dir.display()
        )),
        _ => Err(format!(
            "Browser command produced multiple unexpected files for one output in {}",
            output.stage_dir.display()
        )),
    }
}

fn destination_for_staged_output(
    workspace_root: &Path,
    output: &BrowserOutputStage,
    actual_staged: &Path,
) -> Result<PathBuf, String> {
    let destination = if actual_staged.file_name() == output.staged_requested_path.file_name() {
        output.destination.clone()
    } else {
        if actual_staged.file_stem() != output.staged_requested_path.file_stem() {
            return Err(format!(
                "Browser command changed the staged output basename unexpectedly: {}",
                actual_staged.display()
            ));
        }
        let mut adjusted = output.destination.clone();
        adjusted.set_extension(actual_staged.extension().unwrap_or_default());
        adjusted
    };
    validate_workspace_output_destination(workspace_root, &destination)
}

struct BrowserPathStager<'a> {
    managed_temp_output: Option<&'a Path>,
    staging_root: BrowserStagingRoot,
    slot: usize,
    outputs: Vec<BrowserOutputStage>,
    rewrites: Vec<BrowserPathRewrite>,
}

impl<'a> BrowserPathStager<'a> {
    fn new(managed_temp_output: Option<&'a Path>) -> Self {
        Self {
            managed_temp_output,
            staging_root: BrowserStagingRoot::default(),
            slot: 0,
            outputs: Vec::new(),
            rewrites: Vec::new(),
        }
    }
}

fn stage_browser_path(
    workspace_root: &Path,
    raw_path: &str,
    kind: BrowserPathKind,
    allow_external_absolute_input_files: bool,
    stager: &mut BrowserPathStager<'_>,
    deadline: Option<tokio::time::Instant>,
) -> Result<PathBuf, String> {
    ensure_browser_deadline(deadline)?;
    if matches!(kind, BrowserPathKind::OutputFile) {
        let raw = PathBuf::from(raw_path);
        let candidate = if raw.is_absolute() {
            raw
        } else {
            workspace_root.join(raw)
        };
        if stager
            .managed_temp_output
            .is_some_and(|allowed| candidate == allowed)
        {
            return Ok(candidate);
        }
    }

    match kind {
        BrowserPathKind::InputFile => {
            let source = validate_workspace_input(
                workspace_root,
                raw_path,
                allow_external_absolute_input_files,
            )?;
            let root = ensure_browser_staging_root(&mut stager.staging_root)?;
            let current_slot = stager.slot;
            stager.slot += 1;
            let stage_dir = root.join(format!("input-{current_slot}"));
            create_private_browser_directory(&stage_dir).map_err(|error| {
                format!("Could not create browser input staging directory: {error}")
            })?;
            let name = source.file_name().ok_or_else(|| {
                format!("Browser input path has no filename: {}", source.display())
            })?;
            let staged = stage_dir.join(name);
            copy_browser_file_with_deadline(&source, &staged, deadline).map_err(|error| {
                format!(
                    "Could not stage browser input file {}: {error}",
                    source.display()
                )
            })?;
            stager.rewrites.push(BrowserPathRewrite {
                staged: staged.clone(),
                visible: source,
            });
            Ok(staged)
        }
        BrowserPathKind::OutputFile => {
            let requested = PathBuf::from(raw_path);
            let destination = validate_workspace_output_destination(workspace_root, &requested)?;
            let root = ensure_browser_staging_root(&mut stager.staging_root)?;
            let current_slot = stager.slot;
            stager.slot += 1;
            let stage_dir = root.join(format!("output-{current_slot}"));
            create_private_browser_directory(&stage_dir).map_err(|error| {
                format!("Could not create browser output staging directory: {error}")
            })?;
            let name = destination.file_name().ok_or_else(|| {
                format!(
                    "Browser output path has no filename: {}",
                    destination.display()
                )
            })?;
            let staged_requested_path = stage_dir.join(name);
            stager.outputs.push(BrowserOutputStage {
                stage_dir,
                staged_requested_path: staged_requested_path.clone(),
                destination: destination.clone(),
                kind,
            });
            stager.rewrites.push(BrowserPathRewrite {
                staged: staged_requested_path.clone(),
                visible: destination,
            });
            Ok(staged_requested_path)
        }
    }
}

#[cfg(test)]
fn prepare_browser_invocation(
    workspace_root: &str,
    command: &str,
    args: &[String],
) -> Result<PreparedBrowserInvocation, String> {
    prepare_browser_invocation_with_policy(workspace_root, command, args, false)
}

#[cfg(test)]
fn prepare_browser_invocation_with_policy(
    workspace_root: &str,
    command: &str,
    args: &[String],
    allow_external_absolute_input_files: bool,
) -> Result<PreparedBrowserInvocation, String> {
    prepare_browser_invocation_with_managed_temp_deadline(
        workspace_root,
        command,
        args,
        allow_external_absolute_input_files,
        None,
        None,
    )
}

#[cfg(test)]
fn prepare_browser_invocation_with_managed_temp(
    workspace_root: &str,
    command: &str,
    args: &[String],
    managed_temp_output: Option<&Path>,
) -> Result<PreparedBrowserInvocation, String> {
    prepare_browser_invocation_with_managed_temp_deadline(
        workspace_root,
        command,
        args,
        false,
        managed_temp_output,
        None,
    )
}

fn prepare_browser_invocation_with_managed_temp_deadline(
    workspace_root: &str,
    command: &str,
    args: &[String],
    allow_external_absolute_input_files: bool,
    managed_temp_output: Option<&Path>,
    deadline: Option<tokio::time::Instant>,
) -> Result<PreparedBrowserInvocation, String> {
    ensure_browser_deadline(deadline)?;
    let workspace_root =
        crate::workspaces::canonicalize_existing_workspace_root(Path::new(workspace_root))?;
    let mut prepared = args.to_vec();
    let mut stager = BrowserPathStager::new(managed_temp_output);

    for &(index, kind, flag_name) in positional_browser_paths(command) {
        if prepared
            .iter()
            .any(|arg| canonical_browser_flag_name(arg).as_deref() == Some(flag_name))
        {
            continue;
        }
        let Some(value) = prepared.get(index).cloned() else {
            continue;
        };
        if value.starts_with('-') {
            continue;
        }
        let staged = stage_browser_path(
            &workspace_root,
            &value,
            kind,
            allow_external_absolute_input_files,
            &mut stager,
            deadline,
        )?;
        prepared[index] = staged.to_string_lossy().into_owned();
    }

    let mut seen_path_flags = std::collections::HashSet::new();
    let mut index = 0;
    while index < prepared.len() {
        let arg = prepared[index].clone();
        let Some(flag) = canonical_browser_flag_name(&arg) else {
            index += 1;
            continue;
        };

        if flag == "sessionid" {
            return Err(
                "Browser command arguments may not override MoonDesk's private session ID"
                    .to_string(),
            );
        }

        if let Some(kind) = browser_path_flag_kind(command, &flag) {
            if !seen_path_flags.insert(flag.clone()) {
                return Err(format!(
                    "Browser path flag '{arg}' may only be supplied once per command"
                ));
            }
            if let Some((prefix, raw_value)) = arg.split_once('=') {
                let staged = stage_browser_path(
                    &workspace_root,
                    raw_value,
                    kind,
                    allow_external_absolute_input_files,
                    &mut stager,
                    deadline,
                )?;
                prepared[index] = format!("{prefix}={}", staged.to_string_lossy());
            } else {
                let value_index = index + 1;
                let Some(raw_value) = prepared.get(value_index).cloned() else {
                    return Err(format!("Browser path flag '{arg}' requires a value"));
                };
                if raw_value.starts_with('-') {
                    return Err(format!("Browser path flag '{arg}' requires a path value"));
                }
                let staged = stage_browser_path(
                    &workspace_root,
                    &raw_value,
                    kind,
                    allow_external_absolute_input_files,
                    &mut stager,
                    deadline,
                )?;
                prepared[value_index] = staged.to_string_lossy().into_owned();
                index += 1;
            }
        } else if flag.contains("path") {
            return Err(format!(
                "Unrecognized path-bearing browser argument '{arg}' is blocked by MoonDesk's workspace boundary"
            ));
        }
        index += 1;
    }

    Ok(PreparedBrowserInvocation {
        args: prepared,
        workspace_root,
        _staging_root: stager.staging_root,
        outputs: stager.outputs,
        rewrites: stager.rewrites,
    })
}

fn bounded_output(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_CAPTURED_OUTPUT_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let head_len = MAX_CAPTURED_OUTPUT_BYTES * 3 / 4;
    let tail_len = MAX_CAPTURED_OUTPUT_BYTES - head_len;
    format!(
        "{}\n\n...[MoonDesk truncated {} bytes of browser output]...\n\n{}",
        String::from_utf8_lossy(&bytes[..head_len]),
        bytes.len() - MAX_CAPTURED_OUTPUT_BYTES,
        String::from_utf8_lossy(&bytes[bytes.len() - tail_len..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    #[test]
    fn service_worker_evaluation_cannot_bypass_page_routing_contract() {
        let error = browser_structured_arguments_to_cli(
            "evaluate_script",
            &serde_json::json!({
                "function": "() => self.location.href",
                "serviceWorkerId": "worker-1",
            }),
        )
        .expect_err("MoonDesk's pinned public contract must reject raw service-worker targets");
        assert!(error.contains("Unknown argument 'serviceWorkerId'"));
    }

    #[test]
    fn every_native_browser_command_has_an_explicit_routing_class() {
        let contract: Value = serde_json::from_str(include_str!("browser_contract.json"))
            .expect("parse native browser contract");
        let commands = contract
            .get("commands")
            .and_then(Value::as_object)
            .expect("pinned browser commands");
        for command in commands.keys() {
            assert!(
                browser_logical_page_control_command(command)
                    || browser_page_scoped_command(command),
                "native browser command {command:?} has no MoonDesk routing class"
            );
        }
    }

    #[test]
    fn service_commands_are_reserved_for_runtime_lifecycle() {
        for command in ["start", "status", "stop"] {
            assert!(browser_service_command(command));
        }
        for command in ["list_pages", "take_snapshot", "click", "resize_page"] {
            assert!(!browser_service_command(command));
        }
    }

    #[test]
    fn external_browser_input_files_follow_mode_trust_boundary() {
        assert!(!external_browser_input_files_allowed(Some(Mode::Browser)));
        assert!(external_browser_input_files_allowed(Some(Mode::Both)));
        assert!(external_browser_input_files_allowed(Some(Mode::Computer)));
        assert!(external_browser_input_files_allowed(None));
    }

    #[test]
    fn browser_session_context_is_workspace_scoped_not_chat_scoped() {
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace_a, Some("subject-a"), "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace_a, Some("subject-a"), "chat-b");
        let other_workspace = BrowserSessionKey::openai(&workspace_b, Some("subject-a"), "chat-a");

        assert_eq!(
            chat_a.workspace_context_name(),
            chat_b.workspace_context_name()
        );
        assert_ne!(
            chat_a.workspace_context_name(),
            other_workspace.workspace_context_name()
        );
        assert!(chat_a != chat_b);
    }

    #[tokio::test]
    async fn logical_pages_are_isolated_between_chats_in_same_workspace() {
        let runtime = BrowserRuntime::standalone();
        let workspace = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace, Some("subject-a"), "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace, Some("subject-a"), "chat-b");
        let context = chat_a.workspace_context_name();

        let pages_a = vec![
            UpstreamPageInfo {
                id: 101,
                url: "http://example.test/a1".into(),
                title: "A1".into(),
                selected: true,
                isolated_context: Some(context.clone()),
            },
            UpstreamPageInfo {
                id: 102,
                url: "http://example.test/a2".into(),
                title: "A2".into(),
                selected: false,
                isolated_context: Some(context.clone()),
            },
        ];
        let claim_a = upstream_page_ids(&pages_a);
        runtime
            .reconcile_pages(&chat_a, &pages_a, Some(&claim_a))
            .await;

        let pages_all = vec![
            pages_a[0].clone(),
            pages_a[1].clone(),
            UpstreamPageInfo {
                id: 201,
                url: "http://example.test/b1".into(),
                title: "B1".into(),
                selected: true,
                isolated_context: Some(context),
            },
        ];
        let claim_b = HashSet::from([201]);
        runtime
            .reconcile_pages(&chat_b, &pages_all, Some(&claim_b))
            .await;

        let safe_a = runtime.session_pages(&chat_a).await;
        let safe_b = runtime.session_pages(&chat_b).await;
        assert_eq!(safe_a.len(), 2);
        assert_eq!(safe_b.len(), 1);
        assert_eq!(safe_a[0].get("id").and_then(Value::as_u64), Some(1));
        assert_eq!(safe_a[1].get("id").and_then(Value::as_u64), Some(2));
        assert_eq!(safe_b[0].get("id").and_then(Value::as_u64), Some(1));
        assert_eq!(
            runtime.owned_upstream_page_id(&chat_a, 2).await.unwrap(),
            102
        );
        assert!(runtime.owned_upstream_page_id(&chat_b, 2).await.is_err());
    }

    #[tokio::test]
    async fn page_results_hide_other_sessions_and_upstream_ids() {
        let runtime = BrowserRuntime::standalone();
        let workspace = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace, None, "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace, None, "chat-b");
        let context = chat_a.workspace_context_name();
        let pages = vec![
            UpstreamPageInfo {
                id: 101,
                url: "http://example.test/a".into(),
                title: "Chat A".into(),
                selected: true,
                isolated_context: Some(context.clone()),
            },
            UpstreamPageInfo {
                id: 202,
                url: "http://example.test/b".into(),
                title: "Chat B".into(),
                selected: false,
                isolated_context: Some(context),
            },
        ];
        runtime
            .reconcile_pages(&chat_a, &pages, Some(&HashSet::from([101])))
            .await;
        runtime
            .reconcile_pages(&chat_b, &pages, Some(&HashSet::from([202])))
            .await;

        let mut result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "## Pages\n101: Chat A (http://example.test/a) [selected] isolatedContext=secret-a\n202: Chat B (http://example.test/b) isolatedContext=secret-a"
            }],
            "structuredContent": {
                "pages": [
                    {"id":101,"url":"http://example.test/a","title":"Chat A","selected":true,"isolatedContext":"secret-a"},
                    {"id":202,"url":"http://example.test/b","title":"Chat B","selected":false,"isolatedContext":"secret-a"}
                ]
            }
        });
        runtime
            .sanitize_page_result_for_session(&chat_a, &mut result)
            .await;

        let visible = result
            .pointer("/structuredContent/pages")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].get("id").and_then(Value::as_u64), Some(1));
        assert!(visible[0].get("isolatedContext").is_none());
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.contains("1: Chat A"));
        assert!(!text.contains("101:"));
        assert!(!text.contains("202:"));
        assert!(!text.contains("Chat B"));
        assert!(!text.contains("isolatedContext"));
    }

    #[tokio::test]
    async fn vanished_global_recording_page_is_detected_before_reconciliation() {
        let runtime = BrowserRuntime::standalone();
        let workspace = WorkspaceId::new();
        let chat = BrowserSessionKey::openai(&workspace, None, "chat-a");
        runtime
            .begin_performance_trace(&chat, 41)
            .await
            .expect("claim performance trace");
        assert_eq!(
            runtime.missing_global_recording_page(&[]).await,
            Some("performance trace")
        );
        assert_eq!(
            runtime
                .missing_global_recording_page(&[UpstreamPageInfo {
                    id: 41,
                    url: "about:blank".into(),
                    title: String::new(),
                    selected: true,
                    isolated_context: Some(chat.workspace_context_name()),
                }])
                .await,
            None
        );
    }

    #[tokio::test]
    async fn performance_trace_is_session_and_page_owned() {
        let runtime = BrowserRuntime::standalone();
        let workspace = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace, None, "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace, None, "chat-b");

        runtime
            .begin_performance_trace(&chat_a, 41)
            .await
            .expect("chat A claims trace");
        assert!(runtime.begin_performance_trace(&chat_b, 52).await.is_err());
        assert_eq!(
            runtime
                .performance_trace_page(&chat_a)
                .await
                .expect("chat A owns active trace"),
            41
        );
        assert!(runtime.performance_trace_page(&chat_b).await.is_err());
        assert!(runtime.ensure_page_can_close(&chat_a, 41).await.is_err());
        assert!(runtime.ensure_page_can_close(&chat_a, 99).await.is_ok());

        runtime.finish_performance_trace(&chat_a).await;
        assert!(runtime.performance_trace_page(&chat_a).await.is_err());
        assert!(runtime.ensure_page_can_close(&chat_a, 41).await.is_ok());

        runtime
            .begin_performance_trace(&chat_b, 52)
            .await
            .expect("chat B can claim trace after chat A stops");
        assert_eq!(
            runtime
                .performance_trace_page(&chat_b)
                .await
                .expect("chat B owns active trace"),
            52
        );
        runtime.finish_performance_trace(&chat_b).await;
    }

    #[tokio::test]
    async fn releasing_workspace_drops_only_its_logical_browser_state_when_idle() {
        let runtime = BrowserRuntime::standalone();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace_a, None, "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace_b, None, "chat-b");
        let pages = vec![
            UpstreamPageInfo {
                id: 11,
                url: "https://a.example/".into(),
                title: "A".into(),
                selected: true,
                isolated_context: Some(chat_a.workspace_context_name()),
            },
            UpstreamPageInfo {
                id: 22,
                url: "https://b.example/".into(),
                title: "B".into(),
                selected: false,
                isolated_context: Some(chat_b.workspace_context_name()),
            },
        ];
        runtime
            .reconcile_pages(&chat_a, &pages, Some(&HashSet::from([11])))
            .await;
        runtime
            .reconcile_pages(&chat_b, &pages, Some(&HashSet::from([22])))
            .await;
        runtime
            .begin_performance_trace(&chat_a, 11)
            .await
            .expect("workspace A trace lease");

        runtime
            .release_workspace(&workspace_a)
            .await
            .expect("release idle workspace A state");

        assert!(runtime.session_pages(&chat_a).await.is_empty());
        assert_eq!(runtime.session_pages(&chat_b).await.len(), 1);
        assert!(runtime.performance_trace_page(&chat_a).await.is_err());
        assert_eq!(
            runtime
                .owned_upstream_page_id(&chat_b, 1)
                .await
                .expect("workspace B page survives release"),
            22
        );
    }

    #[test]
    fn close_page_prefers_unowned_keeper_when_target_is_selected() {
        let pages = vec![
            UpstreamPageInfo {
                id: 1,
                url: "about:blank".into(),
                title: String::new(),
                selected: false,
                isolated_context: None,
            },
            UpstreamPageInfo {
                id: 7,
                url: "https://project.example/".into(),
                title: "Project".into(),
                selected: true,
                isolated_context: Some("moondesk-ws-a".into()),
            },
            UpstreamPageInfo {
                id: 9,
                url: "https://other.example/".into(),
                title: "Other".into(),
                selected: false,
                isolated_context: Some("moondesk-ws-b".into()),
            },
        ];
        assert_eq!(browser_close_survivor_page_id(&pages, 7), Some(1));
        assert_eq!(browser_close_survivor_page_id(&pages, 9), None);

        let without_keeper = vec![pages[1].clone(), pages[2].clone()];
        assert_eq!(browser_close_survivor_page_id(&without_keeper, 7), Some(9));
    }

    #[test]
    fn page_listing_parser_tracks_selected_and_surviving_pages() {
        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "## Pages\n1: about:blank\n7: Audit (data:text/html,test) [selected]"
            }]
        });
        assert_eq!(browser_page_listing(&result), vec![(1, false), (7, true)]);
    }

    #[test]
    fn mcp_tool_error_signal_controls_browser_command_success() {
        let parsed = ParsedBrowserInvocation {
            arguments: serde_json::Map::new(),
            output_format: BrowserOutputFormat::Markdown,
        };
        let failure = browser_output_from_result(
            serde_json::json!({
                "isError": true,
                "content": [{"type": "text", "text": "Element uid 99_99 was not found"}]
            }),
            parsed.clone(),
            false,
        )
        .expect("format tool error");
        assert!(!failure.success());
        assert!(failure.failure_details().contains("Element uid 99_99"));

        let success = browser_output_from_result(
            serde_json::json!({
                "isError": false,
                "content": [{"type": "text", "text": "ordinary evaluate_script value"}]
            }),
            parsed,
            false,
        )
        .expect("format tool success");
        assert!(success.success());
    }

    #[test]
    fn browser_cli_response_rendering_matches_native_contract() {
        let json_output = browser_output_from_result(
            serde_json::json!({
                "isError": false,
                "content": [{"type": "text", "text": "fallback"}],
                "structuredContent": {"pages": [{"id": 1}]}
            }),
            ParsedBrowserInvocation {
                arguments: serde_json::Map::new(),
                output_format: BrowserOutputFormat::Json,
            },
            false,
        )
        .expect("format structured JSON browser result");
        let decoded: Value = serde_json::from_str(&json_output.stdout).expect("decode JSON output");
        assert_eq!(
            decoded.pointer("/pages/0/id").and_then(Value::as_i64),
            Some(1)
        );
        assert!(decoded.get("content").is_none());

        let oversized_json = browser_output_from_result(
            serde_json::json!({
                "isError": false,
                "content": [{"type": "text", "text": "fallback"}],
                "structuredContent": {"value": "x".repeat(MAX_CAPTURED_OUTPUT_BYTES + 1024)}
            }),
            ParsedBrowserInvocation {
                arguments: serde_json::Map::new(),
                output_format: BrowserOutputFormat::Json,
            },
            false,
        )
        .expect("bound oversized structured JSON browser result");
        let oversized_decoded: Value = serde_json::from_str(&oversized_json.stdout)
            .expect("bounded browser JSON output must remain syntactically valid");
        assert_eq!(
            oversized_decoded
                .pointer("/_moondesk/truncated")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            oversized_decoded
                .pointer("/_moondesk/originalBytes")
                .and_then(Value::as_u64)
                .is_some_and(|bytes| bytes > MAX_CAPTURED_OUTPUT_BYTES as u64)
        );

        let markdown = browser_output_from_result(
            serde_json::json!({
                "isError": false,
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"}
                ]
            }),
            ParsedBrowserInvocation {
                arguments: serde_json::Map::new(),
                output_format: BrowserOutputFormat::Markdown,
            },
            false,
        )
        .expect("format markdown browser result");
        assert_eq!(markdown.stdout, "first second");

        let error = browser_output_from_result(
            serde_json::json!({
                "isError": true,
                "content": [{"type": "text", "text": "failed"}]
            }),
            ParsedBrowserInvocation {
                arguments: serde_json::Map::new(),
                output_format: BrowserOutputFormat::Markdown,
            },
            false,
        )
        .expect("format browser tool error");
        assert!(!error.success());
        let decoded_error: Value =
            serde_json::from_str(&error.stdout).expect("decode serialized error content");
        assert_eq!(
            decoded_error.pointer("/0/text").and_then(Value::as_str),
            Some("failed")
        );

        let image = browser_output_from_result(
            serde_json::json!({
                "isError": false,
                "content": [{
                    "type": "image",
                    "data": "aGVsbG8=",
                    "mimeType": "image/png"
                }]
            }),
            ParsedBrowserInvocation {
                arguments: serde_json::Map::new(),
                output_format: BrowserOutputFormat::Markdown,
            },
            false,
        )
        .expect("materialize browser image response");
        let path = image
            .stdout
            .strip_prefix("Saved to ")
            .and_then(|value| value.strip_suffix('.'))
            .map(PathBuf::from)
            .expect("browser image output path");
        assert_eq!(
            std::fs::read(&path).expect("read browser image output"),
            b"hello"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("browser image metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn browser_command_help_is_owned_locally() {
        let help = run_cli_help("list_pages").expect("render native browser help");
        assert!(help.success());
        assert!(help.stdout.contains("MoonDesk browser command: list_pages"));
        assert!(!help.stdout.contains("chrome-devtools-mcp"));
    }

    #[tokio::test]
    async fn browser_presentation_change_returns_busy_instead_of_waiting_for_active_operation() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-presentation-busy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create presentation workspace");
        let config_path = workspace.join("config.toml");
        let app = AppState::new_for_test(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create browser presentation app");
        let state = Arc::new(Mutex::new(app));
        let runtime = BrowserRuntime::new(state.clone());
        let operation = runtime.operation.lock().await;

        let started = tokio::time::Instant::now();
        let change = runtime
            .set_presentation(BrowserPresentation::Visible, false)
            .await;
        assert_eq!(change, BrowserPresentationChange::Busy);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            state.lock().await.browser_presentation,
            BrowserPresentation::Headless
        );

        drop(operation);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn changing_idle_browser_presentation_updates_state_without_starting_chromium() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-presentation-idle-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create presentation workspace");
        let config_path = workspace.join("config.toml");
        let app = AppState::new_for_test(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create browser presentation app");
        let state = Arc::new(Mutex::new(app));
        let runtime = BrowserRuntime::new(state.clone());

        let change = runtime
            .set_presentation(BrowserPresentation::Visible, false)
            .await;
        assert_eq!(change, BrowserPresentationChange::Updated);
        let app = state.lock().await;
        assert_eq!(app.browser_presentation, BrowserPresentation::Visible);
        assert!(!app.browser_runtime_running);
        drop(app);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn browser_workspace_paths_are_staged_in_temp_and_outputs_copy_back() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-path-policy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(workspace.join("reports")).expect("create workspace fixture");
        std::fs::write(workspace.join("upload.txt"), b"hello").expect("write upload fixture");
        let workspace_str = workspace.to_string_lossy().into_owned();

        let upload = prepare_browser_invocation(
            &workspace_str,
            "upload_file",
            &["1_2".to_string(), "upload.txt".to_string()],
        )
        .expect("stage upload path");
        let staged_upload = PathBuf::from(&upload.args[1]);
        let upload_stage_root = upload
            ._staging_root
            .path
            .clone()
            .expect("upload staging root");
        assert!(staged_upload.is_absolute());
        assert!(path_within(&std::env::temp_dir(), &staged_upload));
        assert!(!path_within(&workspace, &staged_upload));
        assert_eq!(
            std::fs::read(&staged_upload).expect("read staged upload"),
            b"hello"
        );
        drop(upload);
        assert!(
            !upload_stage_root.exists(),
            "input staging must be cleaned up"
        );

        let structured_upload_args = browser_structured_arguments_to_cli(
            "upload_file",
            &serde_json::json!({ "uid": "1_2", "filePath": "upload.txt" }),
        )
        .expect("normalize structured upload arguments");
        prepare_browser_invocation(&workspace_str, "upload_file", &structured_upload_args)
            .expect("structured upload arguments must survive workspace staging");
        prepare_browser_invocation(
            &workspace_str,
            "upload_file",
            &["--filePath=upload.txt".to_string(), "1_2".to_string()],
        )
        .expect("named path plus positional uid must survive workspace staging");

        let structured_heap_args = browser_structured_arguments_to_cli(
            "take_heapsnapshot",
            &serde_json::json!({ "filePath": "reports/heap.heapsnapshot" }),
        )
        .expect("normalize structured heap snapshot arguments");
        prepare_browser_invocation(&workspace_str, "take_heapsnapshot", &structured_heap_args)
            .expect("structured heap snapshot arguments must survive workspace staging");

        let mut screenshot = prepare_browser_invocation(
            &workspace_str,
            "take_screenshot",
            &["--FILE-PATH=reports/shot.png".to_string()],
        )
        .expect("stage screenshot output path");
        let (_, staged_output) = screenshot.args[0].split_once('=').expect("rewritten flag");
        let staged_output = PathBuf::from(staged_output);
        let screenshot_stage_root = screenshot
            ._staging_root
            .path
            .clone()
            .expect("screenshot staging root");
        assert!(path_within(&std::env::temp_dir(), &staged_output));
        assert!(!path_within(&workspace, &staged_output));
        std::fs::write(&staged_output, b"fake-png").expect("simulate browser screenshot output");
        screenshot
            .commit_outputs(None)
            .expect("copy screenshot back");
        assert_eq!(
            std::fs::read(workspace.join("reports/shot.png")).expect("read copied screenshot"),
            b"fake-png"
        );
        let visible_destination = screenshot
            .outputs
            .first()
            .expect("screenshot output mapping")
            .destination
            .clone();
        let mut output = BrowserCommandOutput {
            stdout: format!("Saved screenshot to {}.", staged_output.display()),
            stderr: String::new(),
            exit_code: 0,
            restarted: false,
        };
        screenshot.rewrite_output_paths(&mut output);
        assert!(
            output
                .stdout
                .contains(&visible_destination.to_string_lossy().to_string()),
            "rewritten output path was unexpected: {}",
            output.stdout
        );
        assert!(!output.stdout.contains("moondesk-browser-files-"));
        drop(screenshot);
        assert!(
            !screenshot_stage_root.exists(),
            "output staging must be cleaned up"
        );

        let managed_temp =
            std::env::temp_dir().join(format!("moondesk-view-page-{}.jpeg", uuid::Uuid::new_v4()));
        let managed_arg = format!("--filePath={}", managed_temp.display());
        let ordinary_temp_attempt = prepare_browser_invocation(
            &workspace_str,
            "take_screenshot",
            std::slice::from_ref(&managed_arg),
        );
        assert!(
            ordinary_temp_attempt
                .expect_err("ordinary browser command must not get the managed temp exemption")
                .contains("outside the active workspace")
        );
        let managed = prepare_browser_invocation_with_managed_temp(
            &workspace_str,
            "take_screenshot",
            std::slice::from_ref(&managed_arg),
            Some(&managed_temp),
        )
        .expect("exact managed view_page output should be authorized");
        assert_eq!(managed.args, vec![managed_arg.clone()]);
        assert!(managed.outputs.is_empty());
        assert!(managed._staging_root.path.is_none());
        let other_managed_temp =
            std::env::temp_dir().join(format!("moondesk-view-page-{}.jpeg", uuid::Uuid::new_v4()));
        let mismatched = prepare_browser_invocation_with_managed_temp(
            &workspace_str,
            "take_screenshot",
            &[managed_arg],
            Some(&other_managed_temp),
        );
        assert!(
            mismatched
                .expect_err("managed authorization must match one exact temp path")
                .contains("outside the active workspace")
        );

        let mut snapshot = prepare_browser_invocation(
            &workspace_str,
            "take_snapshot",
            &["--filePath=reports/snapshot.any".to_string()],
        )
        .expect("stage extension-normalized output");
        let output_stage = snapshot.outputs.first().expect("snapshot output stage");
        let staged_txt = output_stage.stage_dir.join("snapshot.txt");
        std::fs::write(&staged_txt, b"snapshot").expect("simulate upstream extension change");
        snapshot
            .commit_outputs(None)
            .expect("copy normalized snapshot output");
        assert_eq!(
            std::fs::read(workspace.join("reports/snapshot.txt"))
                .expect("read normalized snapshot output"),
            b"snapshot"
        );

        let outside = workspace
            .parent()
            .expect("workspace parent")
            .join(format!("outside-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&outside, b"outside").expect("write outside fixture");
        let browser_only_upload = prepare_browser_invocation(
            &workspace_str,
            "upload_file",
            &["1_2".to_string(), outside.to_string_lossy().into_owned()],
        );
        assert!(
            browser_only_upload
                .expect_err("browser-only policy must keep external uploads workspace-bound")
                .contains("outside the active workspace")
        );

        let external_upload = prepare_browser_invocation_with_policy(
            &workspace_str,
            "upload_file",
            &["1_2".to_string(), outside.to_string_lossy().into_owned()],
            true,
        )
        .expect("Both/CLI policy should stage an explicit absolute upload input");
        let staged_external_upload = PathBuf::from(&external_upload.args[1]);
        assert_ne!(staged_external_upload, outside);
        assert!(path_within(&std::env::temp_dir(), &staged_external_upload));
        assert_eq!(
            std::fs::read(&staged_external_upload).expect("read staged external upload"),
            b"outside"
        );
        drop(external_upload);

        let relative_escape = Path::new("..")
            .join(outside.file_name().expect("outside fixture name"))
            .to_string_lossy()
            .into_owned();
        let escaped_upload = prepare_browser_invocation_with_policy(
            &workspace_str,
            "upload_file",
            &["1_2".to_string(), relative_escape],
            true,
        );
        assert!(
            escaped_upload
                .expect_err("relative upload escape must be rejected")
                .contains("outside the active workspace")
        );

        let traversal = prepare_browser_invocation(
            &workspace_str,
            "take_screenshot",
            &["--filePath=../escaped.png".to_string()],
        );
        assert!(
            traversal
                .expect_err("output traversal must be rejected")
                .contains("outside the active workspace")
        );

        let unknown_path_flag = prepare_browser_invocation(
            &workspace_str,
            "list_pages",
            &["--futurePath=somewhere".to_string()],
        );
        assert!(unknown_path_flag.is_err());
        let session_override = prepare_browser_invocation(
            &workspace_str,
            "list_pages",
            &["--session-id=deadbeef".to_string()],
        );
        assert!(session_override.is_err());
        let duplicate_path = prepare_browser_invocation(
            &workspace_str,
            "take_screenshot",
            &[
                "--filePath=reports/a.png".to_string(),
                "--file-path=reports/b.png".to_string(),
            ],
        );
        assert!(duplicate_path.is_err());
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(unix)]
    #[test]
    fn browser_workspace_input_staging_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-private-stage-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create private staging workspace");
        let secret = workspace.join("secret.txt");
        std::fs::write(&secret, b"private").expect("write private staging fixture");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
            .expect("make source private");

        let prepared = prepare_browser_invocation(
            &workspace.to_string_lossy(),
            "upload_file",
            &["1_2".to_string(), "secret.txt".to_string()],
        )
        .expect("stage private upload");
        let root = prepared
            ._staging_root
            .path
            .as_ref()
            .expect("private staging root");
        let staged = PathBuf::from(&prepared.args[1]);
        let stage_dir = staged.parent().expect("staged input directory");
        let mode = |path: &Path| {
            std::fs::metadata(path)
                .expect("staging metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(root), 0o700);
        assert_eq!(mode(stage_dir), 0o700);
        assert_eq!(mode(&staged), 0o600);

        drop(prepared);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn expired_deadline_never_publishes_browser_output() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-publish-deadline-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create publish workspace");
        let source = workspace.join("source.txt");
        let destination = workspace.join("destination.txt");
        std::fs::write(&source, b"new output").expect("write staged output fixture");
        std::fs::write(&destination, b"original output").expect("write destination fixture");

        let expired = tokio::time::Instant::now() - Duration::from_millis(1);
        let error = publish_browser_output_file(&source, &destination, Some(expired))
            .expect_err("expired publication must fail before replacing destination");
        assert!(error.contains("timeout budget was exhausted"), "{error}");
        assert_eq!(
            std::fs::read(&destination).expect("read untouched destination"),
            b"original output"
        );
        let leaked_temps = std::fs::read_dir(&workspace)
            .expect("list publish workspace")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".moondesk-"))
            .count();
        assert_eq!(leaked_temps, 0);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn expired_deadline_stops_workspace_input_staging_before_copy() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-stage-deadline-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create staging workspace");
        std::fs::write(workspace.join("upload.txt"), b"upload").expect("write upload fixture");
        let expired = tokio::time::Instant::now() - Duration::from_millis(1);
        let result = prepare_browser_invocation_with_managed_temp_deadline(
            &workspace.to_string_lossy(),
            "upload_file",
            &["1_2".to_string(), "upload.txt".to_string()],
            false,
            None,
            Some(expired),
        );
        assert!(
            result
                .expect_err("expired staging must fail")
                .contains("timeout budget was exhausted")
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn browser_request_bounds_are_shared_and_strict() {
        assert!(validate_browser_request_bounds("list_pages", &[], 1).is_ok());
        assert!(validate_browser_request_bounds("list_pages", &[], MAX_BROWSER_TIMEOUT_MS).is_ok());
        assert!(validate_browser_request_bounds("", &[], 1).is_err());
        assert!(
            validate_browser_request_bounds(&"x".repeat(MAX_BROWSER_COMMAND_BYTES + 1), &[], 1)
                .is_err()
        );
        assert!(
            validate_browser_request_bounds(
                "list_pages",
                &vec![String::new(); MAX_BROWSER_ARGS + 1],
                1
            )
            .is_err()
        );
        assert!(validate_browser_request_bounds("list_pages", &[], 0).is_err());
        assert!(
            validate_browser_request_bounds("list_pages", &[], MAX_BROWSER_TIMEOUT_MS + 1).is_err()
        );
    }

    #[tokio::test]
    async fn browser_timeout_includes_time_waiting_for_operation_queue() {
        let workspace =
            std::env::temp_dir().join(format!("moondesk-browser-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create timeout workspace");
        let runtime = BrowserRuntime::standalone();
        let guard = runtime.operation.lock().await;
        let started = tokio::time::Instant::now();
        let error = runtime
            .run(
                &workspace.to_string_lossy(),
                "list_pages",
                &[],
                Duration::from_millis(50),
            )
            .await
            .expect_err("queued browser operation must respect its total deadline");
        let elapsed = started.elapsed();
        assert!(error.contains("timed out after 50 ms total"), "{error}");
        assert!(
            elapsed < Duration::from_secs(1),
            "queued timeout took too long: {elapsed:?}"
        );
        assert!(
            runtime.runtime.lock().await.transport.is_none(),
            "a timed-out queued request must not start a browser runtime later"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn deferred_trace_output_is_rejected_before_browser_runtime_starts() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-trace-contract-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(workspace.join("reports"))
            .expect("create trace contract workspace");
        let runtime = BrowserRuntime::standalone();
        let error = runtime
            .run(
                &workspace.to_string_lossy(),
                "performance_start_trace",
                &[
                    "--autoStop=false".to_string(),
                    "--filePath=reports/trace.json".to_string(),
                ],
                Duration::from_secs(1),
            )
            .await
            .expect_err("deferred start-trace output must fail before dispatch");
        assert!(error.contains("performance_stop_trace"), "{error}");
        assert!(
            runtime.runtime.lock().await.transport.is_none(),
            "invalid deferred trace output must not start the browser runtime"
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows browser presentation restart smoke"]
    async fn windows_browser_presentation_change_requires_confirmation_and_restarts() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-presentation-restart-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create presentation restart workspace");
        let workspace_str = workspace.to_string_lossy().into_owned();
        let config_path = workspace.join("config.toml");
        let app = AppState::new_for_test(8787, workspace_str.clone(), config_path.clone())
            .expect("create presentation restart app");
        let state = Arc::new(Mutex::new(app));
        let runtime = BrowserRuntime::new(state.clone());

        let first = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("start default headless browser");
        assert!(first.success(), "headless list_pages failed: {first:?}");
        let first_transport = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("headless transport");
        let first_pid = first_transport.pid().await.expect("headless transport pid");
        assert!(first_transport.is_alive());

        let refused = runtime
            .set_presentation(BrowserPresentation::Visible, false)
            .await;
        assert_eq!(refused, BrowserPresentationChange::RequiresRestart);
        assert!(first_transport.is_alive());
        assert_eq!(
            state.lock().await.browser_presentation,
            BrowserPresentation::Headless
        );

        let session_closed = runtime
            .set_presentation(BrowserPresentation::Visible, true)
            .await;
        assert_eq!(
            session_closed,
            BrowserPresentationChange::UpdatedAndSessionClosed
        );
        assert!(!first_transport.is_alive());
        {
            let app = state.lock().await;
            assert_eq!(app.browser_presentation, BrowserPresentation::Visible);
            assert!(!app.browser_runtime_running);
        }

        let visible = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("start visible browser after confirmed presentation change");
        assert!(visible.success(), "visible list_pages failed: {visible:?}");
        let visible_transport = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("visible transport");
        let visible_pid = visible_transport
            .pid()
            .await
            .expect("visible transport pid");
        assert_ne!(visible_pid, first_pid);
        assert!(visible_transport.is_alive());
        assert!(state.lock().await.browser_runtime_running);

        let back_to_headless = runtime
            .set_presentation(BrowserPresentation::Headless, true)
            .await;
        assert_eq!(
            back_to_headless,
            BrowserPresentationChange::UpdatedAndSessionClosed
        );
        assert!(!visible_transport.is_alive());
        assert_eq!(
            state.lock().await.browser_presentation,
            BrowserPresentation::Headless
        );

        runtime.stop().await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows shared Chromium session-routing smoke"]
    async fn windows_shared_chromium_isolates_chat_pages_and_workspace_storage() {
        use axum::{Router, response::Html, routing::get};

        const SITE_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>MoonDesk Routing E2E</title></head>
<body>
<h1 id="path"></h1>
<button id="popup" onclick="window.open('/popup','_blank')">Open popup</button>
<script>document.getElementById('path').textContent=location.pathname;</script>
</body></html>"#;
        let site = Router::new()
            .route("/", get(|| async { Html(SITE_HTML) }))
            .fallback(|| async { Html(SITE_HTML) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind routing test site");
        let address = listener.local_addr().expect("routing test site address");
        let site_server = tokio::spawn(async move {
            let _ = axum::serve(listener, site).await;
        });
        let origin = format!("http://{address}");

        let root_a =
            std::env::temp_dir().join(format!("moondesk-routing-a-{}", uuid::Uuid::new_v4()));
        let root_b =
            std::env::temp_dir().join(format!("moondesk-routing-b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root_a).expect("create routing workspace A");
        std::fs::create_dir_all(&root_b).expect("create routing workspace B");
        let mut app = AppState::new_for_test(
            0,
            root_a.to_string_lossy().into_owned(),
            root_a.join("config.toml"),
        )
        .expect("create routing app");
        app.mode = Mode::Both;
        let state = Arc::new(Mutex::new(app));
        let runtime = BrowserRuntime::new(state);

        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let chat_a = BrowserSessionKey::openai(&workspace_a, Some("subject"), "chat-a");
        let chat_b = BrowserSessionKey::openai(&workspace_a, Some("subject"), "chat-b");
        let chat_c = BrowserSessionKey::openai(&workspace_b, Some("subject"), "chat-c");
        let root_a_str = root_a.to_string_lossy().into_owned();
        let root_b_str = root_b.to_string_lossy().into_owned();

        let navigate_a = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "navigate_page",
                &[format!("--url={origin}/a")],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("navigate chat A");
        assert!(
            navigate_a.success(),
            "chat A navigation failed: {navigate_a:?}"
        );
        let shared_pid = runtime
            .transport_pid()
            .await
            .expect("shared browser transport pid");

        let set_storage = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "evaluate_script",
                &["() => { localStorage.setItem('md-owner','workspace-a'); document.cookie='md-owner=workspace-a; path=/'; return 'stored'; }".into()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("set workspace A storage");
        assert!(set_storage.success(), "set storage failed: {set_storage:?}");

        let navigate_b = runtime
            .run_for_session(
                &chat_b,
                &root_a_str,
                "navigate_page",
                &[format!("--url={origin}/b")],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("navigate chat B");
        assert!(
            navigate_b.success(),
            "chat B navigation failed: {navigate_b:?}"
        );
        let storage_b = runtime
            .run_for_session(
                &chat_b,
                &root_a_str,
                "evaluate_script",
                &["() => ({storage: localStorage.getItem('md-owner') === 'workspace-a' ? 'HAS' : 'MISS', cookie: document.cookie.includes('md-owner=workspace-a') ? 'HAS' : 'MISS'})".into()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("inspect same-workspace storage");
        assert!(
            storage_b.stdout.contains("HAS"),
            "same workspace did not share storage: {}",
            storage_b.stdout
        );
        assert!(
            !storage_b.stdout.contains("MISS"),
            "same workspace unexpectedly lost storage: {}",
            storage_b.stdout
        );

        let navigate_c = runtime
            .run_for_session(
                &chat_c,
                &root_b_str,
                "navigate_page",
                &[format!("--url={origin}/c")],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("navigate chat C");
        assert!(
            navigate_c.success(),
            "chat C navigation failed: {navigate_c:?}"
        );
        assert_eq!(
            runtime.transport_pid().await,
            Some(shared_pid),
            "different workspaces must reuse one MoonDesk-owned Chromium runtime"
        );
        let storage_c = runtime
            .run_for_session(
                &chat_c,
                &root_b_str,
                "evaluate_script",
                &["() => ({storage: localStorage.getItem('md-owner') === null ? 'MISS' : 'HAS', cookie: document.cookie.includes('md-owner=workspace-a') ? 'HAS' : 'MISS'})".into()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("inspect cross-workspace storage");
        assert!(
            storage_c.stdout.contains("MISS"),
            "different workspace unexpectedly shared storage: {}",
            storage_c.stdout
        );
        assert!(
            !storage_c.stdout.contains("\"HAS\""),
            "different workspace leaked storage: {}",
            storage_c.stdout
        );

        let second_a = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "new_page",
                &[format!("{origin}/a2")],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("open second chat A page");
        assert!(second_a.success(), "chat A new_page failed: {second_a:?}");
        let pages_a = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("list chat A pages");
        assert!(
            pages_a.stdout.contains("/a"),
            "chat A missing first page: {}",
            pages_a.stdout
        );
        assert!(
            pages_a.stdout.contains("/a2"),
            "chat A missing second page: {}",
            pages_a.stdout
        );
        assert!(
            !pages_a.stdout.contains("/b"),
            "chat A saw chat B page: {}",
            pages_a.stdout
        );
        assert!(
            !pages_a.stdout.contains("/c"),
            "chat A saw workspace B page: {}",
            pages_a.stdout
        );

        let pages_b = runtime
            .run_for_session(
                &chat_b,
                &root_a_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("list chat B pages");
        assert!(
            pages_b.stdout.contains("/b"),
            "chat B missing its page: {}",
            pages_b.stdout
        );
        assert!(
            !pages_b.stdout.contains("/a2"),
            "chat B saw chat A page: {}",
            pages_b.stdout
        );
        let cross_select = runtime
            .run_for_session(
                &chat_b,
                &root_a_str,
                "select_page",
                &["2".into()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect_err("chat B must not select chat A logical page 2");
        assert!(
            cross_select.contains("does not belong to this session"),
            "unexpected cross-session error: {cross_select}"
        );

        let snapshot = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "take_snapshot",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("snapshot chat A before popup");
        let popup_uid = snapshot
            .stdout
            .lines()
            .find(|line| line.contains("button \"Open popup\""))
            .and_then(|line| line.split("uid=").nth(1))
            .and_then(|value| value.split_whitespace().next())
            .map(str::to_string)
            .expect("popup button uid");
        let popup = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "click",
                &[popup_uid],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("open popup from chat A");
        assert!(popup.success(), "popup click failed: {popup:?}");
        let pages_after_popup = runtime
            .run_for_session(
                &chat_a,
                &root_a_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("list chat A pages after popup");
        assert!(
            pages_after_popup.stdout.contains("/popup"),
            "popup was not attributed to chat A: {}",
            pages_after_popup.stdout
        );
        let pages_b_after_popup = runtime
            .run_for_session(
                &chat_b,
                &root_a_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("list chat B pages after chat A popup");
        assert!(
            !pages_b_after_popup.stdout.contains("/popup"),
            "chat B saw chat A popup: {}",
            pages_b_after_popup.stdout
        );

        runtime
            .release_workspace(&workspace_a)
            .await
            .expect("release workspace A without disturbing shared Chromium");
        assert_eq!(
            runtime.transport_pid().await,
            Some(shared_pid),
            "workspace release must not restart Chromium when cleanup succeeds"
        );
        assert!(runtime.session_pages(&chat_a).await.is_empty());
        assert!(runtime.session_pages(&chat_b).await.is_empty());
        let transport = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("shared transport after workspace A release");
        let upstream_pages = runtime
            .list_upstream_pages(
                &transport,
                tokio::time::Instant::now() + DEFAULT_BROWSER_COMMAND_TIMEOUT,
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("inspect upstream pages after workspace A release");
        let workspace_a_context = chat_a.workspace_context_name();
        assert!(
            upstream_pages.iter().all(|page| {
                page.isolated_context.as_deref() != Some(workspace_a_context.as_str())
            }),
            "workspace A pages remained alive after release: {upstream_pages:?}"
        );
        let workspace_b_after_release = runtime
            .run_for_session(
                &chat_c,
                &root_b_str,
                "evaluate_script",
                &["() => ({path: location.pathname, storage: localStorage.getItem('md-owner') === null ? 'MISS' : 'HAS'})".into()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("workspace B remains usable after workspace A release");
        assert!(
            workspace_b_after_release.success(),
            "workspace B failed after workspace A release: {workspace_b_after_release:?}"
        );
        assert!(workspace_b_after_release.stdout.contains("/c"));
        assert!(workspace_b_after_release.stdout.contains("MISS"));
        assert_eq!(runtime.transport_pid().await, Some(shared_pid));

        runtime.stop().await;
        site_server.abort();
        let _ = site_server.await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows owned browser lifecycle smoke"]
    async fn windows_owned_browser_runtime_is_lazy_and_recovers_after_child_exit() {
        let workspace = std::env::temp_dir().join(format!(
            "moondesk-owned-browser-lifecycle-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create lifecycle workspace");
        let workspace_str = workspace.to_string_lossy().into_owned();
        let runtime = BrowserRuntime::standalone();

        let help = runtime
            .run(
                &workspace_str,
                "list_pages",
                &["--help".to_string()],
                Duration::from_secs(30),
            )
            .await
            .expect("browser help should remain host-independent");
        assert!(help.success(), "browser help failed: {help:?}");
        assert!(
            runtime.runtime.lock().await.transport.is_none(),
            "command help must not start the owned browser runtime"
        );

        let first = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("first browser operation should start the owned runtime");
        assert!(first.success(), "first list_pages failed: {first:?}");
        let first_transport = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("first transport");
        let first_pid = first_transport.pid().await.expect("first transport pid");
        assert!(first_transport.is_alive());

        let opened = runtime
            .run(
                &workspace_str,
                "new_page",
                &["data:text/html,<title>close-selected-smoke</title>".to_string()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("open second page before selected-page close smoke");
        assert!(opened.success(), "new_page failed: {opened:?}");
        let opened_pages = browser_page_listing_text(&opened.stdout);
        assert!(
            opened_pages.len() >= 2,
            "expected at least two pages: {opened:?}"
        );
        let selected_page_id = opened_pages
            .iter()
            .find_map(|(page_id, selected)| selected.then_some(*page_id))
            .expect("new page should be selected");
        let closed = runtime
            .run(
                &workspace_str,
                "close_page",
                &[selected_page_id.to_string()],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("close selected page without leaving stale browser selection");
        assert!(closed.success(), "close_page failed: {closed:?}");
        let after_close = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("list_pages should work immediately after closing selected page");
        assert!(
            after_close.success(),
            "post-close list_pages failed: {after_close:?}"
        );
        let surviving_pages = browser_page_listing_text(&after_close.stdout);
        assert!(
            surviving_pages.iter().any(|(_, selected)| *selected),
            "a surviving page must remain selected: {after_close:?}"
        );
        assert!(
            surviving_pages
                .iter()
                .all(|(page_id, _)| *page_id != selected_page_id),
            "closed page must not remain listed: {after_close:?}"
        );

        let second_runtime = BrowserRuntime::standalone();
        let second = second_runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("second host-style runtime should start independently");
        assert!(second.success(), "second list_pages failed: {second:?}");
        let second_transport = second_runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("second transport");
        let second_pid = second_transport.pid().await.expect("second transport pid");
        assert_ne!(first_pid, second_pid);

        second_runtime.stop().await;
        assert!(!second_transport.is_alive());
        assert!(
            first_transport.is_alive(),
            "stopping an independent runtime must not stop the first runtime"
        );

        // Simulate the exact owned MCP child disappearing. There is no detached daemon/session
        // namespace to recover: the next operation must replace the dead child, never replay the
        // previous operation, and continue with a new isolated browser session.
        first_transport.shutdown().await;
        assert!(!first_transport.is_alive());
        let recovered = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("dead owned child should be replaced before the next operation");
        assert!(
            recovered.success(),
            "recovered list_pages failed: {recovered:?}"
        );
        assert!(
            recovered.restarted,
            "replacement should be reported as a restart"
        );
        let replacement = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("replacement transport");
        let replacement_pid = replacement.pid().await.expect("replacement pid");
        assert_ne!(replacement_pid, first_pid);

        runtime.stop().await;
        assert!(!replacement.is_alive());
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows dispatched-timeout cancellation smoke"]
    async fn windows_browser_timeout_cancels_dispatched_mutation() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let workspace = std::env::temp_dir().join(format!(
            "moondesk-browser-timeout-cancel-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create timeout-cancel workspace");
        let workspace_str = workspace.to_string_lossy().into_owned();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind timeout side-effect server");
        let address = listener.local_addr().expect("side-effect server address");
        let started = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicBool::new(false));
        let started_server = started.clone();
        let late_server = late.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request = vec![0u8; 8192];
                let Ok(read) = socket.read(&mut request).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&request[..read]);
                let first_line = request.lines().next().unwrap_or_default();
                if first_line.contains(" /started ") {
                    started_server.store(true, Ordering::Release);
                }
                if first_line.contains(" /late ") {
                    late_server.store(true, Ordering::Release);
                }
                let body = if first_line.contains(" /page ") {
                    "<!doctype html><title>timeout probe</title><body>ready</body>"
                } else {
                    "ok"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let runtime = BrowserRuntime::standalone();
        let page_url = format!("http://{address}/page");
        let navigate = runtime
            .run(
                &workspace_str,
                "navigate_page",
                &[format!("--url={page_url}")],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("warm owned runtime and navigate to probe page");
        assert!(navigate.success(), "probe navigation failed: {navigate:?}");
        let timed_out_transport = runtime
            .runtime
            .lock()
            .await
            .transport
            .clone()
            .expect("warm transport");

        let script = "async () => { await fetch('/started', {method:'POST'}); await new Promise(resolve => setTimeout(resolve, 1500)); await fetch('/late', {method:'POST'}); return 'late mutation completed'; }";
        let error = runtime
            .run(
                &workspace_str,
                "evaluate_script",
                &[script.to_string()],
                Duration::from_millis(500),
            )
            .await
            .expect_err("dispatched delayed mutation must hit MoonDesk's deadline");
        assert!(error.contains("timed out after 500 ms total"), "{error}");
        assert!(
            started.load(Ordering::Acquire),
            "the regression must prove the tool was dispatched before timeout"
        );
        assert!(
            runtime.runtime.lock().await.transport.is_none(),
            "timeout must invalidate ownership before returning"
        );
        assert!(
            !timed_out_transport.is_alive(),
            "timed-out transport must be terminated before serialization is released"
        );

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !late.load(Ordering::Acquire),
            "timed-out browser JavaScript continued mutating after MoonDesk returned"
        );

        let recovered = runtime
            .run(
                &workspace_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("next operation should start a fresh runtime after timeout invalidation");
        assert!(
            recovered.success(),
            "fresh runtime list_pages failed: {recovered:?}"
        );
        assert!(recovered.restarted);

        runtime.stop().await;
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_dir_all(workspace);
    }
}
