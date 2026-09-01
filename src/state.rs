use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::{
    Mutex,
    mpsc::{self, Receiver, Sender, error::TryRecvError, error::TrySendError},
};
use uuid::Uuid;

use crate::browser::DetectedBrowser;
use crate::command_jobs::CommandJobManager;
use crate::mascot::{self, MascotPack};
use crate::theme;
use crate::workspaces::{self, WorkspaceConfig, WorkspaceId, WorkspaceRuntime};

/// Log entry displayed in the TUI.
#[derive(Clone)]
pub struct LogEntry {
    pub workspace_id: Option<WorkspaceId>,
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

/// Local shell-command activity shown only in the MoonDesk TUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandActivityState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug)]
pub struct CommandActivity {
    pub workspace_id: WorkspaceId,
    pub id: String,
    pub time: String,
    pub command: String,
    pub background: bool,
    pub job_id: Option<String>,
    pub state: CommandActivityState,
    pub exit_code: Option<i32>,
    pub preview: Option<String>,
}

const MAX_COMMAND_ACTIVITIES: usize = 300;

/// MCP request flow rendered as a single timeline line.
#[derive(Clone)]
pub struct FlowLane {
    pub flow_id: String,
    pub short_id: String,
    pub events: Vec<String>,
    pub bootstrap_status_active: bool,
    pub bootstrap_completed_steps: usize,
    pub bootstrap_pending_steps: VecDeque<usize>,
    pub bootstrap_status_close_deadline_ms: Option<u128>,
    pub anim_queue: VecDeque<FlowAnimSegment>,
    pub last_direction: FlowDirection,
    pub closing_started_ms: Option<u128>,
    pub closing_step_ms: u64,
}

#[derive(Clone, Default)]
pub struct FlowBootstrapProgress {
    pub completed_steps: usize,
    pub pending_steps: VecDeque<usize>,
}

const APP_CONFIG_DIR_NAME: &str = ".moondesk";
const APP_CONFIG_FILE_NAME: &str = "config.toml";
const CURRENT_CONFIG_VERSION: u32 = 2;
const WORKSPACE_DRAIN_TIMEOUT: Duration = Duration::from_secs(130);
const WORKSPACE_CLEANUP_RETRY_ATTEMPTS: usize = 4;
const WORKSPACE_CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(1);
pub const GPT_5_6_AND_EARLIER_USAGE_BUCKET: &str = "through-gpt-5.6";
pub const CURRENT_USAGE_BUCKET: &str = GPT_5_6_AND_EARLIER_USAGE_BUCKET;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub tool_input_tokens: u64,
    pub tool_output_tokens: u64,
    pub total_tokens: u64,
    pub tool_call_count: u64,
}

impl UsageTotals {
    pub fn accumulate(
        &mut self,
        tool_input_tokens: u64,
        tool_output_tokens: u64,
        tool_call_count: u64,
    ) {
        self.tool_input_tokens = self.tool_input_tokens.saturating_add(tool_input_tokens);
        self.tool_output_tokens = self.tool_output_tokens.saturating_add(tool_output_tokens);
        self.total_tokens = self
            .tool_input_tokens
            .saturating_add(self.tool_output_tokens);
        self.tool_call_count = self.tool_call_count.saturating_add(tool_call_count);
    }

    pub fn merge(&mut self, other: &Self) {
        self.accumulate(
            other.tool_input_tokens,
            other.tool_output_tokens,
            other.tool_call_count,
        );
    }

    fn normalized(mut self) -> Self {
        self.total_tokens = self
            .tool_input_tokens
            .saturating_add(self.tool_output_tokens);
        self
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyUsageTotals {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    tool_call_count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageConfigMigration {
    usage_totals: Option<LegacyUsageTotals>,
    usage_by_model: Option<BTreeMap<String, UsageTotals>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentsPathMode {
    #[default]
    Default,
    Workspace,
    Moondesk,
    Codex,
    Disabled,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub config_version: u32,
    pub ngrok_authtoken: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_slug: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    pub ngrok_domain: Option<String>,
    #[serde(default)]
    pub agents_path_mode: AgentsPathMode,
    #[serde(default)]
    pub set_moondesk_as_co_author: bool,
    pub theme: String,
    pub mode: Mode,
    pub tool_mode: ToolMode,
    #[serde(default)]
    pub usage_by_model: BTreeMap<String, UsageTotals>,
    pub selected_browser: Option<DetectedBrowser>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: 0,
            ngrok_authtoken: None,
            mcp_slug: None,
            workspaces: Vec::new(),
            ngrok_domain: None,
            agents_path_mode: AgentsPathMode::Default,
            set_moondesk_as_co_author: false,
            theme: theme::DEFAULT_THEME_ID.to_string(),
            mode: Mode::Both,
            tool_mode: ToolMode::MultiTools,
            usage_by_model: BTreeMap::new(),
            selected_browser: None,
        }
    }
}

impl AppConfig {
    fn normalized(mut self) -> Self {
        self.ngrok_authtoken = self
            .ngrok_authtoken
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.ngrok_domain = self
            .ngrok_domain
            .take()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.usage_by_model = self
            .usage_by_model
            .into_iter()
            .map(|(bucket, usage)| (bucket, usage.normalized()))
            .collect();
        self
    }

    fn validate_versioned(&self) -> std::io::Result<()> {
        match self.config_version {
            0 => {
                if !self.workspaces.is_empty() {
                    return Err(std::io::Error::other(
                        "legacy config must not contain a workspace registry",
                    ));
                }
            }
            CURRENT_CONFIG_VERSION => {
                if self.mcp_slug.is_some() {
                    return Err(std::io::Error::other(
                        "config v2 must not contain the legacy mcpSlug field",
                    ));
                }
                if self.workspaces.is_empty() {
                    return Err(std::io::Error::other(
                        "config v2 must contain at least one workspace",
                    ));
                }
                workspaces::validate_workspace_registry(&self.workspaces)
                    .map_err(std::io::Error::other)?;
            }
            version => {
                return Err(std::io::Error::other(format!(
                    "unsupported MoonDesk config version: {version}"
                )));
            }
        }
        Ok(())
    }

    fn load_for_app(path: &Path, legacy_workspace_root: &Path) -> std::io::Result<(Self, bool)> {
        let mut config = Self::load_from_path(path)?;
        let had_existing_workspace = match config.config_version {
            CURRENT_CONFIG_VERSION => !config.workspaces.is_empty(),
            0 => config
                .mcp_slug
                .as_deref()
                .is_some_and(|slug| !slug.is_empty()),
            _ => false,
        };

        if config.config_version == CURRENT_CONFIG_VERSION {
            return Ok((config, had_existing_workspace));
        }

        let root = workspaces::canonicalize_existing_workspace_root(legacy_workspace_root)
            .map_err(std::io::Error::other)?;
        let mcp_slug = match config.mcp_slug.take() {
            Some(slug) if !slug.is_empty() => slug,
            _ => workspaces::generate_mcp_slug(),
        };
        workspaces::validate_mcp_slug(&mcp_slug).map_err(std::io::Error::other)?;
        let workspace = WorkspaceConfig {
            id: workspaces::WorkspaceId::new(),
            name: workspaces::derive_workspace_name(&root),
            root,
            mcp_slug,
        };

        config.config_version = CURRENT_CONFIG_VERSION;
        config.mcp_slug = None;
        config.workspaces = vec![workspace];
        config.validate_versioned()?;
        config.save_to_path(path)?;
        Ok((config, had_existing_workspace))
    }

    fn load_from_path(path: &Path) -> std::io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut config = toml::from_str::<Self>(&text).map_err(std::io::Error::other)?;
        let migration =
            toml::from_str::<UsageConfigMigration>(&text).map_err(std::io::Error::other)?;

        let migrated_usage = match (migration.usage_totals, migration.usage_by_model) {
            (Some(_), Some(_)) => {
                return Err(std::io::Error::other(
                    "config contains both legacy usageTotals and usageByModel",
                ));
            }
            (Some(legacy), None) => {
                let _legacy_total_tokens = legacy.total_tokens;
                config.usage_by_model.insert(
                    GPT_5_6_AND_EARLIER_USAGE_BUCKET.to_string(),
                    UsageTotals {
                        tool_input_tokens: legacy.input_tokens,
                        tool_output_tokens: legacy.output_tokens,
                        total_tokens: legacy.input_tokens.saturating_add(legacy.output_tokens),
                        tool_call_count: legacy.tool_call_count,
                    },
                );
                true
            }
            (None, _) => false,
        };
        let config = config.normalized();
        config.validate_versioned()?;
        if migrated_usage {
            config.save_to_path(path)?;
        }
        Ok(config)
    }

    fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        let config = self.clone().normalized();
        config.validate_versioned()?;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::other("failed to resolve config directory for config.toml")
        })?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        let text = toml::to_string_pretty(&config).map_err(std::io::Error::other)?;
        let temp_path = parent.join(format!(".{APP_CONFIG_FILE_NAME}.{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write_result = (|| -> std::io::Result<()> {
            let mut file = options.open(&temp_path)?;
            use std::io::Write as _;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            replace_config_file(&temp_path, path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                if let Ok(directory) = fs::File::open(parent) {
                    let _ = directory.sync_all();
                }
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }
}

#[cfg(not(windows))]
fn replace_config_file(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    fs::rename(temp_path, target_path)
}

#[cfg(windows)]
fn replace_config_file(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temp_wide = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target_wide = target_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Direction for flow animation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlowDirection {
    Forward,  // request: Your computer -> ChatGPT Web
    Backward, // response: ChatGPT Web -> Your computer
}

pub const UI_EVENT_QUEUE_CAPACITY: usize = 2_048;

#[derive(Clone)]
pub struct UiEventSender {
    sender: Sender<ServerUiEvent>,
    dropped: Arc<AtomicU64>,
}

pub struct UiEventReceiver {
    receiver: Receiver<ServerUiEvent>,
    dropped: Arc<AtomicU64>,
    reported_dropped: u64,
}

pub fn ui_event_channel() -> (UiEventSender, UiEventReceiver) {
    let (sender, receiver) = mpsc::channel(UI_EVENT_QUEUE_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        UiEventSender {
            sender,
            dropped: dropped.clone(),
        },
        UiEventReceiver {
            receiver,
            dropped,
            reported_dropped: 0,
        },
    )
}

impl UiEventSender {
    /// Best-effort transport for transient local log/diagnostic events only.
    /// Authoritative connection, flow, and command state is applied directly to
    /// AppState so queue pressure cannot make the TUI state incorrect.
    pub fn send(&self, event: ServerUiEvent) -> Result<(), ()> {
        match self.sender.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
            Err(TrySendError::Closed(_)) => Err(()),
        }
    }

    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl UiEventReceiver {
    pub fn try_recv(&mut self) -> Result<ServerUiEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn take_dropped_since_last_report(&mut self) -> u64 {
        let total = self.dropped.load(Ordering::Relaxed);
        let delta = total.saturating_sub(self.reported_dropped);
        self.reported_dropped = total;
        delta
    }
}

#[derive(Clone)]
pub enum ServerUiEvent {
    IncrementRequestCount {
        workspace_id: WorkspaceId,
    },
    SetRemoteConnected {
        workspace_id: WorkspaceId,
        connected: bool,
    },
    RecordFlow {
        workspace_id: WorkspaceId,
        flow_id: String,
        events: Vec<String>,
        direction: FlowDirection,
    },
    BeginFlowClose {
        workspace_id: WorkspaceId,
        flow_id: String,
    },
    CommandStarted {
        workspace_id: WorkspaceId,
        activity_id: String,
        command: String,
        background: bool,
    },
    CommandBoundToJob {
        workspace_id: WorkspaceId,
        activity_id: String,
        job_id: String,
    },
    CommandUpdated {
        workspace_id: WorkspaceId,
        activity_id: Option<String>,
        job_id: Option<String>,
        state: CommandActivityState,
        exit_code: Option<i32>,
        preview: Option<String>,
    },
    Log {
        workspace_id: Option<WorkspaceId>,
        level: &'static str,
        message: String,
    },
}

/// Per-flow queued animation segment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlowAnimKind {
    Move,
    Turn,
}

#[derive(Clone, Copy)]
pub struct FlowAnimSegment {
    pub kind: FlowAnimKind,
    pub direction: FlowDirection,
    pub started_ms: u128,
    pub ends_ms: u128,
    pub step_ms: u64,
    pub start_cells: usize,
    pub end_cells: usize,
}

#[derive(Clone, Copy)]
pub struct FlowBootstrapStep {
    pub event: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Copy)]
pub struct FlowBootstrapPhase {
    pub title: &'static str,
    pub steps: &'static [FlowBootstrapStep],
}

const FLOW_BOOTSTRAP_PHASE_1_STEPS: &[FlowBootstrapStep] = &[
    FlowBootstrapStep {
        event: "initialize",
        label: "initialize#1",
    },
    FlowBootstrapStep {
        event: "initialize",
        label: "initialize#2",
    },
    FlowBootstrapStep {
        event: "notifications/initialized",
        label: "initialized",
    },
    FlowBootstrapStep {
        event: "tools/list",
        label: "tools/list",
    },
];

pub const FLOW_BOOTSTRAP_PHASES: &[FlowBootstrapPhase] = &[FlowBootstrapPhase {
    title: "Checking tools",
    steps: FLOW_BOOTSTRAP_PHASE_1_STEPS,
}];

/// Which MCP backends to enable.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Computer, // run_command only
    Browser,  // chrome-devtools-mcp only
    Both,     // both
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Computer => "Computer",
            Mode::Browser => "Browser",
            Mode::Both => "Both",
        }
    }
    pub fn computer_enabled(self) -> bool {
        matches!(self, Mode::Computer | Mode::Both)
    }
    pub fn browser_enabled(self) -> bool {
        matches!(self, Mode::Browser | Mode::Both)
    }
}

/// Which local toolset to expose in MCP.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolMode {
    MultiTools, // codex/claude-style workspace tools
    ReadOnly,   // read-only safe tools only
}

impl ToolMode {
    pub fn all() -> &'static [Self] {
        const TOOL_MODES: [ToolMode; 2] = [ToolMode::MultiTools, ToolMode::ReadOnly];
        &TOOL_MODES
    }

    pub fn label(self) -> &'static str {
        match self {
            ToolMode::MultiTools => "multi-tools",
            ToolMode::ReadOnly => "read-only",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ToolMode::MultiTools => {
                "Expose workspace tools plus the user's normal developer shell."
            }
            ToolMode::ReadOnly => "Expose safe read-only workspace tools only.",
        }
    }

    pub fn run_command_enabled(self) -> bool {
        matches!(self, ToolMode::MultiTools)
    }

    pub fn write_tools_enabled(self) -> bool {
        matches!(self, ToolMode::MultiTools)
    }

    pub fn read_only(self) -> bool {
        matches!(self, ToolMode::ReadOnly)
    }
}

/// Browser process launched and owned by MoonDesk for remote debugging.
pub struct OwnedRemoteBrowser {
    pub child: tokio::process::Child,
    #[cfg(windows)]
    pub process_tree: crate::process_runner::WindowsProcessTreeGuard,
    pub profile_dir: PathBuf,
}

/// Shared application state across server, ngrok, and TUI.
pub struct AppState {
    pub theme: String,
    pub mode: Mode,
    pub tool_mode: ToolMode,
    pub mcp_slug: String,
    pub workspaces: Vec<WorkspaceConfig>,
    pub workspace_runtimes: HashMap<WorkspaceId, Arc<WorkspaceRuntime>>,
    workspace_mutation_lock: Arc<Mutex<()>>,
    pub ngrok_domain: Option<String>,
    ngrok_authtoken: Option<String>,
    agents_path_mode: AgentsPathMode,
    config_dirty: bool,
    pub is_returning_user: bool,
    pub server_running: bool,
    pub ngrok_running: bool,
    pub ngrok_url: Option<String>,
    pub remote_connected: bool,
    pub last_remote_activity_ms: Option<u128>,
    pub devtools_running: bool,
    pub port: u16,
    pub workspace_root: String,
    pub set_moondesk_as_co_author: bool,
    pub mascot: MascotPack,
    pub detected_browsers: Vec<DetectedBrowser>,
    pub selected_browser: Option<DetectedBrowser>,
    pub logs: Vec<LogEntry>,
    pub command_activities: VecDeque<CommandActivity>,
    pub flows: Vec<FlowLane>,
    pub flow_bootstrap_progress: HashMap<String, FlowBootstrapProgress>,
    pub request_count: u64,
    pub usage_by_model: BTreeMap<String, UsageTotals>,
    pub session_usage_totals: UsageTotals,
    pub command_jobs: CommandJobManager,
    config_path: PathBuf,
    pub server_handle: Option<tokio::task::JoinHandle<()>>,
    pub ngrok_task: Option<tokio::task::JoinHandle<()>>,
    pub remote_browser: Option<OwnedRemoteBrowser>,
}

pub type SharedState = Arc<Mutex<AppState>>;

pub const FLOW_ANIM_CELLS: usize = 32;
const FLOW_LINK_CELLS: u64 = FLOW_ANIM_CELLS as u64;
const FLOW_CHAIN_DELAY_CELLS: u64 = 0;
const FLOW_FORWARD_ANIMATION_DURATION_MS: u64 = 125;
const FLOW_BACKWARD_ANIMATION_DURATION_MS: u64 = 125;
const FLOW_STEP_FIXED_MS: u64 = FLOW_FORWARD_ANIMATION_DURATION_MS.div_ceil(FLOW_LINK_CELLS);
const FLOW_TURN_TRANSITION_MS: u64 = 24;
const FLOW_CLOSE_PRUNE_MULTIPLIER: u64 = 3;
const FLOW_BOOTSTRAP_STATUS_CLOSE_DELAY_MS: u128 = 3_000;

fn short_flow_id(flow_id: &str) -> String {
    flow_id[..flow_id.len().min(8)].to_string()
}

#[cfg(test)]
pub fn user_home_dir() -> std::io::Result<PathBuf> {
    Ok(std::env::temp_dir().join(format!("moondesk-test-home-{}", std::process::id())))
}

#[cfg(not(test))]
pub fn user_home_dir() -> std::io::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(user_profile) =
            std::env::var_os("USERPROFILE").filter(|value| !value.is_empty())
        {
            return Ok(PathBuf::from(user_profile));
        }

        let home_drive = std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty());
        let home_path = std::env::var_os("HOMEPATH").filter(|value| !value.is_empty());
        if let (Some(home_drive), Some(home_path)) = (home_drive, home_path) {
            let mut path = PathBuf::from(home_drive);
            path.push(home_path);
            return Ok(path);
        }
    }

    Err(std::io::Error::other(
        "could not resolve the user home directory from HOME, USERPROFILE, or HOMEDRIVE/HOMEPATH",
    ))
}

pub fn app_config_path() -> std::io::Result<PathBuf> {
    Ok(user_home_dir()?
        .join(APP_CONFIG_DIR_NAME)
        .join(APP_CONFIG_FILE_NAME))
}

pub fn load_app_config() -> std::io::Result<AppConfig> {
    AppConfig::load_from_path(&app_config_path()?)
}

pub fn normalize_ngrok_domain(value: &str) -> Result<Option<String>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let host = if trimmed.contains("://") {
        let url = reqwest::Url::parse(trimmed)
            .map_err(|error| format!("invalid ngrok domain URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("ngrok domain URL must use http:// or https://".into());
        }
        let authority = trimmed
            .split_once("://")
            .map(|(_, remainder)| remainder)
            .unwrap_or_default()
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        if !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || host_port.contains(':')
        {
            return Err("ngrok domain must not include credentials or a port".into());
        }
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            return Err("ngrok domain must not include a path, query, or fragment".into());
        }
        url.host_str()
            .ok_or_else(|| "ngrok domain URL is missing a host".to_string())?
            .to_string()
    } else {
        if trimmed.contains(['/', '?', '#', '@', ':']) || trimmed.chars().any(char::is_whitespace) {
            return Err("ngrok domain must be a bare hostname or an http(s) URL".into());
        }
        trimmed.trim_end_matches('.').to_string()
    };

    let host = host.to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err("ngrok domain hostname is empty or too long".into());
    }
    for label in host.split('.') {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return Err(format!("invalid hostname label in ngrok domain: {label}"));
        }
    }
    Ok(Some(host))
}

fn format_hms_in_offset(now: time::OffsetDateTime, offset: time::UtcOffset) -> String {
    let local = now.to_offset(offset);
    format!(
        "{:02}:{:02}:{:02}",
        local.hour(),
        local.minute(),
        local.second()
    )
}

fn now_hms() -> String {
    let now = time::OffsetDateTime::now_utc();
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    format_hms_in_offset(now, offset)
}

fn now_unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn derive_flow_step_ms() -> u64 {
    FLOW_STEP_FIXED_MS
}

fn prune_finished_segments(queue: &mut VecDeque<FlowAnimSegment>, now_ms: u128) {
    while let Some(seg) = queue.front() {
        if seg.ends_ms <= now_ms {
            queue.pop_front();
        } else {
            break;
        }
    }
}

fn current_queue_segment(
    queue: &VecDeque<FlowAnimSegment>,
    now_ms: u128,
) -> Option<FlowAnimSegment> {
    if let Some(seg) = queue
        .iter()
        .find(|seg| seg.started_ms <= now_ms && now_ms < seg.ends_ms)
    {
        return Some(*seg);
    }
    queue.front().copied()
}

pub(crate) fn flow_anim_lit_count(seg: FlowAnimSegment, now_ms: u128) -> usize {
    if seg.started_ms >= seg.ends_ms {
        return seg.end_cells;
    }
    if now_ms <= seg.started_ms {
        return seg.start_cells;
    }
    if now_ms >= seg.ends_ms {
        return seg.end_cells;
    }

    let duration_ms = seg.ends_ms.saturating_sub(seg.started_ms);
    if duration_ms == 0 {
        return seg.end_cells;
    }

    let elapsed_ms = now_ms.saturating_sub(seg.started_ms);
    let distance = seg.end_cells.abs_diff(seg.start_cells) as u128;
    let progressed = ((distance * elapsed_ms) / duration_ms) as usize;

    if seg.end_cells >= seg.start_cells {
        (seg.start_cells + progressed).min(seg.end_cells)
    } else {
        seg.start_cells
            .saturating_sub(progressed.min(seg.start_cells - seg.end_cells))
    }
}

fn move_segment_duration_ms(
    direction: FlowDirection,
    _step_ms: u64,
    start_cells: usize,
    end_cells: usize,
) -> u128 {
    let cells_to_travel = end_cells.abs_diff(start_cells) as u128;
    if cells_to_travel == 0 {
        return 0;
    }
    let base_duration_ms = match direction {
        FlowDirection::Forward => FLOW_FORWARD_ANIMATION_DURATION_MS as u128,
        FlowDirection::Backward => FLOW_BACKWARD_ANIMATION_DURATION_MS as u128,
    };
    ((cells_to_travel + FLOW_CHAIN_DELAY_CELLS as u128) * base_duration_ms)
        .div_ceil(FLOW_LINK_CELLS as u128)
}

fn enqueue_flow_segment(
    queue: &mut VecDeque<FlowAnimSegment>,
    direction: FlowDirection,
    now_ms: u128,
    step_ms: u64,
) {
    prune_finished_segments(queue, now_ms);

    let current_seg = current_queue_segment(queue, now_ms);
    let current_direction = current_seg
        .map(|seg| seg.direction)
        .or_else(|| queue.back().map(|seg| seg.direction));
    let current_cells = current_seg
        .map(|seg| flow_anim_lit_count(seg, now_ms))
        .or_else(|| queue.back().map(|seg| seg.end_cells))
        .unwrap_or(0)
        .min(FLOW_ANIM_CELLS);

    queue.clear();

    let mut start_ms = now_ms;
    let mut move_start_cells = 0usize;

    if let Some(current_direction) = current_direction {
        if current_direction == direction {
            move_start_cells = current_cells;
        } else if current_cells > 0 {
            let turn_end = start_ms + FLOW_TURN_TRANSITION_MS as u128;
            queue.push_back(FlowAnimSegment {
                kind: FlowAnimKind::Turn,
                direction: current_direction,
                started_ms: start_ms,
                ends_ms: turn_end,
                step_ms,
                start_cells: current_cells,
                end_cells: 0,
            });
            start_ms = turn_end;
        }
    }

    let move_end =
        start_ms + move_segment_duration_ms(direction, step_ms, move_start_cells, FLOW_ANIM_CELLS);
    if move_end > start_ms {
        queue.push_back(FlowAnimSegment {
            kind: FlowAnimKind::Move,
            direction,
            started_ms: start_ms,
            ends_ms: move_end,
            step_ms,
            start_cells: move_start_cells,
            end_cells: FLOW_ANIM_CELLS,
        });
    }
}

fn flow_bootstrap_step(index: usize) -> Option<&'static FlowBootstrapStep> {
    let mut offset = 0;
    for phase in FLOW_BOOTSTRAP_PHASES {
        let end = offset + phase.steps.len();
        if index < end {
            return phase.steps.get(index - offset);
        }
        offset = end;
    }
    None
}

pub fn flow_bootstrap_steps_total() -> usize {
    FLOW_BOOTSTRAP_PHASES
        .iter()
        .map(|phase| phase.steps.len())
        .sum()
}

fn events_start_bootstrap_status(events: &[String]) -> bool {
    events.iter().any(|event| event == "initialize")
}

fn is_bootstrap_status_event(event: &str) -> bool {
    FLOW_BOOTSTRAP_PHASES
        .iter()
        .flat_map(|phase| phase.steps)
        .any(|step| step.event == event)
}

fn events_are_bootstrap_status_events(events: &[String]) -> bool {
    events.iter().all(|event| is_bootstrap_status_event(event))
}

fn advance_bootstrap_progress(
    completed_steps: &mut usize,
    pending_steps: &mut VecDeque<usize>,
    events: &[String],
    direction: FlowDirection,
) {
    match direction {
        FlowDirection::Forward => {
            for event in events {
                let next_index = completed_steps.saturating_add(pending_steps.len());
                let Some(step) = flow_bootstrap_step(next_index) else {
                    break;
                };
                if step.event != event {
                    continue;
                }
                if step.event == "notifications/initialized" {
                    *completed_steps = next_index + 1;
                    continue;
                }
                pending_steps.push_back(next_index);
            }
        }
        FlowDirection::Backward => {
            for event in events {
                let Some(pending_index) = pending_steps.front().copied() else {
                    break;
                };
                let Some(step) = flow_bootstrap_step(pending_index) else {
                    pending_steps.clear();
                    break;
                };
                if step.event == event {
                    pending_steps.pop_front();
                    *completed_steps = pending_index + 1;
                }
            }
        }
    }
}

impl AppState {
    pub fn new(port: u16, workspace_root: String) -> std::io::Result<Self> {
        let config_path = app_config_path()?;
        Self::from_config_path(port, workspace_root, config_path)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        port: u16,
        workspace_root: String,
        config_path: PathBuf,
    ) -> std::io::Result<Self> {
        Self::from_config_path(port, workspace_root, config_path)
    }

    fn from_config_path(
        port: u16,
        workspace_root: String,
        config_path: PathBuf,
    ) -> std::io::Result<Self> {
        let (config, had_existing_workspace) =
            AppConfig::load_for_app(&config_path, Path::new(&workspace_root))?;
        let ngrok_authtoken = config.ngrok_authtoken.clone();
        let agents_path_mode = config.agents_path_mode;
        let mascot_seed = rand::random::<u64>();
        let mascot = mascot::build_workspace_mascot(mascot_seed);
        let is_returning_user = had_existing_workspace && config.ngrok_domain.is_some();
        let primary_workspace = config.workspaces.first().cloned().ok_or_else(|| {
            std::io::Error::other("config v2 did not provide a primary workspace")
        })?;
        let mcp_slug = primary_workspace.mcp_slug.clone();
        let workspace_root = primary_workspace.root.to_string_lossy().into_owned();
        let workspace_runtimes = config
            .workspaces
            .iter()
            .map(|workspace| (workspace.id.clone(), Arc::new(WorkspaceRuntime::default())))
            .collect::<HashMap<_, _>>();

        let mut app = Self {
            theme: config.theme,
            mode: config.mode,
            tool_mode: config.tool_mode,
            mcp_slug,
            workspaces: config.workspaces,
            workspace_runtimes,
            workspace_mutation_lock: Arc::new(Mutex::new(())),
            ngrok_domain: config.ngrok_domain.clone(),
            ngrok_authtoken,
            agents_path_mode,
            config_dirty: false,
            is_returning_user,
            server_running: false,
            ngrok_running: false,
            ngrok_url: None,
            remote_connected: false,
            last_remote_activity_ms: None,
            devtools_running: false,
            port,
            set_moondesk_as_co_author: config.set_moondesk_as_co_author,
            mascot,
            workspace_root,
            detected_browsers: Vec::new(),
            selected_browser: config.selected_browser,
            logs: Vec::new(),
            command_activities: VecDeque::new(),
            flows: Vec::new(),
            flow_bootstrap_progress: HashMap::new(),
            request_count: 0,
            usage_by_model: config.usage_by_model,
            session_usage_totals: UsageTotals::default(),
            command_jobs: CommandJobManager::new(),
            config_path,
            server_handle: None,
            ngrok_task: None,
            remote_browser: None,
        };
        app.log("INFO", format!("ClippyMoon seed: {mascot_seed:016x}"));
        Ok(app)
    }

    pub fn current_theme(&self) -> &'static theme::ThemeDef {
        theme::resolve(&self.theme)
    }

    pub fn mcp_path(&self) -> String {
        format!("/{}/mcp", self.mcp_slug)
    }

    pub fn public_mcp_url(&self) -> Option<String> {
        let base = self.ngrok_url.clone().or_else(|| {
            self.ngrok_domain
                .as_ref()
                .map(|domain| format!("https://{domain}"))
        })?;
        Some(format!("{}{}", base.trim_end_matches('/'), self.mcp_path()))
    }

    fn recompute_remote_connection_state(&mut self) {
        self.remote_connected = self
            .workspace_runtimes
            .values()
            .any(|runtime| runtime.remote_connected());
        self.last_remote_activity_ms = self
            .workspace_runtimes
            .values()
            .filter_map(|runtime| runtime.last_remote_activity_ms())
            .max()
            .map(u128::from);
    }

    pub fn clear_remote_connection_state(&mut self) {
        for runtime in self.workspace_runtimes.values() {
            runtime.set_remote_connected(false);
        }
        self.recompute_remote_connection_state();
    }

    fn purge_workspace_observability(&mut self, workspace_id: &WorkspaceId) {
        self.logs
            .retain(|entry| entry.workspace_id.as_ref() != Some(workspace_id));
        self.command_activities
            .retain(|activity| &activity.workspace_id != workspace_id);

        let flow_prefix = format!("{}:", workspace_id.as_str());
        self.flows
            .retain(|flow| !flow.flow_id.starts_with(&flow_prefix));
        self.flow_bootstrap_progress
            .retain(|flow_id, _| !flow_id.starts_with(&flow_prefix));
    }

    pub fn log(&mut self, level: &'static str, message: String) {
        self.log_scoped(None, level, message);
    }

    pub fn log_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        level: &'static str,
        message: String,
    ) {
        self.log_scoped(Some(workspace_id), level, message);
    }

    fn log_scoped(
        &mut self,
        workspace_id: Option<WorkspaceId>,
        level: &'static str,
        message: String,
    ) {
        self.logs.push(LogEntry {
            workspace_id: workspace_id.clone(),
            time: now_hms(),
            level,
            message,
        });
        let matching = self
            .logs
            .iter()
            .filter(|entry| entry.workspace_id == workspace_id)
            .count();
        if matching > 500
            && let Some(index) = self
                .logs
                .iter()
                .position(|entry| entry.workspace_id == workspace_id)
        {
            self.logs.remove(index);
        }
    }

    fn command_started(
        &mut self,
        workspace_id: WorkspaceId,
        activity_id: String,
        command: String,
        background: bool,
    ) {
        self.command_activities.push_back(CommandActivity {
            workspace_id: workspace_id.clone(),
            id: activity_id,
            time: now_hms(),
            command,
            background,
            job_id: None,
            state: CommandActivityState::Running,
            exit_code: None,
            preview: None,
        });
        let matching = self
            .command_activities
            .iter()
            .filter(|activity| activity.workspace_id == workspace_id)
            .count();
        if matching > MAX_COMMAND_ACTIVITIES
            && let Some(index) = self
                .command_activities
                .iter()
                .position(|activity| activity.workspace_id == workspace_id)
        {
            self.command_activities.remove(index);
        }
    }

    fn command_bind_job(&mut self, workspace_id: &WorkspaceId, activity_id: &str, job_id: String) {
        let target_index = self.command_activities.iter().position(|activity| {
            &activity.workspace_id == workspace_id && activity.id == activity_id
        });
        let existing_index = self.command_activities.iter().position(|activity| {
            &activity.workspace_id == workspace_id
                && activity.job_id.as_deref() == Some(job_id.as_str())
        });

        if let (Some(target_index), Some(existing_index)) = (target_index, existing_index)
            && target_index != existing_index
        {
            // start_command is retry-deduplicated by the command manager. If
            // the same job is returned to a retried MCP request, keep one
            // visual command entry instead of showing a duplicate execution.
            self.command_activities.remove(target_index);
            return;
        }

        if let Some(activity) =
            self.command_activities.iter_mut().rev().find(|activity| {
                &activity.workspace_id == workspace_id && activity.id == activity_id
            })
        {
            activity.job_id = Some(job_id);
        }
    }

    fn command_update(
        &mut self,
        workspace_id: &WorkspaceId,
        activity_id: Option<&str>,
        job_id: Option<&str>,
        state: CommandActivityState,
        exit_code: Option<i32>,
        preview: Option<String>,
    ) {
        let activity = self.command_activities.iter_mut().rev().find(|activity| {
            &activity.workspace_id == workspace_id
                && (activity_id.is_some_and(|id| activity.id == id)
                    || job_id.is_some_and(|id| activity.job_id.as_deref() == Some(id)))
        });
        let Some(activity) = activity else {
            return;
        };
        activity.state = state;
        activity.exit_code = exit_code;
        if preview.as_deref().is_some_and(|text| !text.is_empty()) {
            activity.preview = preview;
        }
    }

    fn app_config(&self) -> AppConfig {
        AppConfig {
            config_version: CURRENT_CONFIG_VERSION,
            ngrok_authtoken: self.ngrok_authtoken.clone(),
            mcp_slug: None,
            workspaces: self.workspaces.clone(),
            ngrok_domain: self.ngrok_domain.clone(),
            agents_path_mode: self.agents_path_mode,
            set_moondesk_as_co_author: self.set_moondesk_as_co_author,
            theme: self.theme.clone(),
            mode: self.mode,
            tool_mode: self.tool_mode,
            usage_by_model: self.usage_by_model.clone(),
            selected_browser: self.selected_browser.clone(),
        }
        .normalized()
    }

    fn app_config_with_workspaces(&self, workspaces: Vec<WorkspaceConfig>) -> AppConfig {
        let mut config = self.app_config();
        config.workspaces = workspaces;
        config
    }

    fn sync_primary_workspace_compatibility_fields(&mut self) {
        if let Some(primary) = self.workspaces.first() {
            self.mcp_slug = primary.mcp_slug.clone();
            self.workspace_root = primary.root.to_string_lossy().into_owned();
        }
    }

    fn workspace_registry_snapshot(
        &self,
        workspaces: Vec<WorkspaceConfig>,
    ) -> (AppConfig, PathBuf) {
        (
            self.app_config_with_workspaces(workspaces),
            self.config_path.clone(),
        )
    }

    pub fn ngrok_authtoken(&self) -> Option<&str> {
        self.ngrok_authtoken.as_deref()
    }

    pub fn set_ngrok_authtoken(&mut self, token: Option<String>) {
        self.ngrok_authtoken = token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.config_dirty = true;
    }

    pub fn set_ngrok_domain(&mut self, domain: Option<String>) {
        self.ngrok_domain = domain;
        self.config_dirty = true;
    }

    pub fn mark_config_dirty(&mut self) {
        self.config_dirty = true;
    }

    #[cfg(test)]
    pub fn persist_state(&self) -> std::io::Result<()> {
        self.app_config().save_to_path(&self.config_path)
    }

    fn take_config_snapshot(&mut self, force: bool) -> Option<(AppConfig, PathBuf)> {
        if !force && !self.config_dirty {
            return None;
        }
        self.config_dirty = false;
        Some((self.app_config(), self.config_path.clone()))
    }

    pub fn all_time_usage_totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for usage in self.usage_by_model.values() {
            totals.merge(usage);
        }
        totals
    }

    pub fn record_turn_usage(&mut self, tool_input_tokens: u64, tool_output_tokens: u64) {
        self.usage_by_model
            .entry(CURRENT_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(tool_input_tokens, tool_output_tokens, 1);
        self.session_usage_totals
            .accumulate(tool_input_tokens, tool_output_tokens, 1);
        self.config_dirty = true;
    }

    pub fn apply_server_ui_event(&mut self, event: ServerUiEvent) {
        // Server events are delivered asynchronously. A workspace can be removed
        // after an event was queued but before the TUI drains it; never let those
        // late events recreate logs/flows/command history for a deleted workspace.
        let event_workspace_id = match &event {
            ServerUiEvent::IncrementRequestCount { workspace_id }
            | ServerUiEvent::SetRemoteConnected { workspace_id, .. }
            | ServerUiEvent::RecordFlow { workspace_id, .. }
            | ServerUiEvent::BeginFlowClose { workspace_id, .. }
            | ServerUiEvent::CommandStarted { workspace_id, .. }
            | ServerUiEvent::CommandBoundToJob { workspace_id, .. }
            | ServerUiEvent::CommandUpdated { workspace_id, .. } => Some(workspace_id),
            ServerUiEvent::Log { workspace_id, .. } => workspace_id.as_ref(),
        };
        if event_workspace_id
            .is_some_and(|workspace_id| !self.workspace_runtimes.contains_key(workspace_id))
        {
            return;
        }

        match event {
            ServerUiEvent::IncrementRequestCount { workspace_id } => {
                self.request_count = self.request_count.saturating_add(1);
                if let Some(runtime) = self.workspace_runtimes.get(&workspace_id) {
                    runtime.mark_remote_activity(now_unix_millis().min(u64::MAX as u128) as u64);
                }
            }
            ServerUiEvent::SetRemoteConnected {
                workspace_id,
                connected,
            } => {
                if let Some(runtime) = self.workspace_runtimes.get(&workspace_id) {
                    runtime.set_remote_connected(connected);
                }
                self.recompute_remote_connection_state();
            }
            ServerUiEvent::RecordFlow {
                workspace_id,
                flow_id,
                events,
                direction,
            } => {
                let flow_id = format!("{}:{flow_id}", workspace_id.as_str());
                self.record_flow(&flow_id, &events, direction);
            }
            ServerUiEvent::BeginFlowClose {
                workspace_id,
                flow_id,
            } => {
                let flow_id = format!("{}:{flow_id}", workspace_id.as_str());
                self.begin_flow_close(&flow_id);
            }
            ServerUiEvent::CommandStarted {
                workspace_id,
                activity_id,
                command,
                background,
            } => {
                self.command_started(workspace_id, activity_id, command, background);
            }
            ServerUiEvent::CommandBoundToJob {
                workspace_id,
                activity_id,
                job_id,
            } => {
                self.command_bind_job(&workspace_id, &activity_id, job_id);
            }
            ServerUiEvent::CommandUpdated {
                workspace_id,
                activity_id,
                job_id,
                state,
                exit_code,
                preview,
            } => {
                self.command_update(
                    &workspace_id,
                    activity_id.as_deref(),
                    job_id.as_deref(),
                    state,
                    exit_code,
                    preview,
                );
            }
            ServerUiEvent::Log {
                workspace_id,
                level,
                message,
            } => match workspace_id {
                Some(workspace_id) => self.log_workspace(workspace_id, level, message),
                None => self.log(level, message),
            },
        }
    }

    pub fn record_flow(&mut self, flow_id: &str, events: &[String], direction: FlowDirection) {
        if events.is_empty() {
            return;
        }
        let now_ms = now_unix_millis();
        self.last_remote_activity_ms = Some(now_ms);
        self.remote_connected = true;
        let step_ms = derive_flow_step_ms();
        let mut bootstrap = self
            .flow_bootstrap_progress
            .get(flow_id)
            .cloned()
            .unwrap_or_default();
        let starts_bootstrap_status = events_start_bootstrap_status(events);
        let only_bootstrap_status_events = events_are_bootstrap_status_events(events);

        if let Some(idx) = self.flows.iter().position(|flow| flow.flow_id == flow_id) {
            let mut flow = self.flows.remove(idx);
            if starts_bootstrap_status {
                flow.bootstrap_status_active = true;
            } else if flow.bootstrap_status_active && !only_bootstrap_status_events {
                flow.bootstrap_status_active = false;
                flow.bootstrap_status_close_deadline_ms = None;
            }
            flow.events.extend(events.iter().cloned());
            if flow.events.len() > 12 {
                let drop_n = flow.events.len() - 12;
                flow.events.drain(0..drop_n);
            }
            flow.bootstrap_completed_steps = bootstrap.completed_steps;
            flow.bootstrap_pending_steps = bootstrap.pending_steps.clone();
            advance_bootstrap_progress(
                &mut flow.bootstrap_completed_steps,
                &mut flow.bootstrap_pending_steps,
                events,
                direction,
            );
            bootstrap.completed_steps = flow.bootstrap_completed_steps;
            bootstrap.pending_steps = flow.bootstrap_pending_steps.clone();
            self.flow_bootstrap_progress
                .insert(flow_id.to_string(), bootstrap);
            flow.closing_started_ms = None;
            flow.closing_step_ms = 0;
            flow.bootstrap_status_close_deadline_ms = None;
            flow.last_direction = direction;
            enqueue_flow_segment(&mut flow.anim_queue, direction, now_ms, step_ms);
            self.flows.insert(0, flow);
            return;
        }

        let mut trimmed = events.to_vec();
        if trimmed.len() > 12 {
            trimmed = trimmed[trimmed.len() - 12..].to_vec();
        }
        self.flows.insert(
            0,
            FlowLane {
                flow_id: flow_id.to_string(),
                short_id: short_flow_id(flow_id),
                events: trimmed,
                bootstrap_status_active: starts_bootstrap_status,
                bootstrap_completed_steps: bootstrap.completed_steps,
                bootstrap_pending_steps: bootstrap.pending_steps.clone(),
                bootstrap_status_close_deadline_ms: None,
                anim_queue: VecDeque::new(),
                last_direction: direction,
                closing_started_ms: None,
                closing_step_ms: 0,
            },
        );
        if let Some(flow) = self.flows.first_mut() {
            advance_bootstrap_progress(
                &mut flow.bootstrap_completed_steps,
                &mut flow.bootstrap_pending_steps,
                events,
                direction,
            );
            bootstrap.completed_steps = flow.bootstrap_completed_steps;
            bootstrap.pending_steps = flow.bootstrap_pending_steps.clone();
            self.flow_bootstrap_progress
                .insert(flow_id.to_string(), bootstrap);
            enqueue_flow_segment(&mut flow.anim_queue, direction, now_ms, step_ms);
        }
    }

    pub fn begin_flow_close(&mut self, flow_id: &str) {
        let now_ms = now_unix_millis();
        self.flow_bootstrap_progress.remove(flow_id);
        if let Some(flow) = self.flows.iter_mut().find(|flow| flow.flow_id == flow_id)
            && flow.closing_started_ms.is_none()
        {
            flow.closing_started_ms = Some(now_ms);
            flow.closing_step_ms = flow
                .anim_queue
                .back()
                .map(|seg| seg.step_ms.max(1))
                .unwrap_or_else(derive_flow_step_ms);
            flow.anim_queue.clear();
            flow.bootstrap_status_active = false;
            flow.bootstrap_status_close_deadline_ms = None;
        }
    }

    pub fn prune_closed_flows(&mut self) {
        let now_ms = now_unix_millis();
        let bootstrap_steps_total = flow_bootstrap_steps_total();

        for flow in &mut self.flows {
            prune_finished_segments(&mut flow.anim_queue, now_ms);
            if !flow.bootstrap_status_active {
                flow.bootstrap_status_close_deadline_ms = None;
                continue;
            }
            let bootstrap_complete = flow.bootstrap_completed_steps >= bootstrap_steps_total
                && flow.bootstrap_pending_steps.is_empty();
            if flow.closing_started_ms.is_none() && bootstrap_complete {
                if flow.anim_queue.is_empty() {
                    match flow.bootstrap_status_close_deadline_ms {
                        Some(deadline) if now_ms >= deadline => {
                            flow.bootstrap_status_active = false;
                            flow.bootstrap_status_close_deadline_ms = None;
                        }
                        Some(_) => {}
                        None => {
                            flow.bootstrap_status_close_deadline_ms =
                                Some(now_ms + FLOW_BOOTSTRAP_STATUS_CLOSE_DELAY_MS);
                        }
                    }
                } else {
                    flow.bootstrap_status_close_deadline_ms = None;
                }
            } else {
                flow.bootstrap_status_close_deadline_ms = None;
            }
        }
        self.flows.retain(|flow| {
            let Some(closing_started_ms) = flow.closing_started_ms else {
                return true;
            };
            let step_ms = flow.closing_step_ms.max(1) as u128;
            let ttl_ms = (FLOW_LINK_CELLS * FLOW_CLOSE_PRUNE_MULTIPLIER) as u128 * step_ms;
            now_ms.saturating_sub(closing_started_ms) < ttl_ms
        });
    }
}

async fn persist_workspace_registry(config: AppConfig, path: PathBuf) -> Result<(), String> {
    match tokio::task::spawn_blocking(move || config.save_to_path(&path)).await {
        Ok(result) => {
            result.map_err(|error| format!("failed to persist workspace registry: {error}"))
        }
        Err(error) => Err(format!(
            "workspace registry persistence task failed: {error}"
        )),
    }
}

async fn validate_workspace_registry_off_thread(
    workspaces: Vec<WorkspaceConfig>,
) -> Result<Vec<WorkspaceConfig>, String> {
    tokio::task::spawn_blocking(move || {
        workspaces::validate_workspace_registry(&workspaces)?;
        Ok(workspaces)
    })
    .await
    .map_err(|error| format!("workspace registry validation task failed: {error}"))?
}

async fn build_workspace_config_off_thread(
    name: String,
    root: PathBuf,
    slug: String,
) -> Result<WorkspaceConfig, String> {
    tokio::task::spawn_blocking(move || WorkspaceConfig::new(name, root, slug))
        .await
        .map_err(|error| format!("workspace path validation task failed: {error}"))?
}

fn unique_workspace_slug(workspaces: &[WorkspaceConfig]) -> Result<String, String> {
    for _ in 0..64 {
        let candidate = workspaces::generate_mcp_slug();
        if workspaces
            .iter()
            .all(|workspace| workspace.mcp_slug != candidate)
        {
            return Ok(candidate);
        }
    }
    Err("failed to generate a unique workspace MCP slug".to_string())
}

#[derive(Debug)]
pub enum AddWorkspaceError {
    Validation(String),
    Persistence(String),
}

impl std::fmt::Display for AddWorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message) | Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AddWorkspaceError {}

pub async fn add_workspace(
    state: &SharedState,
    name: String,
    root: PathBuf,
) -> Result<WorkspaceConfig, AddWorkspaceError> {
    let mutation_lock = { state.lock().await.workspace_mutation_lock.clone() };
    let _mutation_guard = mutation_lock.lock().await;
    let (slug, mut proposed) = {
        let app = state.lock().await;
        (
            unique_workspace_slug(&app.workspaces).map_err(AddWorkspaceError::Validation)?,
            app.workspaces.clone(),
        )
    };

    // Canonicalization and overlap validation can touch the filesystem. Keep that
    // work off the async AppState mutex so requests for other workspaces continue.
    let workspace = build_workspace_config_off_thread(name, root, slug)
        .await
        .map_err(AddWorkspaceError::Validation)?;
    proposed.push(workspace.clone());
    let proposed = validate_workspace_registry_off_thread(proposed)
        .await
        .map_err(AddWorkspaceError::Validation)?;
    let (config, path) = {
        let app = state.lock().await;
        app.workspace_registry_snapshot(proposed.clone())
    };

    persist_workspace_registry(config, path)
        .await
        .map_err(AddWorkspaceError::Persistence)?;

    let mut app = state.lock().await;
    app.workspaces = proposed;
    app.workspace_runtimes
        .insert(workspace.id.clone(), Arc::new(WorkspaceRuntime::default()));
    app.sync_primary_workspace_compatibility_fields();
    app.config_dirty = true;
    Ok(workspace)
}

pub async fn rename_workspace(
    state: &SharedState,
    workspace_id: &WorkspaceId,
    name: String,
) -> Result<(), String> {
    let mutation_lock = { state.lock().await.workspace_mutation_lock.clone() };
    let _mutation_guard = mutation_lock.lock().await;
    let normalized = workspaces::normalize_workspace_name(&name)?;
    let mut proposed = { state.lock().await.workspaces.clone() };
    let workspace = proposed
        .iter_mut()
        .find(|workspace| &workspace.id == workspace_id)
        .ok_or_else(|| "workspace not found".to_string())?;
    workspace.name = normalized;
    let proposed = validate_workspace_registry_off_thread(proposed).await?;
    let (config, path) = {
        let app = state.lock().await;
        app.workspace_registry_snapshot(proposed.clone())
    };

    persist_workspace_registry(config, path).await?;

    let mut app = state.lock().await;
    app.workspaces = proposed;
    app.sync_primary_workspace_compatibility_fields();
    app.config_dirty = true;
    Ok(())
}

pub async fn rotate_workspace_secret(
    state: &SharedState,
    workspace_id: &WorkspaceId,
) -> Result<String, String> {
    let mutation_lock = { state.lock().await.workspace_mutation_lock.clone() };
    let _mutation_guard = mutation_lock.lock().await;
    let (slug, mut proposed) = {
        let app = state.lock().await;
        (
            unique_workspace_slug(&app.workspaces)?,
            app.workspaces.clone(),
        )
    };
    let workspace = proposed
        .iter_mut()
        .find(|workspace| &workspace.id == workspace_id)
        .ok_or_else(|| "workspace not found".to_string())?;
    workspace.mcp_slug = slug.clone();
    let proposed = validate_workspace_registry_off_thread(proposed).await?;
    let (config, path) = {
        let app = state.lock().await;
        app.workspace_registry_snapshot(proposed.clone())
    };

    persist_workspace_registry(config, path).await?;

    let mut app = state.lock().await;
    app.workspaces = proposed;
    app.sync_primary_workspace_compatibility_fields();
    app.config_dirty = true;
    Ok(slug)
}

pub async fn remove_workspace(
    state: &SharedState,
    workspace_id: &WorkspaceId,
) -> Result<(), String> {
    let mutation_lock = { state.lock().await.workspace_mutation_lock.clone() };
    let _mutation_guard = mutation_lock.lock().await;
    let (runtime, command_jobs, proposed) = {
        let app = state.lock().await;
        if app.workspaces.len() <= 1 {
            return Err("cannot remove the final workspace".to_string());
        }
        if !app
            .workspaces
            .iter()
            .any(|workspace| &workspace.id == workspace_id)
        {
            return Err("workspace not found".to_string());
        }
        let runtime = app
            .workspace_runtimes
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| "workspace runtime not found".to_string())?;
        let proposed = app
            .workspaces
            .iter()
            .filter(|workspace| &workspace.id != workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        (runtime, app.command_jobs.clone(), proposed)
    };
    let proposed = validate_workspace_registry_off_thread(proposed).await?;
    let (config, path) = {
        let app = state.lock().await;
        app.workspace_registry_snapshot(proposed.clone())
    };

    // Preparation is reversible: block new background jobs, then stop admitting
    // new HTTP requests. Existing foreground/file operations keep their leases
    // and are allowed to finish without killing already-running background jobs.
    command_jobs.begin_workspace_removal(workspace_id).await?;
    runtime.revoke();
    if tokio::time::timeout(WORKSPACE_DRAIN_TIMEOUT, runtime.wait_for_drain())
        .await
        .is_err()
    {
        command_jobs.abort_workspace_removal(workspace_id).await;
        runtime.enable();
        return Err(format!(
            "timed out after {} seconds waiting for in-flight workspace requests to finish; workspace removal was aborted and the workspace was re-enabled",
            WORKSPACE_DRAIN_TIMEOUT.as_secs()
        ));
    }

    // No irreversible job cancellation happens until the registry removal is
    // durable. A persistence failure therefore restores normal request/job
    // admission without changing the workspace's running commands.
    if let Err(error) = persist_workspace_registry(config, path).await {
        command_jobs.abort_workspace_removal(workspace_id).await;
        runtime.enable();
        return Err(format!(
            "{error}; workspace removal was not persisted and the workspace was re-enabled"
        ));
    }

    // Persistence is committed at this point, so irreversible cancellation and
    // retained-output cleanup are now safe. The closing marker stays installed
    // until purge succeeds, preventing a stale pre-revocation caller from
    // creating an orphaned background job.
    let cleanup_error = command_jobs
        .finalize_workspace_removal(workspace_id)
        .await
        .err();

    {
        let mut app = state.lock().await;
        app.workspaces = proposed;
        app.workspace_runtimes.remove(workspace_id);
        app.purge_workspace_observability(workspace_id);
        app.recompute_remote_connection_state();
        app.sync_primary_workspace_compatibility_fields();
        app.config_dirty = true;
    }

    if let Some(initial_error) = cleanup_error {
        // The workspace is already durably removed at this point. Keep its command
        // closing marker installed and retry cleanup without blocking the workspace
        // manager; runners that needed a little longer to terminate can then be
        // purged instead of leaking retained state until the host exits.
        let retry_state = state.clone();
        let retry_jobs = command_jobs.clone();
        let retry_workspace_id = workspace_id.clone();
        tokio::spawn(async move {
            let mut last_error = initial_error;
            for attempt in 1..=WORKSPACE_CLEANUP_RETRY_ATTEMPTS {
                tokio::time::sleep(WORKSPACE_CLEANUP_RETRY_DELAY).await;
                match retry_jobs
                    .finalize_workspace_removal(&retry_workspace_id)
                    .await
                {
                    Ok(()) => {
                        retry_state.lock().await.log(
                            "INFO",
                            format!(
                                "Finished deferred command cleanup for removed workspace after {attempt} retry attempt(s)"
                            ),
                        );
                        return;
                    }
                    Err(error) => last_error = error,
                }
            }
            retry_state.lock().await.log(
                "WARN",
                format!(
                    "Workspace was removed, but command cleanup is still incomplete after {WORKSPACE_CLEANUP_RETRY_ATTEMPTS} retries: {last_error}"
                ),
            );
        });
    }
    Ok(())
}

pub async fn flush_config(state: &SharedState, force: bool) -> std::io::Result<bool> {
    // Serialize generic settings/usage flushes with workspace registry mutations.
    // Otherwise an older snapshot could finish after a secret/root update and
    // overwrite the newly persisted workspace registry.
    let mutation_lock = { state.lock().await.workspace_mutation_lock.clone() };
    let _mutation_guard = mutation_lock.lock().await;
    let snapshot = {
        let mut app = state.lock().await;
        app.take_config_snapshot(force)
    };
    let Some((config, path)) = snapshot else {
        return Ok(false);
    };

    let write_result = match tokio::task::spawn_blocking(move || config.save_to_path(&path)).await {
        Ok(result) => result,
        Err(error) => {
            state.lock().await.config_dirty = true;
            return Err(std::io::Error::other(format!(
                "config persistence task failed: {error}"
            )));
        }
    };
    if let Err(error) = write_result {
        state.lock().await.config_dirty = true;
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_CONFIG_FIXTURE: &str = include_str!("../tests/fixtures/legacy_config.toml");

    #[test]
    fn wall_clock_time_applies_local_offset_across_midnight() {
        let utc = time::macros::datetime!(2026-09-01 19:41:25 UTC);
        let ist = time::macros::offset!(+5:30);

        assert_eq!(format_hms_in_offset(utc, ist), "01:11:25");
    }

    #[test]
    fn ui_event_channel_is_bounded_and_reports_drops() {
        let (sender, mut receiver) = ui_event_channel();
        for index in 0..(UI_EVENT_QUEUE_CAPACITY + 50) {
            let _ = sender.send(ServerUiEvent::Log {
                workspace_id: None,
                level: "INFO",
                message: format!("event-{index}"),
            });
        }

        assert_eq!(sender.dropped_count(), 50);
        let mut queued = 0usize;
        while receiver.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, UI_EVENT_QUEUE_CAPACITY);
        assert_eq!(receiver.take_dropped_since_last_report(), 50);
        assert_eq!(receiver.take_dropped_since_last_report(), 0);
    }

    fn test_app(name: &str) -> (AppState, PathBuf, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("{name}-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);
        let app = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        (app, workspace, config_path)
    }

    #[test]
    fn tests_use_isolated_home_directory() {
        let test_home = user_home_dir().expect("resolve test home");
        let process_home = std::env::var_os("HOME").map(PathBuf::from);

        assert!(test_home.starts_with(std::env::temp_dir()));
        assert_ne!(Some(test_home), process_home);
    }

    #[test]
    fn app_state_logs_clippymoon_seed_for_reproduction() {
        let (app, workspace, config_path) = test_app("moondesk-clippymoon-seed-log");
        let seed_text = app
            .logs
            .iter()
            .find_map(|entry| entry.message.strip_prefix("ClippyMoon seed: "))
            .expect("startup should log the ClippyMoon seed");
        assert_eq!(seed_text.len(), 16);
        assert!(seed_text.chars().all(|ch| ch.is_ascii_hexdigit()));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn app_state_loads_persisted_config_file() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-config-load-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);
        std::fs::write(&config_path, LEGACY_CONFIG_FIXTURE).expect("write legacy config fixture");

        let app = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("load app state");

        assert_eq!(app.theme, "neon");
        assert!(matches!(app.mode, Mode::Browser));
        assert!(matches!(app.tool_mode, ToolMode::MultiTools));
        assert!(app.set_moondesk_as_co_author);
        let all_time_usage = app.all_time_usage_totals();
        assert_eq!(all_time_usage.tool_input_tokens, 120);
        assert_eq!(all_time_usage.tool_output_tokens, 34);
        assert_eq!(all_time_usage.total_tokens, 154);
        assert_eq!(all_time_usage.tool_call_count, 7);
        assert_eq!(app.session_usage_totals, UsageTotals::default());

        let migrated = std::fs::read_to_string(&config_path).expect("read migrated config");
        assert!(!migrated.contains("[usageTotals]"));
        assert!(migrated.contains("[usageByModel.\"through-gpt-5.6\"]"));
        assert!(migrated.contains("toolInputTokens = 120"));
        assert!(migrated.contains("toolOutputTokens = 34"));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir(workspace);
    }

    #[test]
    fn app_config_rejects_legacy_and_new_usage_formats_together() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("moondesk-config-usage-conflict-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);
        std::fs::write(
            &config_path,
            r#"theme = "neon"
mode = "both"
toolMode = "multiTools"

[usageTotals]
inputTokens = 1
outputTokens = 2
totalTokens = 3
toolCallCount = 1

[usageByModel."through-gpt-5.6"]
toolInputTokens = 1
toolOutputTokens = 2
totalTokens = 3
toolCallCount = 1
"#,
        )
        .expect("write conflicting config");

        let error = match AppConfig::load_from_path(&config_path) {
            Ok(_) => panic!("expected usage migration conflict"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("both legacy usageTotals and usageByModel")
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir(workspace);
    }

    #[test]
    fn persist_state_writes_single_config_file() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-config-save-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);

        let mut app = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        app.theme = "neon".into();
        app.mode = Mode::Computer;
        app.tool_mode = ToolMode::ReadOnly;
        app.usage_by_model
            .entry(CURRENT_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(12, 8, 3);
        app.session_usage_totals.accumulate(100, 200, 1);
        app.persist_state().expect("persist state");

        let saved = AppConfig::load_from_path(&config_path).expect("load config file");
        assert_eq!(saved.config_version, CURRENT_CONFIG_VERSION);
        assert!(saved.mcp_slug.is_none());
        assert_eq!(saved.workspaces.len(), 1);
        assert_eq!(saved.workspaces[0].mcp_slug, app.mcp_slug);
        assert_eq!(saved.theme, "neon");
        assert!(matches!(saved.mode, Mode::Computer));
        assert!(matches!(saved.tool_mode, ToolMode::ReadOnly));
        let saved_usage = saved
            .usage_by_model
            .get(CURRENT_USAGE_BUCKET)
            .expect("saved current usage bucket");
        assert_eq!(saved_usage.tool_input_tokens, 12);
        assert_eq!(saved_usage.tool_output_tokens, 8);
        assert_eq!(saved_usage.total_tokens, 20);
        assert_eq!(saved_usage.tool_call_count, 3);

        let reloaded = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("reload app state");
        assert_eq!(reloaded.all_time_usage_totals().total_tokens, 20);
        assert_eq!(reloaded.session_usage_totals, UsageTotals::default());

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir(workspace);
    }

    #[test]
    fn app_config_round_trips_ngrok_authtoken() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-config-token-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp config dir");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);

        let config = AppConfig {
            ngrok_authtoken: Some("test-token-123".into()),
            ..AppConfig::default()
        };
        config.save_to_path(&config_path).expect("save config");

        let saved = AppConfig::load_from_path(&config_path).expect("load config");
        assert_eq!(saved.ngrok_authtoken.as_deref(), Some("test-token-123"));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir(workspace);
    }

    #[test]
    fn app_config_round_trips_agents_path_mode() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-config-agents-mode-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temp config dir");
        let config_path = workspace.join(APP_CONFIG_FILE_NAME);

        let config = AppConfig {
            agents_path_mode: AgentsPathMode::Codex,
            ..AppConfig::default()
        };
        config.save_to_path(&config_path).expect("save config");

        let saved = AppConfig::load_from_path(&config_path).expect("load config");
        assert!(matches!(saved.agents_path_mode, AgentsPathMode::Codex));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir(workspace);
    }

    #[test]
    fn legacy_workspace_migration_preserves_slug_root_and_is_idempotent() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-legacy-root-{unique}"));
        let other_workspace =
            std::env::temp_dir().join(format!("moondesk-legacy-other-root-{unique}"));
        let config_root = std::env::temp_dir().join(format!("moondesk-legacy-config-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create legacy workspace");
        std::fs::create_dir_all(&other_workspace).expect("create alternate workspace");
        std::fs::create_dir_all(&config_root).expect("create config root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let legacy_slug = "Ab3kL9xQ2pTm7VhC";
        std::fs::write(
            &config_path,
            format!(
                r#"mcpSlug = "{legacy_slug}"
ngrokDomain = "example.ngrok-free.dev"
theme = "neon"
mode = "both"
toolMode = "multiTools"
"#
            ),
        )
        .expect("write legacy config");

        let first = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("migrate legacy config");
        let expected_root = workspaces::canonicalize_existing_workspace_root(&workspace)
            .expect("canonicalize legacy root");
        assert_eq!(first.mcp_slug, legacy_slug);
        assert_eq!(first.mcp_path(), format!("/{legacy_slug}/mcp"));
        assert_eq!(
            first.public_mcp_url().as_deref(),
            Some("https://example.ngrok-free.dev/Ab3kL9xQ2pTm7VhC/mcp")
        );
        assert_eq!(first.workspace_root, expected_root.to_string_lossy());
        assert!(first.is_returning_user);
        assert_eq!(first.workspaces.len(), 1);
        let first_workspace_id = first.workspaces[0].id.clone();

        let saved = AppConfig::load_from_path(&config_path).expect("load migrated config");
        assert_eq!(saved.config_version, CURRENT_CONFIG_VERSION);
        assert!(saved.mcp_slug.is_none());
        assert_eq!(saved.workspaces.len(), 1);
        assert_eq!(saved.workspaces[0].id, first_workspace_id);
        assert_eq!(saved.workspaces[0].mcp_slug, legacy_slug);
        assert_eq!(saved.workspaces[0].root, expected_root);

        let reloaded = AppState::from_config_path(
            8787,
            other_workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("reload migrated config");
        assert_eq!(reloaded.mcp_slug, legacy_slug);
        assert_eq!(
            reloaded.public_mcp_url().as_deref(),
            first.public_mcp_url().as_deref()
        );
        assert_eq!(reloaded.workspaces[0].id, first_workspace_id);
        assert_eq!(reloaded.workspace_root, first.workspace_root);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(other_workspace);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn legacy_workspace_without_domain_keeps_secret_when_domain_is_added() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("moondesk-legacy-no-domain-root-{unique}"));
        let alternate_launch_root =
            std::env::temp_dir().join(format!("moondesk-legacy-no-domain-other-{unique}"));
        let config_root =
            std::env::temp_dir().join(format!("moondesk-legacy-no-domain-config-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create legacy workspace");
        std::fs::create_dir_all(&alternate_launch_root).expect("create alternate launch root");
        std::fs::create_dir_all(&config_root).expect("create config root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let legacy_slug = "LegacyStableSlug1";
        std::fs::write(
            &config_path,
            format!(
                r#"mcpSlug = "{legacy_slug}"
theme = "neon"
mode = "both"
toolMode = "multiTools"
"#
            ),
        )
        .expect("write legacy config without domain");

        let mut migrated = AppState::from_config_path(
            8787,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("migrate legacy config without domain");
        assert_eq!(migrated.mcp_slug, legacy_slug);
        assert!(migrated.ngrok_domain.is_none());
        assert!(migrated.public_mcp_url().is_none());

        migrated.set_ngrok_domain(Some("stable-after-migration.ngrok-free.app".into()));
        migrated.persist_state().expect("persist static domain");

        let reloaded = AppState::from_config_path(
            8787,
            alternate_launch_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("reload migrated config with domain");
        assert_eq!(reloaded.mcp_slug, legacy_slug);
        assert_eq!(
            reloaded.public_mcp_url().as_deref(),
            Some("https://stable-after-migration.ngrok-free.app/LegacyStableSlug1/mcp")
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(alternate_launch_root);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(unix)]
    #[test]
    fn config_v2_loads_noncanonical_existing_root_as_unavailable() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let config_root = std::env::temp_dir().join(format!("moondesk-v2-alias-config-{unique}"));
        let target = std::env::temp_dir().join(format!("moondesk-v2-alias-target-{unique}"));
        let alias = std::env::temp_dir().join(format!("moondesk-v2-alias-root-{unique}"));
        std::fs::create_dir_all(&config_root).expect("create config root");
        std::fs::create_dir_all(&target).expect("create target root");
        symlink(&target, &alias).expect("create root alias");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let config = AppConfig {
            config_version: CURRENT_CONFIG_VERSION,
            workspaces: vec![WorkspaceConfig {
                id: workspaces::WorkspaceId::new(),
                name: "Alias".into(),
                root: alias.clone(),
                mcp_slug: "Ab3kL9xQ2pTm7VhC".into(),
            }],
            ..AppConfig::default()
        };
        std::fs::write(
            &config_path,
            toml::to_string_pretty(&config).expect("serialize alias config"),
        )
        .expect("write alias config");

        let loaded = AppConfig::load_from_path(&config_path)
            .expect("noncanonical root must not prevent config loading");
        assert_eq!(loaded.workspaces[0].root, alias);
        assert_eq!(
            workspaces::workspace_availability(&loaded.workspaces[0].root),
            workspaces::WorkspaceAvailability::Unavailable
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_file(&loaded.workspaces[0].root);
        let _ = std::fs::remove_dir_all(target);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn config_v2_rejects_duplicate_workspace_secrets() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let config_root = std::env::temp_dir().join(format!("moondesk-v2-duplicate-{unique}"));
        std::fs::create_dir_all(&config_root).expect("create config root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let secret = "Ab3kL9xQ2pTm7VhC".to_string();
        let config = AppConfig {
            config_version: CURRENT_CONFIG_VERSION,
            workspaces: vec![
                WorkspaceConfig {
                    id: workspaces::WorkspaceId::new(),
                    name: "One".into(),
                    root: std::env::temp_dir().join(format!("moondesk-v2-one-{unique}")),
                    mcp_slug: secret.clone(),
                },
                WorkspaceConfig {
                    id: workspaces::WorkspaceId::new(),
                    name: "Two".into(),
                    root: std::env::temp_dir().join(format!("moondesk-v2-two-{unique}")),
                    mcp_slug: secret,
                },
            ],
            ..AppConfig::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialize corrupt config");
        std::fs::write(&config_path, text).expect("write corrupt config");

        let error = match AppConfig::load_from_path(&config_path) {
            Ok(_) => panic!("duplicate secrets must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("duplicate workspace MCP slug"));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn config_rejects_unsupported_future_version() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let config_root = std::env::temp_dir().join(format!("moondesk-config-version-{unique}"));
        std::fs::create_dir_all(&config_root).expect("create config root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        std::fs::write(
            &config_path,
            "configVersion = 999\ntheme = \"neon\"\nmode = \"both\"\ntoolMode = \"multiTools\"\n",
        )
        .expect("write future config");

        let error = match AppConfig::load_from_path(&config_path) {
            Ok(_) => panic!("future config version must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("unsupported MoonDesk config version")
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn config_v2_rejects_legacy_mcp_slug_field() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let config_root = std::env::temp_dir().join(format!("moondesk-v2-legacy-slug-{unique}"));
        let workspace_root = config_root.join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("create workspace root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let workspace = WorkspaceConfig::new(
            "Workspace",
            &workspace_root,
            workspaces::generate_mcp_slug(),
        )
        .expect("create workspace config");
        let config = AppConfig {
            config_version: CURRENT_CONFIG_VERSION,
            mcp_slug: Some("LegacySlug123456".into()),
            workspaces: vec![workspace],
            ..AppConfig::default()
        };
        std::fs::write(
            &config_path,
            toml::to_string_pretty(&config).expect("serialize invalid v2 config"),
        )
        .expect("write invalid v2 config");

        let error = AppConfig::load_from_path(&config_path)
            .err()
            .expect("legacy mcpSlug in config v2 must fail closed");
        assert!(
            error
                .to_string()
                .contains("config v2 must not contain the legacy mcpSlug field")
        );

        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn config_v2_rejects_empty_workspace_registry() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let config_root = std::env::temp_dir().join(format!("moondesk-v2-empty-registry-{unique}"));
        std::fs::create_dir_all(&config_root).expect("create config root");
        let config_path = config_root.join(APP_CONFIG_FILE_NAME);
        let config = AppConfig {
            config_version: CURRENT_CONFIG_VERSION,
            workspaces: Vec::new(),
            ..AppConfig::default()
        };
        std::fs::write(
            &config_path,
            toml::to_string_pretty(&config).expect("serialize empty v2 config"),
        )
        .expect("write empty v2 config");

        let error = AppConfig::load_from_path(&config_path)
            .err()
            .expect("empty config v2 workspace registry must fail closed");
        assert!(
            error
                .to_string()
                .contains("config v2 must contain at least one workspace")
        );

        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn flow_anim_lit_count_interpolates_between_endpoints() {
        let duration_ms = move_segment_duration_ms(
            FlowDirection::Forward,
            derive_flow_step_ms(),
            0,
            FLOW_ANIM_CELLS,
        );
        let seg = FlowAnimSegment {
            kind: FlowAnimKind::Move,
            direction: FlowDirection::Forward,
            started_ms: 100,
            ends_ms: 100 + duration_ms,
            step_ms: derive_flow_step_ms(),
            start_cells: 0,
            end_cells: FLOW_ANIM_CELLS,
        };

        assert_eq!(flow_anim_lit_count(seg, 100), 0);
        assert!(flow_anim_lit_count(seg, 100 + duration_ms / 2) > 0);
        assert!(flow_anim_lit_count(seg, 100 + duration_ms / 2) < FLOW_ANIM_CELLS);
        assert_eq!(flow_anim_lit_count(seg, 100 + duration_ms), FLOW_ANIM_CELLS);
    }

    #[test]
    fn backward_move_uses_longer_duration() {
        let forward = move_segment_duration_ms(
            FlowDirection::Forward,
            derive_flow_step_ms(),
            0,
            FLOW_ANIM_CELLS,
        );
        let backward = move_segment_duration_ms(
            FlowDirection::Backward,
            derive_flow_step_ms(),
            0,
            FLOW_ANIM_CELLS,
        );

        assert_eq!(forward, FLOW_FORWARD_ANIMATION_DURATION_MS as u128);
        assert_eq!(backward, FLOW_BACKWARD_ANIMATION_DURATION_MS as u128);
    }

    #[test]
    fn enqueue_flow_segment_preempts_inflight_move() {
        let mut queue = VecDeque::new();
        let step_ms = derive_flow_step_ms();
        enqueue_flow_segment(&mut queue, FlowDirection::Forward, 0, step_ms);
        assert_eq!(queue.len(), 1);

        enqueue_flow_segment(&mut queue, FlowDirection::Backward, 40, step_ms);
        assert_eq!(queue.len(), 2);
        assert!(matches!(queue[0].kind, FlowAnimKind::Turn));
        assert!(queue[0].direction == FlowDirection::Forward);
        assert!(queue[0].start_cells > 0);
        assert_eq!(queue[0].end_cells, 0);
        assert!(matches!(queue[1].kind, FlowAnimKind::Move));
        assert!(queue[1].direction == FlowDirection::Backward);
        assert_eq!(queue[1].start_cells, 0);
        assert_eq!(queue[1].end_cells, FLOW_ANIM_CELLS);
    }

    #[test]
    fn record_flow_tool_call_does_not_activate_bootstrap_status() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-tool-call");

        app.record_flow(
            "stateless",
            &["tools/call:run_command".to_string()],
            FlowDirection::Forward,
        );

        let flow = app.flows.first().expect("missing flow");
        assert!(!flow.bootstrap_status_active);
        assert_eq!(flow.bootstrap_completed_steps, 0);
        assert!(flow.bootstrap_pending_steps.is_empty());

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_initialize_activates_bootstrap_status() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-initialize");

        app.record_flow(
            "stateless",
            &["initialize".to_string()],
            FlowDirection::Forward,
        );

        let flow = app.flows.first().expect("missing flow");
        assert!(flow.bootstrap_status_active);
        assert_eq!(flow.bootstrap_completed_steps, 0);
        assert_eq!(flow.bootstrap_pending_steps.front(), Some(&0));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_bootstrap_event_keeps_bootstrap_status_active() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-bootstrap-event");

        app.record_flow(
            "stateless",
            &["initialize".to_string()],
            FlowDirection::Forward,
        );
        app.record_flow(
            "stateless",
            &["tools/list".to_string()],
            FlowDirection::Forward,
        );

        let flow = app.flows.first().expect("missing flow");
        assert!(flow.bootstrap_status_active);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_bootstrap_tracks_tool_discovery_sequence() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-bootstrap-tools");

        let sequence = [
            // Phase 1: Checking tools
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("notifications/initialized", FlowDirection::Forward),
            ("tools/list", FlowDirection::Forward),
            ("tools/list", FlowDirection::Backward),
        ];

        for (event, direction) in sequence {
            app.record_flow("stateless", &[event.to_string()], direction);
        }

        let flow = app.flows.first().expect("missing flow");
        assert!(flow.bootstrap_status_active);
        let phase_step_counts: Vec<usize> = FLOW_BOOTSTRAP_PHASES
            .iter()
            .map(|phase| phase.steps.len())
            .collect();
        assert_eq!(phase_step_counts, vec![4]);
        assert_eq!(flow_bootstrap_steps_total(), 4);
        assert_eq!(flow.bootstrap_completed_steps, flow_bootstrap_steps_total());
        assert!(flow.bootstrap_pending_steps.is_empty());

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_bootstrap_ignores_optional_reinitialize_after_tool_discovery() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-bootstrap-reinitialize");

        let sequence = [
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("notifications/initialized", FlowDirection::Forward),
            ("tools/list", FlowDirection::Forward),
            ("tools/list", FlowDirection::Backward),
            ("server/discover", FlowDirection::Forward),
            ("server/discover", FlowDirection::Backward),
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("initialize", FlowDirection::Forward),
            ("initialize", FlowDirection::Backward),
            ("notifications/initialized", FlowDirection::Forward),
        ];

        for (event, direction) in sequence {
            app.record_flow("stateless", &[event.to_string()], direction);
        }

        let flow = app.flows.first().expect("missing flow");
        assert!(flow.bootstrap_status_active);
        assert_eq!(flow.bootstrap_completed_steps, 4);
        assert!(flow.bootstrap_pending_steps.is_empty());

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_tool_call_after_initialize_deactivates_bootstrap_status() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-tool-after-initialize");

        app.record_flow(
            "stateless",
            &["initialize".to_string()],
            FlowDirection::Forward,
        );
        app.record_flow(
            "stateless",
            &["tools/call:moondesk_instruction".to_string()],
            FlowDirection::Forward,
        );

        let flow = app.flows.first().expect("missing flow");
        assert!(!flow.bootstrap_status_active);
        assert!(flow.bootstrap_status_close_deadline_ms.is_none());

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn command_activity_tracks_background_job_without_poll_duplicates() {
        let (mut app, workspace, config_path) = test_app("moondesk-command-activity");
        let workspace_id = app.workspaces[0].id.clone();

        app.apply_server_ui_event(ServerUiEvent::CommandStarted {
            workspace_id: workspace_id.clone(),
            activity_id: "activity-a".into(),
            command: "cargo test".into(),
            background: true,
        });
        app.apply_server_ui_event(ServerUiEvent::CommandBoundToJob {
            workspace_id: workspace_id.clone(),
            activity_id: "activity-a".into(),
            job_id: "job-1".into(),
        });
        app.apply_server_ui_event(ServerUiEvent::CommandUpdated {
            workspace_id: workspace_id.clone(),
            activity_id: None,
            job_id: Some("job-1".into()),
            state: CommandActivityState::Running,
            exit_code: None,
            preview: Some("Compiling moondesk".into()),
        });
        app.apply_server_ui_event(ServerUiEvent::CommandUpdated {
            workspace_id: workspace_id.clone(),
            activity_id: None,
            job_id: Some("job-1".into()),
            state: CommandActivityState::Succeeded,
            exit_code: Some(0),
            preview: Some("109 passed; 0 failed".into()),
        });

        assert_eq!(app.command_activities.len(), 1);
        let activity = app.command_activities.front().expect("command activity");
        assert_eq!(activity.command, "cargo test");
        assert_eq!(activity.job_id.as_deref(), Some("job-1"));
        assert_eq!(activity.state, CommandActivityState::Succeeded);
        assert_eq!(activity.exit_code, Some(0));
        assert_eq!(activity.preview.as_deref(), Some("109 passed; 0 failed"));

        // A retried start_command can return the same deduplicated job. The TUI
        // should still show one actual execution, not a duplicate command row.
        app.apply_server_ui_event(ServerUiEvent::CommandStarted {
            workspace_id: workspace_id.clone(),
            activity_id: "activity-b".into(),
            command: "cargo test".into(),
            background: true,
        });
        app.apply_server_ui_event(ServerUiEvent::CommandBoundToJob {
            workspace_id: workspace_id.clone(),
            activity_id: "activity-b".into(),
            job_id: "job-1".into(),
        });
        assert_eq!(app.command_activities.len(), 1);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn late_workspace_events_are_ignored_after_runtime_removal() {
        let (mut app, workspace, config_path) = test_app("moondesk-late-workspace-events");
        let workspace_id = app.workspaces[0].id.clone();
        let request_count_before = app.request_count;
        app.workspace_runtimes.remove(&workspace_id);
        app.purge_workspace_observability(&workspace_id);

        app.apply_server_ui_event(ServerUiEvent::Log {
            workspace_id: Some(workspace_id.clone()),
            level: "INFO",
            message: "late log".into(),
        });
        app.apply_server_ui_event(ServerUiEvent::CommandStarted {
            workspace_id: workspace_id.clone(),
            activity_id: "late-command".into(),
            command: "echo late".into(),
            background: false,
        });
        app.apply_server_ui_event(ServerUiEvent::RecordFlow {
            workspace_id: workspace_id.clone(),
            flow_id: "late-flow".into(),
            events: vec!["ping".into()],
            direction: FlowDirection::Forward,
        });
        app.apply_server_ui_event(ServerUiEvent::IncrementRequestCount {
            workspace_id: workspace_id.clone(),
        });

        assert_eq!(app.request_count, request_count_before);
        assert!(
            app.logs
                .iter()
                .all(|entry| entry.workspace_id.as_ref() != Some(&workspace_id))
        );
        assert!(
            app.command_activities
                .iter()
                .all(|activity| activity.workspace_id != workspace_id)
        );
        assert!(
            app.flows
                .iter()
                .all(|flow| !flow.flow_id.starts_with(workspace_id.as_str()))
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn command_activity_history_is_bounded() {
        let (mut app, workspace, config_path) = test_app("moondesk-command-history");

        let workspace_id = app.workspaces[0].id.clone();

        for index in 0..(MAX_COMMAND_ACTIVITIES + 20) {
            app.command_started(
                workspace_id.clone(),
                format!("activity-{index}"),
                format!("command-{index}"),
                false,
            );
        }

        assert_eq!(app.command_activities.len(), MAX_COMMAND_ACTIVITIES);
        assert_eq!(
            app.command_activities
                .front()
                .map(|activity| activity.command.as_str()),
            Some("command-20")
        );
        assert_eq!(
            app.command_activities
                .back()
                .map(|activity| activity.command.clone()),
            Some(format!("command-{}", MAX_COMMAND_ACTIVITIES + 19))
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_logs_keep_independent_history_limits() {
        let (mut app, workspace, config_path) = test_app("moondesk-workspace-log-history");
        let workspace_a = app.workspaces[0].id.clone();
        let workspace_b = WorkspaceId::new();

        for index in 0..520 {
            app.log_workspace(workspace_a.clone(), "INFO", format!("a-{index}"));
        }
        for index in 0..510 {
            app.log_workspace(workspace_b.clone(), "INFO", format!("b-{index}"));
        }

        let a = app
            .logs
            .iter()
            .filter(|entry| entry.workspace_id.as_ref() == Some(&workspace_a))
            .collect::<Vec<_>>();
        let b = app
            .logs
            .iter()
            .filter(|entry| entry.workspace_id.as_ref() == Some(&workspace_b))
            .collect::<Vec<_>>();
        assert_eq!(a.len(), 500);
        assert_eq!(b.len(), 500);
        assert_eq!(a.first().map(|entry| entry.message.as_str()), Some("a-20"));
        assert_eq!(b.first().map(|entry| entry.message.as_str()), Some("b-10"));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_command_history_limits_are_independent() {
        let (mut app, workspace, config_path) = test_app("moondesk-workspace-command-history");
        let workspace_a = app.workspaces[0].id.clone();
        let workspace_b = WorkspaceId::new();

        for index in 0..(MAX_COMMAND_ACTIVITIES + 20) {
            app.command_started(
                workspace_a.clone(),
                format!("a-{index}"),
                format!("a-command-{index}"),
                false,
            );
        }
        for index in 0..(MAX_COMMAND_ACTIVITIES + 10) {
            app.command_started(
                workspace_b.clone(),
                format!("b-{index}"),
                format!("b-command-{index}"),
                false,
            );
        }

        assert_eq!(
            app.command_activities
                .iter()
                .filter(|activity| activity.workspace_id == workspace_a)
                .count(),
            MAX_COMMAND_ACTIVITIES
        );
        assert_eq!(
            app.command_activities
                .iter()
                .filter(|activity| activity.workspace_id == workspace_b)
                .count(),
            MAX_COMMAND_ACTIVITIES
        );
        assert!(app.command_activities.iter().any(|activity| {
            activity.workspace_id == workspace_a && activity.command == "a-command-20"
        }));
        assert!(app.command_activities.iter().any(|activity| {
            activity.workspace_id == workspace_b && activity.command == "b-command-10"
        }));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_flows_and_connection_status_are_isolated() {
        let (mut app, workspace, config_path) = test_app("moondesk-workspace-flow-isolation");
        let workspace_a = app.workspaces[0].id.clone();
        let workspace_b = WorkspaceId::new();
        app.workspace_runtimes
            .insert(workspace_b.clone(), Arc::new(WorkspaceRuntime::default()));

        app.apply_server_ui_event(ServerUiEvent::RecordFlow {
            workspace_id: workspace_a.clone(),
            flow_id: "stateless".into(),
            events: vec!["initialize".into()],
            direction: FlowDirection::Forward,
        });
        app.apply_server_ui_event(ServerUiEvent::RecordFlow {
            workspace_id: workspace_b.clone(),
            flow_id: "stateless".into(),
            events: vec!["tools/list".into()],
            direction: FlowDirection::Forward,
        });
        assert_eq!(app.flows.len(), 2);
        assert!(
            app.flows
                .iter()
                .any(|flow| flow.flow_id == format!("{}:stateless", workspace_a))
        );
        assert!(
            app.flows
                .iter()
                .any(|flow| flow.flow_id == format!("{}:stateless", workspace_b))
        );

        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace_a.clone(),
            connected: true,
        });
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace_b.clone(),
            connected: true,
        });
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace_b.clone(),
            connected: false,
        });
        assert!(app.remote_connected);
        assert!(
            app.workspace_runtimes
                .get(&workspace_a)
                .is_some_and(|runtime| runtime.remote_connected())
        );
        assert!(
            app.workspace_runtimes
                .get(&workspace_b)
                .is_some_and(|runtime| !runtime.remote_connected())
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn clear_remote_connection_state_resets_every_workspace() {
        let (mut app, workspace, config_path) = test_app("moondesk-clear-remote-state");
        let workspace_a = app.workspaces[0].id.clone();
        let workspace_b = WorkspaceId::new();
        app.workspace_runtimes
            .insert(workspace_b.clone(), Arc::new(WorkspaceRuntime::default()));

        for workspace_id in [&workspace_a, &workspace_b] {
            let runtime = app
                .workspace_runtimes
                .get(workspace_id)
                .expect("workspace runtime");
            runtime.set_remote_connected(true);
            runtime.mark_remote_activity(99);
        }
        app.remote_connected = true;
        app.last_remote_activity_ms = Some(99);

        app.clear_remote_connection_state();

        assert!(!app.remote_connected);
        assert_eq!(app.last_remote_activity_ms, None);
        for runtime in app.workspace_runtimes.values() {
            assert!(!runtime.remote_connected());
            assert_eq!(runtime.last_remote_activity_ms(), None);
        }

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn record_flow_tool_call_after_close_does_not_reactivate_bootstrap_status() {
        let (mut app, workspace, config_path) = test_app("moondesk-flow-after-close");

        app.record_flow(
            "stateless",
            &["initialize".to_string()],
            FlowDirection::Forward,
        );
        app.begin_flow_close("stateless");
        app.record_flow(
            "stateless",
            &["tools/call:run_command".to_string()],
            FlowDirection::Forward,
        );

        let flow = app.flows.first().expect("missing flow");
        assert!(!flow.bootstrap_status_active);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }
    #[tokio::test]
    async fn workspace_connector_identity_survives_persistence_and_reload() {
        let (mut app, primary_root, config_path) = test_app("moondesk-workspace-url-stability");
        let alternate_launch_root = std::env::temp_dir().join(format!(
            "moondesk-workspace-url-stability-launch-{}",
            Uuid::new_v4()
        ));
        let secondary_root = std::env::temp_dir().join(format!(
            "moondesk-workspace-url-stability-secondary-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&alternate_launch_root).expect("create alternate launch root");
        std::fs::create_dir_all(&secondary_root).expect("create secondary workspace");

        app.set_ngrok_domain(Some("stable-test.ngrok-free.app".into()));
        app.persist_state().expect("persist stable ngrok domain");
        let primary_id = app.workspaces[0].id.clone();
        let primary_slug = app.workspaces[0].mcp_slug.clone();
        let primary_url = app.public_mcp_url().expect("primary connector URL");
        let state = Arc::new(Mutex::new(app));

        let secondary = add_workspace(&state, "Secondary".into(), secondary_root.clone())
            .await
            .expect("add secondary workspace");
        rename_workspace(&state, &secondary.id, "Secondary Renamed".into())
            .await
            .expect("rename secondary workspace");
        let secondary_slug = secondary.mcp_slug.clone();
        let secondary_url = format!("https://stable-test.ngrok-free.app/{secondary_slug}/mcp");
        drop(state);

        let reloaded = AppState::from_config_path(
            8787,
            alternate_launch_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("reload persisted workspace registry");

        assert_eq!(
            reloaded.ngrok_domain.as_deref(),
            Some("stable-test.ngrok-free.app")
        );
        assert_eq!(reloaded.workspaces[0].id, primary_id);
        assert_eq!(reloaded.workspaces[0].mcp_slug, primary_slug);
        assert_eq!(
            reloaded.public_mcp_url().as_deref(),
            Some(primary_url.as_str())
        );
        let reloaded_secondary = reloaded
            .workspaces
            .iter()
            .find(|workspace| workspace.id == secondary.id)
            .expect("reloaded secondary workspace");
        assert_eq!(reloaded_secondary.name, "Secondary Renamed");
        assert_eq!(reloaded_secondary.mcp_slug, secondary_slug);
        assert_eq!(
            format!(
                "https://{}/{}/mcp",
                reloaded.ngrok_domain.as_deref().expect("persisted domain"),
                reloaded_secondary.mcp_slug
            ),
            secondary_url
        );

        let _ = std::fs::remove_dir_all(secondary_root);
        let _ = std::fs::remove_dir_all(alternate_launch_root);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn workspace_lifecycle_mutations_persist_transactionally() {
        let (app, primary_root, config_path) = test_app("moondesk-workspace-lifecycle");
        let state = Arc::new(Mutex::new(app));
        let secondary_root =
            std::env::temp_dir().join(format!("moondesk-workspace-secondary-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&secondary_root).expect("create secondary workspace");

        let added = add_workspace(&state, "Secondary".into(), secondary_root.clone())
            .await
            .expect("add workspace");
        assert_eq!(state.lock().await.workspaces.len(), 2);

        rename_workspace(&state, &added.id, "Renamed".into())
            .await
            .expect("rename workspace");
        let old_slug = added.mcp_slug.clone();
        let new_slug = rotate_workspace_secret(&state, &added.id)
            .await
            .expect("rotate workspace secret");
        assert_ne!(old_slug, new_slug);

        let saved = AppConfig::load_from_path(&config_path).expect("load persisted registry");
        let persisted = saved
            .workspaces
            .iter()
            .find(|workspace| workspace.id == added.id)
            .expect("persisted workspace");
        assert_eq!(persisted.name, "Renamed");
        assert_eq!(persisted.mcp_slug, new_slug);
        assert!(
            !saved
                .workspaces
                .iter()
                .any(|workspace| workspace.mcp_slug == old_slug)
        );

        let _ = std::fs::remove_dir_all(secondary_root);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn concurrent_workspace_additions_do_not_overwrite_each_other() {
        let (app, primary_root, config_path) = test_app("moondesk-workspace-concurrent-add");
        let state = Arc::new(Mutex::new(app));
        let root_a = std::env::temp_dir().join(format!(
            "moondesk-workspace-concurrent-a-{}",
            Uuid::new_v4()
        ));
        let root_b = std::env::temp_dir().join(format!(
            "moondesk-workspace-concurrent-b-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root_a).expect("create workspace A");
        std::fs::create_dir_all(&root_b).expect("create workspace B");

        let (added_a, added_b) = tokio::join!(
            add_workspace(&state, "Workspace A".into(), root_a.clone()),
            add_workspace(&state, "Workspace B".into(), root_b.clone())
        );
        let added_a = added_a.expect("add workspace A");
        let added_b = added_b.expect("add workspace B");
        assert_ne!(added_a.id, added_b.id);

        let app = state.lock().await;
        assert_eq!(app.workspaces.len(), 3);
        assert!(
            app.workspaces
                .iter()
                .any(|workspace| workspace.id == added_a.id)
        );
        assert!(
            app.workspaces
                .iter()
                .any(|workspace| workspace.id == added_b.id)
        );
        drop(app);

        let saved = AppConfig::load_from_path(&config_path).expect("load concurrent registry");
        assert_eq!(saved.workspaces.len(), 3);
        assert!(
            saved
                .workspaces
                .iter()
                .any(|workspace| workspace.id == added_a.id)
        );
        assert!(
            saved
                .workspaces
                .iter()
                .any(|workspace| workspace.id == added_b.id)
        );

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn failed_workspace_persistence_does_not_publish_registry_changes() {
        let (mut app, primary_root, original_config_path) =
            test_app("moondesk-workspace-persist-failure");
        let blocked_parent = primary_root.join("blocked-config-parent");
        std::fs::write(&blocked_parent, "not a directory").expect("create blocked config parent");
        app.config_path = blocked_parent.join(APP_CONFIG_FILE_NAME);
        let original_workspace_id = app.workspaces[0].id.clone();
        let state = Arc::new(Mutex::new(app));
        let secondary_root = std::env::temp_dir().join(format!(
            "moondesk-workspace-persist-failure-secondary-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&secondary_root).expect("create secondary workspace");

        let error = add_workspace(&state, "Secondary".into(), secondary_root.clone())
            .await
            .expect_err("failed registry persistence must reject workspace addition");
        assert!(matches!(error, AddWorkspaceError::Persistence(_)));
        assert!(
            error
                .to_string()
                .contains("failed to persist workspace registry")
        );

        let app = state.lock().await;
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.workspaces[0].id, original_workspace_id);
        assert_eq!(app.workspace_runtimes.len(), 1);
        assert!(
            !app.workspaces
                .iter()
                .any(|workspace| workspace.root == secondary_root)
        );
        drop(app);

        let _ = std::fs::remove_dir_all(secondary_root);
        let _ = std::fs::remove_file(original_config_path);
        let _ = std::fs::remove_file(blocked_parent);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn workspace_removal_revokes_cancels_drains_and_purges() {
        let (app, primary_root, config_path) = test_app("moondesk-workspace-remove");
        let state = Arc::new(Mutex::new(app));
        let secondary_root = std::env::temp_dir().join(format!(
            "moondesk-workspace-remove-secondary-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&secondary_root).expect("create secondary workspace");
        let added = add_workspace(&state, "Secondary".into(), secondary_root.clone())
            .await
            .expect("add workspace");

        {
            let mut app = state.lock().await;
            let primary_id = app.workspaces[0].id.clone();
            app.log_workspace(primary_id.clone(), "INFO", "primary survives".into());
            app.log_workspace(added.id.clone(), "INFO", "secondary removed".into());
            app.apply_server_ui_event(ServerUiEvent::CommandStarted {
                workspace_id: added.id.clone(),
                activity_id: "secondary-command".into(),
                command: "echo secondary".into(),
                background: false,
            });
            app.apply_server_ui_event(ServerUiEvent::RecordFlow {
                workspace_id: added.id.clone(),
                flow_id: "stateless".into(),
                events: vec!["initialize".into()],
                direction: FlowDirection::Forward,
            });
            app.apply_server_ui_event(ServerUiEvent::IncrementRequestCount {
                workspace_id: added.id.clone(),
            });
            app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
                workspace_id: added.id.clone(),
                connected: true,
            });
            assert!(app.remote_connected);
            assert!(app.last_remote_activity_ms.is_some());
        }

        let (runtime, manager) = {
            let app = state.lock().await;
            (
                app.workspace_runtimes
                    .get(&added.id)
                    .cloned()
                    .expect("workspace runtime"),
                app.command_jobs.clone(),
            )
        };
        let lease = runtime
            .try_acquire()
            .expect("acquire in-flight request lease");
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let started = manager
            .start_for_workspace(
                &added.id,
                command.to_string(),
                secondary_root.clone(),
                10_000,
                None,
            )
            .await
            .expect("start workspace job");

        let mut removal = Box::pin(remove_workspace(&state, &added.id));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut removal)
                .await
                .is_err(),
            "removal must wait for the existing request lease"
        );
        assert!(!runtime.accepting_requests());
        assert!(runtime.try_acquire().is_none());
        assert_eq!(runtime.in_flight_requests(), 1);
        let still_running = manager
            .poll_for_workspace(&added.id, &started.snapshot.job_id, 0, 0)
            .await
            .expect("existing job remains visible while removal is reversible");
        assert_eq!(
            still_running.state,
            crate::command_jobs::CommandJobState::Running
        );
        let late_start_error = manager
            .start_for_workspace(
                &added.id,
                command.to_string(),
                secondary_root.clone(),
                10_000,
                None,
            )
            .await
            .expect_err("pre-revocation request must not start a new job during removal");
        assert!(late_start_error.contains("workspace removal is in progress"));

        drop(lease);
        removal.await.expect("remove workspace after drain");

        {
            let app = state.lock().await;
            assert_eq!(app.workspaces.len(), 1);
            assert!(
                !app.workspaces
                    .iter()
                    .any(|workspace| workspace.id == added.id)
            );
            assert!(!app.workspace_runtimes.contains_key(&added.id));
            assert!(
                app.logs
                    .iter()
                    .all(|entry| entry.workspace_id.as_ref() != Some(&added.id)),
                "removed workspace logs must be purged"
            );
            assert!(
                app.logs
                    .iter()
                    .any(|entry| entry.message == "primary survives"),
                "other workspace logs must be preserved"
            );
            assert!(
                app.command_activities
                    .iter()
                    .all(|activity| activity.workspace_id != added.id),
                "removed workspace command history must be purged"
            );
            let removed_flow_prefix = format!("{}:", added.id.as_str());
            assert!(
                app.flows
                    .iter()
                    .all(|flow| !flow.flow_id.starts_with(&removed_flow_prefix)),
                "removed workspace flows must be purged"
            );
            assert!(
                app.flow_bootstrap_progress
                    .keys()
                    .all(|flow_id| !flow_id.starts_with(&removed_flow_prefix)),
                "removed workspace bootstrap state must be purged"
            );
            assert!(!app.remote_connected);
            assert_eq!(app.last_remote_activity_ms, None);
        }
        assert!(
            manager
                .poll_for_workspace(&added.id, &started.snapshot.job_id, 0, 0)
                .await
                .is_err(),
            "removed workspace retained jobs must be purged"
        );
        let saved = AppConfig::load_from_path(&config_path).expect("load registry after remove");
        assert_eq!(saved.workspaces.len(), 1);
        assert!(
            !saved
                .workspaces
                .iter()
                .any(|workspace| workspace.id == added.id)
        );

        let _ = std::fs::remove_dir_all(secondary_root);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn failed_workspace_removal_persistence_keeps_running_jobs_and_reenables_workspace() {
        let (app, primary_root, original_config_path) =
            test_app("moondesk-workspace-remove-persist-failure");
        let state = Arc::new(Mutex::new(app));
        let secondary_root = std::env::temp_dir().join(format!(
            "moondesk-workspace-remove-persist-failure-secondary-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&secondary_root).expect("create secondary workspace");
        let added = add_workspace(&state, "Secondary".into(), secondary_root.clone())
            .await
            .expect("add workspace");

        let blocked_parent = primary_root.join("blocked-remove-config-parent");
        std::fs::write(&blocked_parent, "not a directory").expect("create blocked config parent");
        {
            let mut app = state.lock().await;
            app.config_path = blocked_parent.join(APP_CONFIG_FILE_NAME);
        }

        let (runtime, manager) = {
            let app = state.lock().await;
            (
                app.workspace_runtimes
                    .get(&added.id)
                    .cloned()
                    .expect("workspace runtime"),
                app.command_jobs.clone(),
            )
        };
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let started = manager
            .start_for_workspace(
                &added.id,
                command.to_string(),
                secondary_root.clone(),
                10_000,
                None,
            )
            .await
            .expect("start workspace job");

        let error = remove_workspace(&state, &added.id)
            .await
            .expect_err("removal persistence must fail");
        assert!(error.contains("workspace removal was not persisted"));
        assert!(runtime.accepting_requests());
        assert!(runtime.try_acquire().is_some());
        let snapshot = manager
            .poll_for_workspace(&added.id, &started.snapshot.job_id, 0, 0)
            .await
            .expect("running job must survive failed removal");
        assert_eq!(
            snapshot.state,
            crate::command_jobs::CommandJobState::Running,
            "failed removal must not cancel an existing background job"
        );
        assert!(
            state
                .lock()
                .await
                .workspaces
                .iter()
                .any(|workspace| workspace.id == added.id),
            "failed removal must keep the workspace registered"
        );

        manager
            .cancel_workspace(&added.id)
            .await
            .expect("cancel surviving test job");
        let _ = std::fs::remove_file(original_config_path);
        let _ = std::fs::remove_file(blocked_parent);
        let _ = std::fs::remove_dir_all(secondary_root);
        let _ = std::fs::remove_dir_all(primary_root);
    }

    #[tokio::test]
    async fn final_workspace_cannot_be_removed() {
        let (app, root, config_path) = test_app("moondesk-final-workspace-remove");
        let workspace_id = app.workspaces[0].id.clone();
        let state = Arc::new(Mutex::new(app));
        assert_eq!(
            remove_workspace(&state, &workspace_id)
                .await
                .expect_err("final workspace removal must fail"),
            "cannot remove the final workspace"
        );
        assert!(state.lock().await.workspace_runtimes[&workspace_id].accepting_requests());
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(root);
    }
}
