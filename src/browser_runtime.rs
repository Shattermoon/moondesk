use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;

pub use crate::browser_contract::canonical_browser_flag_name;
use crate::browser_contract::{
    BrowserOutputFormat, ParsedBrowserInvocation, parse_browser_cli_invocation,
};
use crate::browser_transport::{BrowserMcpTransport, BrowserTransportError};
use crate::state::SharedState;

// Keep this exact pin until MoonDesk's browser command contract is deliberately migrated and
// re-tested. The checked-in browser_contract_v1_7.json is generated from this exact package.
pub const CHROME_DEVTOOLS_PACKAGE_VERSION: &str = "1.7.0";
pub const DEFAULT_BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_BROWSER_TIMEOUT_MS: u64 = 120_000;
pub const MAX_BROWSER_COMMAND_BYTES: usize = 128;
pub const MAX_BROWSER_ARGS: usize = 64;
pub const MAX_BROWSER_ARG_BYTES: usize = 8 * 1024;
pub const MAX_BROWSER_CONTROL_BODY_BYTES: usize = 128 * 1024;
const MAX_CAPTURED_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct BrowserCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub restarted: bool,
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

#[derive(Default)]
struct BrowserRuntimeState {
    transport: Option<Arc<BrowserMcpTransport>>,
    has_started: bool,
}

/// Shared, lazy Chrome DevTools runtime owned directly by the MoonDesk host.
///
/// Constructing this value never launches Chromium. The first browser operation starts one pinned
/// `chrome-devtools-mcp` stdio child in a MoonDesk-owned process tree. MCP `browser_command`,
/// `view_page`, and the local `moondesk browser` client all share that same isolated session.
pub struct BrowserRuntime {
    state: Option<SharedState>,
    runtime: Mutex<BrowserRuntimeState>,
    operation: Mutex<()>,
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
        self.run_internal(workspace_root, command, args, timeout, None)
            .await
    }

    pub(crate) async fn run_managed_temp_output(
        &self,
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
            workspace_root,
            command,
            args,
            timeout,
            Some(managed_temp_output),
        )
        .await
    }

    async fn run_internal(
        &self,
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
            return run_cli_help(workspace_root, command, args, timeout).await;
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let prepare_workspace = workspace_root.to_string();
        let prepare_command = command.to_string();
        let prepare_args = args.to_vec();
        let prepare_managed = managed_temp_output.map(Path::to_path_buf);
        let prepare_task = tokio::task::spawn_blocking(move || {
            prepare_browser_invocation_with_managed_temp_deadline(
                &prepare_workspace,
                &prepare_command,
                &prepare_args,
                prepare_managed.as_deref(),
                Some(deadline),
            )
        });
        let mut prepared = tokio::time::timeout_at(deadline, prepare_task)
            .await
            .map_err(|_| total_timeout_message(timeout))?
            .map_err(|error| format!("Browser staging task failed: {error}"))??;
        let parsed = parse_browser_cli_invocation(command, &prepared.args)?;

        // Page selection, snapshot UIDs, dialogs, and DevTools state are session-global. Queueing is
        // part of the caller's total deadline, and timeout cleanup happens while this guard is held.
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| total_timeout_message(timeout))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(total_timeout_message(timeout));
        }

        let (transport, restarted) = self.ensure_transport(workspace_root, deadline).await?;
        let result = match transport
            .call_tool(command, Value::Object(parsed.arguments.clone()), deadline)
            .await
        {
            Ok(result) => result,
            Err(BrowserTransportError::Timeout) => {
                self.invalidate_transport(&transport, "browser operation timed out")
                    .await;
                return Err(total_timeout_message(timeout));
            }
            Err(BrowserTransportError::Disconnected(error)) => {
                self.invalidate_transport(&transport, "browser runtime disconnected")
                    .await;
                return Err(format!(
                    "Browser runtime was lost before the operation completed: {error}. The session was invalidated; retry from a fresh page/snapshot."
                ));
            }
            Err(BrowserTransportError::Protocol(error)) => return Err(error),
        };

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

    async fn ensure_transport(
        &self,
        workspace_root: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(Arc<BrowserMcpTransport>, bool), String> {
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

        let (server_args, browser_name) = browser_server_args();
        let transport = BrowserMcpTransport::start(
            workspace_root,
            CHROME_DEVTOOLS_PACKAGE_VERSION,
            &server_args,
            self.state.clone(),
            deadline,
        )
        .await
        .map_err(|error| match error {
            BrowserTransportError::Timeout => {
                "Browser runtime startup exhausted the caller's total timeout".to_string()
            }
            other => format!("Could not start isolated MoonDesk browser runtime: {other}"),
        })?;

        {
            let mut runtime = self.runtime.lock().await;
            runtime.transport = Some(transport.clone());
            runtime.has_started = true;
        }
        if let Some(state) = &self.state {
            let mut app = state.lock().await;
            app.browser_runtime_running = true;
            app.log(
                "INFO",
                format!("Isolated agent browser runtime started lazily with {browser_name}"),
            );
        }
        Ok((transport, has_started))
    }

    async fn invalidate_transport(&self, expected: &Arc<BrowserMcpTransport>, reason: &str) {
        let transport = {
            let mut runtime = self.runtime.lock().await;
            let matches = runtime
                .transport
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected));
            matches.then(|| runtime.transport.take()).flatten()
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

    pub async fn stop_if_owned(&self, _workspace_root: &str) {
        let _operation = self.operation.lock().await;
        let transport = self.runtime.lock().await.transport.take();
        if let Some(transport) = transport {
            transport.shutdown().await;
        }
        if let Some(state) = &self.state {
            state.lock().await.browser_runtime_running = false;
        }
    }
}

fn browser_server_args() -> (Vec<String>, String) {
    let mut args = vec![
        "--headless=false".to_string(),
        "--isolated=true".to_string(),
        "--screenshotFormat=jpeg".to_string(),
        "--screenshotQuality=82".to_string(),
        "--screenshotMaxWidth=1920".to_string(),
        "--screenshotMaxHeight=4096".to_string(),
        "--usageStatistics=false".to_string(),
        "--performanceCrux=false".to_string(),
        "--redactNetworkHeaders=true".to_string(),
        "--allowUnrestrictedPaths=false".to_string(),
        "--viaCli=true".to_string(),
        "--experimentalStructuredContent=true".to_string(),
    ];
    let mut browser_name = "default Chromium browser".to_string();
    if let Some(browser) = crate::browser::detect_browsers()
        .into_iter()
        .find(|browser| browser.mcp_supported && Path::new(&browser.path).is_file())
    {
        args.push(format!("--executablePath={}", browser.path));
        browser_name = browser.name;
    }
    (args, browser_name)
}

fn browser_output_from_result(
    result: Value,
    parsed: ParsedBrowserInvocation,
    restarted: bool,
) -> Result<BrowserCommandOutput, String> {
    use base64::Engine as _;

    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
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

        match parsed.output_format {
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
        stdout: bounded_text(&stdout),
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

async fn run_cli_help(
    workspace_root: &str,
    command_name: &str,
    args: &[String],
    timeout: Duration,
) -> Result<BrowserCommandOutput, String> {
    let package = format!("chrome-devtools-mcp@{CHROME_DEVTOOLS_PACKAGE_VERSION}");
    let mut command = Command::new(npx_program());
    command
        .args(["-y", "-p", package.as_str(), "chrome-devtools"])
        .arg(command_name)
        .args(args)
        .env("CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS", "1")
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("Browser help timed out after {} ms", timeout.as_millis()))?
        .map_err(|error| format!("Failed to run pinned browser help: {error}"))?;
    Ok(BrowserCommandOutput {
        stdout: bounded_output(&output.stdout),
        stderr: bounded_output(&output.stderr),
        exit_code: output.status.code().unwrap_or(-1),
        restarted: false,
    })
}

fn npx_program() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

fn bounded_text(text: &str) -> String {
    bounded_output(text.as_bytes())
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
    InputDirectory,
    OutputFile,
    OutputDirectory,
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
                BrowserPathKind::OutputDirectory => {
                    if !output.staged_requested_path.is_dir() {
                        return Err(format!(
                            "Browser command completed without producing the staged output directory {}",
                            output.staged_requested_path.display()
                        ));
                    }
                    let destination = validate_workspace_output_destination(
                        &self.workspace_root,
                        &output.destination,
                        BrowserPathKind::OutputDirectory,
                    )?;
                    std::fs::create_dir_all(&destination).map_err(|error| {
                        format!(
                            "Could not create browser output directory {}: {error}",
                            destination.display()
                        )
                    })?;
                    copy_directory_contents_strict(
                        &output.staged_requested_path,
                        &destination,
                        &self.workspace_root,
                        deadline,
                    )?;
                    self.rewrites.push(BrowserPathRewrite {
                        staged: output.staged_requested_path,
                        visible: destination,
                    });
                }
                BrowserPathKind::InputFile | BrowserPathKind::InputDirectory => {
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
        | ("screencast_start", "filepath")
        | ("take_screenshot", "filepath")
        | ("take_snapshot", "filepath")
        | ("take_heapsnapshot", "filepath") => Some(BrowserPathKind::OutputFile),
        ("close_heapsnapshot", "filepath")
        | ("get_heapsnapshot_class_nodes", "filepath")
        | ("get_heapsnapshot_details", "filepath")
        | ("get_heapsnapshot_dominators", "filepath")
        | ("get_heapsnapshot_duplicate_strings", "filepath")
        | ("get_heapsnapshot_edges", "filepath")
        | ("get_heapsnapshot_object_details", "filepath")
        | ("get_heapsnapshot_retainers", "filepath")
        | ("get_heapsnapshot_retaining_paths", "filepath")
        | ("get_heapsnapshot_summary", "filepath")
        | ("upload_file", "filepath")
        | ("compare_heapsnapshots", "basefilepath")
        | ("compare_heapsnapshots", "currentfilepath") => Some(BrowserPathKind::InputFile),
        ("get_network_request", "requestfilepath")
        | ("get_network_request", "responsefilepath") => Some(BrowserPathKind::OutputFile),
        ("lighthouse_audit", "outputdirpath") => Some(BrowserPathKind::OutputDirectory),
        ("install_extension", "path") => Some(BrowserPathKind::InputDirectory),
        _ => None,
    }
}

fn positional_browser_paths(command: &str) -> &'static [(usize, BrowserPathKind)] {
    use BrowserPathKind::{InputDirectory, InputFile, OutputFile};
    match command {
        "close_heapsnapshot"
        | "get_heapsnapshot_class_nodes"
        | "get_heapsnapshot_details"
        | "get_heapsnapshot_dominators"
        | "get_heapsnapshot_duplicate_strings"
        | "get_heapsnapshot_edges"
        | "get_heapsnapshot_object_details"
        | "get_heapsnapshot_retainers"
        | "get_heapsnapshot_retaining_paths"
        | "get_heapsnapshot_summary" => &[(0, InputFile)],
        "compare_heapsnapshots" => &[(0, InputFile), (1, InputFile)],
        "install_extension" => &[(0, InputDirectory)],
        "take_heapsnapshot" => &[(0, OutputFile)],
        "upload_file" => &[(1, InputFile)],
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
    kind: BrowserPathKind,
) -> Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err("Browser file path cannot be empty".to_string());
    }
    let raw = PathBuf::from(raw_path);
    let candidate = if raw.is_absolute() {
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
    let expects_directory = matches!(kind, BrowserPathKind::InputDirectory);
    if (expects_directory && !canonical.is_dir()) || (!expects_directory && !canonical.is_file()) {
        return Err(format!(
            "Browser input path has the wrong type: {}",
            candidate.display()
        ));
    }
    if !path_within(workspace_root, &canonical) {
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
    kind: BrowserPathKind,
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
        let expects_directory = matches!(kind, BrowserPathKind::OutputDirectory);
        if (expects_directory && !canonical.is_dir())
            || (!expects_directory && !canonical.is_file())
        {
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

fn ensure_browser_staging_root(staging_root: &mut BrowserStagingRoot) -> Result<PathBuf, String> {
    if let Some(root) = staging_root.path.as_ref() {
        return Ok(root.clone());
    }
    let root = std::env::temp_dir().join(format!(
        "moondesk-browser-files-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&root).map_err(|error| {
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
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(destination)
        .map_err(|error| {
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

fn copy_directory_tree_strict(
    source: &Path,
    destination: &Path,
    workspace_root: &Path,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), String> {
    fn copy_one(
        source: &Path,
        destination: &Path,
        workspace_root: &Path,
        source_root: &Path,
        visited: &mut std::collections::HashSet<PathBuf>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), String> {
        ensure_browser_deadline(deadline)?;
        let metadata = std::fs::symlink_metadata(source).map_err(|error| {
            format!(
                "Could not inspect staged browser input {}: {error}",
                source.display()
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "Browser input directory contains a symlink or reparse point: {}",
                source.display()
            ));
        }
        let canonical = source.canonicalize().map_err(|error| {
            format!(
                "Could not resolve browser input {}: {error}",
                source.display()
            )
        })?;
        let canonical = crate::command::normalize_windows_verbatim_path(canonical);
        if !path_within(workspace_root, &canonical) || !path_within(source_root, &canonical) {
            return Err(format!(
                "Browser input directory traversal escaped its validated root: {}",
                source.display()
            ));
        }
        if metadata.is_dir() {
            if !visited.insert(canonical.clone()) {
                return Err(format!(
                    "Browser input directory contains a filesystem cycle: {}",
                    source.display()
                ));
            }
            std::fs::create_dir_all(destination).map_err(|error| {
                format!(
                    "Could not create staged browser input directory {}: {error}",
                    destination.display()
                )
            })?;
            for entry in std::fs::read_dir(source).map_err(|error| {
                format!(
                    "Could not read browser input directory {}: {error}",
                    source.display()
                )
            })? {
                let entry = entry.map_err(|error| {
                    format!("Could not read browser input directory entry: {error}")
                })?;
                copy_one(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    workspace_root,
                    source_root,
                    visited,
                    deadline,
                )?;
            }
            visited.remove(&canonical);
            return Ok(());
        }
        if !metadata.is_file() {
            return Err(format!(
                "Browser input directory contains an unsupported filesystem entry: {}",
                source.display()
            ));
        }
        copy_browser_file_with_deadline(source, destination, deadline).map_err(|error| {
            format!(
                "Could not stage browser input file {}: {error}",
                source.display()
            )
        })
    }

    let source_root = source.to_path_buf();
    let mut visited = std::collections::HashSet::new();
    copy_one(
        source,
        destination,
        workspace_root,
        &source_root,
        &mut visited,
        deadline,
    )
}

fn copy_directory_contents_strict(
    source: &Path,
    destination: &Path,
    workspace_root: &Path,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), String> {
    ensure_browser_deadline(deadline)?;
    for entry in std::fs::read_dir(source).map_err(|error| {
        format!(
            "Could not read staged browser output directory {}: {error}",
            source.display()
        )
    })? {
        let entry =
            entry.map_err(|error| format!("Could not read staged browser output: {error}"))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path).map_err(|error| {
            format!(
                "Could not inspect staged browser output {}: {error}",
                source_path.display()
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "Browser output contained an unexpected symlink or reparse point: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            let validated = validate_workspace_output_destination(
                workspace_root,
                &destination_path,
                BrowserPathKind::OutputDirectory,
            )?;
            std::fs::create_dir_all(&validated).map_err(|error| {
                format!(
                    "Could not create browser output directory {}: {error}",
                    validated.display()
                )
            })?;
            copy_directory_contents_strict(&source_path, &validated, workspace_root, deadline)?;
        } else if metadata.is_file() {
            let validated = validate_workspace_output_destination(
                workspace_root,
                &destination_path,
                BrowserPathKind::OutputFile,
            )?;
            publish_browser_output_file(&source_path, &validated, deadline)?;
        } else {
            return Err(format!(
                "Browser output contained an unsupported filesystem entry: {}",
                source_path.display()
            ));
        }
    }
    Ok(())
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
    validate_workspace_output_destination(workspace_root, &destination, BrowserPathKind::OutputFile)
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
    command: &str,
    raw_path: &str,
    kind: BrowserPathKind,
    stager: &mut BrowserPathStager<'_>,
    deadline: Option<tokio::time::Instant>,
) -> Result<PathBuf, String> {
    ensure_browser_deadline(deadline)?;
    if command == "screencast_start" && matches!(kind, BrowserPathKind::OutputFile) {
        return Err(
            "MoonDesk does not accept an explicit screencast output path because the pinned browser runtime keeps that file open until a later screencast_stop command"
                .to_string(),
        );
    }

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
        BrowserPathKind::InputFile | BrowserPathKind::InputDirectory => {
            let source = validate_workspace_input(workspace_root, raw_path, kind)?;
            let root = ensure_browser_staging_root(&mut stager.staging_root)?;
            let current_slot = stager.slot;
            stager.slot += 1;
            let stage_dir = root.join(format!("input-{current_slot}"));
            std::fs::create_dir(&stage_dir).map_err(|error| {
                format!("Could not create browser input staging directory: {error}")
            })?;
            let name = source.file_name().ok_or_else(|| {
                format!("Browser input path has no filename: {}", source.display())
            })?;
            let staged = stage_dir.join(name);
            if matches!(kind, BrowserPathKind::InputDirectory) {
                copy_directory_tree_strict(&source, &staged, workspace_root, deadline)?;
            } else {
                copy_browser_file_with_deadline(&source, &staged, deadline).map_err(|error| {
                    format!(
                        "Could not stage browser input file {}: {error}",
                        source.display()
                    )
                })?;
            }
            stager.rewrites.push(BrowserPathRewrite {
                staged: staged.clone(),
                visible: source,
            });
            Ok(staged)
        }
        BrowserPathKind::OutputFile | BrowserPathKind::OutputDirectory => {
            let requested = PathBuf::from(raw_path);
            let destination =
                validate_workspace_output_destination(workspace_root, &requested, kind)?;
            let root = ensure_browser_staging_root(&mut stager.staging_root)?;
            let current_slot = stager.slot;
            stager.slot += 1;
            let stage_dir = root.join(format!("output-{current_slot}"));
            std::fs::create_dir(&stage_dir).map_err(|error| {
                format!("Could not create browser output staging directory: {error}")
            })?;
            let name = destination.file_name().ok_or_else(|| {
                format!(
                    "Browser output path has no filename: {}",
                    destination.display()
                )
            })?;
            let staged_requested_path = stage_dir.join(name);
            if matches!(kind, BrowserPathKind::OutputDirectory) {
                std::fs::create_dir(&staged_requested_path).map_err(|error| {
                    format!("Could not create staged browser output directory: {error}")
                })?;
            }
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
    prepare_browser_invocation_with_managed_temp(workspace_root, command, args, None)
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
        managed_temp_output,
        None,
    )
}

fn prepare_browser_invocation_with_managed_temp_deadline(
    workspace_root: &str,
    command: &str,
    args: &[String],
    managed_temp_output: Option<&Path>,
    deadline: Option<tokio::time::Instant>,
) -> Result<PreparedBrowserInvocation, String> {
    ensure_browser_deadline(deadline)?;
    let workspace_root =
        crate::workspaces::canonicalize_existing_workspace_root(Path::new(workspace_root))?;
    let mut prepared = args.to_vec();
    let mut stager = BrowserPathStager::new(managed_temp_output);

    for &(index, kind) in positional_browser_paths(command) {
        let Some(value) = prepared.get(index).cloned() else {
            continue;
        };
        if value.starts_with('-') {
            return Err(format!(
                "Browser command '{command}' requires its path argument in the pinned v{CHROME_DEVTOOLS_PACKAGE_VERSION} positional form"
            ));
        }
        let staged = stage_browser_path(
            &workspace_root,
            command,
            &value,
            kind,
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
                    command,
                    raw_value,
                    kind,
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
                    command,
                    &raw_value,
                    kind,
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
    fn browser_cli_response_rendering_matches_pinned_upstream_contract() {
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
    fn browser_server_args_enforce_isolated_safe_runtime() {
        let (args, _browser_name) = browser_server_args();
        for expected in [
            "--headless=false",
            "--isolated=true",
            "--screenshotFormat=jpeg",
            "--screenshotQuality=82",
            "--screenshotMaxWidth=1920",
            "--screenshotMaxHeight=4096",
            "--usageStatistics=false",
            "--performanceCrux=false",
            "--redactNetworkHeaders=true",
            "--allowUnrestrictedPaths=false",
            "--viaCli=true",
            "--experimentalStructuredContent=true",
        ] {
            assert!(args.iter().any(|arg| arg == expected), "missing {expected}");
        }
        assert!(!args.iter().any(|arg| arg.starts_with("--userDataDir=")));
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
        let escaped = prepare_browser_invocation(
            &workspace_str,
            "upload_file",
            &["1_2".to_string(), outside.to_string_lossy().into_owned()],
        );
        assert!(
            escaped
                .expect_err("outside upload must be rejected")
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
        let screencast_path = prepare_browser_invocation(
            &workspace_str,
            "screencast_start",
            &["--filePath=reports/cast.mp4".to_string()],
        );
        assert!(
            screencast_path
                .expect_err("long-lived screencast output path must be rejected")
                .contains("keeps that file open")
        );

        let _ = std::fs::remove_file(outside);
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

        second_runtime.stop_if_owned(&workspace_str).await;
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

        runtime.stop_if_owned(&workspace_str).await;
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

        runtime.stop_if_owned(&workspace_str).await;
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn pinned_runtime_version_is_explicit() {
        assert_eq!(CHROME_DEVTOOLS_PACKAGE_VERSION, "1.7.0");
    }
}
