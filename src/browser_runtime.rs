use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Mutex;

use crate::command_jobs::process_is_live;
use crate::state::SharedState;

// Keep this exact pin until MoonDesk's browser command contract is deliberately migrated and
// re-tested. chrome-devtools-mcp 1.8.0 changed required CLI argument shapes (including commands
// MoonDesk currently invokes with 1.7-style optional flags), so it is not a drop-in upgrade.
pub const CHROME_DEVTOOLS_PACKAGE_VERSION: &str = "1.7.0";
pub const CHROME_DEVTOOLS_SESSION_ID: &str = "6d6f6f6e6465736b";
pub const DEFAULT_BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
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
        self.exit_code == 0 && cli_tool_error_message(&self.stdout).is_none()
    }

    pub fn failure_details(&self) -> String {
        if let Some(message) = cli_tool_error_message(&self.stdout) {
            return message;
        }
        [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Default)]
struct BrowserRuntimeState {
    ready: bool,
    started_by_moondesk: bool,
}

/// Shared, lazy Chrome DevTools CLI runtime.
///
/// Constructing this value never launches Chrome. The first browser operation checks for the
/// namespaced Chrome DevTools daemon and starts one only when needed. Each daemon owns a temporary
/// isolated agent-browser profile, and both MCP browser_command and view_page share that session.
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
    /// Normal `moondesk browser` commands do not use this path: they are lightweight clients
    /// to the running MoonDesk host so shell invocations cannot accidentally own browser state.
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
        let command = command.trim();
        if command.is_empty() {
            return Err("Browser command cannot be empty".to_string());
        }
        if browser_service_command(command) {
            return Err(
                "MoonDesk manages the browser daemon automatically; use a browser operation such as list_pages, new_page, take_snapshot, click, fill, resize_page, or evaluate_script instead"
                    .to_string(),
            );
        }
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
        {
            return self.run_cli(workspace_root, command, args, timeout).await;
        }

        // Latest-snapshot UIDs and page state are shared. Keep browser operations ordered so
        // simultaneous MCP/CLI requests cannot race the selected page or DOM.
        let _operation = self.operation.lock().await;
        self.ensure_started(workspace_root).await?;
        let first = self.run_cli(workspace_root, command, args, timeout).await?;
        if first.success() || !browser_connectivity_failure(&first.stdout, &first.stderr) {
            return Ok(first);
        }

        self.restart(workspace_root).await?;
        let mut retry = self.run_cli(workspace_root, command, args, timeout).await?;
        retry.restarted = true;
        Ok(retry)
    }

    pub async fn stop_if_owned(&self, workspace_root: &str) {
        let _operation = self.operation.lock().await;
        let owned = {
            let mut runtime = self.runtime.lock().await;
            let owned = runtime.started_by_moondesk;
            runtime.ready = false;
            runtime.started_by_moondesk = false;
            owned
        };
        if !owned {
            return;
        }
        if let Some(state) = &self.state {
            state.lock().await.browser_runtime_running = false;
        }
        if let Err(error) = self
            .run_cli_raw(
                workspace_root,
                &["stop".to_string()],
                Duration::from_secs(20),
            )
            .await
            && let Some(state) = &self.state
        {
            state.lock().await.log(
                "WARN",
                format!("Could not stop MoonDesk browser runtime: {error}"),
            );
        }
    }

    async fn ensure_started(&self, workspace_root: &str) -> Result<(), String> {
        let mut runtime = self.runtime.lock().await;
        if runtime.ready {
            if daemon_process_is_alive() {
                return Ok(());
            }
            runtime.ready = false;
            runtime.started_by_moondesk = false;
            if let Some(state) = &self.state {
                let mut app = state.lock().await;
                app.browser_runtime_running = false;
                app.log(
                    "WARN",
                    "MoonDesk browser daemon exited; restarting it before the next browser operation"
                        .to_string(),
                );
            }
        }

        let status = self
            .run_cli_raw(
                workspace_root,
                &["status".to_string()],
                Duration::from_secs(20),
            )
            .await?;
        if status.exit_code == 0 && daemon_is_running(&status.stdout, &status.stderr) {
            if self.daemon_status_matches_expected_configuration(&status.stdout) {
                runtime.ready = true;
                runtime.started_by_moondesk = true;
                if let Some(state) = &self.state {
                    let mut app = state.lock().await;
                    app.browser_runtime_running = true;
                    app.log(
                        "INFO",
                        "Reusing the existing MoonDesk browser runtime".to_string(),
                    );
                }
                return Ok(());
            }
            if let Some(state) = &self.state {
                state.lock().await.log(
                    "INFO",
                    "Existing MoonDesk browser daemon uses stale settings; restarting it with the current isolated agent-browser configuration"
                        .to_string(),
                );
            }
        }

        self.start_locked(workspace_root, &mut runtime).await
    }

    async fn restart(&self, workspace_root: &str) -> Result<(), String> {
        let mut runtime = self.runtime.lock().await;
        let _ = self
            .run_cli_raw(
                workspace_root,
                &["stop".to_string()],
                Duration::from_secs(20),
            )
            .await;
        runtime.ready = false;
        runtime.started_by_moondesk = false;
        self.start_locked(workspace_root, &mut runtime).await?;
        if let Some(state) = &self.state {
            state.lock().await.log(
                "INFO",
                "Browser session was unavailable, so MoonDesk restarted it and will retry the request"
                    .to_string(),
            );
        }
        Ok(())
    }

    async fn start_locked(
        &self,
        workspace_root: &str,
        runtime: &mut BrowserRuntimeState,
    ) -> Result<(), String> {
        // Prefer chrome-devtools-mcp's normal supported-browser resolution. MoonDesk never
        // attaches to the user's everyday browser profile: every daemon starts an isolated
        // temporary agent profile that is discarded when the browser session ends.
        let start_args = self.start_args(None);
        let mut output = self
            .run_cli_raw(workspace_root, &start_args, Duration::from_secs(60))
            .await?;
        let mut browser_name = "default Chromium browser".to_string();

        // Some machines have Edge/Brave/etc. but no Chrome in the location upstream probes by
        // default. Keep selection automatic: retry once with the first supported local Chromium
        // executable instead of asking the user to choose and persist a browser.
        if output.exit_code != 0
            && let Some(fallback) = crate::browser::detect_browsers()
                .into_iter()
                .find(|browser| browser.mcp_supported)
        {
            let fallback_path = Path::new(&fallback.path);
            if fallback_path.is_file() {
                let fallback_args = self.start_args(Some(fallback_path));
                output = self
                    .run_cli_raw(workspace_root, &fallback_args, Duration::from_secs(60))
                    .await?;
                browser_name = fallback.name;
            }
        }

        if output.exit_code != 0 {
            let details = [output.stdout.trim(), output.stderr.trim()]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if details.is_empty() {
                "Could not start the isolated MoonDesk agent browser".to_string()
            } else {
                format!("Could not start the isolated MoonDesk agent browser: {details}")
            });
        }
        runtime.ready = true;
        runtime.started_by_moondesk = true;
        if let Some(state) = &self.state {
            let mut app = state.lock().await;
            app.browser_runtime_running = true;
            app.log(
                "INFO",
                format!("Isolated agent browser started lazily with {browser_name}"),
            );
        }
        Ok(())
    }

    fn start_args(&self, executable: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "start".to_string(),
            "--headless=false".to_string(),
            "--isolated=true".to_string(),
            "--screenshotFormat=jpeg".to_string(),
            "--screenshotQuality=82".to_string(),
            "--screenshotMaxWidth=1920".to_string(),
            "--screenshotMaxHeight=4096".to_string(),
            "--usageStatistics=false".to_string(),
            "--performanceCrux=false".to_string(),
            "--redactNetworkHeaders=true".to_string(),
        ];
        if let Some(path) = executable.filter(|path| path.is_file()) {
            args.push(format!("--executablePath={}", path.display()));
        }
        args
    }

    fn daemon_status_matches_expected_configuration(&self, stdout: &str) -> bool {
        let expected_version = format!("version={CHROME_DEVTOOLS_PACKAGE_VERSION}");
        if !stdout
            .split_whitespace()
            .any(|token| token == expected_version)
        {
            return false;
        }
        let Some(args) = daemon_status_args(stdout) else {
            return false;
        };

        for required in [
            "--no-headless",
            "--isolated",
            "--screenshot-format=jpeg",
            "--screenshot-quality=82",
            "--screenshot-max-width=1920",
            "--screenshot-max-height=4096",
            "--no-usage-statistics",
            "--no-performance-crux",
            "--redact-network-headers",
            "--no-allow-unrestricted-paths",
        ] {
            if !args.iter().any(|arg| arg == required) {
                return false;
            }
        }
        if args.iter().any(|arg| arg.starts_with("--user-data-dir=")) {
            return false;
        }

        let Some(actual_executable) = daemon_arg_value(&args, "--executable-path=") else {
            return true;
        };
        crate::browser::detect_browsers()
            .into_iter()
            .any(|browser| {
                browser.mcp_supported && Path::new(&browser.path) == Path::new(actual_executable)
            })
    }

    async fn run_cli(
        &self,
        workspace_root: &str,
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        let mut cli_args = Vec::with_capacity(args.len() + 1);
        cli_args.push(command.to_string());
        cli_args.extend(args.iter().cloned());
        self.run_cli_raw(workspace_root, &cli_args, timeout).await
    }

    async fn run_cli_raw(
        &self,
        workspace_root: &str,
        cli_args: &[String],
        timeout: Duration,
    ) -> Result<BrowserCommandOutput, String> {
        let package = format!("chrome-devtools-mcp@{CHROME_DEVTOOLS_PACKAGE_VERSION}");
        let mut command = Command::new(npx_program());
        command
            .args(["-y", "-p", package.as_str(), "chrome-devtools"])
            .args(cli_args)
            .arg(format!("--sessionId={CHROME_DEVTOOLS_SESSION_ID}"))
            .env("CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS", "1")
            .current_dir(workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| {
                format!(
                    "Browser command timed out after {} seconds",
                    timeout.as_secs()
                )
            })?
            .map_err(|error| format!("Failed to run chrome-devtools CLI: {error}"))?;

        Ok(BrowserCommandOutput {
            stdout: bounded_output(&output.stdout),
            stderr: bounded_output(&output.stderr),
            exit_code: output.status.code().unwrap_or(-1),
            restarted: false,
        })
    }
}

fn daemon_pid_file_path() -> PathBuf {
    let app_name = format!("chrome-devtools-mcp-{CHROME_DEVTOOLS_SESSION_ID}");

    #[cfg(windows)]
    {
        std::env::temp_dir().join(app_name).join("daemon.pid")
    }

    #[cfg(unix)]
    {
        if let Some(runtime_dir) =
            std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())
        {
            return PathBuf::from(runtime_dir).join(app_name).join("daemon.pid");
        }
        let uid = unsafe { libc::geteuid() };
        PathBuf::from("/tmp")
            .join(format!("{app_name}-{uid}"))
            .join("daemon.pid")
    }
}

fn daemon_process_is_alive() -> bool {
    std::fs::read_to_string(daemon_pid_file_path())
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .is_some_and(process_is_live)
}

fn daemon_status_args(stdout: &str) -> Option<Vec<String>> {
    let encoded = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("args="))?;
    serde_json::from_str(encoded).ok()
}

fn daemon_arg_value<'a>(args: &'a [String], prefix: &str) -> Option<&'a str> {
    args.iter().find_map(|arg| arg.strip_prefix(prefix))
}

fn npx_program() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

pub fn browser_service_command(command: &str) -> bool {
    matches!(command.trim(), "start" | "status" | "stop")
}

pub fn daemon_is_running(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    combined.contains("daemon is running") && !combined.contains("is not running")
}

fn cli_tool_error_message(stdout: &str) -> Option<String> {
    let content = serde_json::from_str::<Vec<serde_json::Value>>(stdout.trim()).ok()?;
    let messages = content
        .iter()
        .filter_map(|item| {
            (item.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .then(|| item.get("text").and_then(serde_json::Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>();
    messages
        .iter()
        .any(|message| message.trim_start().starts_with("Error:"))
        .then(|| messages.join("\n"))
}

pub fn browser_connectivity_failure(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    [
        "could not connect to chrome",
        "failed to fetch browser websocket",
        "browser has disconnected",
        "browser is not connected",
        "target closed",
        "connection refused",
        "econnrefused",
        "socket hang up",
        "socket closed",
        "daemon is not running",
        "chrome is not running",
    ]
    .iter()
    .any(|needle| combined.contains(needle))
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
    fn running_status_does_not_match_not_running_status() {
        assert!(daemon_is_running(
            "chrome-devtools-mcp daemon is running.",
            ""
        ));
        assert!(!daemon_is_running(
            "chrome-devtools-mcp daemon is not running.",
            ""
        ));
    }

    #[test]
    fn zero_exit_cli_tool_errors_are_not_reported_as_success() {
        let output = BrowserCommandOutput {
            stdout: r#"[{"type":"text","text":"Error: Element uid \"99_99\" not found."}]"#
                .to_string(),
            stderr: String::new(),
            exit_code: 0,
            restarted: false,
        };
        assert!(!output.success());
        assert!(
            cli_tool_error_message(&output.stdout)
                .is_some_and(|message| message.contains("Element uid"))
        );

        let legitimate_json = BrowserCommandOutput {
            stdout: r#"[{"type":"text","text":"ordinary evaluate_script value"}]"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            restarted: false,
        };
        assert!(legitimate_json.success());
    }

    #[test]
    fn connectivity_failures_are_narrow_enough_for_retry() {
        assert!(browser_connectivity_failure(
            "",
            "Could not connect to Chrome. Failed to fetch browser WebSocket URL"
        ));
        assert!(!browser_connectivity_failure(
            "",
            "No element with uid 4_12 exists in the latest snapshot"
        ));
    }

    #[test]
    fn start_args_use_a_clean_isolated_agent_profile_and_redact_sensitive_network_headers() {
        let runtime = BrowserRuntime::standalone();
        let args = runtime.start_args(None);
        assert!(args.iter().any(|arg| arg == "--headless=false"));
        assert!(args.iter().any(|arg| arg == "--isolated=true"));
        assert!(!args.iter().any(|arg| arg.starts_with("--userDataDir=")));
        assert!(!args.iter().any(|arg| arg.starts_with("--executablePath=")));
        assert!(args.iter().any(|arg| arg == "--usageStatistics=false"));
        assert!(args.iter().any(|arg| arg == "--performanceCrux=false"));
        assert!(args.iter().any(|arg| arg == "--redactNetworkHeaders=true"));
    }

    #[test]
    fn daemon_status_reuse_requires_the_pinned_safe_isolated_configuration() {
        let runtime = BrowserRuntime::standalone();
        let args = vec![
            "--no-headless".to_string(),
            "--isolated".to_string(),
            "--screenshot-format=jpeg".to_string(),
            "--screenshot-quality=82".to_string(),
            "--screenshot-max-width=1920".to_string(),
            "--screenshot-max-height=4096".to_string(),
            "--no-usage-statistics".to_string(),
            "--no-performance-crux".to_string(),
            "--redact-network-headers".to_string(),
            "--no-allow-unrestricted-paths".to_string(),
            "--viaCli".to_string(),
            "--experimentalStructuredContent".to_string(),
        ];
        let status = format!(
            "chrome-devtools-mcp daemon is running.\npid=42 version={}\nargs={}",
            CHROME_DEVTOOLS_PACKAGE_VERSION,
            serde_json::to_string(&args).expect("serialize daemon args")
        );
        assert!(runtime.daemon_status_matches_expected_configuration(&status));

        let unsafe_headers = status.replace("\"--redact-network-headers\",", "");
        assert!(!runtime.daemon_status_matches_expected_configuration(&unsafe_headers));
        let persistent_profile = status.replace(
            "\"--isolated\",",
            "\"--isolated\",\"--user-data-dir=C:/Users/example/profile\",",
        );
        assert!(!runtime.daemon_status_matches_expected_configuration(&persistent_profile));
        let wrong_version = status.replace(
            &format!("version={CHROME_DEVTOOLS_PACKAGE_VERSION}"),
            "version=999.0.0",
        );
        assert!(!runtime.daemon_status_matches_expected_configuration(&wrong_version));
    }

    #[test]
    fn daemon_session_is_namespaced_and_uses_upstream_valid_id_shape() {
        assert!(!CHROME_DEVTOOLS_SESSION_ID.is_empty());
        assert!(
            CHROME_DEVTOOLS_SESSION_ID
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == '-')
        );
        assert!(
            daemon_pid_file_path()
                .to_string_lossy()
                .contains(CHROME_DEVTOOLS_SESSION_ID)
        );
    }

    #[cfg(windows)]
    fn windows_browser_descendant(root_pid: u32) -> Option<(u32, String)> {
        use std::collections::{HashMap, VecDeque};
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut found = Process32FirstW(snapshot, &mut entry) != 0;
            let mut children: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
            while found {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                children
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push((entry.th32ProcessID, name));
                found = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);

            let mut queue = VecDeque::from([root_pid]);
            while let Some(parent) = queue.pop_front() {
                if let Some(descendants) = children.get(&parent) {
                    for (pid, name) in descendants {
                        let lower = name.to_ascii_lowercase();
                        if matches!(
                            lower.as_str(),
                            "chrome.exe"
                                | "msedge.exe"
                                | "brave.exe"
                                | "vivaldi.exe"
                                | "opera.exe"
                                | "chromium.exe"
                        ) {
                            return Some((*pid, name.clone()));
                        }
                        queue.push_back(*pid);
                    }
                }
            }
            None
        }
    }

    #[cfg(windows)]
    fn windows_terminate_process(pid: u32) -> Result<(), String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(format!(
                    "OpenProcess(PROCESS_TERMINATE) failed for browser pid {pid}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let terminated = TerminateProcess(process, 91) != 0;
            let error = (!terminated).then(std::io::Error::last_os_error);
            CloseHandle(process);
            match error {
                Some(error) => Err(format!(
                    "TerminateProcess failed for browser pid {pid}: {error}"
                )),
                None => Ok(()),
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows lazy browser lifecycle smoke"]
    async fn windows_browser_runtime_is_lazy_and_recovers_after_daemon_exit() {
        let workspace_root = std::env::temp_dir().join(format!(
            "moondesk-browser-runtime-smoke-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace_root).expect("create browser runtime smoke workspace");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let runtime = BrowserRuntime::standalone();

        let _ = runtime
            .run_cli_raw(
                &workspace_root_str,
                &["stop".to_string()],
                Duration::from_secs(20),
            )
            .await;
        assert!(!daemon_process_is_alive());

        let help = runtime
            .run(
                &workspace_root_str,
                "list_pages",
                &["--help".to_string()],
                Duration::from_secs(30),
            )
            .await
            .expect("browser help should run without starting the daemon");
        assert!(help.success());
        assert!(
            !daemon_process_is_alive(),
            "browser help must not launch Chrome or the daemon"
        );

        let first = runtime
            .run(
                &workspace_root_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("first browser command should lazily start the daemon");
        assert!(first.success(), "first list_pages failed: {first:?}");
        assert!(daemon_process_is_alive());

        runtime
            .run_cli_raw(
                &workspace_root_str,
                &["stop".to_string()],
                Duration::from_secs(20),
            )
            .await
            .expect("stop browser daemon during recovery smoke");
        assert!(!daemon_process_is_alive());

        let recovered = runtime
            .run(
                &workspace_root_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("browser runtime should recover before the CLI can auto-start defaults");
        assert!(
            recovered.success(),
            "recovered list_pages failed: {recovered:?}"
        );
        assert!(daemon_process_is_alive());

        let status = runtime
            .run_cli_raw(
                &workspace_root_str,
                &["status".to_string()],
                Duration::from_secs(20),
            )
            .await
            .expect("read recovered browser daemon status");
        assert!(runtime.daemon_status_matches_expected_configuration(&status.stdout));

        runtime.stop_if_owned(&workspace_root_str).await;
        assert!(!daemon_process_is_alive());
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "serialized Windows agent-browser process recovery smoke"]
    async fn windows_browser_runtime_recovers_after_agent_browser_process_exit() {
        let workspace_root = std::env::temp_dir().join(format!(
            "moondesk-browser-process-exit-smoke-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace_root)
            .expect("create browser process-exit smoke workspace");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let runtime = BrowserRuntime::standalone();

        let _ = runtime
            .run_cli_raw(
                &workspace_root_str,
                &["stop".to_string()],
                Duration::from_secs(20),
            )
            .await;

        let first = runtime
            .run(
                &workspace_root_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("first browser command should lazily start the isolated agent browser");
        assert!(first.success(), "first list_pages failed: {first:?}");

        let daemon_pid = std::fs::read_to_string(daemon_pid_file_path())
            .expect("read namespaced daemon pid")
            .trim()
            .parse::<u32>()
            .expect("parse namespaced daemon pid");
        assert!(
            process_is_live(daemon_pid),
            "MoonDesk browser daemon is not alive"
        );

        let (browser_pid, browser_name) = (0..40)
            .find_map(|_| {
                let found = windows_browser_descendant(daemon_pid);
                if found.is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                found
            })
            .expect("find isolated agent-browser descendant of MoonDesk daemon");
        assert_ne!(browser_pid, daemon_pid);
        windows_terminate_process(browser_pid).unwrap_or_else(|error| {
            panic!("terminate isolated {browser_name} pid {browser_pid}: {error}")
        });

        for _ in 0..50 {
            if !process_is_live(browser_pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !process_is_live(browser_pid),
            "isolated agent-browser process {browser_pid} did not exit"
        );
        assert!(
            process_is_live(daemon_pid),
            "terminating the agent browser unexpectedly killed the namespaced DevTools daemon"
        );

        let recovered = runtime
            .run(
                &workspace_root_str,
                "list_pages",
                &[],
                DEFAULT_BROWSER_COMMAND_TIMEOUT,
            )
            .await
            .expect("MoonDesk should recover when the isolated agent browser is closed");
        assert!(
            recovered.success(),
            "list_pages after browser-process exit failed: {recovered:?}"
        );
        // chrome-devtools-mcp may recreate its isolated browser inside the still-running daemon,
        // in which case MoonDesk correctly leaves `restarted=false`. The invariant is that the
        // next operation succeeds with a replacement isolated browser, not that the daemon must
        // be restarted unnecessarily.
        assert!(daemon_process_is_alive());

        let recovered_daemon_pid = std::fs::read_to_string(daemon_pid_file_path())
            .expect("read recovered namespaced daemon pid")
            .trim()
            .parse::<u32>()
            .expect("parse recovered namespaced daemon pid");
        let (recovered_browser_pid, _) = (0..40)
            .find_map(|_| {
                let found = windows_browser_descendant(recovered_daemon_pid);
                if found.is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                found
            })
            .expect("find replacement isolated agent-browser process");
        assert_ne!(recovered_browser_pid, browser_pid);

        let status = runtime
            .run_cli_raw(
                &workspace_root_str,
                &["status".to_string()],
                Duration::from_secs(20),
            )
            .await
            .expect("read recovered browser daemon status");
        assert!(runtime.daemon_status_matches_expected_configuration(&status.stdout));

        runtime.stop_if_owned(&workspace_root_str).await;
        assert!(!daemon_process_is_alive());
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn pinned_runtime_version_is_explicit() {
        assert_eq!(CHROME_DEVTOOLS_PACKAGE_VERSION, "1.7.0");
    }
}
