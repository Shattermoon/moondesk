mod browser;
mod browser_runtime;
mod clippymoon_gen;
mod command;
mod command_jobs;

mod macos_terminal;
mod mascot;
mod mcp;
mod ngrok;
mod process_runner;
mod server;
mod state;
mod theme;
mod update;
mod vision;
mod workspace_tools;
mod workspaces;

use browser_runtime::BrowserRuntime;
use crossterm::{
    ExecutableCommand,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mascot::{TUI_MASCOT_BLOCK_HEIGHT, TUI_MASCOT_BLOCK_WIDTH, render_tui_lines};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use state::{
    AppState, CommandActivityState, FLOW_ANIM_CELLS, FLOW_BOOTSTRAP_PHASES, FlowAnimKind,
    FlowAnimSegment, FlowDirection, FlowLane, Mode, SharedState, ToolMode, UiEventReceiver,
    UiEventSender, UsageTotals, add_workspace, app_config_path, flow_anim_lit_count, flush_config,
    normalize_ngrok_domain, remove_workspace, rename_workspace, rotate_workspace_secret,
    ui_event_channel, user_home_dir,
};
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use uuid::Uuid;
use workspaces::{WorkspaceAvailability, WorkspaceConfig, WorkspaceId, workspace_availability};

const FLOW_ROW_CELLS: usize = FLOW_ANIM_CELLS;
const FLOW_LANE_LEFT_LABEL: &str = "Your computer ";
const REMOTE_CONNECT_UI_GRACE_MS: u128 = 8_000;
const UI_POLL_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
const CONFIG_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const UPDATE_STATE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const UPDATE_CONFIRM_SESSION_WARNING: &str =
    "Make sure no ChatGPT/MCP session or command is currently running.";
const UPDATE_CONFIRM_CONNECTION_WARNING: &str =
    "Updating restarts MoonDesk, so the current connection will be lost.";
const MCP_URL_REVEAL_DURATION: Duration = Duration::from_secs(10);
const MCP_URL_MASK: &str = "https://▓▓▓▓▓▓▓▓/▓▓▓▓▓▓▓▓/mcp";
const NGROK_URL_MASK: &str = "https://▓▓▓▓▓▓▓▓";
const NGROK_DOMAIN_MASK: &str = "▓▓▓▓▓▓▓▓";
const MCP_URL_REVEAL_BAR_CELLS: usize = 10;
// Reserve one extra row for Version without removing the normal live-flow slot.
const STATUS_PANEL_HEIGHT: u16 = TUI_MASCOT_BLOCK_HEIGHT + 5;
const STATUS_LABEL_WIDTH: usize = 10;
const DASHBOARD_THREE_COLUMN_MIN_WIDTH: u16 = 120;
const DASHBOARD_WORKSPACE_COLUMN_WIDTH: u16 = 32;
const STATUS_PRIMARY_MCP_URL_LINE: usize = 6;
const STATUS_WORKSPACES_INSERT_INDEX: usize = STATUS_PRIMARY_MCP_URL_LINE + 1;
// Status is a single live-activity lane; workspace filtering chooses its scope.
const STATUS_VISIBLE_FLOW_ROWS: usize = 1;
const CONNECT_GUIDE_PRIMARY_MCP_URL_LINE: usize = 7;
// GPT-5.6 Sol promotional rates current as of 2026-09-03. OpenAI advertises the
// promotion through at least 2026-11-21. MoonDesk only sees MCP traffic, not
// ChatGPT's model/cache billing metadata, so these power an equivalent-cost estimate.
const GPT_5_6_SOL_INPUT_USD_PER_1M: f64 = 4.0;
const GPT_5_6_SOL_CACHED_INPUT_USD_PER_1M: f64 = 0.4;
const GPT_5_6_SOL_CACHE_WRITE_USD_PER_1M: f64 = GPT_5_6_SOL_INPUT_USD_PER_1M * 1.25;
const GPT_5_6_SOL_OUTPUT_USD_PER_1M: f64 = 20.0;
const PRICE_DISPLAY_DECIMALS: usize = 2;
const NGROK_SETUP_URL: &str = "https://dashboard.ngrok.com/get-started/setup";
#[cfg(target_os = "windows")]
const WORKSPACE_BROWSE_ACTION_LABEL: &str = "[b] Explorer ";
#[cfg(not(target_os = "windows"))]
const WORKSPACE_BROWSE_ACTION_LABEL: &str = "[b] Browse ";

#[derive(Clone, Default)]
struct InterruptState {
    pending: Arc<AtomicUsize>,
    shutdown_started: Arc<AtomicBool>,
}

impl InterruptState {
    fn request(&self) {
        self.pending.fetch_add(1, Ordering::Release);
    }

    fn take_pending(&self) -> usize {
        self.pending.swap(0, Ordering::AcqRel)
    }

    fn begin_shutdown(&self) {
        self.shutdown_started.store(true, Ordering::Release);
    }

    fn shutdown_started(&self) -> bool {
        self.shutdown_started.load(Ordering::Acquire)
    }
}

struct TerminalRestoreGuard {
    raw_mode: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}

impl TerminalRestoreGuard {
    fn enter() -> std::io::Result<Self> {
        let mut guard = Self {
            raw_mode: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
        };
        enable_raw_mode()?;
        guard.raw_mode = true;
        stdout().execute(EnterAlternateScreen)?;
        guard.alternate_screen = true;
        stdout().execute(EnableBracketedPaste)?;
        guard.bracketed_paste = true;
        stdout().execute(EnableMouseCapture)?;
        guard.mouse_capture = true;
        Ok(guard)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        let mut first_error = None;
        if self.bracketed_paste {
            if let Err(error) = stdout().execute(DisableBracketedPaste) {
                first_error.get_or_insert(error);
            }
            self.bracketed_paste = false;
        }
        if self.mouse_capture {
            if let Err(error) = stdout().execute(DisableMouseCapture) {
                first_error.get_or_insert(error);
            }
            self.mouse_capture = false;
        }
        if self.raw_mode {
            if let Err(error) = disable_raw_mode() {
                first_error.get_or_insert(error);
            }
            self.raw_mode = false;
        }
        if self.alternate_screen {
            if let Err(error) = stdout().execute(LeaveAlternateScreen) {
                first_error.get_or_insert(error);
            }
            self.alternate_screen = false;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn spawn_interrupt_listener(interrupts: InterruptState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if interrupts.shutdown_started() {
                eprintln!("\nMoonDesk shutdown interrupted. Forcing exit.");
                std::process::exit(130);
            }
            interrupts.request();
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppExit {
    Quit,
    UpdateRestart(String),
}

// ── Selection ───────────────────────────────────────────────

#[derive(Clone, Debug)]
struct DashboardWorkspaceRow {
    id: WorkspaceId,
    name: String,
    connected: bool,
}

#[derive(Clone)]
struct UiSnapshot {
    theme: String,
    mode: Mode,
    tool_mode: ToolMode,
    public_mcp_url: Option<String>,
    ngrok_domain: Option<String>,
    ngrok_url: Option<String>,
    is_returning_user: bool,
    server_running: bool,
    ngrok_running: bool,
    remote_connected: bool,
    last_remote_activity_ms: Option<u128>,
    browser_runtime_running: bool,
    port: u16,
    workspace_count: usize,
    connected_workspace_count: usize,
    workspaces: Vec<DashboardWorkspaceRow>,
    workspace_names: std::collections::HashMap<WorkspaceId, String>,
    mascot: mascot::MascotPack,
    logs: Vec<state::LogEntry>,
    command_activities: std::collections::VecDeque<state::CommandActivity>,
    flows: Vec<FlowLane>,
    request_count: u64,
    usage_by_model: std::collections::BTreeMap<String, UsageTotals>,
    session_usage_totals: UsageTotals,
}

impl UiSnapshot {
    fn from_app(app: &AppState) -> Self {
        Self {
            theme: app.theme.clone(),
            mode: app.mode,
            tool_mode: app.tool_mode,
            public_mcp_url: app.public_mcp_url(),
            ngrok_domain: app.ngrok_domain.clone(),
            ngrok_url: app.ngrok_url.clone(),
            is_returning_user: app.is_returning_user,
            server_running: app.server_running,
            ngrok_running: app.ngrok_running,
            remote_connected: app.remote_connected,
            last_remote_activity_ms: app.last_remote_activity_ms,
            browser_runtime_running: app.browser_runtime_running,
            port: app.port,
            workspace_count: app.workspaces.len(),
            connected_workspace_count: app
                .workspace_runtimes
                .values()
                .filter(|runtime| runtime.remote_connected())
                .count(),
            workspaces: app
                .workspaces
                .iter()
                .map(|workspace| DashboardWorkspaceRow {
                    id: workspace.id.clone(),
                    name: workspace.name.clone(),
                    connected: app
                        .workspace_runtimes
                        .get(&workspace.id)
                        .is_some_and(|runtime| runtime.remote_connected()),
                })
                .collect(),
            workspace_names: app
                .workspaces
                .iter()
                .map(|workspace| (workspace.id.clone(), workspace.name.clone()))
                .collect(),
            mascot: app.mascot.clone(),
            logs: app.logs.clone(),
            command_activities: app.command_activities.clone(),
            flows: app.flows.clone(),
            request_count: app.request_count,
            usage_by_model: app.usage_by_model.clone(),
            session_usage_totals: app.session_usage_totals.clone(),
        }
    }

    fn current_theme(&self) -> &'static theme::ThemeDef {
        theme::resolve(&self.theme)
    }

    fn public_mcp_url(&self) -> Option<String> {
        self.public_mcp_url.clone()
    }

    fn all_time_usage_totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for usage in self.usage_by_model.values() {
            totals.merge(usage);
        }
        totals
    }
}

struct Selection {
    start: Option<(u16, u16)>,
    end: Option<(u16, u16)>,
    dragging: bool,
}

impl Selection {
    fn new() -> Self {
        Self {
            start: None,
            end: None,
            dragging: false,
        }
    }
    fn clear(&mut self) {
        self.start = None;
        self.end = None;
        self.dragging = false;
    }
    fn range(&self) -> Option<((u16, u16), (u16, u16))> {
        match (self.start, self.end) {
            (Some(s), Some(e)) => {
                let (r0, c0, r1, c1) = if (s.1, s.0) <= (e.1, e.0) {
                    (s.1, s.0, e.1, e.0)
                } else {
                    (e.1, e.0, s.1, s.0)
                };
                Some(((c0, r0), (c1, r1)))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashboardFocus {
    Workspaces,
    Logs,
    ShellCommands,
}

impl DashboardFocus {
    fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "Workspaces",
            Self::Logs => "Logs",
            Self::ShellCommands => "Shell Commands",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
enum WorkspaceFilter {
    #[default]
    All,
    Workspace(WorkspaceId),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservabilityCutoff {
    logs: u64,
    commands: u64,
}

fn workspace_filter_index(filter: &WorkspaceFilter, workspaces: &[DashboardWorkspaceRow]) -> usize {
    match filter {
        WorkspaceFilter::All => 0,
        WorkspaceFilter::Workspace(workspace_id) => workspaces
            .iter()
            .position(|workspace| &workspace.id == workspace_id)
            .map(|index| index + 1)
            .unwrap_or(0),
    }
}

fn workspace_filter_from_index(
    index: usize,
    workspaces: &[DashboardWorkspaceRow],
) -> WorkspaceFilter {
    if index == 0 {
        WorkspaceFilter::All
    } else {
        workspaces
            .get(index - 1)
            .map(|workspace| WorkspaceFilter::Workspace(workspace.id.clone()))
            .unwrap_or(WorkspaceFilter::All)
    }
}

fn workspace_filter_exists(filter: &WorkspaceFilter, workspaces: &[DashboardWorkspaceRow]) -> bool {
    match filter {
        WorkspaceFilter::All => true,
        WorkspaceFilter::Workspace(workspace_id) => workspaces
            .iter()
            .any(|workspace| &workspace.id == workspace_id),
    }
}

fn reconcile_workspace_filter(
    filter: &mut WorkspaceFilter,
    workspaces: &[DashboardWorkspaceRow],
) -> bool {
    if workspace_filter_exists(filter, workspaces) {
        false
    } else {
        *filter = WorkspaceFilter::All;
        true
    }
}

fn apply_workspace_observability_filter(
    app: &mut UiSnapshot,
    filter: &WorkspaceFilter,
    cutoffs: &std::collections::HashMap<WorkspaceFilter, ObservabilityCutoff>,
) {
    let cutoff = cutoffs.get(filter).copied().unwrap_or_default();
    app.logs.retain(|entry| {
        let in_scope = match filter {
            WorkspaceFilter::All => true,
            WorkspaceFilter::Workspace(workspace_id) => {
                entry.workspace_id.as_ref() == Some(workspace_id)
            }
        };
        in_scope && entry.sequence > cutoff.logs
    });
    app.command_activities.retain(|activity| {
        let in_scope = match filter {
            WorkspaceFilter::All => true,
            WorkspaceFilter::Workspace(workspace_id) => &activity.workspace_id == workspace_id,
        };
        in_scope && activity.sequence > cutoff.commands
    });
    app.flows.retain(|flow| match filter {
        WorkspaceFilter::All => true,
        WorkspaceFilter::Workspace(workspace_id) => &flow.workspace_id == workspace_id,
    });
}

fn record_clear_view(
    app: &UiSnapshot,
    filter: &WorkspaceFilter,
    cutoffs: &mut std::collections::HashMap<WorkspaceFilter, ObservabilityCutoff>,
) {
    let cutoff = cutoffs.entry(filter.clone()).or_default();
    if let Some(sequence) = app.logs.iter().map(|entry| entry.sequence).max() {
        cutoff.logs = cutoff.logs.max(sequence);
    }
    if let Some(sequence) = app
        .command_activities
        .iter()
        .map(|activity| activity.sequence)
        .max()
    {
        cutoff.commands = cutoff.commands.max(sequence);
    }
}

fn cycle_dashboard_focus(
    current: DashboardFocus,
    workspace_available: bool,
    command_available: bool,
    reverse: bool,
) -> DashboardFocus {
    let mut available = Vec::with_capacity(3);
    if workspace_available {
        available.push(DashboardFocus::Workspaces);
    }
    available.push(DashboardFocus::Logs);
    if command_available {
        available.push(DashboardFocus::ShellCommands);
    }
    let current_index = available
        .iter()
        .position(|focus| *focus == current)
        .unwrap_or(0);
    let next_index = if reverse {
        current_index.checked_sub(1).unwrap_or(available.len() - 1)
    } else {
        (current_index + 1) % available.len()
    };
    available[next_index]
}

#[derive(Clone, Copy, Default)]
struct PanelScrollView {
    max_scroll: usize,
    effective_scroll: usize,
}

#[derive(Clone, Copy, Default)]
struct BottomPanelAreas {
    logs: Rect,
    shell_commands: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelItemHit {
    top: u16,
    bottom: u16,
    index: usize,
}

#[derive(Default)]
struct BottomPanelHitMaps {
    logs: Vec<PanelItemHit>,
    shell_commands: Vec<PanelItemHit>,
}

#[derive(Default)]
struct DashboardHitAreas {
    secrets: Vec<DashboardSecretHit>,
    workspaces: Option<Rect>,
    workspace_rows: Vec<PanelItemHit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashboardSecretTarget {
    PrimaryMcpUrl,
    LogMcpUrl,
    LogNgrokUrl,
    LogNgrokDomain,
}

impl DashboardSecretTarget {
    fn reveal_message(self) -> &'static str {
        match self {
            Self::LogNgrokDomain => "Domain revealed for 10s",
            Self::PrimaryMcpUrl | Self::LogMcpUrl | Self::LogNgrokUrl => "URL revealed for 10s",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DashboardSecretHit {
    target: DashboardSecretTarget,
    area: Rect,
}

fn dashboard_secret_target_at(
    hit_areas: &DashboardHitAreas,
    column: u16,
    row: u16,
) -> Option<DashboardSecretTarget> {
    hit_areas
        .secrets
        .iter()
        .find(|hit| rect_contains(hit.area, column, row))
        .map(|hit| hit.target)
}

fn log_secret_target(message: &str) -> Option<DashboardSecretTarget> {
    if message.starts_with("MCP Server URL: ") {
        Some(DashboardSecretTarget::LogMcpUrl)
    } else if message.starts_with("ngrok URL: ") {
        Some(DashboardSecretTarget::LogNgrokUrl)
    } else if message.starts_with("Auto-saved ngrok static domain: ") {
        Some(DashboardSecretTarget::LogNgrokDomain)
    } else {
        None
    }
}

fn item_under_cursor(hits: &[PanelItemHit], row: u16) -> Option<usize> {
    hits.iter()
        .find(|hit| row >= hit.top && row <= hit.bottom)
        .map(|hit| hit.index)
}

fn sync_panel_selection(selected: &mut Option<usize>, item_count: usize, follow_tail: bool) {
    if item_count == 0 {
        *selected = None;
    } else if follow_tail {
        *selected = Some(item_count - 1);
    } else {
        *selected = Some(selected.unwrap_or(item_count - 1).min(item_count - 1));
    }
}

fn move_panel_selection(selected: &mut Option<usize>, item_count: usize, delta: isize) {
    if item_count == 0 {
        *selected = None;
        return;
    }
    let current = selected.unwrap_or(item_count - 1).min(item_count - 1);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(item_count - 1)
    };
    *selected = Some(next);
}

fn tail_start_index(item_heights: &[usize], visible_lines: usize) -> usize {
    if item_heights.is_empty() {
        return 0;
    }
    let visible_lines = visible_lines.max(1);
    let mut used = 0usize;
    let mut start = item_heights.len() - 1;
    for index in (0..item_heights.len()).rev() {
        let height = item_heights[index].max(1);
        if used > 0 && used.saturating_add(height) > visible_lines {
            break;
        }
        start = index;
        used = used.saturating_add(height);
        if used >= visible_lines {
            break;
        }
    }
    start
}

fn truncate_with_ellipsis(text: &str, width: usize, expand_hint: bool) -> (String, bool) {
    let char_count = text.chars().count();
    if char_count <= width {
        return (text.to_string(), false);
    }
    if width == 0 {
        return (String::new(), true);
    }
    let preferred_suffix = if expand_hint { "… [Enter]" } else { "…" };
    let suffix = if preferred_suffix.chars().count() <= width {
        preferred_suffix
    } else {
        "…"
    };
    let keep = width.saturating_sub(suffix.chars().count());
    let mut output: String = text.chars().take(keep).collect();
    output.push_str(suffix);
    (output, true)
}

fn wrap_preserving_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();
    for logical_line in text.split('\n') {
        let logical_line = logical_line.strip_suffix('\r').unwrap_or(logical_line);
        if logical_line.is_empty() {
            output.push(String::new());
            continue;
        }
        let mut chunk = String::new();
        let mut count = 0usize;
        for ch in logical_line.chars() {
            if count == width {
                output.push(std::mem::take(&mut chunk));
                count = 0;
            }
            chunk.push(ch);
            count += 1;
        }
        if !chunk.is_empty() {
            output.push(chunk);
        }
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn scroll_panel_up(
    scroll: &mut usize,
    follow_tail: &mut bool,
    view: PanelScrollView,
    amount: usize,
) {
    let amount = amount.max(1);
    if *follow_tail {
        *follow_tail = false;
        *scroll = view.effective_scroll.saturating_sub(amount);
    } else {
        *scroll = scroll.saturating_sub(amount);
    }
}

fn scroll_panel_down(
    scroll: &mut usize,
    follow_tail: &mut bool,
    view: PanelScrollView,
    amount: usize,
) {
    if *follow_tail {
        return;
    }
    *scroll = scroll.saturating_add(amount.max(1)).min(view.max_scroll);
    if *scroll >= view.max_scroll {
        *follow_tail = true;
    }
}

fn follow_panel_latest(scroll: &mut usize, follow_tail: &mut bool, view: PanelScrollView) {
    *follow_tail = true;
    *scroll = view.max_scroll;
}

fn reset_filtered_navigation(
    log: (&mut usize, &mut bool),
    command: (&mut usize, &mut bool),
    selected: (&mut Option<usize>, &mut Option<usize>),
    expanded: (&mut Option<usize>, &mut Option<usize>),
) {
    let (log_scroll, log_follow_tail) = log;
    let (command_scroll, command_follow_tail) = command;
    let (selected_log, selected_command) = selected;
    let (expanded_log, expanded_command) = expanded;
    *log_scroll = 0;
    *log_follow_tail = true;
    *command_scroll = 0;
    *command_follow_tail = true;
    *selected_log = None;
    *selected_command = None;
    *expanded_log = None;
    *expanded_command = None;
}

fn panel_under_cursor(areas: BottomPanelAreas, column: u16, row: u16) -> Option<DashboardFocus> {
    if areas
        .shell_commands
        .is_some_and(|area| rect_contains(area, column, row))
    {
        return Some(DashboardFocus::ShellCommands);
    }
    if rect_contains(areas.logs, column, row) {
        return Some(DashboardFocus::Logs);
    }
    None
}

fn extract_from_screen(lines: &[String], start: (u16, u16), end: (u16, u16)) -> String {
    let (c0, r0) = start;
    let (c1, r1) = end;
    let mut result = String::new();
    for row in r0..=r1 {
        let idx = row as usize;
        if idx >= lines.len() {
            break;
        }
        let line: Vec<char> = lines[idx].chars().collect();
        let cs = if row == r0 { c0 as usize } else { 0 };
        let ce = if row == r1 {
            (c1 as usize).min(line.len().saturating_sub(1))
        } else {
            line.len().saturating_sub(1)
        };
        for col in cs..=ce {
            if col < line.len() {
                result.push(line[col]);
            }
        }
        if row != r1 {
            result.push('\n');
        }
    }
    result
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn current_anim_segment(flow: &FlowLane, now_millis: u128) -> Option<FlowAnimSegment> {
    if let Some(seg) = flow
        .anim_queue
        .iter()
        .find(|seg| seg.started_ms <= now_millis && now_millis < seg.ends_ms)
    {
        return Some(*seg);
    }
    flow.anim_queue.front().copied()
}

fn should_display_flow_row(flow: &FlowLane) -> bool {
    flow.bootstrap_status_active || flow.closing_started_ms.is_some() || !flow.anim_queue.is_empty()
}

fn flow_direction(flow: Option<&FlowLane>, now_millis: u128) -> FlowDirection {
    if let Some(flow) = flow {
        if let Some(seg) = current_anim_segment(flow, now_millis) {
            return seg.direction;
        }
        return flow.last_direction;
    }
    FlowDirection::Forward
}

fn flow_lit_count(flow: Option<&FlowLane>, now_millis: u128, cells: usize) -> usize {
    let Some(flow) = flow else {
        return 0;
    };
    if flow.closing_started_ms.is_some() {
        return 0;
    }
    current_anim_segment(flow, now_millis)
        .map(|seg| flow_anim_lit_count(seg, now_millis).min(cells))
        .unwrap_or(0)
}

fn debug_lane(direction: Option<FlowDirection>, lit_count: usize, cells: usize) -> String {
    let mut out = String::with_capacity(cells);
    for i in 0..cells {
        let lit_here = match direction {
            Some(FlowDirection::Forward) => lit_count > 0 && i < lit_count,
            Some(FlowDirection::Backward) => lit_count > 0 && i >= cells.saturating_sub(lit_count),
            None => false,
        };
        out.push(if lit_here { '#' } else { '-' });
    }
    out
}

fn flow_lane_spans(
    active: bool,
    flow: Option<&FlowLane>,
    palette: &theme::Palette,
    now_millis: u128,
) -> Vec<Span<'static>> {
    flow_lane_spans_with_cells(active, flow, palette, now_millis, FLOW_ROW_CELLS)
}

fn flow_lane_spans_with_cells(
    active: bool,
    flow: Option<&FlowLane>,
    palette: &theme::Palette,
    now_millis: u128,
    cells: usize,
) -> Vec<Span<'static>> {
    let cells = cells.clamp(1, FLOW_ROW_CELLS);
    let unlit = Style::default().fg(palette.muted_fg);
    let lit = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);

    let direction = flow.map(|flow| flow_direction(Some(flow), now_millis));
    let lit_count = if active {
        flow_lit_count(flow, now_millis, FLOW_ROW_CELLS)
            .saturating_mul(cells)
            .div_ceil(FLOW_ROW_CELLS)
    } else {
        0
    };

    if lit_count == 0 || direction.is_none() {
        return vec![Span::styled("-".repeat(cells), unlit), Span::raw(" ")];
    }

    let direction = direction.unwrap_or(FlowDirection::Forward);
    let mut spans = Vec::with_capacity(cells + 1);
    for i in 0..cells {
        let lit_here = match direction {
            FlowDirection::Forward => i < lit_count,
            FlowDirection::Backward => i >= cells.saturating_sub(lit_count),
        };
        let style = if lit_here { lit } else { unlit };
        spans.push(Span::styled("-".to_string(), style));
    }
    spans.push(Span::raw(" "));
    spans
}

fn trim_line(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let kept = chars[..max_chars.saturating_sub(3)]
        .iter()
        .collect::<String>();
    format!("{kept}...")
}

fn format_token_compact(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }

    let (unit, suffix) = if value >= 1_000_000_000 {
        (1_000_000_000.0, "B")
    } else if value >= 1_000_000 {
        (1_000_000.0, "M")
    } else {
        (1_000.0, "K")
    };
    let scaled = value as f64 / unit;
    let decimals = if scaled >= 100.0 { 0 } else { 1 };
    let formatted = format!("{scaled:.prec$}", prec = decimals);
    format!("{}{}", formatted.trim_end_matches(".0"), suffix)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ToolCostEstimate {
    standard_usd: f64,
    cached_read_usd: f64,
    cache_write_usd: f64,
}

fn estimate_gpt_5_6_sol_tool_cost(usage: &UsageTotals) -> ToolCostEstimate {
    // MCP tool arguments are generated by the model, so they correspond to model
    // output tokens. Tool results are fed back to the model, so they correspond to
    // model input tokens. Cache read/write status is not exposed over MCP.
    let generated_tool_args_usd =
        usage.tool_input_tokens as f64 * GPT_5_6_SOL_OUTPUT_USD_PER_1M / 1_000_000.0;
    let tool_result_tokens_m = usage.tool_output_tokens as f64 / 1_000_000.0;
    ToolCostEstimate {
        standard_usd: generated_tool_args_usd + tool_result_tokens_m * GPT_5_6_SOL_INPUT_USD_PER_1M,
        cached_read_usd: generated_tool_args_usd
            + tool_result_tokens_m * GPT_5_6_SOL_CACHED_INPUT_USD_PER_1M,
        cache_write_usd: generated_tool_args_usd
            + tool_result_tokens_m * GPT_5_6_SOL_CACHE_WRITE_USD_PER_1M,
    }
}

fn format_usd_compact(usd: f64) -> String {
    let formatted = format!("{usd:.prec$}", prec = PRICE_DISPLAY_DECIMALS);
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn mcp_url_reveal_seconds(remaining: Duration) -> u64 {
    remaining
        .as_millis()
        .div_ceil(1_000)
        .min(MCP_URL_REVEAL_DURATION.as_secs() as u128) as u64
}

fn active_reveal_remaining(deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    deadline
        .and_then(|deadline| deadline.checked_duration_since(now))
        .filter(|remaining| !remaining.is_zero())
}

#[derive(Debug, PartialEq, Eq)]
enum TimedSecretClick {
    Revealed,
    Copy(String),
}

fn timed_secret_click(
    value: Option<&str>,
    revealed_until: &mut Option<Instant>,
    now: Instant,
) -> Option<TimedSecretClick> {
    let value = value?;
    if active_reveal_remaining(*revealed_until, now).is_some() {
        Some(TimedSecretClick::Copy(value.to_string()))
    } else {
        *revealed_until = Some(now + MCP_URL_REVEAL_DURATION);
        Some(TimedSecretClick::Revealed)
    }
}

fn mcp_url_reveal_bar_segments(remaining: Duration) -> (String, String) {
    let total_millis = MCP_URL_REVEAL_DURATION.as_millis();
    let remaining_millis = remaining.as_millis().min(total_millis);
    let lit = remaining_millis
        .saturating_mul(MCP_URL_REVEAL_BAR_CELLS as u128)
        .div_ceil(total_millis) as usize;
    (
        "━".repeat(lit.min(MCP_URL_REVEAL_BAR_CELLS)),
        "─".repeat(MCP_URL_REVEAL_BAR_CELLS.saturating_sub(lit)),
    )
}

fn formatted_usage_values(usage: &UsageTotals, cost_usd: f64) -> [String; 5] {
    [
        format_token_compact(usage.tool_input_tokens),
        format_token_compact(usage.tool_output_tokens),
        format_token_compact(usage.total_tokens),
        format_token_compact(usage.tool_call_count),
        format_usd_compact(cost_usd),
    ]
}

fn usage_value_widths(
    first: &UsageTotals,
    first_cost_usd: f64,
    second: &UsageTotals,
    second_cost_usd: f64,
) -> [usize; 5] {
    let first = formatted_usage_values(first, first_cost_usd);
    let second = formatted_usage_values(second, second_cost_usd);
    std::array::from_fn(|index| first[index].len().max(second[index].len()))
}

fn usage_line(
    usage: &UsageTotals,
    cost_usd: f64,
    status_label: Span<'static>,
    palette: &theme::Palette,
    value_widths: &[usize; 5],
) -> Line<'static> {
    let label_style = Style::default().fg(palette.muted_fg);
    let value_style = Style::default()
        .fg(palette.secondary_fg)
        .add_modifier(Modifier::BOLD);
    let price_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);
    let values = formatted_usage_values(usage, cost_usd);

    Line::from(vec![
        status_label,
        Span::styled("↓", label_style),
        Span::styled(
            format!("{:<width$}", values[0], width = value_widths[0]),
            value_style,
        ),
        Span::raw(" "),
        Span::styled("↑", label_style),
        Span::styled(
            format!("{:<width$}", values[1], width = value_widths[1]),
            value_style,
        ),
        Span::raw("  "),
        Span::styled("Σ", label_style),
        Span::styled(
            format!("{:<width$}", values[2], width = value_widths[2]),
            value_style,
        ),
        Span::raw("  "),
        Span::styled("ƒ", label_style),
        Span::styled(
            format!("{:<width$}", values[3], width = value_widths[3]),
            value_style,
        ),
        Span::raw(" "),
        Span::styled("~$", label_style),
        Span::styled(
            format!("{:<width$}", values[4], width = value_widths[4]),
            price_style,
        ),
    ])
}

fn flow_call_offset(text: &str) -> String {
    let text_width = text.chars().count();
    let centered_in_lane = FLOW_ROW_CELLS.saturating_sub(text_width) / 2;
    " ".repeat(FLOW_LANE_LEFT_LABEL.len() + centered_in_lane)
}

fn flow_phase(flow: &FlowLane, now_millis: u128) -> &'static str {
    if flow.closing_started_ms.is_some() {
        return "close";
    }
    if let Some(seg) = current_anim_segment(flow, now_millis) {
        return match seg.kind {
            FlowAnimKind::Turn => "turn",
            FlowAnimKind::Move => match seg.direction {
                FlowDirection::Forward => "request",
                FlowDirection::Backward => "response",
            },
        };
    }
    "idle"
}

fn latest_flow_action(flow: &FlowLane) -> String {
    flow.events
        .iter()
        .rev()
        .find_map(|event| {
            if let Some(tool) = event.strip_prefix("tools/call:") {
                if tool.is_empty() {
                    None
                } else {
                    Some(tool.to_string())
                }
            } else if event.is_empty() {
                None
            } else {
                Some(event.clone())
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlowPhaseStepState {
    Future,
    Pending,
    Complete,
}

fn flow_phase_bounds(phase_index: usize) -> (usize, usize) {
    let start = FLOW_BOOTSTRAP_PHASES
        .iter()
        .take(phase_index)
        .map(|phase| phase.steps.len())
        .sum::<usize>();
    let end = start + FLOW_BOOTSTRAP_PHASES[phase_index].steps.len();
    (start, end)
}

fn flow_phase_step_state(flow: Option<&FlowLane>, step_index: usize) -> FlowPhaseStepState {
    let Some(flow) = flow else {
        return FlowPhaseStepState::Future;
    };
    if step_index < flow.bootstrap_completed_steps {
        FlowPhaseStepState::Complete
    } else if flow.bootstrap_pending_steps.contains(&step_index) {
        FlowPhaseStepState::Pending
    } else {
        FlowPhaseStepState::Future
    }
}

fn flow_phase_status_label(flow: Option<&FlowLane>, phase_index: usize) -> Option<String> {
    let flow = flow?;
    let (start, end) = flow_phase_bounds(phase_index);
    if flow.bootstrap_completed_steps >= end {
        return Some("✓".to_string());
    }
    if let Some(step_index) = flow
        .bootstrap_pending_steps
        .iter()
        .copied()
        .find(|step_index| (start..end).contains(step_index))
    {
        let step = &FLOW_BOOTSTRAP_PHASES[phase_index].steps[step_index - start];
        return Some(step.label.to_string());
    }
    if (start..end).contains(&flow.bootstrap_completed_steps.saturating_sub(1))
        && flow.bootstrap_completed_steps > start
    {
        let step_index = flow.bootstrap_completed_steps - 1;
        let step = &FLOW_BOOTSTRAP_PHASES[phase_index].steps[step_index - start];
        return Some(step.label.to_string());
    }
    None
}

fn flow_phase_lines(
    flow: Option<&FlowLane>,
    palette: &theme::Palette,
    status_style: Style,
) -> Vec<Line<'static>> {
    const TITLE_STATUS_GAP: usize = 4;
    const STATUS_ANIM_GAP: usize = 4;
    let title_width = FLOW_BOOTSTRAP_PHASES
        .iter()
        .enumerate()
        .map(|(phase_index, phase)| format!("    Phase {}  {}", phase_index + 1, phase.title))
        .map(|title| title.chars().count())
        .max()
        .unwrap_or(0);
    let status_width = FLOW_BOOTSTRAP_PHASES
        .iter()
        .flat_map(|phase| {
            std::iter::once("✓".to_string())
                .chain(phase.steps.iter().map(|step| step.label.to_string()))
                .map(|status| format!("[{status}]").chars().count())
        })
        .max()
        .unwrap_or(0);
    let pending_style = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);
    let complete_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);
    let future_style = Style::default().fg(palette.muted_fg);
    let label_style = Style::default().fg(palette.primary_fg);

    FLOW_BOOTSTRAP_PHASES
        .iter()
        .enumerate()
        .map(|(phase_index, phase)| {
            let title = format!("    Phase {}  {}", phase_index + 1, phase.title);
            let title_padding = title_width.saturating_sub(title.chars().count());
            let status_label = flow_phase_status_label(flow, phase_index);
            let status_text = status_label
                .map(|label| format!("[{label}]"))
                .unwrap_or_default();
            let status_padding = status_width.saturating_sub(status_text.chars().count());
            let mut spans = vec![
                Span::styled(title, label_style),
                Span::styled(" ".repeat(title_padding + TITLE_STATUS_GAP), future_style),
                Span::styled(status_text, status_style),
                Span::styled(" ".repeat(status_padding + STATUS_ANIM_GAP), future_style),
            ];
            let (start, _) = flow_phase_bounds(phase_index);
            for (step_offset, _) in phase.steps.iter().enumerate() {
                if step_offset > 0 {
                    spans.push(Span::raw(" "));
                }
                let step_index = start + step_offset;
                match flow_phase_step_state(flow, step_index) {
                    FlowPhaseStepState::Future => {
                        spans.push(Span::styled("✧", future_style));
                    }
                    FlowPhaseStepState::Pending => {
                        spans.push(Span::styled("✧", pending_style));
                    }
                    FlowPhaseStepState::Complete => {
                        spans.push(Span::styled("✦", complete_style));
                    }
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn flow_bootstrap_steps_total() -> usize {
    state::flow_bootstrap_steps_total()
}

fn flow_bootstrap_complete(flow: &FlowLane) -> bool {
    flow.bootstrap_completed_steps >= flow_bootstrap_steps_total()
        && flow.bootstrap_pending_steps.is_empty()
}

fn flow_bootstrap_status_visible(flow: &FlowLane, now_millis: u128) -> bool {
    if !flow_bootstrap_complete(flow) {
        return true;
    }
    if current_anim_segment(flow, now_millis).is_some() {
        return true;
    }
    flow.bootstrap_status_close_deadline_ms
        .is_some_and(|deadline| now_millis < deadline)
}

fn flow_bootstrap_countdown_remaining_seconds(flow: &FlowLane, now_millis: u128) -> Option<u128> {
    let deadline = flow.bootstrap_status_close_deadline_ms?;
    if now_millis >= deadline {
        return Some(0);
    }
    Some(deadline.saturating_sub(now_millis).div_ceil(1000))
}

fn active_bootstrap_status_flow(app: &UiSnapshot, now_millis: u128) -> Option<&FlowLane> {
    app.flows.iter().find(|flow| {
        should_display_flow_row(flow)
            && flow.bootstrap_status_active
            && flow.closing_started_ms.is_none()
            && flow_bootstrap_status_visible(flow, now_millis)
    })
}

fn should_show_connect_guide(app: &UiSnapshot, now_millis: u128) -> bool {
    let both_running = app.server_running && app.ngrok_running;
    let has_url = app.ngrok_url.is_some();
    let visible_flow_count = app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow))
        .count() as u16;
    let within_connect_grace = app
        .last_remote_activity_ms
        .map(|t| now_millis.saturating_sub(t) < REMOTE_CONNECT_UI_GRACE_MS)
        .unwrap_or(false);
    !app.is_returning_user
        && both_running
        && has_url
        && !app.remote_connected
        && visible_flow_count == 0
        && !within_connect_grace
}

fn flow_bootstrap_status_lines(
    app: &UiSnapshot,
    flow: &FlowLane,
    palette: &theme::Palette,
    now_millis: u128,
) -> Vec<Line<'static>> {
    let action_label = latest_flow_action(flow);
    let bootstrap_complete = flow_bootstrap_complete(flow);
    let header_title = if bootstrap_complete {
        "Initialize completed"
    } else {
        "Initialize connector in progress"
    };
    let call_text = trim_line(&format!("call {action_label}"), FLOW_ROW_CELLS);
    let call_offset = flow_call_offset(&call_text);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {header_title}"),
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", Style::default().fg(palette.muted_fg)),
            Span::styled(call_offset, Style::default().fg(palette.muted_fg)),
            Span::styled(
                call_text,
                Style::default()
                    .fg(palette.info_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from({
            let computer_role_style = Style::default()
                .fg(if app.server_running {
                    palette.success_fg
                } else {
                    palette.muted_fg
                })
                .add_modifier(Modifier::BOLD);
            let chatgpt_role_style = Style::default()
                .fg(if app.remote_connected {
                    palette.success_fg
                } else {
                    palette.muted_fg
                })
                .add_modifier(Modifier::BOLD);
            let mut row = vec![Span::styled(
                format!("  {FLOW_LANE_LEFT_LABEL}"),
                computer_role_style,
            )];
            row.extend(flow_lane_spans(true, Some(flow), palette, now_millis));
            row.push(Span::styled("ChatGPT Web", chatgpt_role_style));
            row
        }),
        Line::from(""),
    ];
    lines.extend(flow_phase_lines(
        Some(flow),
        palette,
        Style::default()
            .fg(palette.info_fg)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(""));

    let footer_text = if bootstrap_complete && current_anim_segment(flow, now_millis).is_none() {
        match flow_bootstrap_countdown_remaining_seconds(flow, now_millis) {
            Some(0) => "Completed.".to_string(),
            Some(seconds) => format!("Completed. Closing in {seconds}s..."),
            None => "Completed.".to_string(),
        }
    } else {
        "Auto closes after initialize is completed.".to_string()
    };
    lines.push(Line::from(Span::styled(
        format!("  {footer_text}"),
        Style::default().fg(palette.muted_fg),
    )));
    lines
}

fn build_animation_snapshot(app: &UiSnapshot) -> Vec<String> {
    if app.flows.is_empty() {
        return Vec::new();
    }
    let now_millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut rows = Vec::new();
    for flow in app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow))
    {
        let latest_action = latest_flow_action(flow);
        let closing = flow.closing_started_ms.is_some();
        let lane_active = closing
            || !flow.anim_queue.is_empty()
            || (app.server_running && app.ngrok_running && app.remote_connected);
        let direction = lane_active.then_some(flow_direction(Some(flow), now_millis));
        let phase = flow_phase(flow, now_millis);
        let lit = flow_lit_count(Some(flow), now_millis, FLOW_ROW_CELLS);
        let lane = debug_lane(direction, lit, FLOW_ROW_CELLS);
        rows.push(format!(
            "flow {} phase={:<8} tool={:<16} Your computer {} ChatGPT Web (via Ngrok)",
            flow.short_id, phase, latest_action, lane
        ));
    }
    if rows.is_empty() {
        return Vec::new();
    }
    rows
}

#[cfg(target_os = "macos")]
fn clipboard_copy(text: &str) -> bool {
    let mut child = match std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(text.as_bytes()).is_err() {
        drop(stdin);
        let _ = child.wait();
        return false;
    }

    drop(stdin);

    match child.wait() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "windows")]
fn clipboard_copy(text: &str) -> bool {
    let mut child = match std::process::Command::new("clip.exe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(text.as_bytes()).is_err() {
        drop(stdin);
        let _ = child.wait();
        return false;
    }

    drop(stdin);

    match child.wait() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn clipboard_copy(text: &str) -> bool {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")
        .and_then(|_| out.flush())
        .is_ok()
}

#[cfg(target_os = "macos")]
fn clipboard_paste() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/pbpaste")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .filter(|text| !text.is_empty())
}

#[cfg(target_os = "windows")]
fn clipboard_paste() -> Option<String> {
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .filter(|text| !text.is_empty())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn clipboard_paste() -> Option<String> {
    const CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[
        ("wl-paste", &["-n"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];

    for (program, args) in CLIPBOARD_COMMANDS {
        let output = match std::process::Command::new(program)
            .args(*args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };

        if let Ok(text) = String::from_utf8(output.stdout)
            && !text.is_empty()
        {
            return Some(text);
        }
    }

    None
}

fn key_is_clipboard_paste(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Insert) && key.modifiers.contains(KeyModifiers::SHIFT)
        || matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'v'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn key_is_interrupt(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn key_is_plain_quit(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'q'))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

fn normalize_ngrok_authtoken_input(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if let Some(idx) = parts.iter().position(|part| *part == "add-authtoken")
        && let Some(token) = parts.get(idx + 1)
    {
        return token.trim_matches(['"', '\'']).to_string();
    }

    trimmed.to_string()
}

fn drain_server_ui_events(app: &mut AppState, ui_events: &mut UiEventReceiver) -> bool {
    let mut changed = false;
    while let Ok(event) = ui_events.try_recv() {
        app.apply_server_ui_event(event);
        changed = true;
    }
    let dropped = ui_events.take_dropped_since_last_report();
    if dropped > 0 {
        app.log(
            "WARN",
            format!("Dropped {dropped} transient local UI events during heavy activity"),
        );
        changed = true;
    }
    changed
}

// ── Main ────────────────────────────────────────────────────

#[derive(Debug)]
struct ClippyMoonExportArgs {
    seed: Option<u64>,
    output_dir: std::path::PathBuf,
}

fn parse_clippymoon_export_args(args: &[String]) -> Result<Option<ClippyMoonExportArgs>, String> {
    if args.first().map(String::as_str) != Some("clippymoon") {
        return Ok(None);
    }
    if args.get(1).map(String::as_str) != Some("export") {
        return Err("usage: moondesk clippymoon export [--seed HEX] [--out DIRECTORY]".to_string());
    }

    let mut seed = None;
    let mut output_dir = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--seed" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--seed requires a hexadecimal seed".to_string())?;
                let value = value.strip_prefix("0x").unwrap_or(value);
                seed = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|error| format!("invalid ClippyMoon seed `{value}`: {error}"))?,
                );
            }
            "--out" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--out requires a directory path".to_string())?;
                output_dir = std::path::PathBuf::from(value);
            }
            "-h" | "--help" => {
                return Err(
                    "usage: moondesk clippymoon export [--seed HEX] [--out DIRECTORY]".to_string(),
                );
            }
            unknown => {
                return Err(format!(
                    "unknown ClippyMoon export option `{unknown}`\nusage: moondesk clippymoon export [--seed HEX] [--out DIRECTORY]"
                ));
            }
        }
        index += 1;
    }

    Ok(Some(ClippyMoonExportArgs { seed, output_dir }))
}

fn cli_version_requested(args: &[String]) -> bool {
    matches!(args, [arg] if arg == "--version" || arg == "-V")
}

fn print_clippymoon_export(export: &mascot::ClippyMoonExport) {
    println!("ClippyMoon exported");
    println!("seed: {:016x}", export.seed);
    println!("phase: {}", export.traits.phase.name());
    println!("color: {}", export.traits.color.name());
    println!("expression: {}", export.traits.expression.name());
    println!("png: {}", export.png_path.display());
    println!("gif: {}", export.gif_path.display());
}

fn parse_port_value(value: Option<&str>) -> Result<u16, String> {
    let Some(value) = value else {
        return Ok(3200);
    };
    let port = value
        .parse::<u16>()
        .map_err(|_| "PORT must be an integer from 1 to 65535".to_string())?;
    if port == 0 {
        return Err("PORT must be an integer from 1 to 65535".to_string());
    }
    Ok(port)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostRuntimeRegistration {
    port: u16,
    pid: u32,
    token: String,
}

struct HostRuntimeGuard {
    path: PathBuf,
}

impl Drop for HostRuntimeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct HostAttachResult {
    workspace_name: String,
    already_registered: bool,
}

fn host_runtime_path(port: u16) -> std::io::Result<PathBuf> {
    let config_path = app_config_path()?;
    let directory = config_path.parent().ok_or_else(|| {
        std::io::Error::other("MoonDesk config path does not have a parent directory")
    })?;
    Ok(directory.join(format!("host-{port}.json")))
}

fn write_host_runtime_registration(port: u16, token: &str) -> std::io::Result<HostRuntimeGuard> {
    let path = host_runtime_path(port)?;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::other("MoonDesk host runtime path does not have a parent directory")
    })?;
    std::fs::create_dir_all(parent)?;
    let payload = serde_json::to_vec(&HostRuntimeRegistration {
        port,
        pid: std::process::id(),
        token: token.to_string(),
    })
    .map_err(std::io::Error::other)?;

    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&payload)?;
    file.flush()?;
    file.sync_all()?;
    Ok(HostRuntimeGuard { path })
}

fn read_host_runtime_registration(port: u16) -> Result<HostRuntimeRegistration, String> {
    let path = host_runtime_path(port).map_err(|error| error.to_string())?;
    let payload = std::fs::read(&path).map_err(|error| {
        format!(
            "could not read the running host registration file {}: {error}",
            path.display()
        )
    })?;
    let registration: HostRuntimeRegistration = serde_json::from_slice(&payload)
        .map_err(|error| format!("invalid host registration: {error}"))?;
    if registration.port != port {
        return Err(format!(
            "host registration is for port {}, expected {port}",
            registration.port
        ));
    }
    if registration.token.is_empty() {
        return Err("host registration token is empty".into());
    }
    Ok(registration)
}

async fn attach_workspace_to_running_host(
    port: u16,
    root: &std::path::Path,
) -> Result<HostAttachResult, String> {
    let registration = read_host_runtime_registration(port)?;
    let body = serde_json::to_vec(&serde_json::json!({
        "root": root.to_string_lossy(),
    }))
    .map_err(|error| error.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?;
    let endpoint = format!("http://127.0.0.1:{port}{}", server::HOST_CONTROL_ROUTE);
    let response = client
        .post(endpoint)
        .header(server::HOST_CONTROL_HEADER, registration.token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("failed to contact the running MoonDesk host: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read the running host response: {error}"))?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("running host returned an invalid response: {error}"))?;
    if !status.is_success() {
        let message = payload
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("workspace registration was rejected");
        return Err(format!("{message} (HTTP {status})"));
    }
    let workspace_name = payload
        .get("workspaceName")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "running host response did not include a workspace name".to_string())?
        .to_string();
    let already_registered = payload
        .get("alreadyRegistered")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(HostAttachResult {
        workspace_name,
        already_registered,
    })
}

const BROWSER_CLI_FLAG: &str = "--browser-cli";

fn parse_browser_cli_args(cli_args: &[String]) -> Option<Result<(String, Vec<String>), String>> {
    if cli_args.first().map(String::as_str) != Some(BROWSER_CLI_FLAG) {
        return None;
    }
    let Some(command) = cli_args
        .get(1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return Some(Err("MoonDesk browser CLI requires a command".to_string()));
    };
    Some(Ok((command.to_string(), cli_args[2..].to_vec())))
}

async fn run_browser_cli(command: String, args: Vec<String>) -> Result<i32, String> {
    let workspace_root = std::env::current_dir()
        .map_err(|error| format!("Could not resolve browser CLI working directory: {error}"))?
        .to_string_lossy()
        .into_owned();
    let runtime = BrowserRuntime::standalone();
    let output = runtime
        .run(
            &workspace_root,
            &command,
            &args,
            browser_runtime::DEFAULT_BROWSER_COMMAND_TIMEOUT,
        )
        .await?;
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
        let _ = std::io::stdout().flush();
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
        let _ = std::io::stderr().flush();
    }
    Ok(if output.success() {
        0
    } else {
        output.exit_code.max(1)
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli_args = std::env::args().skip(1).collect::<Vec<_>>();
    if cli_version_requested(&cli_args) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(parsed) = parse_browser_cli_args(&cli_args) {
        let (command, args) = parsed.map_err(std::io::Error::other)?;
        let exit_code = run_browser_cli(command, args)
            .await
            .map_err(std::io::Error::other)?;
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    match parse_clippymoon_export_args(&cli_args) {
        Ok(Some(export_args)) => {
            let export = mascot::export_clippymoon(export_args.seed, &export_args.output_dir)?;
            print_clippymoon_export(&export);
            return Ok(());
        }
        Ok(None) => {}
        Err(message) if cli_args.iter().any(|arg| arg == "-h" || arg == "--help") => {
            println!("{message}");
            return Ok(());
        }
        Err(message) => return Err(std::io::Error::other(message).into()),
    }

    let port_value = std::env::var("PORT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let port = parse_port_value(port_value.as_deref()).map_err(std::io::Error::other)?;
    let workspace_root = match std::env::var("WORKSPACE_ROOT") {
        Ok(path) => path,
        Err(_) => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    // Attaching another project is a non-interactive client action, so do it
    // before any terminal-profile bootstrap. This keeps `cd project && moondesk`
    // fast on macOS as well as Windows/Linux when a host is already running.
    if port_hosts_moondesk(port).await {
        match attach_workspace_to_running_host(port, std::path::Path::new(&workspace_root)).await {
            Ok(result) => {
                if result.already_registered {
                    println!(
                        "MoonDesk is already running on port {port}; workspace '{}' is already attached.",
                        result.workspace_name
                    );
                } else {
                    println!(
                        "MoonDesk is already running on port {port}; attached this directory as workspace '{}'.",
                        result.workspace_name
                    );
                }
                return Ok(());
            }
            Err(error) => {
                return Err(std::io::Error::other(format!(
                    "MoonDesk is already running on port {port}, but this directory could not be attached automatically: {error}. Open [w] Workspaces in the running MoonDesk host to add it manually."
                ))
                .into());
            }
        }
    }

    match macos_terminal::maybe_relaunch_in_terminal_profile() {
        Ok(macos_terminal::LaunchAction::Continue) => {}
        #[cfg(target_os = "macos")]
        Ok(macos_terminal::LaunchAction::ExitAfterProfileBootstrap) => {
            eprintln!(
                "MoonDesk applied the Terminal.app profile. Run the same command again in this tab."
            );
            return Ok(());
        }
        Err(error) => {
            return Err(std::io::Error::other(format!(
                "MoonDesk: macOS Terminal profile bootstrap failed: {error}"
            ))
            .into());
        }
    }

    let state: SharedState = Arc::new(Mutex::new(AppState::new(port, workspace_root)?));
    let interrupts = InterruptState::default();
    let mut interrupt_listener = None;

    let mut terminal_guard = TerminalRestoreGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(
        &mut terminal,
        state.clone(),
        interrupts.clone(),
        &mut interrupt_listener,
    )
    .await;

    interrupts.begin_shutdown();
    let terminal_restore_result = terminal_guard.restore();

    // Cleanup after the TUI is gone so quit never appears frozen on screen.
    // Stop accepting new MCP work first, then terminate owned command trees and
    // shared host services before finally clearing local runtime status.
    let (server_handle, command_jobs) = {
        let mut app = state.lock().await;
        app.server_running = false;
        (app.server_handle.take(), app.command_jobs.clone())
    };
    if let Some(handle) = server_handle {
        handle.abort();
        let _ = handle.await;
    }
    command_jobs.cancel_all().await;
    ngrok::stop(state.clone()).await;
    state.lock().await.clear_remote_connection_state();

    if let Some(listener) = interrupt_listener.take() {
        listener.abort();
    }
    if let Err(error) = terminal_restore_result {
        return Err(error.into());
    }

    match result {
        Ok(AppExit::Quit) => {
            println!("MoonDesk stopped.");
            Ok(())
        }
        Ok(AppExit::UpdateRestart(target_version)) => {
            update::write_update_request(&target_version).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not prepare MoonDesk {target_version} update restart: {error}"),
                )
            })?;
            std::process::exit(update::UPDATE_EXIT_CODE);
        }
        Err(error) => Err(error),
    }
}

// ── Phase 1: Mode selection ─────────────────────────────────

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    interrupts: InterruptState,
    interrupt_listener: &mut Option<tokio::task::JoinHandle<()>>,
) -> Result<AppExit, Box<dyn std::error::Error>> {
    // Draw mode selection screen
    loop {
        let (current_theme, current_tool_mode) = {
            let app = state.lock().await;
            (app.current_theme(), app.tool_mode)
        };
        terminal.draw(|f| draw_mode_select(f, current_theme, current_tool_mode))?;

        if event::poll(UI_POLL_INTERVAL)?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let mode = match key.code {
                KeyCode::Char('1') => Mode::Computer,
                KeyCode::Char('2') => Mode::Browser,
                KeyCode::Char('3') => Mode::Both,
                KeyCode::Char('q') => return Ok(AppExit::Quit),
                KeyCode::Char('w') => {
                    run_workspaces(terminal, state.clone()).await?;
                    continue;
                }
                KeyCode::Char('s') => {
                    run_settings(terminal, state.clone()).await?;
                    continue;
                }
                _ => continue,
            };
            {
                let mut app = state.lock().await;
                app.mode = mode;
                app.log("INFO", format!("Mode: {}", mode.label()));
                app.mark_config_dirty();
            }
            break;
        }
    }

    let continue_run = run_ngrok_auth_setup(terminal, state.clone(), None).await?;
    if !continue_run {
        return Ok(AppExit::Quit);
    }

    let continue_run = run_ngrok_domain_setup(terminal, state.clone()).await?;
    if !continue_run {
        return Ok(AppExit::Quit);
    }

    if let Err(error) = flush_config(&state, true).await {
        state.lock().await.log(
            "WARN",
            format!("Failed to persist config before startup: {error}"),
        );
    }

    // Start services
    let (ui_event_tx, ui_event_rx) = ui_event_channel();
    let mut services = start_services(state.clone(), ui_event_tx)
        .await
        .map_err(std::io::Error::other)?;
    let browser_workspace_root = { state.lock().await.workspace_root.clone() };

    while services
        .ngrok_start_error
        .as_ref()
        .is_some_and(ngrok::StartFailure::is_authentication)
    {
        let continue_run = run_ngrok_auth_setup(
            terminal,
            state.clone(),
            Some(
                "ngrok rejected the saved authtoken. Paste a fresh token from the ngrok dashboard."
                    .into(),
            ),
        )
        .await?;
        if !continue_run {
            if let Some(runtime) = services.browser_runtime.take() {
                runtime.stop_if_owned(&browser_workspace_root).await;
            }
            return Ok(AppExit::Quit);
        }

        services.ngrok_start_error = match ngrok::start(state.clone()).await {
            Ok(()) => None,
            Err(error) => {
                state.lock().await.log("ERROR", format!("ngrok: {error}"));
                Some(error)
            }
        };
    }

    // Phase 2: main TUI loop. Once the shared host is live, intercept console
    // interrupts so an accidental Ctrl+C cannot tear down every workspace before
    // the TUI has a chance to confirm and run its normal shutdown path.
    *interrupt_listener = Some(spawn_interrupt_listener(interrupts.clone()));
    let result = run_tui(terminal, state, ui_event_rx, interrupts.clone()).await;
    interrupts.begin_shutdown();
    if let Some(runtime) = services.browser_runtime.take() {
        runtime.stop_if_owned(&browser_workspace_root).await;
    }
    result
}

fn draw_mode_select(f: &mut Frame, theme: &theme::ThemeDef, tool_mode: ToolMode) {
    let palette = theme.palette;
    render_theme_background(f, palette);
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Header
            Constraint::Length(16), // Mode selection
            Constraint::Min(0),     // Spacer
        ])
        .split(area);

    let header = Paragraph::new("  MoonDesk - Turns ChatGPT Web into a coding agent")
        .style(
            Style::default()
                .fg(palette.header_fg)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg)),
        );
    f.render_widget(header, chunks[0]);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Select mode",
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  [1] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Control Computer   ",
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled("(local tools)", Style::default().fg(palette.muted_fg)),
        ]),
        Line::from(vec![
            Span::styled(
                "  [2] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Control Browser    ",
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(
                "(isolated agent browser)",
                Style::default().fg(palette.muted_fg),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  [3] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Both", Style::default().fg(palette.primary_fg)),
        ]),
        Line::from(vec![
            Span::styled(
                "  [w] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Workspaces", Style::default().fg(palette.primary_fg)),
        ]),
        Line::from(vec![
            Span::styled(
                "  [s] ",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Settings", Style::default().fg(palette.primary_fg)),
            Span::styled(
                format!(" (theme {}, tool mode {})", theme.label, tool_mode.label()),
                Style::default().fg(palette.muted_fg),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [q] ", Style::default().fg(palette.danger_fg)),
            Span::styled("Quit", Style::default().fg(palette.muted_fg)),
        ]),
    ];

    let select = Paragraph::new(lines).block(
        Block::default()
            .title(" Mode ")
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(select, chunks[1]);
}

async fn run_ngrok_auth_setup(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    initial_error: Option<String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    if initial_error.is_none() && state.lock().await.ngrok_authtoken().is_some() {
        return Ok(true);
    }

    let config_path = app_config_path()?;
    let config_path_text = config_path.to_string_lossy().into_owned();
    let mut input = String::new();
    let mut error_message = initial_error;
    let mut toast: Option<(&str, (u16, u16), Instant)> = None;

    loop {
        if let Some((_, _, t)) = &toast
            && t.elapsed().as_secs() >= 2
        {
            toast = None;
        }

        let (current_theme, current_tool_mode) = {
            let app = state.lock().await;
            (app.current_theme(), app.tool_mode)
        };
        let toast_ref = toast
            .as_ref()
            .filter(|(_, _, t)| t.elapsed().as_secs() < 2)
            .map(|(m, pos, _)| (*m, *pos));
        let mut ngrok_setup_copy_area = Rect::default();
        terminal.draw(|f| {
            let anchor_area = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(16),
                    Constraint::Min(0),
                ])
                .split(f.area())[1];
            ngrok_setup_copy_area = ngrok_auth_setup_copy_area(anchor_area);
            draw_mode_select(f, current_theme, current_tool_mode);
            draw_ngrok_auth_setup(
                f,
                current_theme,
                anchor_area,
                &config_path_text,
                &masked_secret_preview(&input),
                error_message.as_deref(),
            );
            if let Some((message, pos)) = toast_ref {
                render_toast(f, current_theme.palette, message, pos);
            }
        })?;

        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
                    KeyCode::Enter => {
                        let token = normalize_ngrok_authtoken_input(&input);
                        if token.is_empty() {
                            error_message = Some("NGROK_AUTHTOKEN cannot be empty".into());
                            continue;
                        }
                        {
                            let mut app = state.lock().await;
                            app.set_ngrok_authtoken(Some(token.clone()));
                        }
                        match flush_config(&state, true).await {
                            Ok(_) => {
                                state.lock().await.log(
                                    "INFO",
                                    format!(
                                        "Saved ngrok authtoken to {}",
                                        config_path.to_string_lossy()
                                    ),
                                );
                                return Ok(true);
                            }
                            Err(error) => {
                                state.lock().await.set_ngrok_authtoken(None);
                                error_message = Some(format!(
                                    "Failed to save ~/.moondesk/config.toml: {error}"
                                ));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        error_message = None;
                    }
                    KeyCode::Char(c) => {
                        if key_is_clipboard_paste(&key) {
                            if let Some(text) = clipboard_paste() {
                                input.push_str(&normalize_ngrok_authtoken_input(&text));
                                error_message = None;
                            }
                        } else {
                            input.push(c);
                            error_message = None;
                        }
                    }
                    KeyCode::Insert if key_is_clipboard_paste(&key) => {
                        if let Some(text) = clipboard_paste() {
                            input.push_str(&normalize_ngrok_authtoken_input(&text));
                            error_message = None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                input.push_str(&normalize_ngrok_authtoken_input(&text));
                error_message = None;
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    && rect_contains(ngrok_setup_copy_area, mouse.column, mouse.row) =>
            {
                let message = if clipboard_copy(NGROK_SETUP_URL) {
                    "Copied!"
                } else {
                    "Copy failed"
                };
                toast = Some((message, (mouse.column, mouse.row), Instant::now()));
            }
            _ => {}
        }
    }
}

async fn run_ngrok_domain_setup(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<bool, Box<dyn std::error::Error>> {
    if state.lock().await.ngrok_domain.is_some() {
        return Ok(true);
    }

    let config_path = app_config_path()?;
    let mut input = String::new();
    let mut error_message: Option<String> = None;

    loop {
        let (current_theme, current_tool_mode) = {
            let app = state.lock().await;
            (app.current_theme(), app.tool_mode)
        };
        terminal.draw(|f| {
            let anchor_area = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(16),
                    Constraint::Min(0),
                ])
                .split(f.area())[1];
            draw_mode_select(f, current_theme, current_tool_mode);
            draw_ngrok_domain_setup(
                f,
                current_theme,
                anchor_area,
                &input,
                error_message.as_deref(),
            );
        })?;

        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
                    KeyCode::Enter => {
                        let domain = match normalize_ngrok_domain(&input) {
                            Ok(Some(domain)) => domain,
                            Ok(None) => {
                                error_message = Some("ngrok domain cannot be empty".into());
                                continue;
                            }
                            Err(error) => {
                                error_message = Some(error);
                                continue;
                            }
                        };
                        {
                            let mut app = state.lock().await;
                            app.set_ngrok_domain(Some(domain.clone()));
                        }
                        match flush_config(&state, true).await {
                            Ok(_) => {
                                state.lock().await.log(
                                    "INFO",
                                    format!(
                                        "Saved ngrok domain to {}",
                                        config_path.to_string_lossy()
                                    ),
                                );
                                return Ok(true);
                            }
                            Err(error) => {
                                state.lock().await.set_ngrok_domain(None);
                                error_message = Some(format!(
                                    "Failed to save ~/.moondesk/config.toml: {error}"
                                ));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        error_message = None;
                    }
                    KeyCode::Char(c) => {
                        if key_is_clipboard_paste(&key) {
                            if let Some(text) = clipboard_paste() {
                                input.push_str(&normalize_ngrok_domain_input(&text));
                                error_message = None;
                            }
                        } else {
                            input.push(c);
                            error_message = None;
                        }
                    }
                    KeyCode::Insert if key_is_clipboard_paste(&key) => {
                        if let Some(text) = clipboard_paste() {
                            input.push_str(&normalize_ngrok_domain_input(&text));
                            error_message = None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                input.push_str(&normalize_ngrok_domain_input(&text));
                error_message = None;
            }
            _ => {}
        }
    }
}

fn normalize_ngrok_domain_input(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(url) = reqwest::Url::parse(trimmed)
        && let Some(host) = url.host_str()
    {
        return host.to_string();
    }
    trimmed.to_string()
}

fn draw_ngrok_domain_setup(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    anchor_area: Rect,
    domain_value: &str,
    error_message: Option<&str>,
) {
    let palette = theme.palette;
    let modal_bg = palette.modal_bg;
    let modal_fg = palette.modal_fg;

    let modal_area = centered_rect(90, 12, anchor_area);
    f.render_widget(Clear, modal_area);
    let modal_block = Block::default()
        .title(" ngrok domain ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg))
        .style(Style::default().fg(modal_fg).bg(modal_bg));
    f.render_widget(modal_block, modal_area);

    let inner = Rect::new(
        modal_area.x.saturating_add(1),
        modal_area.y.saturating_add(1),
        modal_area.width.saturating_sub(2),
        modal_area.height.saturating_sub(2),
    );
    let content_area = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let modal_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);

    let step_style = Style::default()
        .fg(palette.title_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let body_lines = vec![
        Line::from(Span::styled("ngrok domain setup", step_style)),
        Line::from(""),
        Line::from(Span::styled(
            "Enter your ngrok static domain (e.g. my-app.ngrok-free.dev)",
            step_style,
        )),
    ];
    let body = Paragraph::new(body_lines)
        .style(Style::default().fg(modal_fg).bg(modal_bg))
        .wrap(Wrap { trim: false });
    f.render_widget(body, modal_chunks[0]);

    let input_line = if domain_value.is_empty() {
        "_".to_string()
    } else {
        domain_value.to_string()
    };
    let input_widget = Paragraph::new(format!("  {input_line}"))
        .style(Style::default().fg(palette.title_fg).bg(modal_bg))
        .block(
            Block::default()
                .title(" NGROK_DOMAIN ")
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg))
                .style(Style::default().fg(modal_fg).bg(modal_bg)),
        );
    f.render_widget(input_widget, modal_chunks[1]);

    let footer = if let Some(message) = error_message {
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(palette.danger_fg).bg(modal_bg),
        )))
    } else {
        Paragraph::new(Line::from(Span::styled(
            "[Enter] Save  [q/Esc] Quit  [Paste/Ctrl+V] Insert domain",
            Style::default().fg(palette.muted_fg).bg(modal_bg),
        )))
    };
    f.render_widget(footer, modal_chunks[2]);
}

fn masked_secret_preview(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    let visible = chars.len().min(4);
    let masked_len = chars.len().saturating_sub(visible);
    let mut preview = "*".repeat(masked_len);
    preview.extend(chars[chars.len() - visible..].iter());
    preview
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn wrapped_line_hit_area(area: Rect, lines: &[Line<'_>], line_index: usize) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let marker_color = Color::Rgb(1, 2, 3);
    let marker_style = Style::default().bg(marker_color);
    let mut measured_lines = lines.to_vec();
    let target_line = measured_lines.get_mut(line_index)?;
    target_line.style = target_line.style.patch(marker_style);
    for span in &mut target_line.spans {
        span.style = span.style.patch(marker_style);
    }

    let measurement_area = Rect::new(0, 0, area.width, area.height);
    let mut measurement_buffer = Buffer::empty(measurement_area);
    Paragraph::new(measured_lines)
        .wrap(Wrap { trim: false })
        .render(measurement_area, &mut measurement_buffer);

    let mut first_row = None;
    let mut last_row = None;
    for row in 0..measurement_area.height {
        let contains_target = (0..measurement_area.width)
            .any(|column| measurement_buffer[(column, row)].bg == marker_color);
        if contains_target {
            first_row.get_or_insert(row);
            last_row = Some(row);
        }
    }

    let first_row = first_row?;
    let last_row = last_row?;

    Some(Rect::new(
        area.x,
        area.y.saturating_add(first_row),
        area.width,
        last_row.saturating_sub(first_row).saturating_add(1),
    ))
}

fn primary_mcp_url_line_index(
    show_guide: bool,
    is_returning_user: bool,
    bootstrap_status_visible: bool,
) -> Option<usize> {
    if show_guide && !is_returning_user {
        Some(CONNECT_GUIDE_PRIMARY_MCP_URL_LINE)
    } else if !show_guide && !bootstrap_status_visible {
        Some(STATUS_PRIMARY_MCP_URL_LINE)
    } else {
        None
    }
}

fn ngrok_auth_setup_modal_area(anchor_area: Rect) -> Rect {
    centered_rect(90, 15, anchor_area)
}

fn ngrok_auth_setup_content_area(anchor_area: Rect) -> Rect {
    let modal_area = ngrok_auth_setup_modal_area(anchor_area);
    let inner = Rect::new(
        modal_area.x.saturating_add(1),
        modal_area.y.saturating_add(1),
        modal_area.width.saturating_sub(2),
        modal_area.height.saturating_sub(2),
    );
    inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    })
}

fn ngrok_auth_setup_copy_area(anchor_area: Rect) -> Rect {
    let content_area = ngrok_auth_setup_content_area(anchor_area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);
    let body = chunks[0];
    if body.height <= 2 {
        return Rect::new(body.x, body.y, 0, 0);
    }
    Rect::new(body.x, body.y.saturating_add(2), body.width, 2)
}

fn draw_ngrok_auth_setup(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    anchor_area: Rect,
    _config_path: &str,
    masked_value: &str,
    error_message: Option<&str>,
) {
    let palette = theme.palette;
    let modal_bg = palette.modal_bg;
    let modal_fg = palette.modal_fg;

    let modal_area = ngrok_auth_setup_modal_area(anchor_area);
    f.render_widget(Clear, modal_area);
    let modal_block = Block::default()
        .title(" ngrok auth ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg))
        .style(Style::default().fg(modal_fg).bg(modal_bg));
    f.render_widget(modal_block, modal_area);
    let content_area = ngrok_auth_setup_content_area(anchor_area);

    let modal_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);

    let link_style = Style::default()
        .fg(palette.primary_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let step_style = Style::default()
        .fg(palette.title_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let body_lines = vec![
        Line::from(Span::styled("ngrok setup required", step_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("1. Open in browser and get your authtoken", step_style),
            Span::raw(" "),
            Span::styled(
                "(click to copy)",
                Style::default().fg(palette.secondary_fg).bg(modal_bg),
            ),
        ]),
        Line::from(vec![
            Span::raw("   "),
            Span::styled(NGROK_SETUP_URL, link_style),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "2. Paste the token or ngrok config command below",
            step_style,
        )),
    ];
    let body = Paragraph::new(body_lines)
        .style(Style::default().fg(modal_fg).bg(modal_bg))
        .wrap(Wrap { trim: false });
    f.render_widget(body, modal_chunks[0]);

    let input_line = if masked_value.is_empty() {
        "_".to_string()
    } else {
        masked_value.to_string()
    };
    let input = Paragraph::new(format!("  {input_line}"))
        .style(Style::default().fg(palette.title_fg).bg(modal_bg))
        .block(
            Block::default()
                .title(" NGROK_AUTHTOKEN ")
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg))
                .style(Style::default().fg(modal_fg).bg(modal_bg)),
        );
    f.render_widget(input, modal_chunks[1]);

    let footer = if let Some(message) = error_message {
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(palette.danger_fg).bg(modal_bg),
        )))
    } else {
        Paragraph::new(Line::from(Span::styled(
            "[Enter] Save  [q/Esc] Quit  [Paste/Ctrl+V] Insert token",
            Style::default().fg(palette.muted_fg).bg(modal_bg),
        )))
    };
    f.render_widget(footer, modal_chunks[2]);
}

fn render_theme_background(f: &mut Frame, palette: theme::Palette) {
    let mut style = Style::default().bg(palette.background_bg);
    if palette.background_bg != Color::Reset {
        style = style.fg(palette.primary_fg);
    }
    f.render_widget(Block::default().style(style), f.area());
}

fn render_toast(f: &mut Frame, palette: theme::Palette, msg: &str, pos: (u16, u16)) {
    let area = f.area();
    let (col, row) = pos;
    let label = format!(" {msg} ");
    let w = label.len() as u16;
    let x = col.saturating_add(1).min(area.width.saturating_sub(w));
    let y = if row > 0 { row - 1 } else { row + 1 }.min(area.height.saturating_sub(1));
    let toast_area = Rect::new(x, y, w, 1);
    let toast_widget = Paragraph::new(label).style(
        Style::default()
            .bg(palette.toast_bg)
            .fg(palette.toast_fg)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(toast_widget, toast_area);
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(percent_x)
        .saturating_div(100)
        .max(44);
    let width = width.min(area.width.saturating_sub(2).max(1));
    let popup_height = height.min(area.height.saturating_sub(2).max(1));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;
    Rect::new(x, y, width, popup_height)
}

fn draw_prompt(f: &mut Frame, palette: theme::Palette, prompt_title: &str, input: &str) {
    render_theme_background(f, palette);
    let area = centered_rect(60, 20, f.area());
    let block = Block::default()
        .title(prompt_title)
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg))
        .style(Style::default().fg(palette.modal_fg).bg(palette.modal_bg));

    let text = Paragraph::new(format!("> {input}_"))
        .style(Style::default().fg(palette.primary_fg).bg(palette.modal_bg))
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(text, area);
}

async fn run_prompt(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    palette: theme::Palette,
    prompt_title: &str,
    initial_value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut input = initial_value.to_string();
    loop {
        terminal.draw(|f| draw_prompt(f, palette, prompt_title, &input))?;

        if crossterm::event::poll(std::time::Duration::from_millis(100))? {
            let event = crossterm::event::read()?;
            match event {
                crossterm::event::Event::Paste(text) => {
                    input.push_str(&text);
                }
                crossterm::event::Event::Key(key) => {
                    if key.kind != crossterm::event::KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        crossterm::event::KeyCode::Enter => return Ok(Some(input)),
                        crossterm::event::KeyCode::Esc => return Ok(None),
                        crossterm::event::KeyCode::Backspace => {
                            input.pop();
                        }
                        crossterm::event::KeyCode::Insert if key_is_clipboard_paste(&key) => {
                            if let Some(text) = clipboard_paste() {
                                input.push_str(&text);
                            }
                        }
                        crossterm::event::KeyCode::Char(c) => {
                            if key_is_clipboard_paste(&key) {
                                if let Some(text) = clipboard_paste() {
                                    input.push_str(&text);
                                }
                            } else if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                input.push(c);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone)]
struct WorkspaceUiRow {
    config: WorkspaceConfig,
    availability: WorkspaceAvailability,
    connected: bool,
    accepting_requests: bool,
    in_flight_requests: usize,
    request_count: u64,
}

async fn workspace_ui_snapshot(
    state: &SharedState,
) -> (
    &'static theme::ThemeDef,
    Option<String>,
    Vec<WorkspaceUiRow>,
) {
    let app = state.lock().await;
    let public_base = app.ngrok_url.clone().or_else(|| {
        app.ngrok_domain
            .as_ref()
            .map(|domain| format!("https://{domain}"))
    });
    let rows = app
        .workspaces
        .iter()
        .map(|workspace| {
            let runtime = app.workspace_runtimes.get(&workspace.id);
            WorkspaceUiRow {
                config: workspace.clone(),
                availability: workspace_availability(&workspace.root),
                connected: runtime.is_some_and(|runtime| runtime.remote_connected()),
                accepting_requests: runtime.is_some_and(|runtime| runtime.accepting_requests()),
                in_flight_requests: runtime.map_or(0, |runtime| runtime.in_flight_requests()),
                request_count: runtime.map_or(0, |runtime| runtime.request_count()),
            }
        })
        .collect();
    (app.current_theme(), public_base, rows)
}

fn workspace_public_mcp_url(base: Option<&str>, workspace: &WorkspaceConfig) -> Option<String> {
    base.map(|base| format!("{}/{}/mcp", base.trim_end_matches('/'), workspace.mcp_slug))
}

fn normalize_workspace_path_input(value: &str) -> std::io::Result<PathBuf> {
    let value = value.trim().trim_matches(['\'', '"']);
    if value == "~" {
        return user_home_dir();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return Ok(user_home_dir()?.join(rest));
    }
    Ok(PathBuf::from(value))
}

#[cfg(target_os = "windows")]
struct WindowsComApartment;

#[cfg(target_os = "windows")]
impl WindowsComApartment {
    fn initialize_sta() -> Result<Self, String> {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        result
            .ok()
            .map_err(|error| format!("failed to initialize the Windows folder picker: {error}"))?;
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsComApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

#[cfg(target_os = "windows")]
fn configure_windows_workspace_folder_dialog(
    dialog: &windows::Win32::UI::Shell::IFileOpenDialog,
) -> Result<(), String> {
    use windows::{
        Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS},
        core::PCWSTR,
    };

    unsafe {
        let options = dialog
            .GetOptions()
            .map_err(|error| format!("failed to read Windows folder picker options: {error}"))?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .map_err(|error| format!("failed to configure the Windows folder picker: {error}"))?;

        let title: Vec<u16> = "Choose a MoonDesk workspace\0".encode_utf16().collect();
        dialog
            .SetTitle(PCWSTR(title.as_ptr()))
            .map_err(|error| format!("failed to title the Windows folder picker: {error}"))?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn pick_workspace_folder_blocking() -> Result<Option<PathBuf>, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
        UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH},
    };

    const HRESULT_CANCELLED: i32 = 0x8007_04C7_u32 as i32;

    let _apartment = WindowsComApartment::initialize_sta()?;
    let dialog: IFileOpenDialog = unsafe {
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(|error| {
            format!("failed to create the Windows Explorer folder picker: {error}")
        })?
    };
    configure_windows_workspace_folder_dialog(&dialog)?;

    unsafe {
        if let Err(error) = dialog.Show(None) {
            return if error.code().0 == HRESULT_CANCELLED {
                Ok(None)
            } else {
                Err(format!("Windows Explorer folder picker failed: {error}"))
            };
        }

        let item = dialog
            .GetResult()
            .map_err(|error| format!("failed to read the selected Windows folder: {error}"))?;
        let display_name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|error| format!("failed to resolve the selected Windows folder: {error}"))?;
        if display_name.is_null() {
            return Err("Windows Explorer returned an empty folder selection".into());
        }

        let mut len = 0usize;
        while *display_name.0.add(len) != 0 {
            len += 1;
        }
        let selected = OsString::from_wide(std::slice::from_raw_parts(display_name.0, len));
        CoTaskMemFree(Some(display_name.0.cast()));

        Ok(Some(PathBuf::from(selected)))
    }
}

#[cfg(target_os = "macos")]
fn pick_workspace_folder_blocking() -> Result<Option<PathBuf>, String> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"Choose a MoonDesk workspace\")",
        ])
        .output()
        .map_err(|error| format!("failed to open the macOS folder picker: {error}"))?;
    if !output.status.success() {
        // AppleScript returns a non-zero status when the user presses Cancel.
        return Ok(None);
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(selected)))
    }
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn pick_workspace_folder_blocking() -> Result<Option<PathBuf>, String> {
    const PICKERS: &[(&str, &[&str])] = &[
        (
            "zenity",
            &[
                "--file-selection",
                "--directory",
                "--title=Choose a MoonDesk workspace",
            ],
        ),
        ("kdialog", &["--getexistingdirectory"]),
    ];
    for (program, args) in PICKERS {
        let output = match std::process::Command::new(program).args(*args).output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("failed to open {program}: {error}")),
        };
        if !output.status.success() {
            return Ok(None);
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(selected)))
        };
    }
    Err("no supported graphical folder picker was found (install zenity or kdialog)".into())
}

#[cfg(target_os = "windows")]
async fn pick_workspace_folder() -> Result<Option<PathBuf>, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("moondesk-explorer-picker".into())
        .spawn(move || {
            let _ = sender.send(pick_workspace_folder_blocking());
        })
        .map_err(|error| format!("failed to start the Windows Explorer picker thread: {error}"))?;

    receiver
        .await
        .map_err(|_| "Windows Explorer picker thread exited unexpectedly".to_string())?
}

#[cfg(not(target_os = "windows"))]
async fn pick_workspace_folder() -> Result<Option<PathBuf>, String> {
    tokio::task::spawn_blocking(pick_workspace_folder_blocking)
        .await
        .map_err(|error| format!("folder picker task failed: {error}"))?
}

#[derive(Default)]
struct WorkspaceHitAreas {
    project_rows: Vec<(usize, Rect)>,
    url: Option<Rect>,
    add: Option<Rect>,
    browse: Option<Rect>,
    rename: Option<Rect>,
    reveal: Option<Rect>,
    copy: Option<Rect>,
    rotate: Option<Rect>,
    remove: Option<Rect>,
}

fn workspace_detail_sections(inner: Rect) -> (Rect, Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(inner);
    (sections[0], sections[1])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceUiAction {
    Back,
    MoveUp,
    MoveDown,
    Select(usize),
    AddPath,
    BrowseAdd,
    Rename,
    Reveal,
    Copy,
    Rotate,
    Remove,
}

fn workspace_action_from_event(
    event: Event,
    hit_areas: &WorkspaceHitAreas,
) -> Option<WorkspaceUiAction> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(WorkspaceUiAction::Back),
            KeyCode::Up => Some(WorkspaceUiAction::MoveUp),
            KeyCode::Down => Some(WorkspaceUiAction::MoveDown),
            KeyCode::Char('a') => Some(WorkspaceUiAction::AddPath),
            KeyCode::Char('b') => Some(WorkspaceUiAction::BrowseAdd),
            KeyCode::Char('r') => Some(WorkspaceUiAction::Rename),
            KeyCode::Enter | KeyCode::Char('v') => Some(WorkspaceUiAction::Reveal),
            KeyCode::Char('c') => Some(WorkspaceUiAction::Copy),
            KeyCode::Char('x') => Some(WorkspaceUiAction::Rotate),
            KeyCode::Char('d') => Some(WorkspaceUiAction::Remove),
            _ => None,
        },
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) => {
            for (index, area) in &hit_areas.project_rows {
                if rect_contains(*area, mouse.column, mouse.row) {
                    return Some(WorkspaceUiAction::Select(*index));
                }
            }
            if hit_areas
                .url
                .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                || hit_areas
                    .reveal
                    .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
            {
                return Some(WorkspaceUiAction::Reveal);
            }
            for (area, action) in [
                (hit_areas.add, WorkspaceUiAction::AddPath),
                (hit_areas.browse, WorkspaceUiAction::BrowseAdd),
                (hit_areas.rename, WorkspaceUiAction::Rename),
                (hit_areas.copy, WorkspaceUiAction::Copy),
                (hit_areas.rotate, WorkspaceUiAction::Rotate),
                (hit_areas.remove, WorkspaceUiAction::Remove),
            ] {
                if area.is_some_and(|area| rect_contains(area, mouse.column, mouse.row)) {
                    return Some(action);
                }
            }
            None
        }
        _ => None,
    }
}

async fn run_workspaces(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut selected = 0usize;
    let mut revealed: Option<(WorkspaceId, Instant)> = None;
    let mut confirm_rotate: Option<WorkspaceId> = None;
    let mut confirm_remove: Option<WorkspaceId> = None;
    let mut message: Option<String> = None;

    loop {
        let (current_theme, public_base, rows) = workspace_ui_snapshot(&state).await;
        if rows.is_empty() {
            state.lock().await.log(
                "ERROR",
                "Workspace manager found no registered workspaces; restart MoonDesk to repair the workspace registry"
                    .into(),
            );
            return Ok(());
        }
        selected = selected.min(rows.len().saturating_sub(1));
        let selected_row = rows[selected].clone();
        let reveal_active = revealed.as_ref().is_some_and(|(workspace_id, deadline)| {
            workspace_id == &selected_row.config.id && Instant::now() < *deadline
        });
        if revealed
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
        {
            revealed = None;
        }
        let selected_url = workspace_public_mcp_url(public_base.as_deref(), &selected_row.config);

        let mut hit_areas = WorkspaceHitAreas::default();
        terminal.draw(|f| {
            draw_workspaces(
                f,
                WorkspacesView {
                    current_theme,
                    rows: &rows,
                    selected,
                    selected_url: selected_url.as_deref(),
                    reveal_url: reveal_active,
                    message: message.as_deref(),
                    confirm_rotate: confirm_rotate.as_ref() == Some(&selected_row.config.id),
                    confirm_remove: confirm_remove.as_ref() == Some(&selected_row.config.id),
                },
                &mut hit_areas,
            )
        })?;

        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        let Some(action) = workspace_action_from_event(event::read()?, &hit_areas) else {
            continue;
        };

        match action {
            WorkspaceUiAction::Back => return Ok(()),
            WorkspaceUiAction::MoveUp => {
                selected = selected.saturating_sub(1);
                confirm_rotate = None;
                confirm_remove = None;
                message = None;
            }
            WorkspaceUiAction::MoveDown => {
                selected = (selected + 1).min(rows.len().saturating_sub(1));
                confirm_rotate = None;
                confirm_remove = None;
                message = None;
            }
            WorkspaceUiAction::Select(index) => {
                selected = index.min(rows.len().saturating_sub(1));
                confirm_rotate = None;
                confirm_remove = None;
                message = None;
            }
            WorkspaceUiAction::AddPath => {
                confirm_rotate = None;
                confirm_remove = None;
                let Some(path_input) = run_prompt(
                    terminal,
                    current_theme.palette,
                    "Workspace folder path:",
                    "",
                )
                .await?
                else {
                    continue;
                };
                let root = match normalize_workspace_path_input(&path_input) {
                    Ok(root) => root,
                    Err(error) => {
                        message = Some(format!("Invalid workspace path: {error}"));
                        continue;
                    }
                };
                let default_name = workspaces::derive_workspace_name(&root);
                let Some(name) = run_prompt(
                    terminal,
                    current_theme.palette,
                    "Workspace name:",
                    &default_name,
                )
                .await?
                else {
                    continue;
                };
                match add_workspace(&state, name, root).await {
                    Ok(workspace) => {
                        let (_, _, refreshed) = workspace_ui_snapshot(&state).await;
                        selected = refreshed
                            .iter()
                            .position(|row| row.config.id == workspace.id)
                            .unwrap_or_else(|| refreshed.len().saturating_sub(1));
                        message = Some(format!("Added workspace {}", workspace.name));
                    }
                    Err(error) => message = Some(format!("Add failed: {error}")),
                }
            }
            WorkspaceUiAction::BrowseAdd => {
                confirm_rotate = None;
                confirm_remove = None;
                let root = match pick_workspace_folder().await {
                    Ok(Some(root)) => root,
                    Ok(None) => {
                        message = Some("Folder selection cancelled".into());
                        continue;
                    }
                    Err(error) => {
                        message = Some(format!("Folder picker failed: {error}"));
                        continue;
                    }
                };
                let default_name = workspaces::derive_workspace_name(&root);
                let Some(name) = run_prompt(
                    terminal,
                    current_theme.palette,
                    "Workspace name:",
                    &default_name,
                )
                .await?
                else {
                    continue;
                };
                match add_workspace(&state, name, root).await {
                    Ok(workspace) => {
                        let (_, _, refreshed) = workspace_ui_snapshot(&state).await;
                        selected = refreshed
                            .iter()
                            .position(|row| row.config.id == workspace.id)
                            .unwrap_or_else(|| refreshed.len().saturating_sub(1));
                        message = Some(format!("Added workspace {}", workspace.name));
                    }
                    Err(error) => message = Some(format!("Add failed: {error}")),
                }
            }
            WorkspaceUiAction::Rename => {
                confirm_rotate = None;
                confirm_remove = None;
                if let Some(name) = run_prompt(
                    terminal,
                    current_theme.palette,
                    "Rename workspace:",
                    &selected_row.config.name,
                )
                .await?
                {
                    match rename_workspace(&state, &selected_row.config.id, name).await {
                        Ok(()) => message = Some("Workspace renamed".into()),
                        Err(error) => message = Some(format!("Rename failed: {error}")),
                    }
                }
            }
            WorkspaceUiAction::Rotate => {
                confirm_remove = None;
                if confirm_rotate.as_ref() == Some(&selected_row.config.id) {
                    match rotate_workspace_secret(&state, &selected_row.config.id).await {
                        Ok(_) => {
                            revealed = None;
                            confirm_rotate = None;
                            message = Some(
                                "MCP URL rotated. Update the ChatGPT app using this workspace."
                                    .into(),
                            );
                        }
                        Err(error) => {
                            confirm_rotate = None;
                            message = Some(format!("Rotate failed: {error}"));
                        }
                    }
                } else {
                    confirm_rotate = Some(selected_row.config.id.clone());
                    message = Some("Press x again to rotate this workspace URL".into());
                }
            }
            WorkspaceUiAction::Remove => {
                confirm_rotate = None;
                if confirm_remove.as_ref() == Some(&selected_row.config.id) {
                    let removed_name = selected_row.config.name.clone();
                    match remove_workspace(&state, &selected_row.config.id).await {
                        Ok(()) => {
                            selected = selected.saturating_sub(1);
                            revealed = None;
                            confirm_remove = None;
                            message = Some(format!("Removed workspace {removed_name}"));
                        }
                        Err(error) => {
                            confirm_remove = None;
                            message = Some(format!("Remove failed: {error}"));
                        }
                    }
                } else {
                    confirm_remove = Some(selected_row.config.id.clone());
                    message = Some("Press d again to remove this workspace".into());
                }
            }
            WorkspaceUiAction::Reveal => {
                confirm_rotate = None;
                confirm_remove = None;
                if selected_url.is_some() {
                    revealed = Some((
                        selected_row.config.id.clone(),
                        Instant::now() + MCP_URL_REVEAL_DURATION,
                    ));
                    message = Some("MCP URL revealed for 10s".into());
                } else {
                    message = Some("Start/configure ngrok before copying a public MCP URL".into());
                }
            }
            WorkspaceUiAction::Copy => {
                confirm_rotate = None;
                confirm_remove = None;
                match selected_url.as_deref() {
                    Some(url) => {
                        message = Some(if clipboard_copy(url) {
                            "MCP URL copied".into()
                        } else {
                            "Copy failed".into()
                        });
                    }
                    None => {
                        message = Some("No public MCP URL is available yet".into());
                    }
                }
            }
        }
    }
}

struct WorkspacesView<'a> {
    current_theme: &'static theme::ThemeDef,
    rows: &'a [WorkspaceUiRow],
    selected: usize,
    selected_url: Option<&'a str>,
    reveal_url: bool,
    message: Option<&'a str>,
    confirm_rotate: bool,
    confirm_remove: bool,
}

fn draw_workspaces(f: &mut Frame, view: WorkspacesView<'_>, hit_areas: &mut WorkspaceHitAreas) {
    *hit_areas = WorkspaceHitAreas::default();
    let WorkspacesView {
        current_theme,
        rows,
        selected,
        selected_url,
        reveal_url,
        message,
        confirm_rotate,
        confirm_remove,
    } = view;
    let palette = current_theme.palette;
    render_theme_background(f, palette);
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(4),
        ])
        .split(area);

    let connected = rows.iter().filter(|row| row.connected).count();
    let header = Paragraph::new(format!(
        "  Workspaces  {} registered · {} connected",
        rows.len(),
        connected
    ))
    .style(
        Style::default()
            .fg(palette.header_fg)
            .add_modifier(Modifier::BOLD),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(header, outer[0]);

    let body = if outer[1].width >= 96 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(outer[1])
    };

    let list_inner_height = body[0].height.saturating_sub(2);
    let visible_count = usize::from((list_inner_height / 2).max(1));
    let list_start = if selected < visible_count {
        0
    } else {
        selected + 1 - visible_count
    };
    let list_end = (list_start + visible_count).min(rows.len());
    let items = rows[list_start..list_end]
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let index = list_start + offset;
            let status = match (row.availability, row.connected, row.accepting_requests) {
                (WorkspaceAvailability::Unavailable, _, _) => "UNAVAILABLE",
                (_, _, false) => "DRAINING",
                (_, true, true) => "CONNECTED",
                _ => "READY",
            };
            let marker = if index == selected { ">" } else { " " };
            let style = if index == selected {
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.primary_fg)
            };
            ListItem::new(vec![
                Line::from(Span::styled(
                    format!(" {marker} {}  [{status}]", row.config.name),
                    style,
                )),
                Line::from(Span::styled(
                    format!("    {}", row.config.root.display()),
                    Style::default().fg(palette.muted_fg),
                )),
            ])
        })
        .collect::<Vec<_>>();
    let list_title = if rows.len() > visible_count {
        format!(
            " Projects  {}-{} of {} ",
            list_start + 1,
            list_end,
            rows.len()
        )
    } else {
        " Projects ".to_string()
    };
    let list = List::new(items).block(
        Block::default()
            .title(list_title)
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(list, body[0]);
    let list_inner = Rect::new(
        body[0].x.saturating_add(1),
        body[0].y.saturating_add(1),
        body[0].width.saturating_sub(2),
        body[0].height.saturating_sub(2),
    );
    for offset in 0..(list_end - list_start) {
        let y = list_inner
            .y
            .saturating_add((offset as u16).saturating_mul(2));
        let height = 2.min(
            list_inner
                .y
                .saturating_add(list_inner.height)
                .saturating_sub(y),
        );
        if height > 0 {
            hit_areas.project_rows.push((
                list_start + offset,
                Rect::new(list_inner.x, y, list_inner.width, height),
            ));
        }
    }

    let row = &rows[selected];
    let availability = match row.availability {
        WorkspaceAvailability::Available => "Available",
        WorkspaceAvailability::Unavailable => "Unavailable",
    };
    let connection = if row.connected { "Connected" } else { "Ready" };
    let url = match (selected_url, reveal_url) {
        (Some(url), true) => url.to_string(),
        (Some(_), false) => MCP_URL_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let url_style = if reveal_url {
        Style::default().fg(palette.info_fg)
    } else {
        Style::default().fg(palette.muted_fg)
    };
    let details_block = Block::default()
        .title(" Selected workspace ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg));
    let details_inner = details_block.inner(body[1]);
    f.render_widget(details_block, body[1]);
    let (metadata_area, url_area) = workspace_detail_sections(details_inner);

    let metadata = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Name           ", Style::default().fg(palette.muted_fg)),
            Span::styled(&row.config.name, Style::default().fg(palette.primary_fg)),
        ]),
        Line::from(vec![
            Span::styled("  Root           ", Style::default().fg(palette.muted_fg)),
            Span::styled(
                row.config.root.to_string_lossy(),
                Style::default().fg(palette.primary_fg),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Filesystem     ", Style::default().fg(palette.muted_fg)),
            Span::styled(availability, Style::default().fg(palette.primary_fg)),
        ]),
        Line::from(vec![
            Span::styled("  Connector      ", Style::default().fg(palette.muted_fg)),
            Span::styled(connection, Style::default().fg(palette.primary_fg)),
        ]),
        Line::from(vec![
            Span::styled("  Requests       ", Style::default().fg(palette.muted_fg)),
            Span::styled(
                row.request_count.to_string(),
                Style::default().fg(palette.primary_fg),
            ),
            Span::styled(
                format!("  \u{00B7} {} in flight", row.in_flight_requests),
                Style::default().fg(palette.muted_fg),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ChatGPT app    ", Style::default().fg(palette.muted_fg)),
            Span::styled(
                format!("MoonDesk \u{00B7} {}", row.config.name),
                Style::default().fg(palette.primary_fg),
            ),
        ]),
        Line::from(Span::styled(
            "  Browser runtime/session is shared by the MoonDesk host.",
            Style::default().fg(palette.muted_fg),
        )),
    ])
    .wrap(Wrap { trim: false });
    f.render_widget(metadata, metadata_area);

    let url_widget = Paragraph::new(Line::from(vec![
        Span::styled("  MCP Server URL ", Style::default().fg(palette.muted_fg)),
        Span::styled(url, url_style),
    ]))
    .wrap(Wrap { trim: false });
    f.render_widget(url_widget, url_area);
    if url_area.width > 0 && url_area.height > 0 {
        hit_areas.url = Some(url_area);
    }
    let footer_actions = [
        (" [a] Path ", WorkspaceUiAction::AddPath),
        (WORKSPACE_BROWSE_ACTION_LABEL, WorkspaceUiAction::BrowseAdd),
        ("[r] Name ", WorkspaceUiAction::Rename),
        ("[v] Reveal ", WorkspaceUiAction::Reveal),
        ("[c] Copy ", WorkspaceUiAction::Copy),
        ("[x] Rotate ", WorkspaceUiAction::Rotate),
        ("[d] Remove ", WorkspaceUiAction::Remove),
    ];
    let mut footer_spans = footer_actions
        .iter()
        .map(|(label, action)| {
            let foreground = if matches!(*action, WorkspaceUiAction::Remove) {
                palette.danger_fg
            } else {
                palette.key_fg
            };
            Span::styled(*label, Style::default().fg(foreground))
        })
        .collect::<Vec<_>>();
    footer_spans.push(Span::styled(
        "[Esc] Back",
        Style::default().fg(palette.muted_fg),
    ));
    let mut footer_lines = vec![Line::from(footer_spans)];
    if let Some(message) = message {
        let style = if confirm_rotate || confirm_remove {
            Style::default().fg(palette.warning_fg)
        } else {
            Style::default().fg(palette.muted_fg)
        };
        footer_lines.push(Line::from(Span::styled(format!(" {message}"), style)));
    }
    let footer = Paragraph::new(footer_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(footer, outer[2]);

    let footer_y = outer[2].y.saturating_add(1);
    let footer_right = outer[2].x.saturating_add(outer[2].width.saturating_sub(1));
    let mut footer_x = outer[2].x.saturating_add(1);
    for (label, action) in footer_actions {
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let available = footer_right.saturating_sub(footer_x);
        let rect = Rect::new(footer_x, footer_y, width.min(available), 1);
        if rect.width > 0 {
            match action {
                WorkspaceUiAction::AddPath => hit_areas.add = Some(rect),
                WorkspaceUiAction::BrowseAdd => hit_areas.browse = Some(rect),
                WorkspaceUiAction::Rename => hit_areas.rename = Some(rect),
                WorkspaceUiAction::Reveal => hit_areas.reveal = Some(rect),
                WorkspaceUiAction::Copy => hit_areas.copy = Some(rect),
                WorkspaceUiAction::Rotate => hit_areas.rotate = Some(rect),
                WorkspaceUiAction::Remove => hit_areas.remove = Some(rect),
                WorkspaceUiAction::Back
                | WorkspaceUiAction::MoveUp
                | WorkspaceUiAction::MoveDown
                | WorkspaceUiAction::Select(_) => {}
            }
        }
        footer_x = footer_x.saturating_add(width);
    }
}

async fn run_settings(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<(), Box<dyn std::error::Error>> {
    let themes = theme::all();
    let tool_modes = ToolMode::all();
    let mut confirm_reset_token_billing = false;
    let mut selected_row = {
        let app = state.lock().await;
        themes.iter().position(|t| t.id == app.theme).unwrap_or(0)
    };
    let total_rows = themes.len() + tool_modes.len() + 2;

    loop {
        let (
            current_theme,
            current_tool_mode,
            usage_totals,
            set_moondesk_as_co_author,
            ngrok_domain,
        ) = {
            let app = state.lock().await;
            (
                app.current_theme(),
                app.tool_mode,
                app.all_time_usage_totals(),
                app.set_moondesk_as_co_author,
                app.ngrok_domain.clone(),
            )
        };
        terminal.draw(|f| {
            draw_settings(
                f,
                SettingsView {
                    current_theme,
                    current_tool_mode,
                    set_moondesk_as_co_author,
                    ngrok_domain: ngrok_domain.as_deref(),
                    usage_totals: &usage_totals,
                    selected_row,
                    confirm_reset_token_billing,
                },
            )
        })?;

        if event::poll(UI_POLL_INTERVAL)?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Up => {
                    confirm_reset_token_billing = false;
                    selected_row = selected_row.saturating_sub(1);
                }
                KeyCode::Down => {
                    confirm_reset_token_billing = false;
                    if selected_row + 1 < total_rows {
                        selected_row += 1;
                    }
                }
                KeyCode::Enter => {
                    confirm_reset_token_billing = false;
                    let mut app = state.lock().await;
                    if selected_row < themes.len() {
                        let picked = themes[selected_row];
                        if app.theme != picked.id {
                            app.theme = picked.id.to_string();
                            app.log("INFO", format!("Theme changed to {}", picked.label));
                            app.mark_config_dirty();
                        }
                    } else {
                        let tool_mode_start = themes.len();
                        let tool_mode_end = tool_mode_start + tool_modes.len();
                        let settings_action_start = tool_mode_end;

                        if selected_row < tool_mode_end {
                            let picked = tool_modes[selected_row - tool_mode_start];
                            if app.tool_mode != picked {
                                app.tool_mode = picked;
                                app.log("INFO", format!("Tool mode: {}", picked.label()));
                                app.mark_config_dirty();
                            }
                        } else if selected_row == settings_action_start {
                            app.set_moondesk_as_co_author = !app.set_moondesk_as_co_author;
                            let enabled = app.set_moondesk_as_co_author;
                            app.log(
                                "INFO",
                                format!(
                                    "Set MoonDesk as co-author: {}",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            );
                            app.mark_config_dirty();
                        } else if selected_row == settings_action_start + 1 {
                            let previous_domain = app.ngrok_domain.clone();
                            let current_domain = previous_domain.clone().unwrap_or_default();
                            drop(app);
                            if let Some(new_domain) = run_prompt(
                                terminal,
                                current_theme.palette,
                                "Enter ngrok static domain (with/without https://, empty to clear):",
                                &current_domain,
                            )
                            .await?
                            {
                                let normalized = match normalize_ngrok_domain(&new_domain) {
                                    Ok(domain) => domain,
                                    Err(error) => {
                                        state.lock().await.log(
                                            "WARN",
                                            format!("Invalid ngrok domain: {error}"),
                                        );
                                        continue;
                                    }
                                };
                                let was_running = {
                                    let mut app = state.lock().await;
                                    app.set_ngrok_domain(normalized);
                                    app.log("INFO", "Updated ngrok static domain".into());
                                    app.ngrok_running
                                };
                                if was_running
                                    && let Err(error) = ngrok::restart(state.clone()).await
                                {
                                    {
                                        let mut app = state.lock().await;
                                        app.set_ngrok_domain(previous_domain.clone());
                                        app.log(
                                            "ERROR",
                                            format!(
                                                "Failed to restart ngrok after domain change: {error}"
                                            ),
                                        );
                                        app.log(
                                            "WARN",
                                            "Restoring the previous ngrok domain".into(),
                                        );
                                    }
                                    if let Err(restore_error) = ngrok::start(state.clone()).await {
                                        state.lock().await.log(
                                            "ERROR",
                                            format!(
                                                "Failed to restore the previous ngrok tunnel: {restore_error}"
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                KeyCode::Char('r') => {
                    if !confirm_reset_token_billing {
                        confirm_reset_token_billing = true;
                        continue;
                    }
                    let mut app = state.lock().await;
                    app.usage_by_model.clear();
                    app.log("INFO", "Tool token totals reset".into());
                    app.mark_config_dirty();
                    confirm_reset_token_billing = false;
                }
                _ => {
                    confirm_reset_token_billing = false;
                }
            }
        }
    }
}

struct SettingsView<'a> {
    current_theme: &'a theme::ThemeDef,
    current_tool_mode: ToolMode,
    set_moondesk_as_co_author: bool,
    ngrok_domain: Option<&'a str>,
    usage_totals: &'a UsageTotals,
    selected_row: usize,
    confirm_reset_token_billing: bool,
}

fn draw_settings(f: &mut Frame, view: SettingsView<'_>) {
    let SettingsView {
        current_theme,
        current_tool_mode,
        set_moondesk_as_co_author,
        ngrok_domain,
        usage_totals,
        selected_row,
        confirm_reset_token_billing,
    } = view;
    let themes = theme::all();
    let tool_modes = ToolMode::all();
    let palette = current_theme.palette;
    let cost_estimate = estimate_gpt_5_6_sol_tool_cost(usage_totals);
    render_theme_background(f, palette);
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    let header = Paragraph::new("  Settings")
        .style(
            Style::default()
                .fg(palette.header_fg)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg)),
        );
    f.render_widget(header, chunks[0]);

    let mut selected_line_idx = 0;
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Choose a theme",
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (idx, theme) in themes.iter().enumerate() {
        let selected = idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(" {} [{}] {}", marker, idx + 1, theme.label),
            name_style,
        )];
        if theme.id == current_theme.id {
            spans.push(Span::styled(
                "  [current]",
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!("     {}", theme.description),
            Style::default().fg(palette.muted_fg),
        )]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Choose a tool mode",
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    for (idx, tool_mode) in tool_modes.iter().enumerate() {
        let row_idx = themes.len() + idx;
        let selected = row_idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(" {} [{}] {}", marker, row_idx + 1, tool_mode.label()),
            name_style,
        )];
        if *tool_mode == current_tool_mode {
            spans.push(Span::styled(
                "  [current]",
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!("     {}", tool_mode.description()),
            Style::default().fg(palette.muted_fg),
        )]));
    }

    let co_author_row = themes.len() + tool_modes.len();
    let co_author_selected = co_author_row == selected_row;
    let co_author_marker = if co_author_selected { ">" } else { " " };
    let co_author_name_style = if co_author_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Commit attribution",
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    if co_author_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] Set MoonDesk as co-author",
            co_author_marker,
            co_author_row + 1
        ),
        co_author_name_style,
    )]));
    lines.push(Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            if set_moondesk_as_co_author {
                "[enabled]"
            } else {
                "[disabled]"
            },
            Style::default().fg(if set_moondesk_as_co_author {
                palette.success_fg
            } else {
                palette.muted_fg
            }),
        ),
    ]));

    lines.push(Line::from(vec![Span::styled(
        "     When enabled, MoonDesk automatically appends \"Co-Authored-By: MoonDesk\" to git commits and blocks manually written MoonDesk co-author trailers.",
        Style::default().fg(palette.muted_fg),
    )]));

    let domain_row = co_author_row + 1;
    let domain_selected = domain_row == selected_row;
    let domain_marker = if domain_selected { ">" } else { " " };
    let domain_name_style = if domain_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Connection",
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(Span::styled(
        "     Workspace-specific MCP URLs are managed from [w] Workspaces.",
        Style::default().fg(palette.muted_fg),
    )));
    if domain_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] Set ngrok static domain",
            domain_marker,
            domain_row + 1
        ),
        domain_name_style,
    )]));
    lines.push(Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            if let Some(domain) = ngrok_domain {
                format!("[{}]", domain)
            } else {
                "[not set]".to_string()
            },
            Style::default().fg(palette.muted_fg),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        "     Pro tip: Your permanent ngrok-free.dev domain is auto-saved above.",
        Style::default().fg(palette.muted_fg),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tool token estimate",
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("  Tool args ", Style::default().fg(palette.muted_fg)),
        Span::styled(
            usage_totals.tool_input_tokens.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
        Span::styled("   Tool results ", Style::default().fg(palette.muted_fg)),
        Span::styled(
            usage_totals.tool_output_tokens.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Total ", Style::default().fg(palette.muted_fg)),
        Span::styled(
            usage_totals.total_tokens.to_string(),
            Style::default()
                .fg(palette.secondary_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Tool calls ", Style::default().fg(palette.muted_fg)),
        Span::styled(
            usage_totals.tool_call_count.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "  Sol High equivalent ",
            Style::default().fg(palette.muted_fg),
        ),
        Span::styled(
            format!("~${}", format_usd_compact(cost_estimate.standard_usd)),
            Style::default()
                .fg(palette.success_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ($4 input / $0.40 cached input / $20 output per 1M)",
            Style::default().fg(palette.muted_fg),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "  Current Sol promotional rates checked 2026-09-03; OpenAI advertises them through at least 2026-11-21.",
        Style::default().fg(palette.muted_fg),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "  Cache-state range at standard context: ~${} - ~${} (cache read to cache write).",
            format_usd_compact(cost_estimate.cached_read_usd),
            format_usd_compact(cost_estimate.cache_write_usd)
        ),
        Style::default().fg(palette.muted_fg),
    )));
    lines.push(Line::from(Span::styled(
        "  Tool args are model output; tool results return as model input. Cache state, >272K context, ordinary chat, and hidden reasoning are not visible over MCP, so this is an MCP-attributable estimate, not exact ChatGPT spend.",
        Style::default().fg(palette.muted_fg),
    )));
    lines.push(Line::from(vec![
        Span::styled("  [r]", Style::default().fg(palette.warning_fg)),
        Span::styled(
            if confirm_reset_token_billing {
                " Press again to confirm tool token reset"
            } else {
                " Reset tool token totals"
            },
            Style::default().fg(if confirm_reset_token_billing {
                palette.danger_fg
            } else {
                palette.muted_fg
            }),
        ),
    ]));

    let visible_height = chunks[1].height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(visible_height);
    let target_scroll = (selected_line_idx as u16).saturating_sub(visible_height / 2);
    let scroll_y = target_scroll.min(max_scroll);

    let body = Paragraph::new(lines).scroll((scroll_y, 0)).block(
        Block::default()
            .title(" Theme, Tool Mode & Usage ")
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(body, chunks[1]);

    let keys = Paragraph::new(Line::from(vec![
        Span::styled("  [Up/Down]", Style::default().fg(palette.key_fg)),
        Span::raw(" Select  "),
        Span::styled("[Enter]", Style::default().fg(palette.success_fg)),
        Span::raw(" Apply  "),
        Span::styled(
            "[r]",
            Style::default().fg(if confirm_reset_token_billing {
                palette.danger_fg
            } else {
                palette.warning_fg
            }),
        ),
        Span::raw(if confirm_reset_token_billing {
            " Confirm reset  "
        } else {
            " Reset tool token totals  "
        }),
        Span::styled("[q/Esc]", Style::default().fg(palette.danger_fg)),
        Span::raw(" Back"),
    ]))
    .block(
        Block::default()
            .title(" Keys ")
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(keys, chunks[2]);
}

// ── Start services ──────────────────────────────────────────

struct StartedServices {
    browser_runtime: Option<Arc<BrowserRuntime>>,
    _host_runtime: HostRuntimeGuard,
    ngrok_start_error: Option<ngrok::StartFailure>,
}

const MAX_HEALTH_PROBE_BODY_BYTES: usize = 4 * 1024;

async fn port_hosts_moondesk(port: u16) -> bool {
    let endpoint = format!("http://127.0.0.1:{port}/");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    let Ok(mut response) = client.get(endpoint).send().await else {
        return false;
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_HEALTH_PROBE_BODY_BYTES as u64)
    {
        return false;
    }

    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > MAX_HEALTH_PROBE_BODY_BYTES {
                    return false;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return false,
        }
    }
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return false;
    };
    payload.get("status").and_then(serde_json::Value::as_str) == Some("ok")
        && payload.get("name").and_then(serde_json::Value::as_str) == Some("MoonDesk")
}

async fn start_services(
    state: SharedState,
    ui_events: UiEventSender,
) -> Result<StartedServices, String> {
    let (port, mode) = {
        let app = state.lock().await;
        (app.port, app.mode)
    };

    // Reserve the HTTP port before creating host services. A second MoonDesk instance should
    // fail cheaply without touching browser state or creating duplicate host services.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            let message = if port_hosts_moondesk(port).await {
                format!(
                    "MoonDesk is already running on port {port}. Add this folder from [w] Workspaces in the running instance."
                )
            } else {
                format!("Failed to bind port {port}: {error}")
            };
            state.lock().await.log("ERROR", message.clone());
            return Err(message);
        }
    };

    if mode.browser_enabled() {
        state.lock().await.log(
            "INFO",
            "Isolated agent-browser runtime ready; no browser process starts until the first browser operation"
                .into(),
        );
    }

    // Browser mode creates only a lightweight runtime handle. The first browser_command/view_page
    // request starts a clean isolated agent browser; no personal browser profile is ever attached.
    let browser_runtime = mode
        .browser_enabled()
        .then(|| Arc::new(BrowserRuntime::new(state.clone())));

    let host_control_token: Arc<str> = Arc::from(format!("{}{}", Uuid::new_v4(), Uuid::new_v4()));
    let host_runtime = write_host_runtime_registration(port, &host_control_token)
        .map_err(|error| format!("failed to publish local host registration: {error}"))?;
    let command_jobs = { state.lock().await.command_jobs.clone() };
    let router = server::router(
        state.clone(),
        browser_runtime.clone(),
        command_jobs,
        ui_events,
        host_control_token,
    );
    let server_state = state.clone();
    let handle = tokio::spawn(async move {
        let result = axum::serve(listener, router).await;
        let mut app = server_state.lock().await;
        app.server_running = false;
        match result {
            Ok(()) => app.log("WARN", "MCP server exited".into()),
            Err(error) => app.log("ERROR", format!("MCP server failed: {error}")),
        }
    });

    {
        let mut app = state.lock().await;
        app.server_running = true;
        app.server_handle = Some(handle);
        app.log("INFO", format!("MCP Server started on port {port}"));
    }

    // Start ngrok
    let ngrok_start_error = match ngrok::start(state.clone()).await {
        Ok(()) => None,
        Err(error) => {
            state.lock().await.log("ERROR", format!("ngrok: {error}"));
            Some(error)
        }
    };

    Ok(StartedServices {
        browser_runtime,
        _host_runtime: host_runtime,
        ngrok_start_error,
    })
}

// ── Phase 2: Main TUI ──────────────────────────────────────

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    mut ui_events: UiEventReceiver,
    interrupts: InterruptState,
) -> Result<AppExit, Box<dyn std::error::Error>> {
    let mut log_scroll: usize = 0;
    let mut log_follow_tail = true;
    let mut command_scroll: usize = 0;
    let mut command_follow_tail = true;
    let mut last_log_view = PanelScrollView::default();
    let mut last_command_view = PanelScrollView::default();
    let mut bottom_panel_areas = BottomPanelAreas::default();
    let mut bottom_panel_hits = BottomPanelHitMaps::default();
    let mut dashboard_hit_areas = DashboardHitAreas::default();
    let mut focused_bottom_panel = DashboardFocus::ShellCommands;
    let mut workspace_filter = WorkspaceFilter::All;
    let mut workspace_scroll: usize = 0;
    let mut workspace_visible_count: usize = 1;
    let mut clear_cutoffs: std::collections::HashMap<WorkspaceFilter, ObservabilityCutoff> =
        std::collections::HashMap::new();
    let mut selected_log: Option<usize> = None;
    let mut selected_command: Option<usize> = None;
    let mut expanded_log: Option<usize> = None;
    let mut expanded_command: Option<usize> = None;
    let mut last_log_count: usize;
    let mut last_command_count: usize;
    let mut selection = Selection::new();
    // (message, position (col, row), created_at)
    let mut toast: Option<(&str, (u16, u16), Instant)> = None;
    #[allow(unused_assignments)]
    let mut screen_lines: Vec<String> = vec![];
    let mut last_animation_snapshot = String::new();
    #[allow(unused_assignments)]
    let mut last_mcp_url: Option<String> = None;
    #[allow(unused_assignments)]
    let mut last_ngrok_url: Option<String> = None;
    #[allow(unused_assignments)]
    let mut last_ngrok_domain: Option<String> = None;
    let mut mcp_url_revealed_until: Option<Instant> = None;
    let mut log_mcp_url_revealed_until: Option<Instant> = None;
    let mut log_ngrok_url_revealed_until: Option<Instant> = None;
    let mut log_ngrok_domain_revealed_until: Option<Instant> = None;
    let mut last_config_flush = Instant::now();
    let mut update_info = update::available_update();
    let mut last_update_refresh = Instant::now();
    let mut pending_changelog_notice = update::pending_changelog_notice();
    let mut exit = AppExit::Quit;

    loop {
        let mut app = {
            let mut live = state.lock().await;
            let _ = drain_server_ui_events(&mut live, &mut ui_events);
            live.prune_closed_flows();
            UiSnapshot::from_app(&live)
        };
        if reconcile_workspace_filter(&mut workspace_filter, &app.workspaces) {
            workspace_scroll = 0;
            reset_filtered_navigation(
                (&mut log_scroll, &mut log_follow_tail),
                (&mut command_scroll, &mut command_follow_tail),
                (&mut selected_log, &mut selected_command),
                (&mut expanded_log, &mut expanded_command),
            );
        }
        apply_workspace_observability_filter(&mut app, &workspace_filter, &clear_cutoffs);
        if interrupts.take_pending() > 0 {
            if run_quit_confirm(terminal, &state, &interrupts).await? {
                break;
            }
            continue;
        }
        if last_config_flush.elapsed() >= CONFIG_FLUSH_INTERVAL {
            if let Err(error) = flush_config(&state, false).await {
                state
                    .lock()
                    .await
                    .log("WARN", format!("Failed to persist config: {error}"));
            }
            last_config_flush = Instant::now();
        }
        if last_update_refresh.elapsed() >= UPDATE_STATE_REFRESH_INTERVAL {
            update_info = update::available_update();
            last_update_refresh = Instant::now();
        }
        {
            let reveal_remaining = active_reveal_remaining(mcp_url_revealed_until, Instant::now());
            if mcp_url_revealed_until.is_some() && reveal_remaining.is_none() {
                mcp_url_revealed_until = None;
            }
            let log_mcp_reveal_remaining =
                active_reveal_remaining(log_mcp_url_revealed_until, Instant::now());
            if log_mcp_url_revealed_until.is_some() && log_mcp_reveal_remaining.is_none() {
                log_mcp_url_revealed_until = None;
            }
            let log_ngrok_reveal_remaining =
                active_reveal_remaining(log_ngrok_url_revealed_until, Instant::now());
            if log_ngrok_url_revealed_until.is_some() && log_ngrok_reveal_remaining.is_none() {
                log_ngrok_url_revealed_until = None;
            }
            let log_ngrok_domain_reveal_remaining =
                active_reveal_remaining(log_ngrok_domain_revealed_until, Instant::now());
            if log_ngrok_domain_revealed_until.is_some()
                && log_ngrok_domain_reveal_remaining.is_none()
            {
                log_ngrok_domain_revealed_until = None;
            }
            last_log_count = app.logs.len();
            last_command_count = app.command_activities.len();
            sync_panel_selection(&mut selected_log, last_log_count, log_follow_tail);
            sync_panel_selection(
                &mut selected_command,
                last_command_count,
                command_follow_tail,
            );
            if expanded_log.is_some_and(|index| index >= last_log_count) {
                expanded_log = None;
            }
            if expanded_command.is_some_and(|index| index >= last_command_count) {
                expanded_command = None;
            }
            last_mcp_url = app.public_mcp_url();
            last_ngrok_url = app.ngrok_url.clone();
            last_ngrok_domain = app.ngrok_domain.clone();
            let toast_ref = toast
                .as_ref()
                .filter(|(_, _, t)| t.elapsed().as_secs() < 2)
                .map(|(m, pos, _)| (*m, *pos));
            let mut new_lines: Vec<String> = Vec::new();
            terminal.draw(|f| {
                draw_ui(
                    f,
                    UiRenderContext {
                        app: &app,
                        update_info: update_info.as_ref(),
                        log_scroll,
                        log_follow_tail,
                        command_scroll,
                        command_follow_tail,
                        workspace_filter: &workspace_filter,
                        workspace_scroll: &mut workspace_scroll,
                        workspace_visible_count: &mut workspace_visible_count,
                        focused_bottom_panel,
                        selected_log,
                        selected_command,
                        expanded_log,
                        expanded_command,
                        log_view: &mut last_log_view,
                        command_view: &mut last_command_view,
                        bottom_panel_areas: &mut bottom_panel_areas,
                        bottom_panel_hits: &mut bottom_panel_hits,
                        dashboard_hit_areas: &mut dashboard_hit_areas,
                        toast: toast_ref,
                        mcp_url_reveal_remaining: reveal_remaining,
                        log_mcp_url_reveal_remaining: log_mcp_reveal_remaining,
                        log_ngrok_url_reveal_remaining: log_ngrok_reveal_remaining,
                        log_ngrok_domain_reveal_remaining,
                    },
                );

                if let Some(((c0, r0), (c1, r1))) = selection.range() {
                    let palette = app.current_theme().palette;
                    let area = f.area();
                    for row in r0..=r1 {
                        if row >= area.height {
                            break;
                        }
                        let cs = if row == r0 { c0 } else { 0 };
                        let ce = if row == r1 {
                            c1
                        } else {
                            area.width.saturating_sub(1)
                        };
                        for col in cs..=ce {
                            if col >= area.width {
                                break;
                            }
                            if let Some(cell) = f.buffer_mut().cell_mut((col, row)) {
                                cell.set_style(
                                    Style::default()
                                        .bg(palette.selection_bg)
                                        .fg(palette.selection_fg),
                                );
                            }
                        }
                    }
                }

                if selection.dragging {
                    let area = f.area();
                    let buf = f.buffer_mut();
                    for row in 0..area.height {
                        let mut line = String::new();
                        for col in 0..area.width {
                            line.push_str(buf[(col, row)].symbol());
                        }
                        new_lines.push(line);
                    }
                }
            })?;
            if !log_follow_tail && log_scroll > last_log_view.max_scroll {
                log_scroll = last_log_view.max_scroll;
            }
            if !command_follow_tail && command_scroll > last_command_view.max_scroll {
                command_scroll = last_command_view.max_scroll;
            }
            if focused_bottom_panel == DashboardFocus::ShellCommands
                && bottom_panel_areas.shell_commands.is_none()
            {
                focused_bottom_panel = DashboardFocus::Logs;
            }
            if focused_bottom_panel == DashboardFocus::Workspaces
                && dashboard_hit_areas.workspaces.is_none()
            {
                focused_bottom_panel = DashboardFocus::Logs;
            }
            screen_lines = new_lines;
        }

        if let Some(notice) = pending_changelog_notice.clone() {
            run_changelog_notice(terminal, &state, &notice).await?;
            pending_changelog_notice = None;
            if let Err(error) = update::dismiss_pending_changelog_notice() {
                state.lock().await.log(
                    "WARN",
                    format!("Could not dismiss update changelog notice: {error}"),
                );
            }
        }

        let snapshots = build_animation_snapshot(&app);
        if !snapshots.is_empty() {
            let snapshot_joined = snapshots.join("\n");
            if snapshot_joined != last_animation_snapshot {
                last_animation_snapshot = snapshot_joined;
            }
        }

        if let Some((_, _, t)) = &toast
            && t.elapsed().as_secs() >= 2
        {
            toast = None;
        }

        let reveal_active = mcp_url_revealed_until.is_some()
            || log_mcp_url_revealed_until.is_some()
            || log_ngrok_url_revealed_until.is_some()
            || log_ngrok_domain_revealed_until.is_some();
        let poll_interval = if !app.flows.is_empty() || reveal_active || selection.dragging {
            UI_POLL_INTERVAL
        } else if terminal.size()?.width >= 120 {
            Duration::from_millis(app.mascot.frame_ms.max(16))
        } else {
            Duration::from_millis(250)
        };

        if event::poll(poll_interval)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    selection.clear();
                    if key_is_interrupt(&key) || key_is_plain_quit(&key) {
                        if run_quit_confirm(terminal, &state, &interrupts).await? {
                            break;
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('w') => {
                            run_workspaces(terminal, state.clone()).await?;
                        }
                        KeyCode::Char('u') if update_info.is_some() => {
                            let Some(selected_update) = update_info.clone() else {
                                continue;
                            };
                            if run_update_confirm(terminal, &state, &selected_update).await? {
                                exit = AppExit::UpdateRestart(selected_update.latest_version);
                                break;
                            }
                        }
                        KeyCode::Tab | KeyCode::BackTab => {
                            focused_bottom_panel = cycle_dashboard_focus(
                                focused_bottom_panel,
                                dashboard_hit_areas.workspaces.is_some(),
                                bottom_panel_areas.shell_commands.is_some(),
                                key.code == KeyCode::BackTab,
                            );
                        }
                        KeyCode::Char('c') => {
                            record_clear_view(&app, &workspace_filter, &mut clear_cutoffs);
                            reset_filtered_navigation(
                                (&mut log_scroll, &mut log_follow_tail),
                                (&mut command_scroll, &mut command_follow_tail),
                                (&mut selected_log, &mut selected_command),
                                (&mut expanded_log, &mut expanded_command),
                            );
                            toast = Some(("View cleared", (2, 2), Instant::now()));
                        }
                        KeyCode::Enter | KeyCode::Char(' ') => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {}
                            DashboardFocus::Logs => {
                                if let Some(index) = selected_log {
                                    expanded_log = if expanded_log == Some(index) {
                                        None
                                    } else {
                                        expanded_command = None;
                                        log_follow_tail = false;
                                        log_scroll = index.saturating_sub(1);
                                        Some(index)
                                    };
                                }
                            }
                            DashboardFocus::ShellCommands => {
                                if let Some(index) = selected_command {
                                    expanded_command = if expanded_command == Some(index) {
                                        None
                                    } else {
                                        expanded_log = None;
                                        command_follow_tail = false;
                                        command_scroll = index.saturating_sub(1);
                                        Some(index)
                                    };
                                }
                            }
                        },
                        KeyCode::Esc => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {}
                            DashboardFocus::Logs => expanded_log = None,
                            DashboardFocus::ShellCommands => expanded_command = None,
                        },
                        KeyCode::Up => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let next = current.saturating_sub(1);
                                let next_filter =
                                    workspace_filter_from_index(next, &app.workspaces);
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, -1);
                                scroll_panel_up(
                                    &mut log_scroll,
                                    &mut log_follow_tail,
                                    last_log_view,
                                    1,
                                );
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, -1);
                                scroll_panel_up(
                                    &mut command_scroll,
                                    &mut command_follow_tail,
                                    last_command_view,
                                    1,
                                );
                            }
                        },
                        KeyCode::Down => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let last = app.workspaces.len();
                                let next = current.saturating_add(1).min(last);
                                let next_filter =
                                    workspace_filter_from_index(next, &app.workspaces);
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, 1);
                                if selected_log == last_log_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                        1,
                                    );
                                }
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, 1);
                                if selected_command == last_command_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                        1,
                                    );
                                }
                            }
                        },
                        KeyCode::PageUp => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let next = current.saturating_sub(workspace_visible_count.max(1));
                                let next_filter =
                                    workspace_filter_from_index(next, &app.workspaces);
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, -5);
                                scroll_panel_up(
                                    &mut log_scroll,
                                    &mut log_follow_tail,
                                    last_log_view,
                                    5,
                                );
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, -5);
                                scroll_panel_up(
                                    &mut command_scroll,
                                    &mut command_follow_tail,
                                    last_command_view,
                                    5,
                                );
                            }
                        },
                        KeyCode::PageDown => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let last = app.workspaces.len();
                                let next = current
                                    .saturating_add(workspace_visible_count.max(1))
                                    .min(last);
                                let next_filter =
                                    workspace_filter_from_index(next, &app.workspaces);
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, 5);
                                if selected_log == last_log_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                        5,
                                    );
                                }
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, 5);
                                if selected_command == last_command_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                        5,
                                    );
                                }
                            }
                        },
                        KeyCode::Home => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                if workspace_filter != WorkspaceFilter::All {
                                    workspace_filter = WorkspaceFilter::All;
                                    workspace_scroll = 0;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                selected_log = (last_log_count > 0).then_some(0);
                                log_follow_tail = false;
                                log_scroll = 0;
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                selected_command = (last_command_count > 0).then_some(0);
                                command_follow_tail = false;
                                command_scroll = 0;
                            }
                        },
                        KeyCode::End => match focused_bottom_panel {
                            DashboardFocus::Workspaces => {
                                let next_filter = workspace_filter_from_index(
                                    app.workspaces.len(),
                                    &app.workspaces,
                                );
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                selected_log = last_log_count.checked_sub(1);
                                follow_panel_latest(
                                    &mut log_scroll,
                                    &mut log_follow_tail,
                                    last_log_view,
                                );
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                selected_command = last_command_count.checked_sub(1);
                                follow_panel_latest(
                                    &mut command_scroll,
                                    &mut command_follow_tail,
                                    last_command_view,
                                );
                            }
                        },
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if dashboard_hit_areas
                            .workspaces
                            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                        {
                            focused_bottom_panel = DashboardFocus::Workspaces;
                            if let Some(index) =
                                item_under_cursor(&dashboard_hit_areas.workspace_rows, mouse.row)
                            {
                                let next_filter =
                                    workspace_filter_from_index(index, &app.workspaces);
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                        } else if let Some(panel) =
                            panel_under_cursor(bottom_panel_areas, mouse.column, mouse.row)
                        {
                            focused_bottom_panel = panel;
                            match panel {
                                DashboardFocus::Workspaces => {}
                                DashboardFocus::Logs => {
                                    if let Some(index) =
                                        item_under_cursor(&bottom_panel_hits.logs, mouse.row)
                                    {
                                        if selected_log != Some(index) {
                                            expanded_log = None;
                                        }
                                        selected_log = Some(index);
                                        log_follow_tail = index + 1 == last_log_count;
                                    }
                                }
                                DashboardFocus::ShellCommands => {
                                    if let Some(index) = item_under_cursor(
                                        &bottom_panel_hits.shell_commands,
                                        mouse.row,
                                    ) {
                                        if selected_command != Some(index) {
                                            expanded_command = None;
                                        }
                                        selected_command = Some(index);
                                        command_follow_tail = index + 1 == last_command_count;
                                    }
                                }
                            }
                        }
                        selection.start = Some((mouse.column, mouse.row));
                        selection.end = Some((mouse.column, mouse.row));
                        selection.dragging = true;
                    }
                    MouseEventKind::Drag(MouseButton::Left) if selection.dragging => {
                        selection.end = Some((mouse.column, mouse.row));
                    }
                    MouseEventKind::Up(MouseButton::Left) if selection.dragging => {
                        selection.end = Some((mouse.column, mouse.row));
                        selection.dragging = false;
                        if let Some((start, end)) = selection.range() {
                            if start != end {
                                let text = extract_from_screen(&screen_lines, start, end);
                                if !text.is_empty() {
                                    let message = if clipboard_copy(&text) {
                                        "Copied!"
                                    } else {
                                        "Copy failed"
                                    };
                                    toast =
                                        Some((message, (mouse.column, mouse.row), Instant::now()));
                                }
                            } else {
                                let row = start.1 as usize;
                                if row < screen_lines.len() {
                                    let line = &screen_lines[row];
                                    let now = Instant::now();
                                    let secret_click = dashboard_secret_target_at(
                                        &dashboard_hit_areas,
                                        mouse.column,
                                        mouse.row,
                                    )
                                    .and_then(|target| {
                                        let click = match target {
                                            DashboardSecretTarget::PrimaryMcpUrl => {
                                                timed_secret_click(
                                                    last_mcp_url.as_deref(),
                                                    &mut mcp_url_revealed_until,
                                                    now,
                                                )
                                            }
                                            DashboardSecretTarget::LogMcpUrl => timed_secret_click(
                                                last_mcp_url.as_deref(),
                                                &mut log_mcp_url_revealed_until,
                                                now,
                                            ),
                                            DashboardSecretTarget::LogNgrokUrl => {
                                                timed_secret_click(
                                                    last_ngrok_url.as_deref(),
                                                    &mut log_ngrok_url_revealed_until,
                                                    now,
                                                )
                                            }
                                            DashboardSecretTarget::LogNgrokDomain => {
                                                timed_secret_click(
                                                    last_ngrok_domain.as_deref(),
                                                    &mut log_ngrok_domain_revealed_until,
                                                    now,
                                                )
                                            }
                                        }?;
                                        Some((target, click))
                                    });
                                    let copy_value = if let Some((target, click)) = secret_click {
                                        match click {
                                            TimedSecretClick::Revealed => {
                                                toast = Some((
                                                    target.reveal_message(),
                                                    (mouse.column, mouse.row),
                                                    now,
                                                ));
                                                None
                                            }
                                            TimedSecretClick::Copy(value) => Some(value),
                                        }
                                    } else if line.contains("chatgpt.com/apps") {
                                        Some(
                                            "https://chatgpt.com/apps#settings/Connectors"
                                                .to_string(),
                                        )
                                    } else {
                                        None
                                    }
                                    .or_else(|| {
                                        if line.contains("\u{2502}") {
                                            if line.contains("Name") {
                                                Some("MoonDesk".to_string())
                                            } else if line.contains("Authentication") {
                                                Some("None".to_string())
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    });
                                    if let Some(text) = copy_value {
                                        let message = if clipboard_copy(&text) {
                                            "Copied!"
                                        } else {
                                            "Copy failed"
                                        };
                                        toast = Some((
                                            message,
                                            (mouse.column, mouse.row),
                                            Instant::now(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        let target = if dashboard_hit_areas
                            .workspaces
                            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                        {
                            DashboardFocus::Workspaces
                        } else {
                            panel_under_cursor(bottom_panel_areas, mouse.column, mouse.row)
                                .unwrap_or(focused_bottom_panel)
                        };
                        focused_bottom_panel = target;
                        match target {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let next_filter = workspace_filter_from_index(
                                    current.saturating_sub(1),
                                    &app.workspaces,
                                );
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, -1);
                                scroll_panel_up(
                                    &mut log_scroll,
                                    &mut log_follow_tail,
                                    last_log_view,
                                    1,
                                );
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, -1);
                                scroll_panel_up(
                                    &mut command_scroll,
                                    &mut command_follow_tail,
                                    last_command_view,
                                    1,
                                );
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let target = if dashboard_hit_areas
                            .workspaces
                            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
                        {
                            DashboardFocus::Workspaces
                        } else {
                            panel_under_cursor(bottom_panel_areas, mouse.column, mouse.row)
                                .unwrap_or(focused_bottom_panel)
                        };
                        focused_bottom_panel = target;
                        match target {
                            DashboardFocus::Workspaces => {
                                let current =
                                    workspace_filter_index(&workspace_filter, &app.workspaces);
                                let last = app.workspaces.len();
                                let next_filter = workspace_filter_from_index(
                                    current.saturating_add(1).min(last),
                                    &app.workspaces,
                                );
                                if next_filter != workspace_filter {
                                    workspace_filter = next_filter;
                                    reset_filtered_navigation(
                                        (&mut log_scroll, &mut log_follow_tail),
                                        (&mut command_scroll, &mut command_follow_tail),
                                        (&mut selected_log, &mut selected_command),
                                        (&mut expanded_log, &mut expanded_command),
                                    );
                                }
                            }
                            DashboardFocus::Logs => {
                                expanded_log = None;
                                move_panel_selection(&mut selected_log, last_log_count, 1);
                                if selected_log == last_log_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut log_scroll,
                                        &mut log_follow_tail,
                                        last_log_view,
                                        1,
                                    );
                                }
                            }
                            DashboardFocus::ShellCommands => {
                                expanded_command = None;
                                move_panel_selection(&mut selected_command, last_command_count, 1);
                                if selected_command == last_command_count.checked_sub(1) {
                                    follow_panel_latest(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                    );
                                } else {
                                    scroll_panel_down(
                                        &mut command_scroll,
                                        &mut command_follow_tail,
                                        last_command_view,
                                        1,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    if let Err(error) = flush_config(&state, true).await {
        state.lock().await.log(
            "WARN",
            format!("Failed to persist config on shutdown: {error}"),
        );
    }
    Ok(exit)
}

fn quit_confirm_action(key: &crossterm::event::KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Enter
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            Some(true)
        }
        KeyCode::Esc => Some(false),
        _ => None,
    }
}

async fn run_quit_confirm(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &SharedState,
    interrupts: &InterruptState,
) -> Result<bool, Box<dyn std::error::Error>> {
    // The interrupt that opened this dialog has already served its purpose. Any
    // additional Ctrl+C events are intentionally absorbed here so shutdown always
    // requires an explicit Enter confirmation instead of an accidental key repeat.
    let _ = interrupts.take_pending();
    loop {
        let (theme, registered, connected, in_flight, active_commands) = {
            let app = state.lock().await;
            let connected = app
                .workspace_runtimes
                .values()
                .filter(|runtime| runtime.remote_connected())
                .count();
            let in_flight = app
                .workspace_runtimes
                .values()
                .map(|runtime| runtime.in_flight_requests())
                .sum();
            let active_commands = app
                .command_activities
                .iter()
                .filter(|activity| activity.state == CommandActivityState::Running)
                .count();
            (
                app.current_theme(),
                app.workspaces.len(),
                connected,
                in_flight,
                active_commands,
            )
        };

        terminal.draw(|f| {
            draw_quit_confirm(f, theme, registered, connected, in_flight, active_commands)
        })?;
        let _ = interrupts.take_pending();
        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(confirmed) = quit_confirm_action(&key) {
                return Ok(confirmed);
            }
        }
    }
}

fn draw_quit_confirm(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    registered: usize,
    connected: usize,
    in_flight: usize,
    active_commands: usize,
) {
    let palette = theme.palette;
    render_theme_background(f, palette);
    let area = centered_rect(78, 14, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(" Stop MoonDesk? ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.danger_fg))
        .style(Style::default().fg(palette.modal_fg).bg(palette.modal_bg));
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    f.render_widget(block, area);

    let compact_actions = inner.width < 48;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(if compact_actions { 2 } else { 1 }),
        ])
        .split(inner);

    let mut lines = vec![
        Line::from(Span::styled(
            "MoonDesk is the shared host for every registered workspace.",
            Style::default()
                .fg(palette.warning_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Current impact: {registered} registered · {connected} connected · {in_flight} in flight"
            ),
            Style::default().fg(palette.primary_fg),
        )),
    ];
    if active_commands > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "Active commands: {active_commands} command{} will be cancelled.",
                if active_commands == 1 { "" } else { "s" }
            ),
            Style::default()
                .fg(palette.danger_fg)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Stopping also closes the public tunnel and disconnects active ChatGPT sessions.",
        Style::default().fg(palette.primary_fg),
    )));
    lines.push(Line::from(""));

    let action_lines = if compact_actions {
        vec![
            Line::from(vec![
                Span::styled(
                    "[Enter]",
                    Style::default()
                        .fg(palette.danger_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Stop MoonDesk"),
            ]),
            Line::from(vec![
                Span::styled(
                    "[Esc]",
                    Style::default()
                        .fg(palette.success_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Keep running"),
            ]),
        ]
    } else {
        vec![Line::from(vec![
            Span::styled(
                "[Enter]",
                Style::default()
                    .fg(palette.danger_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Stop MoonDesk    "),
            Span::styled(
                "[Esc]",
                Style::default()
                    .fg(palette.success_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Keep running"),
        ])]
    };

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    f.render_widget(
        Paragraph::new(action_lines).wrap(Wrap { trim: false }),
        sections[1],
    );
}

fn update_confirm_action(code: KeyCode) -> Option<bool> {
    match code {
        KeyCode::Enter => Some(true),
        KeyCode::Esc => Some(false),
        _ => None,
    }
}

fn changelog_preview_lines(
    notes: &[String],
    line_budget: usize,
    content_width: usize,
    palette: theme::Palette,
) -> Vec<Line<'static>> {
    if line_budget == 0 || content_width == 0 {
        return Vec::new();
    }
    if notes.is_empty() {
        let (text, _) = truncate_with_ellipsis(
            "Release notes are unavailable for this update.",
            content_width,
            false,
        );
        return vec![Line::from(Span::styled(
            text,
            Style::default().fg(palette.muted_fg),
        ))];
    }

    let show_summary = notes.len() > line_budget && line_budget > 1;
    let note_budget = if show_summary {
        line_budget.saturating_sub(1)
    } else {
        line_budget
    };
    let bullet = if content_width >= 4 { "• " } else { "" };
    let note_width = content_width.saturating_sub(bullet.chars().count()).max(1);
    let mut lines = Vec::with_capacity(line_budget.min(notes.len().saturating_add(1)));
    for note in notes.iter().take(note_budget) {
        let (text, _) = truncate_with_ellipsis(note, note_width, false);
        lines.push(Line::from(vec![
            Span::styled(bullet.to_string(), Style::default().fg(palette.success_fg)),
            Span::styled(text, Style::default().fg(palette.primary_fg)),
        ]));
    }
    if show_summary {
        let remaining = notes.len().saturating_sub(note_budget);
        let summary = format!(
            "… {remaining} more change{}",
            if remaining == 1 { "" } else { "s" }
        );
        let (summary, _) = truncate_with_ellipsis(&summary, content_width, false);
        lines.push(Line::from(Span::styled(
            summary,
            Style::default().fg(palette.muted_fg),
        )));
    }
    lines
}

async fn run_update_confirm(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &SharedState,
    update_info: &update::UpdateInfo,
) -> Result<bool, Box<dyn std::error::Error>> {
    loop {
        let (theme, active_commands) = {
            let app = state.lock().await;
            let active_commands = app
                .command_activities
                .iter()
                .filter(|activity| activity.state == CommandActivityState::Running)
                .count();
            (app.current_theme(), active_commands)
        };

        terminal.draw(|f| draw_update_confirm(f, theme, update_info, active_commands))?;
        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(confirmed) = update_confirm_action(key.code) {
                return Ok(confirmed);
            }
        }
    }
}

fn draw_update_confirm(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    update_info: &update::UpdateInfo,
    active_commands: usize,
) {
    let palette = theme.palette;
    render_theme_background(f, palette);
    let area = centered_rect(82, 21, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(" Update MoonDesk ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.warning_fg))
        .style(Style::default().fg(palette.modal_fg).bg(palette.modal_bg));
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    f.render_widget(block, area);
    let compact_actions = inner.width < 48;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(if compact_actions { 2 } else { 1 }),
        ])
        .split(inner);

    let content_width = sections[0].width as usize;
    let compact_height = sections[0].height < 12;
    let active_command_lines = usize::from(active_commands > 0);
    let fixed_lines = if compact_height {
        4 + active_command_lines
    } else {
        7 + active_command_lines + usize::from(update_info.release_url.is_some())
    };
    let changelog_line_budget = (sections[0].height as usize)
        .saturating_sub(fixed_lines)
        .min(6);

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "MoonDesk {}  →  {}",
            update_info.current_version, update_info.latest_version
        ),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    ))];
    if !compact_height {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "What's new",
        Style::default()
            .fg(palette.secondary_fg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(changelog_preview_lines(
        &update_info.release_notes,
        changelog_line_budget,
        content_width,
        palette,
    ));

    if compact_height {
        lines.push(Line::from(Span::styled(
            "Finish active work before updating.",
            Style::default()
                .fg(palette.warning_fg)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Restart disconnects MCP.",
            Style::default().fg(palette.primary_fg),
        )));
    } else {
        if let Some(url) = &update_info.release_url {
            let release = format!("Release: {url}");
            let (release, _) = truncate_with_ellipsis(&release, content_width, false);
            lines.push(Line::from(Span::styled(
                release,
                Style::default().fg(palette.muted_fg),
            )));
        }
        let (session_warning, _) =
            truncate_with_ellipsis(UPDATE_CONFIRM_SESSION_WARNING, content_width, false);
        let (connection_warning, _) =
            truncate_with_ellipsis(UPDATE_CONFIRM_CONNECTION_WARNING, content_width, false);
        let (restart_note, _) = truncate_with_ellipsis(
            "After the exact new version is installed, MoonDesk will restart here.",
            content_width,
            false,
        );
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                session_warning,
                Style::default()
                    .fg(palette.warning_fg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                connection_warning,
                Style::default().fg(palette.primary_fg),
            )),
            Line::from(Span::styled(
                restart_note,
                Style::default().fg(palette.primary_fg),
            )),
        ]);
    }
    if active_commands > 0 {
        let message = if compact_height {
            format!(
                "{active_commands} active command{} will stop.",
                if active_commands == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "Detected now: {active_commands} active command{} will be stopped.",
                if active_commands == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default()
                .fg(palette.danger_fg)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let action_lines = if compact_actions {
        vec![
            Line::from(vec![
                Span::styled(
                    "[Enter]",
                    Style::default()
                        .fg(palette.success_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Update & Restart"),
            ]),
            Line::from(vec![
                Span::styled(
                    "[Esc]",
                    Style::default()
                        .fg(palette.danger_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Abort"),
            ]),
        ]
    } else {
        vec![Line::from(vec![
            Span::styled(
                "[Enter]",
                Style::default()
                    .fg(palette.success_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Continue with Update & Restart    "),
            Span::styled(
                "[Esc]",
                Style::default()
                    .fg(palette.danger_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Abort"),
        ])]
    };

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    f.render_widget(Paragraph::new(action_lines), sections[1]);
}

async fn run_changelog_notice(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &SharedState,
    notice: &update::ChangelogNotice,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let theme = { state.lock().await.current_theme() };
        terminal.draw(|f| draw_changelog_notice(f, theme, notice))?;
        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                return Ok(());
            }
        }
    }
}

fn draw_changelog_notice(f: &mut Frame, theme: &theme::ThemeDef, notice: &update::ChangelogNotice) {
    let palette = theme.palette;
    render_theme_background(f, palette);
    let area = centered_rect(82, 20, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(" MoonDesk Updated ")
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.success_fg))
        .style(Style::default().fg(palette.modal_fg).bg(palette.modal_bg));
    let inner = block.inner(area).inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    f.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let content_width = sections[0].width as usize;
    let compact_height = sections[0].height < 10;
    let show_release_link = !compact_height && notice.release_url.is_some();
    let fixed_lines = if compact_height {
        2
    } else {
        3 + if show_release_link { 2 } else { 0 }
    };
    let changelog_line_budget = (sections[0].height as usize)
        .saturating_sub(fixed_lines)
        .min(9);

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "Updated successfully  {}  →  {}",
            notice.from_version, notice.to_version
        ),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    ))];
    if !compact_height {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "What's new",
        Style::default()
            .fg(palette.secondary_fg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(changelog_preview_lines(
        &notice.release_notes,
        changelog_line_budget,
        content_width,
        palette,
    ));
    if show_release_link && let Some(url) = &notice.release_url {
        let release = format!("Release: {url}");
        let (release, _) = truncate_with_ellipsis(&release, content_width, false);
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(release, Style::default().fg(palette.muted_fg))),
        ]);
    }

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    let actions = if sections[1].width < 34 {
        Line::from(vec![
            Span::styled(
                "[Enter]",
                Style::default()
                    .fg(palette.success_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Got it  "),
            Span::styled(
                "[Esc]",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Close"),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "[Enter]",
                Style::default()
                    .fg(palette.success_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Got it    "),
            Span::styled(
                "[Esc]",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Close"),
        ])
    };
    f.render_widget(Paragraph::new(actions), sections[1]);
}

// -- Draw main UI -------------------------------------------------------------

fn draw_inline_workspaces(
    f: &mut Frame,
    app: &UiSnapshot,
    area: Rect,
    workspace_filter: &WorkspaceFilter,
    focused: bool,
    workspace_view: (&mut usize, &mut usize),
    dashboard_hit_areas: &mut DashboardHitAreas,
) {
    let (workspace_scroll, workspace_visible_count) = workspace_view;
    let palette = app.current_theme().palette;
    let block = Block::default()
        .title(if focused {
            " Workspaces [focused] "
        } else {
            " Workspaces "
        })
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(if focused {
            palette.key_fg
        } else {
            palette.border_fg
        }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    dashboard_hit_areas.workspaces = Some(area);
    dashboard_hit_areas.workspace_rows.clear();

    let total = app.workspaces.len() + 1;
    if inner.height == 0 {
        *workspace_visible_count = 0;
        return;
    }
    let needs_range = total > inner.height as usize;
    let list_height = if needs_range {
        inner.height.saturating_sub(1)
    } else {
        inner.height
    } as usize;
    *workspace_visible_count = list_height.max(1);

    let selected_index = workspace_filter_index(workspace_filter, &app.workspaces);
    let max_start = total.saturating_sub(*workspace_visible_count);
    let mut start = (*workspace_scroll).min(max_start);
    if selected_index < start {
        start = selected_index;
    } else if selected_index >= start.saturating_add(*workspace_visible_count) {
        start = selected_index
            .saturating_add(1)
            .saturating_sub(*workspace_visible_count);
    }
    start = start.min(max_start);
    *workspace_scroll = start;
    let end = start.saturating_add(*workspace_visible_count).min(total);

    let row_width = inner.width.saturating_sub(1) as usize;
    let mut items = Vec::with_capacity(end.saturating_sub(start));
    for (visual_row, index) in (start..end).enumerate() {
        let selected = index == selected_index;
        let marker = if selected { ">" } else { " " };
        let (state_symbol, state_style, label) = if index == 0 {
            (
                "◆",
                Style::default().fg(palette.info_fg),
                "All Workspaces".to_string(),
            )
        } else {
            let workspace = &app.workspaces[index - 1];
            let symbol = if workspace.connected { "●" } else { "○" };
            let status = if workspace.connected {
                " connected"
            } else {
                " idle"
            };
            let style = Style::default().fg(if workspace.connected {
                palette.success_fg
            } else {
                palette.muted_fg
            });
            let available_width = row_width.saturating_sub(4 + status.chars().count());
            let (name, _) = truncate_with_ellipsis(&workspace.name, available_width.max(1), false);
            (symbol, style, format!("{name}{status}"))
        };
        let label_style = if selected {
            Style::default()
                .fg(palette.primary_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.secondary_fg)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(palette.key_fg)),
            Span::styled(format!("{state_symbol} "), state_style),
            Span::styled(label, label_style),
        ])));
        let row = inner.y.saturating_add(visual_row as u16);
        dashboard_hit_areas.workspace_rows.push(PanelItemHit {
            top: row,
            bottom: row,
            index,
        });
    }

    let list_area = Rect::new(inner.x, inner.y, inner.width, list_height as u16);
    f.render_widget(List::new(items), list_area);
    if needs_range {
        let range_area = Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            1,
        );
        f.render_widget(
            Paragraph::new(format!("  {}-{} / {}", start + 1, end, total))
                .style(Style::default().fg(palette.muted_fg)),
            range_area,
        );
    }
}

struct UiRenderContext<'a> {
    app: &'a UiSnapshot,
    update_info: Option<&'a update::UpdateInfo>,
    log_scroll: usize,
    log_follow_tail: bool,
    command_scroll: usize,
    command_follow_tail: bool,
    workspace_filter: &'a WorkspaceFilter,
    workspace_scroll: &'a mut usize,
    workspace_visible_count: &'a mut usize,
    focused_bottom_panel: DashboardFocus,
    selected_log: Option<usize>,
    selected_command: Option<usize>,
    expanded_log: Option<usize>,
    expanded_command: Option<usize>,
    log_view: &'a mut PanelScrollView,
    command_view: &'a mut PanelScrollView,
    bottom_panel_areas: &'a mut BottomPanelAreas,
    bottom_panel_hits: &'a mut BottomPanelHitMaps,
    dashboard_hit_areas: &'a mut DashboardHitAreas,
    toast: Option<(&'a str, (u16, u16))>,
    mcp_url_reveal_remaining: Option<Duration>,
    log_mcp_url_reveal_remaining: Option<Duration>,
    log_ngrok_url_reveal_remaining: Option<Duration>,
    log_ngrok_domain_reveal_remaining: Option<Duration>,
}

fn draw_ui(f: &mut Frame, context: UiRenderContext<'_>) {
    let UiRenderContext {
        app,
        update_info,
        log_scroll,
        log_follow_tail,
        command_scroll,
        command_follow_tail,
        workspace_filter,
        workspace_scroll,
        workspace_visible_count,
        focused_bottom_panel,
        selected_log,
        selected_command,
        expanded_log,
        expanded_command,
        log_view,
        command_view,
        bottom_panel_areas,
        bottom_panel_hits,
        dashboard_hit_areas,
        toast,
        mcp_url_reveal_remaining,
        log_mcp_url_reveal_remaining,
        log_ngrok_url_reveal_remaining,
        log_ngrok_domain_reveal_remaining,
    } = context;
    *dashboard_hit_areas = DashboardHitAreas::default();
    let palette = app.current_theme().palette;
    render_theme_background(f, palette);
    let area = f.area();
    let now_millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let full_mcp_url = app.public_mcp_url();
    // A configured static domain still has a revealable connector URL even if
    // its tunnel is currently offline. Tunnel readiness is reported separately.
    let has_url = full_mcp_url.is_some();
    let visible_flow_count = app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow))
        .count() as u16;
    let show_guide = should_show_connect_guide(app, now_millis);
    let bootstrap_status_flow = active_bootstrap_status_flow(app, now_millis);
    let show_workspace_pane = !show_guide
        && bootstrap_status_flow.is_none()
        && area.width >= DASHBOARD_THREE_COLUMN_MIN_WIDTH;
    let show_flow_panel = !show_guide;
    let compact_flow_layout = show_workspace_pane;
    let compact_status_content_width = area
        .width
        .saturating_sub(DASHBOARD_WORKSPACE_COLUMN_WIDTH + TUI_MASCOT_BLOCK_WIDTH)
        .saturating_sub(6) as usize;
    let compact_flow_lane_cells = compact_status_content_width
        .saturating_sub(25)
        .clamp(8, FLOW_ROW_CELLS);
    let logs_min_height = if show_guide { 3 } else { 5 };
    let max_status_height = area.height.saturating_sub(6 + logs_min_height).max(17);
    // Keep the main panel deterministic: mascot size must not drive layout.
    let status_height = STATUS_PANEL_HEIGHT.min(max_status_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(status_height),
            Constraint::Length(3),
            Constraint::Min(logs_min_height),
        ])
        .split(area);

    // ── Header ──
    let header = Paragraph::new("  MoonDesk - Turns ChatGPT Web into a coding agent")
        .style(
            Style::default()
                .fg(palette.header_fg)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg)),
        );
    f.render_widget(header, chunks[0]);

    // ── Status ──
    let mode_label = app.mode.label();
    let tool_mode_label = app.tool_mode.label();
    let server_status = if app.server_running {
        format!("RUNNING (port {})", app.port)
    } else {
        "STOPPED".into()
    };
    let ngrok_status: &str = if app.ngrok_running {
        "RUNNING"
    } else {
        "STOPPED"
    };
    let browser_runtime_status: &str = if app.browser_runtime_running {
        "RUNNING"
    } else if app.mode.browser_enabled() {
        "IDLE"
    } else {
        "N/A"
    };
    let mcp_url_is_revealed = full_mcp_url.is_some() && mcp_url_reveal_remaining.is_some();
    let mcp_url = match (&full_mcp_url, mcp_url_is_revealed) {
        (Some(url), true) => url.clone(),
        (Some(_), false) => MCP_URL_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let log_mcp_url = match (&full_mcp_url, log_mcp_url_reveal_remaining.is_some()) {
        (Some(url), true) => url.clone(),
        (Some(_), false) => MCP_URL_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let log_ngrok_url = match (&app.ngrok_url, log_ngrok_url_reveal_remaining.is_some()) {
        (Some(url), true) => url.clone(),
        (Some(_), false) => NGROK_URL_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let log_ngrok_domain = match (
        &app.ngrok_domain,
        log_ngrok_domain_reveal_remaining.is_some(),
    ) {
        (Some(domain), true) => domain.clone(),
        (Some(_), false) => NGROK_DOMAIN_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let mcp_url_security_status = mcp_url_reveal_remaining
        .map(|remaining| format!("[ EXPOSED {:>2}s ]", mcp_url_reveal_seconds(remaining)));
    let compact_browser_summary = if app.browser_runtime_running {
        "Isolated agent browser · running".to_string()
    } else {
        "Isolated agent browser · starts on demand".to_string()
    };
    let computer_role_style = Style::default()
        .fg(if app.server_running {
            palette.success_fg
        } else {
            palette.muted_fg
        })
        .add_modifier(Modifier::BOLD);
    let chatgpt_role_style = Style::default()
        .fg(if app.remote_connected {
            palette.success_fg
        } else {
            palette.muted_fg
        })
        .add_modifier(Modifier::BOLD);
    let flow_meta_style = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);
    let lane_for = |active: bool, flow: Option<&FlowLane>| -> Vec<Span<'static>> {
        if compact_flow_layout {
            flow_lane_spans_with_cells(active, flow, &palette, now_millis, compact_flow_lane_cells)
        } else {
            flow_lane_spans(active, flow, &palette, now_millis)
        }
    };
    let request_stats_for = |app: &UiSnapshot| -> Vec<Span<'static>> {
        let request_count = if compact_flow_layout {
            format_token_compact(app.request_count)
        } else {
            app.request_count.to_string()
        };
        vec![
            Span::styled("  Requests ", Style::default().fg(palette.muted_fg)),
            Span::styled(request_count, Style::default().fg(palette.title_fg)),
        ]
    };
    let flow_row_prefix = if compact_flow_layout { "  " } else { "    " };
    let flow_left_label = if compact_flow_layout {
        "PC "
    } else {
        FLOW_LANE_LEFT_LABEL
    };
    let flow_right_label = if compact_flow_layout {
        "Web"
    } else {
        "ChatGPT Web"
    };
    let status_label_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD);
    let status_label = |label: &'static str| -> Span<'static> {
        Span::styled(
            format!("  {label:<width$} ", width = STATUS_LABEL_WIDTH),
            status_label_style,
        )
    };
    let status_content_height = status_height.saturating_sub(4) as usize;
    let flow_block_lines = 2;

    let all_time_usage_totals = app.all_time_usage_totals();
    let session_usage_cost = estimate_gpt_5_6_sol_tool_cost(&app.session_usage_totals);
    let all_time_usage_cost = estimate_gpt_5_6_sol_tool_cost(&all_time_usage_totals);
    let usage_widths = usage_value_widths(
        &app.session_usage_totals,
        session_usage_cost.standard_usd,
        &all_time_usage_totals,
        all_time_usage_cost.standard_usd,
    );
    let version_line = {
        let mut spans = vec![
            status_label("Version"),
            Span::styled(
                update::CURRENT_VERSION.to_string(),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(update) = update_info {
            spans.push(Span::styled("  ·  ", Style::default().fg(palette.muted_fg)));
            spans.push(Span::styled(
                format!("v{} available", update.latest_version),
                Style::default()
                    .fg(palette.warning_fg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "  [u] Update & Restart",
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    };
    let mut status_lines: Vec<Line> = vec![
        version_line,
        Line::from(vec![
            status_label("Mode"),
            Span::styled(
                mode_label,
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            status_label("Tool mode"),
            Span::styled(
                tool_mode_label,
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            status_label("Server"),
            Span::styled(
                &server_status,
                Style::default().fg(if app.server_running {
                    palette.success_fg
                } else {
                    palette.danger_fg
                }),
            ),
        ]),
        Line::from(vec![
            status_label("ngrok"),
            Span::styled(
                ngrok_status,
                Style::default().fg(if app.ngrok_running {
                    palette.success_fg
                } else {
                    palette.danger_fg
                }),
            ),
        ]),
        Line::from(vec![
            status_label("Browser"),
            Span::styled(
                browser_runtime_status,
                Style::default().fg(if app.browser_runtime_running {
                    palette.success_fg
                } else {
                    palette.muted_fg
                }),
            ),
        ]),
        {
            let mut spans = vec![
                status_label("MCP URL"),
                Span::styled(
                    &mcp_url,
                    Style::default().fg(if has_url {
                        if mcp_url_is_revealed {
                            palette.info_fg
                        } else {
                            palette.muted_fg
                        }
                    } else {
                        palette.muted_fg
                    }),
                ),
            ];
            if has_url {
                spans.push(Span::raw("  "));
                let security_text =
                    mcp_url_security_status
                        .as_deref()
                        .unwrap_or(if app.ngrok_running {
                            "Click to reveal"
                        } else {
                            "Offline - click to reveal"
                        });
                let security_color = match mcp_url_reveal_remaining {
                    Some(remaining) if mcp_url_reveal_seconds(remaining) <= 3 => palette.danger_fg,
                    Some(_) => palette.warning_fg,
                    None => palette.muted_fg,
                };
                spans.push(Span::styled(
                    security_text.to_string(),
                    Style::default()
                        .fg(security_color)
                        .add_modifier(Modifier::BOLD),
                ));
                if let Some(remaining) = mcp_url_reveal_remaining {
                    let (remaining_bar, elapsed_bar) = mcp_url_reveal_bar_segments(remaining);
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        remaining_bar,
                        Style::default()
                            .fg(security_color)
                            .add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        elapsed_bar,
                        Style::default().fg(palette.muted_fg),
                    ));
                }
            }
            Line::from(spans)
        },
        {
            let mut spans = vec![status_label("Remote")];
            if app.remote_connected {
                spans.push(Span::styled(
                    "V",
                    Style::default()
                        .fg(palette.success_fg)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    "X",
                    Style::default()
                        .fg(palette.danger_fg)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        },
        usage_line(
            &app.session_usage_totals,
            session_usage_cost.standard_usd,
            status_label("Session"),
            &palette,
            &usage_widths,
        ),
        usage_line(
            &all_time_usage_totals,
            all_time_usage_cost.standard_usd,
            status_label("All-time"),
            &palette,
            &usage_widths,
        ),
    ];

    if !show_workspace_pane && !show_guide {
        status_lines.insert(
            STATUS_WORKSPACES_INSERT_INDEX,
            Line::from(vec![
                status_label("Workspaces"),
                Span::styled(
                    format!(
                        "{} registered · {} connected · [w] manage",
                        app.workspace_count, app.connected_workspace_count
                    ),
                    Style::default().fg(palette.secondary_fg),
                ),
            ]),
        );
    }

    if !show_guide && app.mode.browser_enabled() {
        status_lines.push(Line::from(vec![
            status_label("Browser"),
            Span::styled(
                compact_browser_summary,
                Style::default().fg(palette.secondary_fg),
            ),
        ]));
    }

    let visible_flow_slots = if show_flow_panel {
        (status_content_height.saturating_sub(status_lines.len() + 1) / flow_block_lines.max(1))
            .min(STATUS_VISIBLE_FLOW_ROWS)
    } else {
        0
    };

    if show_flow_panel && visible_flow_slots > 0 {
        status_lines.push(Line::from(""));
        if visible_flow_count == 0 {
            let call_text = if app.remote_connected {
                "awaiting request"
            } else {
                "awaiting connection"
            };
            let call_line = if compact_flow_layout {
                vec![
                    Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                    Span::styled(
                        trim_line(
                            call_text,
                            compact_status_content_width.saturating_sub(flow_row_prefix.len()),
                        ),
                        flow_meta_style,
                    ),
                ]
            } else {
                vec![
                    Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                    Span::styled(
                        flow_call_offset(call_text),
                        Style::default().fg(palette.muted_fg),
                    ),
                    Span::styled(call_text.to_string(), flow_meta_style),
                ]
            };
            status_lines.push(Line::from(call_line));
            let lane = lane_for(false, None);
            let mut row = vec![
                Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                Span::styled(flow_left_label, computer_role_style),
            ];
            row.extend(lane);
            row.push(Span::styled(flow_right_label, chatgpt_role_style));
            if !compact_flow_layout {
                row.push(Span::styled("  ", Style::default().fg(palette.muted_fg)));
            }
            row.extend(request_stats_for(app));
            status_lines.push(Line::from(row));
        } else {
            for flow in app
                .flows
                .iter()
                .filter(|flow| should_display_flow_row(flow))
                .take(visible_flow_slots)
            {
                let latest_action = latest_flow_action(flow);
                let call_text = trim_line(
                    &format!("call {latest_action}"),
                    if compact_flow_layout {
                        compact_status_content_width.saturating_sub(flow_row_prefix.len())
                    } else {
                        FLOW_ROW_CELLS
                    },
                );
                let call_line = if compact_flow_layout {
                    vec![
                        Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                        Span::styled(call_text.clone(), flow_meta_style),
                    ]
                } else {
                    vec![
                        Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                        Span::styled(
                            flow_call_offset(&call_text),
                            Style::default().fg(palette.muted_fg),
                        ),
                        Span::styled(call_text, flow_meta_style),
                    ]
                };
                status_lines.push(Line::from(call_line));
                let closing = flow.closing_started_ms.is_some();
                let lane_active = closing
                    || !flow.anim_queue.is_empty()
                    || (app.server_running && app.ngrok_running && app.remote_connected);
                let lane = lane_for(lane_active, Some(flow));
                let mut row = vec![
                    Span::styled(flow_row_prefix, Style::default().fg(palette.muted_fg)),
                    Span::styled(flow_left_label, computer_role_style),
                ];
                row.extend(lane);
                row.push(Span::styled(flow_right_label, chatgpt_role_style));
                if !compact_flow_layout {
                    row.push(Span::styled("  ", Style::default().fg(palette.muted_fg)));
                }
                row.extend(request_stats_for(app));
                status_lines.push(Line::from(row));
            }
        }
    }

    if let Some(flow) = bootstrap_status_flow {
        status_lines = flow_bootstrap_status_lines(app, flow, &palette, now_millis);
    }

    let guide_step_style = Style::default()
        .fg(palette.title_fg)
        .add_modifier(Modifier::BOLD);
    let guide_text_style = Style::default().fg(palette.primary_fg);
    let guide_detail_style = Style::default().fg(palette.secondary_fg);
    let guide_strong_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD);
    let guide_separator_style = Style::default().fg(palette.secondary_fg);
    let guide_copyable_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let guide_lines = if show_guide {
        if app.is_returning_user {
            vec![
                Line::from(vec![
                    Span::styled("  ✅ ", guide_step_style),
                    Span::styled("Connection URL is fixed and ready!", guide_strong_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("     You do ", guide_text_style),
                    Span::styled("NOT", guide_strong_style),
                    Span::styled(" need to recreate the app in ChatGPT.", guide_text_style),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "     Simply go to your ChatGPT conversation and send a message.",
                    guide_text_style,
                )]),
                Line::from(vec![Span::styled(
                    "     MoonDesk will instantly connect and this screen will disappear.",
                    guide_detail_style,
                )]),
            ]
        } else {
            vec![
                Line::from(vec![
                    Span::styled("  1. ", guide_step_style),
                    Span::styled("Open connector settings: ", guide_text_style),
                    Span::styled("(click to copy)", guide_detail_style),
                ]),
                Line::from(vec![
                    Span::styled("     ", guide_text_style),
                    Span::styled(
                        "https://chatgpt.com/apps#settings/Connectors",
                        guide_copyable_style,
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  2. ", guide_step_style),
                    Span::styled("Click ", guide_text_style),
                    Span::styled("Create app", guide_strong_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  3. ", guide_step_style),
                    Span::styled("Fill in the form: ", guide_text_style),
                    Span::styled("(URL reveals before copy)", guide_detail_style),
                ]),
                Line::from(vec![
                    Span::styled("     Name          ", guide_detail_style),
                    Span::styled(" │ ", guide_separator_style),
                    Span::styled("MoonDesk", guide_copyable_style),
                ]),
                {
                    let mut spans = vec![
                        Span::styled("     Primary MCP URL", guide_detail_style),
                        Span::styled(" │ ", guide_separator_style),
                        Span::styled(
                            mcp_url.clone(),
                            if mcp_url_is_revealed {
                                guide_copyable_style
                            } else {
                                guide_detail_style
                            },
                        ),
                    ];
                    if has_url {
                        spans.push(Span::raw("  "));
                        let security_text = mcp_url_security_status
                            .as_deref()
                            .unwrap_or("Click to reveal");
                        let security_color = match mcp_url_reveal_remaining {
                            Some(remaining) if mcp_url_reveal_seconds(remaining) <= 3 => {
                                palette.danger_fg
                            }
                            Some(_) => palette.warning_fg,
                            None => palette.muted_fg,
                        };
                        spans.push(Span::styled(
                            security_text.to_string(),
                            Style::default()
                                .fg(security_color)
                                .add_modifier(Modifier::BOLD),
                        ));
                        if let Some(remaining) = mcp_url_reveal_remaining {
                            let (remaining_bar, elapsed_bar) =
                                mcp_url_reveal_bar_segments(remaining);
                            spans.push(Span::raw("  "));
                            spans.push(Span::styled(
                                remaining_bar,
                                Style::default()
                                    .fg(security_color)
                                    .add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::styled(
                                elapsed_bar,
                                Style::default().fg(palette.muted_fg),
                            ));
                        }
                    }
                    Line::from(spans)
                },
                Line::from(vec![
                    Span::styled("     Authentication", guide_detail_style),
                    Span::styled(" │ ", guide_separator_style),
                    Span::styled("None", guide_copyable_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  4. ", guide_step_style),
                    Span::styled("Click ", guide_text_style),
                    Span::styled("I understand and want to continue", guide_strong_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  5. ", guide_step_style),
                    Span::styled("Click ", guide_text_style),
                    Span::styled("Create", guide_strong_style),
                ]),
            ]
        }
    } else {
        Vec::new()
    };
    if show_guide {
        status_lines = guide_lines;
    }
    let primary_mcp_url_line = primary_mcp_url_line_index(
        show_guide,
        app.is_returning_user,
        bootstrap_status_flow.is_some(),
    );

    let show_mascot = area.width >= DASHBOARD_THREE_COLUMN_MIN_WIDTH;
    let status_columns = if show_workspace_pane {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(44),
                Constraint::Length(DASHBOARD_WORKSPACE_COLUMN_WIDTH),
                Constraint::Length(TUI_MASCOT_BLOCK_WIDTH),
            ])
            .split(chunks[1])
    } else if show_mascot {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(TUI_MASCOT_BLOCK_WIDTH),
            ])
            .split(chunks[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0)])
            .split(chunks[1])
    };
    let status_title = if show_guide {
        " What to do next? "
    } else if bootstrap_status_flow.is_some() {
        " MCP bootstrap "
    } else {
        " Status "
    };
    let status_block = Block::default()
        .title(status_title)
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg));
    let status_inner = status_block.inner(status_columns[0]);
    f.render_widget(status_block, status_columns[0]);

    if show_workspace_pane {
        draw_inline_workspaces(
            f,
            app,
            status_columns[1],
            workspace_filter,
            focused_bottom_panel == DashboardFocus::Workspaces,
            (workspace_scroll, workspace_visible_count),
            dashboard_hit_areas,
        );
    } else {
        dashboard_hit_areas.workspaces = None;
        dashboard_hit_areas.workspace_rows.clear();
        *workspace_visible_count = 0;
    }

    let mascot_area = if show_workspace_pane {
        Some(status_columns[2])
    } else if show_mascot {
        Some(status_columns[1])
    } else {
        None
    };
    if let Some(mascot_area) = mascot_area {
        let mascot_block = Block::default()
            .title(" ClippyMoon ")
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg));
        let mascot_inner = mascot_block.inner(mascot_area);
        f.render_widget(mascot_block, mascot_area);
        let mascot = Paragraph::new(render_tui_lines(
            app.mascot.current_tui_frame(now_millis),
            mascot_inner.height,
        ))
        .alignment(Alignment::Center);
        f.render_widget(mascot, mascot_inner);
    }

    let status_content = status_inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if let Some(area) = primary_mcp_url_line
        .filter(|_| has_url)
        .and_then(|line_index| wrapped_line_hit_area(status_content, &status_lines, line_index))
    {
        dashboard_hit_areas.secrets.push(DashboardSecretHit {
            target: DashboardSecretTarget::PrimaryMcpUrl,
            area,
        });
    }
    let status = Paragraph::new(status_lines).wrap(Wrap { trim: false });
    f.render_widget(status, status_content);

    let show_command_panel = chunks[3].width >= 100 && chunks[3].height >= 5;
    let effective_bottom_focus = match focused_bottom_panel {
        DashboardFocus::Workspaces if !show_workspace_pane => DashboardFocus::Logs,
        DashboardFocus::ShellCommands if !show_command_panel => DashboardFocus::Logs,
        focus => focus,
    };

    // ── Keys / bottom-pane focus ──
    let mut key_spans = vec![
        Span::styled("  [q]", Style::default().fg(palette.danger_fg)),
        Span::raw(" Quit  "),
        Span::styled("[w]", Style::default().fg(palette.key_fg)),
        Span::raw(" Manage  "),
        Span::styled("[Tab]", Style::default().fg(palette.key_fg)),
        Span::raw(" Focus  "),
        Span::styled("[↑/↓]", Style::default().fg(palette.key_fg)),
        Span::raw(" Navigate  "),
        Span::styled("[PgUp/PgDn]", Style::default().fg(palette.key_fg)),
        Span::raw(" Page  "),
        Span::styled("[Enter/Space]", Style::default().fg(palette.key_fg)),
        Span::raw(" Open/Select  "),
        Span::styled("[c]", Style::default().fg(palette.key_fg)),
        Span::raw(" Clear View"),
    ];
    if update_info.is_some() {
        key_spans.push(Span::raw("  "));
        key_spans.push(Span::styled(
            "[u]",
            Style::default()
                .fg(palette.warning_fg)
                .add_modifier(Modifier::BOLD),
        ));
        key_spans.push(Span::raw(" Update & Restart"));
    }
    let keys = Paragraph::new(Line::from(key_spans)).block(
        Block::default()
            .title(Line::from(vec![
                Span::raw(" Keys · Focus: "),
                Span::styled(
                    effective_bottom_focus.label(),
                    Style::default()
                        .fg(palette.info_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(keys, chunks[2]);

    let bottom_columns = if show_command_panel {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(chunks[3])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(chunks[3])
    };
    let logs_area = bottom_columns[0];
    *bottom_panel_areas = BottomPanelAreas {
        logs: logs_area,
        shell_commands: if show_command_panel {
            Some(bottom_columns[1])
        } else {
            None
        },
    };

    bottom_panel_hits.logs.clear();
    bottom_panel_hits.shell_commands.clear();

    // ── Logs ──
    let log_inner_width = logs_area.width.saturating_sub(2) as usize;
    let log_rows: Vec<Vec<Line<'static>>> = app
        .logs
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let level_color = match entry.level {
                "ERROR" => palette.danger_fg,
                "WARN" => palette.warning_fg,
                _ => palette.muted_fg,
            };
            let message = if entry.message.starts_with("MCP Server URL: ") {
                format!("MCP Server URL: {log_mcp_url}")
            } else if entry.message.starts_with("ngrok URL: ") {
                format!("ngrok URL: {log_ngrok_url}")
            } else if entry
                .message
                .starts_with("Auto-saved ngrok static domain: ")
            {
                format!("Auto-saved ngrok static domain: {log_ngrok_domain}")
            } else {
                entry.message.to_string()
            };
            let selected = selected_log == Some(index);
            let expanded = selected && expanded_log == Some(index);
            let selection_marker = if selected { ">" } else { " " };
            let workspace_tag = entry
                .workspace_id
                .as_ref()
                .and_then(|workspace_id| app.workspace_names.get(workspace_id))
                .map(|name| {
                    let (short, _) = truncate_with_ellipsis(name, 12, false);
                    format!("[{short}] ")
                })
                .unwrap_or_default();
            let prefix = format!(
                "{selection_marker} {} {:5} {workspace_tag}",
                entry.time, entry.level
            );
            let prefix_width = prefix.chars().count();
            let content_width = log_inner_width.saturating_sub(prefix_width).max(1);
            let message_style = if selected {
                Style::default()
                    .fg(palette.primary_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.primary_fg)
            };

            if expanded {
                let wrapped = wrap_preserving_chars(&message, content_width);
                let mut lines = Vec::with_capacity(wrapped.len());
                for (line_index, chunk) in wrapped.into_iter().enumerate() {
                    if line_index == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{selection_marker} {} ", entry.time),
                                Style::default().fg(palette.muted_fg),
                            ),
                            Span::styled(
                                format!("{:5} ", entry.level),
                                Style::default().fg(level_color),
                            ),
                            Span::styled(
                                workspace_tag.clone(),
                                Style::default().fg(palette.info_fg),
                            ),
                            Span::styled(chunk, message_style),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw(" ".repeat(prefix_width)),
                            Span::styled(chunk, message_style),
                        ]));
                    }
                }
                lines
            } else {
                let (compact, _) = truncate_with_ellipsis(&message, content_width, selected);
                vec![Line::from(vec![
                    Span::styled(
                        format!("{selection_marker} {} ", entry.time),
                        Style::default().fg(palette.muted_fg),
                    ),
                    Span::styled(
                        format!("{:5} ", entry.level),
                        Style::default().fg(level_color),
                    ),
                    Span::styled(workspace_tag, Style::default().fg(palette.info_fg)),
                    Span::styled(compact, message_style),
                ])]
            }
        })
        .collect();
    let log_heights: Vec<usize> = log_rows.iter().map(Vec::len).collect();
    let log_visible_height = logs_area.height.saturating_sub(2) as usize;
    let log_tail_start = tail_start_index(&log_heights, log_visible_height);
    let log_max_scroll = if log_rows.is_empty() {
        0
    } else {
        log_tail_start
    };
    let log_effective_scroll = if log_follow_tail {
        log_max_scroll
    } else {
        log_scroll.min(log_max_scroll)
    };
    *log_view = PanelScrollView {
        max_scroll: log_max_scroll,
        effective_scroll: log_effective_scroll,
    };

    let mut visible_log_items = Vec::new();
    let mut used_log_lines = 0usize;
    let log_content_last_row = logs_area
        .y
        .saturating_add(logs_area.height.saturating_sub(2));
    for (index, lines) in log_rows.into_iter().enumerate().skip(log_effective_scroll) {
        if used_log_lines >= log_visible_height {
            break;
        }
        let height = lines.len().max(1);
        let top = logs_area
            .y
            .saturating_add(1)
            .saturating_add(used_log_lines as u16);
        let bottom = top
            .saturating_add(height.saturating_sub(1) as u16)
            .min(log_content_last_row);
        bottom_panel_hits
            .logs
            .push(PanelItemHit { top, bottom, index });
        if let Some(target) = app
            .logs
            .get(index)
            .and_then(|entry| log_secret_target(&entry.message))
        {
            dashboard_hit_areas.secrets.push(DashboardSecretHit {
                target,
                area: Rect::new(
                    logs_area.x.saturating_add(1),
                    top,
                    logs_area.width.saturating_sub(2),
                    bottom.saturating_sub(top).saturating_add(1),
                ),
            });
        }
        visible_log_items.push(ListItem::new(lines));
        used_log_lines = used_log_lines.saturating_add(height);
    }

    let log_focused = effective_bottom_focus == DashboardFocus::Logs;
    let logs = List::new(visible_log_items).block(
        Block::default()
            .title(if log_focused {
                " Logs [focused] "
            } else {
                " Logs "
            })
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(if log_focused {
                palette.key_fg
            } else {
                palette.border_fg
            })),
    );
    f.render_widget(logs, logs_area);

    if show_command_panel {
        let command_area = bottom_columns[1];
        let command_inner_width = command_area.width.saturating_sub(2) as usize;
        let command_rows: Vec<Vec<Line<'static>>> = app
            .command_activities
            .iter()
            .enumerate()
            .map(|(index, activity)| {
                let (marker, state_label, state_color) = match activity.state {
                    CommandActivityState::Running => ("▶", "running", palette.info_fg),
                    CommandActivityState::Succeeded => ("✓", "succeeded", palette.primary_fg),
                    CommandActivityState::Failed => ("✗", "failed", palette.danger_fg),
                    CommandActivityState::Cancelled => ("■", "cancelled", palette.warning_fg),
                    CommandActivityState::TimedOut => ("⌛", "timed out", palette.danger_fg),
                };
                let selected = selected_command == Some(index);
                let expanded = selected && expanded_command == Some(index);
                let selection_marker = if selected { ">" } else { " " };
                let workspace_tag = app
                    .workspace_names
                    .get(&activity.workspace_id)
                    .map(|name| {
                        let (short, _) = truncate_with_ellipsis(name, 12, false);
                        format!("[{short}] ")
                    })
                    .unwrap_or_default();
                let prefix = format!(
                    "{selection_marker} {} {marker} {workspace_tag}",
                    activity.time
                );
                let prefix_width = prefix.chars().count();
                let content_width = command_inner_width.saturating_sub(prefix_width).max(1);
                let command_style = if selected {
                    Style::default()
                        .fg(palette.primary_fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.primary_fg)
                };
                let mut detail = state_label.to_string();
                if activity.background {
                    detail.push_str(" [bg]");
                }
                if let Some(exit_code) = activity.exit_code {
                    detail.push_str(&format!(" exit {exit_code}"));
                }
                if let Some(preview) = activity.preview.as_deref() {
                    detail.push_str(" · ");
                    detail.push_str(preview);
                }

                let mut lines = Vec::new();
                if expanded {
                    for (line_index, chunk) in
                        wrap_preserving_chars(&activity.command, content_width)
                            .into_iter()
                            .enumerate()
                    {
                        if line_index == 0 {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{selection_marker} {} ", activity.time),
                                    Style::default().fg(palette.muted_fg),
                                ),
                                Span::styled(
                                    format!("{marker} "),
                                    Style::default().fg(state_color),
                                ),
                                Span::styled(
                                    workspace_tag.clone(),
                                    Style::default().fg(palette.info_fg),
                                ),
                                Span::styled(chunk, command_style),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::raw(" ".repeat(prefix_width)),
                                Span::styled(chunk, command_style),
                            ]));
                        }
                    }
                    for chunk in wrap_preserving_chars(&detail, content_width) {
                        lines.push(Line::from(vec![
                            Span::raw(" ".repeat(prefix_width)),
                            Span::styled(chunk, Style::default().fg(palette.muted_fg)),
                        ]));
                    }
                } else {
                    let command_clipped = activity.command.chars().count() > content_width;
                    let detail_clipped = detail.chars().count() > content_width;
                    let expand_hint = selected && (command_clipped || detail_clipped);
                    let (compact_command, _) =
                        truncate_with_ellipsis(&activity.command, content_width, expand_hint);
                    let (compact_detail, _) =
                        truncate_with_ellipsis(&detail, content_width, selected && detail_clipped);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{selection_marker} {} ", activity.time),
                            Style::default().fg(palette.muted_fg),
                        ),
                        Span::styled(format!("{marker} "), Style::default().fg(state_color)),
                        Span::styled(workspace_tag.clone(), Style::default().fg(palette.info_fg)),
                        Span::styled(compact_command, command_style),
                    ]));
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(prefix_width)),
                        Span::styled(compact_detail, Style::default().fg(palette.muted_fg)),
                    ]));
                }
                // Deliberate breathing room: every command entry owns one blank
                // line so adjacent executions never visually run together.
                lines.push(Line::from(""));
                lines
            })
            .collect();
        let command_heights: Vec<usize> = command_rows.iter().map(Vec::len).collect();
        let command_visible_height = command_area.height.saturating_sub(2) as usize;
        let command_tail_start = tail_start_index(&command_heights, command_visible_height);
        let command_max_scroll = if command_rows.is_empty() {
            0
        } else {
            command_tail_start
        };
        let command_effective_scroll = if command_follow_tail {
            command_max_scroll
        } else {
            command_scroll.min(command_max_scroll)
        };
        *command_view = PanelScrollView {
            max_scroll: command_max_scroll,
            effective_scroll: command_effective_scroll,
        };

        let mut visible_command_items = Vec::new();
        let mut used_command_lines = 0usize;
        let command_content_last_row = command_area
            .y
            .saturating_add(command_area.height.saturating_sub(2));
        for (index, lines) in command_rows
            .into_iter()
            .enumerate()
            .skip(command_effective_scroll)
        {
            if used_command_lines >= command_visible_height {
                break;
            }
            let height = lines.len().max(1);
            let top = command_area
                .y
                .saturating_add(1)
                .saturating_add(used_command_lines as u16);
            let bottom = top
                .saturating_add(height.saturating_sub(1) as u16)
                .min(command_content_last_row);
            bottom_panel_hits
                .shell_commands
                .push(PanelItemHit { top, bottom, index });
            visible_command_items.push(ListItem::new(lines));
            used_command_lines = used_command_lines.saturating_add(height);
        }

        let command_focused = effective_bottom_focus == DashboardFocus::ShellCommands;
        let commands = List::new(visible_command_items).block(
            Block::default()
                .title(if command_focused {
                    " Shell Commands [focused] "
                } else {
                    " Shell Commands "
                })
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(if command_focused {
                    palette.key_fg
                } else {
                    palette.border_fg
                })),
        );
        f.render_widget(commands, command_area);
    } else {
        *command_view = PanelScrollView::default();
    }

    // ── Floating toast (top-most layer) ──
    if let Some((msg, pos)) = toast {
        render_toast(f, palette, msg, pos);
    }
}

#[cfg(test)]
mod tests {
    use super::state::{CommandActivity, LogEntry};
    use super::{
        AppState, BottomPanelAreas, BottomPanelHitMaps, DashboardFocus, DashboardHitAreas,
        DashboardSecretHit, DashboardSecretTarget, DashboardWorkspaceRow, InterruptState,
        ObservabilityCutoff, PanelItemHit, PanelScrollView, TimedSecretClick, UiRenderContext,
        UiSnapshot, WorkspaceFilter, WorkspaceHitAreas, WorkspaceId, WorkspaceUiAction,
        active_reveal_remaining, apply_workspace_observability_filter, cycle_dashboard_focus,
        dashboard_secret_target_at, draw_changelog_notice, draw_prompt, draw_quit_confirm, draw_ui,
        draw_update_confirm, item_under_cursor, key_is_clipboard_paste, key_is_interrupt,
        key_is_plain_quit, log_secret_target, move_panel_selection,
        normalize_ngrok_authtoken_input, normalize_ngrok_domain, normalize_workspace_path_input,
        panel_under_cursor, parse_clippymoon_export_args, parse_port_value,
        primary_mcp_url_line_index, quit_confirm_action, reconcile_workspace_filter,
        record_clear_view, reset_filtered_navigation, scroll_panel_down, scroll_panel_up,
        tail_start_index, timed_secret_click, truncate_with_ellipsis, update_confirm_action,
        user_home_dir, workspace_action_from_event, workspace_detail_sections,
        workspace_filter_from_index, workspace_filter_index, wrap_preserving_chars,
        wrapped_line_hit_area,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
        text::{Line, Span},
        widgets::{Paragraph, Wrap},
    };
    use std::{collections::HashMap, path::PathBuf};

    struct RenderedDashboard {
        text: String,
        dashboard_hits: DashboardHitAreas,
        bottom_hits: BottomPanelHitMaps,
        bottom_areas: BottomPanelAreas,
        workspace_scroll: usize,
        workspace_visible_count: usize,
    }

    fn assert_paper_frame_has_no_reset_colors(buffer: &Buffer) {
        for row in buffer.area.y..buffer.area.bottom() {
            for column in buffer.area.x..buffer.area.right() {
                let cell = &buffer[(column, row)];
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "paper frame left reset background at ({column}, {row}) for {:?}",
                    cell.symbol()
                );
                if !cell.symbol().trim().is_empty() {
                    assert_ne!(
                        cell.fg,
                        Color::Reset,
                        "paper frame left visible reset foreground at ({column}, {row}) for {:?}",
                        cell.symbol()
                    );
                }
            }
        }
    }

    fn test_workspace_id(index: u64) -> WorkspaceId {
        WorkspaceId::parse(format!("00000000-0000-0000-0000-{index:012}"))
            .expect("valid test workspace id")
    }

    fn test_dashboard_snapshot(tag: &str) -> UiSnapshot {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("{tag}-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temporary dashboard workspace");
        let config_path = workspace.join("config.toml");
        let app = AppState::new_for_test(
            3200,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create test dashboard app");
        let mut snapshot = UiSnapshot::from_app(&app);
        snapshot.logs.clear();
        snapshot.command_activities.clear();
        snapshot.flows.clear();
        snapshot.is_returning_user = true;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
        snapshot
    }

    fn configure_dashboard_workspaces(
        snapshot: &mut UiSnapshot,
        rows: &[(WorkspaceId, &str, bool)],
    ) {
        snapshot.workspaces = rows
            .iter()
            .map(|(id, name, connected)| DashboardWorkspaceRow {
                id: id.clone(),
                name: (*name).to_string(),
                connected: *connected,
            })
            .collect();
        snapshot.workspace_names = rows
            .iter()
            .map(|(id, name, _)| (id.clone(), (*name).to_string()))
            .collect();
        snapshot.workspace_count = rows.len();
        snapshot.connected_workspace_count =
            rows.iter().filter(|(_, _, connected)| *connected).count();
        snapshot.remote_connected = snapshot.connected_workspace_count > 0;
    }

    fn test_log(sequence: u64, workspace_id: Option<WorkspaceId>, message: &str) -> LogEntry {
        LogEntry {
            sequence,
            workspace_id,
            time: "12:34:56".into(),
            level: "INFO",
            message: message.into(),
        }
    }

    fn test_command(sequence: u64, workspace_id: WorkspaceId, command: &str) -> CommandActivity {
        CommandActivity {
            sequence,
            workspace_id,
            id: format!("activity-{sequence}"),
            time: "12:34:56".into(),
            command: command.into(),
            background: false,
            job_id: None,
            state: super::CommandActivityState::Succeeded,
            exit_code: Some(0),
            preview: None,
        }
    }

    fn test_flow(
        workspace_id: WorkspaceId,
        flow_id: &str,
        event: &str,
        active: bool,
    ) -> super::FlowLane {
        let mut anim_queue = std::collections::VecDeque::new();
        if active {
            anim_queue.push_back(super::FlowAnimSegment {
                kind: super::FlowAnimKind::Move,
                direction: super::FlowDirection::Forward,
                started_ms: 0,
                ends_ms: u128::MAX,
                step_ms: 1,
                start_cells: 0,
                end_cells: super::FLOW_ANIM_CELLS,
            });
        }
        super::FlowLane {
            workspace_id,
            flow_id: flow_id.into(),
            short_id: flow_id.into(),
            events: vec![event.into()],
            bootstrap_status_active: false,
            bootstrap_completed_steps: 0,
            bootstrap_pending_steps: Default::default(),
            bootstrap_status_close_deadline_ms: None,
            anim_queue,
            last_direction: super::FlowDirection::Forward,
            closing_started_ms: None,
            closing_step_ms: 0,
        }
    }

    fn render_dashboard(
        snapshot: &UiSnapshot,
        width: u16,
        height: u16,
        focus: DashboardFocus,
        workspace_filter: &WorkspaceFilter,
        selected_log: Option<usize>,
        expanded_log: Option<usize>,
    ) -> RenderedDashboard {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create dashboard terminal");
        let mut log_view = PanelScrollView::default();
        let mut command_view = PanelScrollView::default();
        let mut bottom_areas = BottomPanelAreas::default();
        let mut bottom_hits = BottomPanelHitMaps::default();
        let mut dashboard_hits = DashboardHitAreas::default();
        let mut workspace_scroll = 0usize;
        let mut workspace_visible_count = 0usize;

        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    UiRenderContext {
                        app: snapshot,
                        update_info: None,
                        log_scroll: 0,
                        log_follow_tail: true,
                        command_scroll: 0,
                        command_follow_tail: true,
                        workspace_filter,
                        workspace_scroll: &mut workspace_scroll,
                        workspace_visible_count: &mut workspace_visible_count,
                        focused_bottom_panel: focus,
                        selected_log,
                        selected_command: None,
                        expanded_log,
                        expanded_command: None,
                        log_view: &mut log_view,
                        command_view: &mut command_view,
                        bottom_panel_areas: &mut bottom_areas,
                        bottom_panel_hits: &mut bottom_hits,
                        dashboard_hit_areas: &mut dashboard_hits,
                        toast: None,
                        mcp_url_reveal_remaining: None,
                        log_mcp_url_reveal_remaining: None,
                        log_ngrok_url_reveal_remaining: None,
                        log_ngrok_domain_reveal_remaining: None,
                    },
                );
            })
            .expect("render dashboard");

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in 0..height {
            for column in 0..width {
                text.push_str(buffer[(column, row)].symbol());
            }
            text.push('\n');
        }

        RenderedDashboard {
            text,
            dashboard_hits,
            bottom_hits,
            bottom_areas,
            workspace_scroll,
            workspace_visible_count,
        }
    }

    #[test]
    fn clippymoon_without_export_subcommand_returns_usage() {
        let args = vec!["clippymoon".to_string()];
        let error = parse_clippymoon_export_args(&args).expect_err("missing subcommand must fail");
        assert!(error.starts_with("usage: moondesk clippymoon export"));
    }

    #[test]
    fn clippymoon_help_returns_usage_instead_of_starting_tui() {
        let args = vec!["clippymoon".to_string(), "--help".to_string()];
        let error = parse_clippymoon_export_args(&args).expect_err("help should return usage text");
        assert!(error.starts_with("usage: moondesk clippymoon export"));
    }

    #[test]
    fn version_flags_only_intercept_top_level_version_requests() {
        for flag in ["--version", "-V"] {
            assert!(super::cli_version_requested(&[flag.to_string()]));
        }
        assert!(!super::cli_version_requested(&[]));
        assert!(!super::cli_version_requested(&[
            "clippymoon".to_string(),
            "--version".to_string(),
        ]));
        assert!(!super::cli_version_requested(&[
            "--version".to_string(),
            "extra".to_string(),
        ]));
    }

    #[test]
    fn browser_cli_parser_is_hidden_and_preserves_argument_boundaries() {
        assert!(super::parse_browser_cli_args(&[]).is_none());
        assert!(super::parse_browser_cli_args(&["something-else".to_string()]).is_none());

        let missing = super::parse_browser_cli_args(&[super::BROWSER_CLI_FLAG.to_string()])
            .expect("browser flag should intercept")
            .expect_err("missing browser command should fail");
        assert!(missing.contains("requires a command"));

        let (command, args) = super::parse_browser_cli_args(&[
            super::BROWSER_CLI_FLAG.to_string(),
            "evaluate_script".to_string(),
            "() => location.href.includes('&x=1')".to_string(),
            "--filePath=C:\\Temp\\Moon Desk\\out.json".to_string(),
        ])
        .expect("browser flag should intercept")
        .expect("valid browser CLI arguments");
        assert_eq!(command, "evaluate_script");
        assert_eq!(
            args,
            vec![
                "() => location.href.includes('&x=1')".to_string(),
                "--filePath=C:\\Temp\\Moon Desk\\out.json".to_string(),
            ]
        );
    }

    #[test]
    fn workspace_path_input_expands_home_tilde() {
        let home = user_home_dir().expect("resolve user home directory");
        assert_eq!(
            normalize_workspace_path_input("~").expect("expand bare tilde"),
            home
        );
        assert_eq!(
            normalize_workspace_path_input("~/project").expect("expand tilde child"),
            home.join("project")
        );
        assert_eq!(
            normalize_workspace_path_input("  \"relative/project\"  ")
                .expect("normalize quoted relative path"),
            PathBuf::from("relative/project")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_workspace_picker_configures_real_explorer_folder_mode_without_showing_ui() {
        std::thread::spawn(|| {
            use windows::Win32::{
                System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
                UI::Shell::{
                    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
                    IFileOpenDialog,
                },
            };

            let _apartment = super::WindowsComApartment::initialize_sta()
                .expect("initialize a dedicated STA thread for the Explorer picker");
            let dialog: IFileOpenDialog = unsafe {
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .expect("create the Windows Explorer folder picker")
            };
            super::configure_windows_workspace_folder_dialog(&dialog)
                .expect("configure the Explorer picker for folders");
            let options = unsafe { dialog.GetOptions().expect("read configured picker options") };

            assert_ne!(options.0 & FOS_PICKFOLDERS.0, 0);
            assert_ne!(options.0 & FOS_FORCEFILESYSTEM.0, 0);
            assert_ne!(options.0 & FOS_PATHMUSTEXIST.0, 0);
        })
        .join()
        .expect("Explorer picker test thread must not panic");
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "opens the real Windows Explorer folder picker and requires user interaction"]
    fn windows_workspace_picker_interactive_smoke() {
        let selected = super::pick_workspace_folder_blocking()
            .expect("open the Windows Explorer folder picker");
        if let Some(path) = selected {
            assert!(path.is_dir(), "selected workspace path must be a directory");
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_runtime_registration_tightens_permissions_on_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let port = 49_321;
        let path = super::host_runtime_path(port).expect("resolve host runtime path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create runtime directory");
        }
        std::fs::write(&path, b"stale").expect("create stale runtime file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("make stale runtime file too permissive");

        let guard = super::write_host_runtime_registration(port, "test-secret")
            .expect("rewrite host runtime registration");
        let mode = std::fs::metadata(&path)
            .expect("read runtime metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(guard);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn attach_workspace_client_uses_runtime_token_and_requested_root() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake MoonDesk host");
        let port = listener.local_addr().expect("fake host address").port();
        let token = "attach-client-test-token";
        let runtime_guard = super::write_host_runtime_registration(port, token)
            .expect("write test host runtime registration");
        let root =
            std::env::temp_dir().join(format!("moondesk-attach-client-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create attach test workspace");
        let expected_root = root.to_string_lossy().into_owned();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(None::<(String, String)>));
        let seen_by_handler = seen.clone();

        let app = axum::Router::new().route(
            super::server::HOST_CONTROL_ROUTE,
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let seen = seen_by_handler.clone();
                    async move {
                        let received_token = headers
                            .get(super::server::HOST_CONTROL_HEADER)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let payload: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or_default();
                        let received_root = payload
                            .get("root")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        *seen.lock().await = Some((received_token, received_root));
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            "workspaceName": "Attached Project",
                            "alreadyRegistered": false,
                        }))
                    }
                },
            ),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let result = super::attach_workspace_to_running_host(port, &root)
            .await
            .expect("attach workspace through client path");
        assert_eq!(result.workspace_name, "Attached Project");
        assert!(!result.already_registered);
        assert_eq!(
            seen.lock().await.as_ref(),
            Some(&(token.to_string(), expected_root))
        );

        server.abort();
        let _ = server.await;
        drop(runtime_guard);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn port_parser_rejects_invalid_explicit_values() {
        assert_eq!(parse_port_value(None).expect("default port"), 3200);
        assert_eq!(parse_port_value(Some("8787")).expect("valid port"), 8787);
        assert!(parse_port_value(Some("0")).is_err());
        assert!(parse_port_value(Some("not-a-port")).is_err());
        assert!(parse_port_value(Some("70000")).is_err());
    }

    #[test]
    fn normalizes_and_validates_ngrok_domains() {
        let accepted = [
            ("", None),
            ("   ", None),
            ("Example.Ngrok.App", Some("example.ngrok.app")),
            ("https://EXAMPLE.NGROK.APP", Some("example.ngrok.app")),
            ("http://Example.Ngrok.App/", Some("example.ngrok.app")),
        ];
        for (input, expected) in accepted {
            assert_eq!(
                normalize_ngrok_domain(input).expect("valid ngrok domain input"),
                expected.map(str::to_string),
                "input: {input:?}"
            );
        }

        for input in [
            "ftp://example.ngrok.app",
            "https://user@example.ngrok.app",
            "https://example.ngrok.app:443",
            "https://example.ngrok.app/path",
            "https://example.ngrok.app?query=1",
            "example .ngrok.app",
            "-bad.ngrok.app",
            "bad-.ngrok.app",
            "bad..ngrok.app",
        ] {
            assert!(
                normalize_ngrok_domain(input).is_err(),
                "input should be rejected: {input:?}"
            );
        }
    }

    #[test]
    fn non_clippymoon_args_do_not_intercept_normal_startup() {
        let args = vec!["something-else".to_string()];
        assert!(
            parse_clippymoon_export_args(&args)
                .expect("non-ClippyMoon arguments should parse")
                .is_none()
        );
    }

    #[test]
    fn normalizes_plain_ngrok_token() {
        assert_eq!(
            normalize_ngrok_authtoken_input("  test-token-123  "),
            "test-token-123"
        );
    }

    #[test]
    fn extracts_token_from_ngrok_command() {
        assert_eq!(
            normalize_ngrok_authtoken_input("ngrok config add-authtoken test-token-123"),
            "test-token-123"
        );
    }

    #[test]
    fn detects_ctrl_v_as_clipboard_paste() {
        assert!(key_is_clipboard_paste(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn detects_shift_insert_as_clipboard_paste() {
        assert!(key_is_clipboard_paste(&KeyEvent::new(
            KeyCode::Insert,
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn ctrl_c_is_an_interrupt_but_plain_c_is_not() {
        assert!(key_is_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!key_is_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn only_plain_q_is_a_quit_shortcut() {
        assert!(key_is_plain_quit(&KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )));
        assert!(!key_is_plain_quit(&KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        assert!(!key_is_plain_quit(&KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::ALT,
        )));
    }

    #[test]
    fn quit_confirmation_only_accepts_enter_or_escape() {
        assert_eq!(
            quit_confirm_action(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(true),
        );
        assert_eq!(
            quit_confirm_action(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(false),
        );
        assert_eq!(
            quit_confirm_action(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None,
        );
        assert_eq!(
            quit_confirm_action(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            None,
        );
        assert_eq!(
            quit_confirm_action(&KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            None,
        );
    }

    #[test]
    fn interrupt_state_tracks_running_and_shutdown_phases() {
        let interrupts = InterruptState::default();
        assert!(!interrupts.shutdown_started());

        interrupts.request();
        interrupts.request();
        assert_eq!(interrupts.take_pending(), 2);
        assert_eq!(interrupts.take_pending(), 0);

        interrupts.begin_shutdown();
        assert!(interrupts.shutdown_started());
    }

    #[test]
    fn quit_confirmation_keeps_actions_visible_on_narrow_terminals() {
        let backend = TestBackend::new(52, 18);
        let mut terminal =
            Terminal::new(backend).expect("create compact quit confirmation terminal");
        let theme = super::theme::resolve(super::theme::DEFAULT_THEME_ID);

        terminal
            .draw(|frame| draw_quit_confirm(frame, theme, 7, 5, 2, 3))
            .expect("render quit confirmation");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Stop MoonDesk"));
        assert!(rendered.contains("Keep running"));
    }

    #[test]
    fn workspace_url_uses_a_dedicated_stable_hit_region() {
        let inner = Rect::new(4, 8, 18, 20);
        let (metadata, url) = workspace_detail_sections(inner);

        assert_eq!(metadata.x, inner.x);
        assert_eq!(metadata.y, inner.y);
        assert_eq!(metadata.width, inner.width);
        assert_eq!(metadata.height + url.height, inner.height);
        assert_eq!(url.height, 3);
        assert_eq!(url.y, inner.y + inner.height - url.height);
        assert_eq!(url.x, inner.x);
        assert_eq!(url.width, inner.width);
    }
    #[test]
    fn workspace_mouse_actions_hit_reveal_copy_and_project_rows() {
        let hit_areas = WorkspaceHitAreas {
            project_rows: vec![(3, Rect::new(2, 4, 30, 2))],
            reveal: Some(Rect::new(20, 20, 10, 1)),
            copy: Some(Rect::new(31, 20, 9, 1)),
            ..WorkspaceHitAreas::default()
        };
        let click = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        assert_eq!(
            workspace_action_from_event(click(5, 5), &hit_areas),
            Some(WorkspaceUiAction::Select(3))
        );
        assert_eq!(
            workspace_action_from_event(click(22, 20), &hit_areas),
            Some(WorkspaceUiAction::Reveal)
        );
        assert_eq!(
            workspace_action_from_event(click(34, 20), &hit_areas),
            Some(WorkspaceUiAction::Copy)
        );
    }

    #[test]
    fn dashboard_secret_hitboxes_target_primary_and_log_values_without_text_inference() {
        let hit_areas = DashboardHitAreas {
            secrets: vec![
                DashboardSecretHit {
                    target: DashboardSecretTarget::PrimaryMcpUrl,
                    area: Rect::new(20, 8, 48, 1),
                },
                DashboardSecretHit {
                    target: DashboardSecretTarget::LogNgrokDomain,
                    area: Rect::new(2, 20, 70, 2),
                },
            ],
            ..DashboardHitAreas::default()
        };

        assert_eq!(
            dashboard_secret_target_at(&hit_areas, 24, 8),
            Some(DashboardSecretTarget::PrimaryMcpUrl)
        );
        assert_eq!(
            dashboard_secret_target_at(&hit_areas, 30, 21),
            Some(DashboardSecretTarget::LogNgrokDomain)
        );
        assert_eq!(dashboard_secret_target_at(&hit_areas, 24, 9), None);
    }

    #[test]
    fn every_secret_log_kind_maps_to_a_dedicated_reveal_target() {
        assert_eq!(
            log_secret_target("MCP Server URL: https://secret/mcp"),
            Some(DashboardSecretTarget::LogMcpUrl)
        );
        assert_eq!(
            log_secret_target("ngrok URL: https://secret"),
            Some(DashboardSecretTarget::LogNgrokUrl)
        );
        assert_eq!(
            log_secret_target("Auto-saved ngrok static domain: secret.ngrok.app"),
            Some(DashboardSecretTarget::LogNgrokDomain)
        );
        assert_eq!(log_secret_target("ordinary status message"), None);
    }

    #[test]
    fn every_dashboard_secret_uses_the_same_ten_second_reveal_then_copy_method() {
        let now = std::time::Instant::now();
        let mut revealed_until = None;

        assert_eq!(
            timed_secret_click(Some("https://secret/mcp"), &mut revealed_until, now),
            Some(TimedSecretClick::Revealed)
        );
        assert_eq!(
            revealed_until,
            Some(now + std::time::Duration::from_secs(10))
        );
        assert_eq!(
            timed_secret_click(
                Some("https://secret/mcp"),
                &mut revealed_until,
                now + std::time::Duration::from_secs(5),
            ),
            Some(TimedSecretClick::Copy("https://secret/mcp".into()))
        );

        let mut unavailable_deadline = None;
        assert_eq!(
            timed_secret_click(None, &mut unavailable_deadline, now),
            None
        );
        assert_eq!(unavailable_deadline, None);
    }

    #[test]
    fn dashboard_primary_url_logical_line_tracks_status_and_connect_guide() {
        assert_eq!(primary_mcp_url_line_index(false, false, false), Some(6));
        assert_eq!(primary_mcp_url_line_index(true, false, false), Some(7));
        assert_eq!(primary_mcp_url_line_index(false, false, true), None);
        assert_eq!(primary_mcp_url_line_index(true, true, false), None);
    }

    #[test]
    fn dashboard_primary_url_hitbox_matches_every_wrapped_rendered_row() {
        let target_color = Color::Rgb(17, 181, 229);
        let target_index = 2;
        let lines = vec![
            Line::from("Open connector settings at https://chatgpt.com/apps#settings/Connectors"),
            Line::from("Fill in the form using these connection details"),
            Line::from(Span::styled(
                "Primary MCP URL | https://example.ngrok.app/WorkspaceSecret123/mcp",
                Style::default().fg(target_color),
            )),
            Line::from("Authentication | None"),
        ];
        let content = Rect::new(2, 1, 22, 14);
        let hit_area = wrapped_line_hit_area(content, &lines, target_index)
            .expect("wrapped primary URL should remain visible");
        let backend = TestBackend::new(28, 18);
        let mut terminal = Terminal::new(backend).expect("create narrow dashboard terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
            })
            .expect("render wrapped dashboard content");

        let buffer = terminal.backend().buffer();
        let rendered_target_rows = (content.y..content.y.saturating_add(content.height))
            .filter(|row| {
                (content.x..content.x.saturating_add(content.width))
                    .any(|column| buffer[(column, *row)].fg == target_color)
            })
            .collect::<Vec<_>>();
        let first_target_row = rendered_target_rows
            .first()
            .copied()
            .expect("target URL should render");

        assert!(first_target_row > content.y + target_index as u16);
        assert!(rendered_target_rows.len() > 1);
        assert_eq!(hit_area.y, first_target_row);
        assert_eq!(hit_area.height as usize, rendered_target_rows.len());
        assert_eq!(hit_area.x, content.x);
        assert_eq!(hit_area.width, content.width);
    }

    #[test]
    fn dashboard_primary_url_remains_clickable_with_saved_domain_while_ngrok_is_offline() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("moondesk-offline-url-hitbox-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create temporary workspace");
        let config_path = workspace.join("config.toml");
        let mut app = AppState::new_for_test(
            3200,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create test app state");
        app.set_ngrok_domain(Some("saved-domain.ngrok-free.app".into()));
        app.ngrok_running = false;
        app.ngrok_url = None;
        let snapshot = UiSnapshot::from_app(&app);

        assert!(snapshot.public_mcp_url().is_some());
        assert!(!snapshot.ngrok_running);
        assert!(snapshot.ngrok_url.is_none());

        let backend = TestBackend::new(110, 42);
        let mut terminal = Terminal::new(backend).expect("create dashboard terminal");
        let mut log_view = PanelScrollView::default();
        let mut command_view = PanelScrollView::default();
        let mut bottom_panel_areas = BottomPanelAreas::default();
        let mut bottom_panel_hits = BottomPanelHitMaps::default();
        let mut dashboard_hit_areas = DashboardHitAreas::default();

        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    UiRenderContext {
                        app: &snapshot,
                        update_info: None,
                        log_scroll: 0,
                        log_follow_tail: true,
                        command_scroll: 0,
                        command_follow_tail: true,
                        workspace_filter: &super::WorkspaceFilter::All,
                        workspace_scroll: &mut 0,
                        workspace_visible_count: &mut 0,
                        focused_bottom_panel: DashboardFocus::Logs,
                        selected_log: None,
                        selected_command: None,
                        expanded_log: None,
                        expanded_command: None,
                        log_view: &mut log_view,
                        command_view: &mut command_view,
                        bottom_panel_areas: &mut bottom_panel_areas,
                        bottom_panel_hits: &mut bottom_panel_hits,
                        dashboard_hit_areas: &mut dashboard_hit_areas,
                        toast: None,
                        mcp_url_reveal_remaining: None,
                        log_mcp_url_reveal_remaining: None,
                        log_ngrok_url_reveal_remaining: None,
                        log_ngrok_domain_reveal_remaining: None,
                    },
                );
            })
            .expect("render offline dashboard");

        let hit = dashboard_hit_areas
            .secrets
            .iter()
            .find(|hit| hit.target == DashboardSecretTarget::PrimaryMcpUrl)
            .copied()
            .expect("offline primary URL should have a reveal hit area");
        assert!(hit.area.height > 0);
        assert_eq!(
            dashboard_secret_target_at(&dashboard_hit_areas, hit.area.x, hit.area.y),
            Some(DashboardSecretTarget::PrimaryMcpUrl)
        );

        let now = std::time::Instant::now();
        let mut revealed_until = None;
        assert_eq!(
            timed_secret_click(
                snapshot.public_mcp_url().as_deref(),
                &mut revealed_until,
                now,
            ),
            Some(TimedSecretClick::Revealed)
        );
        assert_eq!(
            revealed_until,
            Some(now + std::time::Duration::from_secs(10))
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn url_reveal_deadline_is_inactive_at_exact_expiry() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);

        assert_eq!(
            active_reveal_remaining(Some(deadline), now),
            Some(std::time::Duration::from_secs(10))
        );
        assert_eq!(active_reveal_remaining(Some(deadline), deadline), None);
        assert_eq!(active_reveal_remaining(None, now), None);
    }

    #[test]
    fn bottom_panels_keep_independent_scroll_state() {
        let view = PanelScrollView {
            max_scroll: 20,
            effective_scroll: 20,
        };
        let log_scroll = 20;
        let log_follow = true;
        let mut command_scroll = 20;
        let mut command_follow = true;

        scroll_panel_up(&mut command_scroll, &mut command_follow, view, 1);
        assert_eq!(command_scroll, 19);
        assert!(!command_follow);
        assert_eq!(log_scroll, 20);
        assert!(log_follow);

        scroll_panel_down(&mut command_scroll, &mut command_follow, view, 1);
        assert_eq!(command_scroll, 20);
        assert!(command_follow);
    }

    #[test]
    fn mouse_targeting_selects_panel_under_cursor() {
        let areas = BottomPanelAreas {
            logs: Rect::new(0, 20, 40, 10),
            shell_commands: Some(Rect::new(40, 20, 60, 10)),
        };
        assert_eq!(
            panel_under_cursor(areas, 10, 24),
            Some(DashboardFocus::Logs)
        );
        assert_eq!(
            panel_under_cursor(areas, 70, 24),
            Some(DashboardFocus::ShellCommands)
        );
        assert_eq!(panel_under_cursor(areas, 10, 5), None);
    }

    #[test]
    fn variable_height_item_hit_testing_selects_the_rendered_entry() {
        let hits = vec![
            PanelItemHit {
                top: 10,
                bottom: 12,
                index: 4,
            },
            PanelItemHit {
                top: 13,
                bottom: 18,
                index: 5,
            },
        ];
        assert_eq!(item_under_cursor(&hits, 11), Some(4));
        assert_eq!(item_under_cursor(&hits, 17), Some(5));
        assert_eq!(item_under_cursor(&hits, 19), None);
    }

    #[test]
    fn compact_text_uses_explicit_ellipsis_and_expand_hint() {
        let (plain, plain_clipped) = truncate_with_ellipsis("abcdefghijklmnop", 8, false);
        assert!(plain_clipped);
        assert_eq!(plain, "abcdefg…");

        let (hinted, hinted_clipped) = truncate_with_ellipsis("abcdefghijklmnop", 12, true);
        assert!(hinted_clipped);
        assert_eq!(hinted, "abc… [Enter]");
    }

    #[test]
    fn expanded_wrapping_preserves_full_command_characters() {
        let command = "Write-Output 'one two'; Start-Sleep -Milliseconds 700";
        let wrapped = wrap_preserving_chars(command, 9);
        assert!(wrapped.len() > 1);
        assert_eq!(wrapped.concat(), command);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 9));
    }

    #[test]
    fn tail_start_respects_variable_entry_heights() {
        // Three compact commands at three lines each fit in nine lines.
        assert_eq!(tail_start_index(&[3, 3, 3, 3], 9), 1);
        // Expanding the newest command to seven lines leaves room for only it.
        assert_eq!(tail_start_index(&[3, 3, 3, 7], 9), 3);
    }

    #[test]
    fn selection_navigation_clamps_to_available_items() {
        let mut selected = Some(4);
        move_panel_selection(&mut selected, 5, -2);
        assert_eq!(selected, Some(2));
        move_panel_selection(&mut selected, 5, 20);
        assert_eq!(selected, Some(4));
        move_panel_selection(&mut selected, 0, -1);
        assert_eq!(selected, None);
    }
    #[test]
    fn workspace_observability_filter_scopes_logs_commands_flows_and_global_rows() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-filter-test");
        let workspace_a = test_workspace_id(10);
        let workspace_b = test_workspace_id(11);
        configure_dashboard_workspaces(
            &mut snapshot,
            &[
                (workspace_a.clone(), "SiteGPT", true),
                (workspace_b.clone(), "KUBA", false),
            ],
        );
        snapshot.logs = vec![
            test_log(1, None, "global-log"),
            test_log(2, Some(workspace_b.clone()), "kuba-log"),
            test_log(3, Some(workspace_a.clone()), "sitegpt-log"),
        ];
        snapshot.command_activities = [
            test_command(1, workspace_b.clone(), "kuba-command"),
            test_command(2, workspace_a.clone(), "sitegpt-command"),
        ]
        .into_iter()
        .collect();
        snapshot.flows = vec![
            test_flow(
                workspace_b.clone(),
                "workspace-b:stateless",
                "tools/call:poll_command",
                true,
            ),
            test_flow(
                workspace_a.clone(),
                "workspace-a:stateless",
                "tools/call:run_command",
                true,
            ),
        ];
        let cutoffs = HashMap::new();

        let mut all = snapshot.clone();
        apply_workspace_observability_filter(&mut all, &WorkspaceFilter::All, &cutoffs);
        assert_eq!(all.logs.len(), 3);
        assert_eq!(all.command_activities.len(), 2);
        assert_eq!(all.flows.len(), 2);
        assert!(all.logs.iter().any(|entry| entry.workspace_id.is_none()));

        let focus = WorkspaceFilter::Workspace(workspace_a.clone());
        let mut selected = snapshot.clone();
        apply_workspace_observability_filter(&mut selected, &focus, &cutoffs);
        assert_eq!(
            selected
                .logs
                .iter()
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            vec!["sitegpt-log"]
        );
        assert_eq!(
            selected
                .command_activities
                .iter()
                .map(|activity| activity.command.as_str())
                .collect::<Vec<_>>(),
            vec!["sitegpt-command"]
        );
        assert_eq!(selected.flows.len(), 1);
        assert_eq!(&selected.flows[0].workspace_id, &workspace_a);
        assert!(
            selected
                .logs
                .iter()
                .all(|entry| entry.workspace_id.is_some())
        );
    }

    #[test]
    fn clear_view_uses_sequence_cutoffs_without_mutating_or_cross_clearing_views() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-clear-test");
        let workspace_a = test_workspace_id(20);
        let workspace_b = test_workspace_id(21);
        configure_dashboard_workspaces(
            &mut snapshot,
            &[
                (workspace_a.clone(), "SiteGPT", true),
                (workspace_b.clone(), "KUBA", true),
            ],
        );
        snapshot.logs = vec![
            test_log(1, None, "global-old"),
            test_log(2, Some(workspace_a.clone()), "a-old"),
            test_log(3, Some(workspace_b.clone()), "b-old"),
        ];
        snapshot.command_activities = [
            test_command(1, workspace_a.clone(), "a-old-command"),
            test_command(2, workspace_b.clone(), "b-old-command"),
        ]
        .into_iter()
        .collect();
        let backing_log_count = snapshot.logs.len();
        let backing_command_count = snapshot.command_activities.len();
        let filter_a = WorkspaceFilter::Workspace(workspace_a.clone());
        let filter_b = WorkspaceFilter::Workspace(workspace_b.clone());
        let mut cutoffs: HashMap<WorkspaceFilter, ObservabilityCutoff> = HashMap::new();

        let mut visible_a = snapshot.clone();
        apply_workspace_observability_filter(&mut visible_a, &filter_a, &cutoffs);
        record_clear_view(&visible_a, &filter_a, &mut cutoffs);
        assert_eq!(snapshot.logs.len(), backing_log_count);
        assert_eq!(snapshot.command_activities.len(), backing_command_count);

        let mut cleared_a = snapshot.clone();
        apply_workspace_observability_filter(&mut cleared_a, &filter_a, &cutoffs);
        assert!(cleared_a.logs.is_empty());
        assert!(cleared_a.command_activities.is_empty());

        let mut unaffected_b = snapshot.clone();
        apply_workspace_observability_filter(&mut unaffected_b, &filter_b, &cutoffs);
        assert_eq!(unaffected_b.logs.len(), 1);
        assert_eq!(unaffected_b.command_activities.len(), 1);

        let mut unaffected_all = snapshot.clone();
        apply_workspace_observability_filter(&mut unaffected_all, &WorkspaceFilter::All, &cutoffs);
        assert_eq!(unaffected_all.logs.len(), 3);
        assert_eq!(unaffected_all.command_activities.len(), 2);

        snapshot
            .logs
            .push(test_log(4, Some(workspace_a.clone()), "a-new"));
        snapshot.command_activities.push_back(test_command(
            3,
            workspace_a.clone(),
            "a-new-command",
        ));
        let mut new_a = snapshot.clone();
        apply_workspace_observability_filter(&mut new_a, &filter_a, &cutoffs);
        assert_eq!(new_a.logs.len(), 1);
        assert_eq!(new_a.logs[0].message, "a-new");
        assert_eq!(new_a.command_activities.len(), 1);
        assert_eq!(new_a.command_activities[0].command, "a-new-command");

        let mut aggregate_now = snapshot.clone();
        apply_workspace_observability_filter(&mut aggregate_now, &WorkspaceFilter::All, &cutoffs);
        record_clear_view(&aggregate_now, &WorkspaceFilter::All, &mut cutoffs);
        let mut cleared_all = snapshot.clone();
        apply_workspace_observability_filter(&mut cleared_all, &WorkspaceFilter::All, &cutoffs);
        assert!(cleared_all.logs.is_empty());
        assert!(cleared_all.command_activities.is_empty());
    }

    #[test]
    fn removed_selected_workspace_falls_back_to_all_workspaces() {
        let workspace = test_workspace_id(30);
        let mut filter = WorkspaceFilter::Workspace(workspace.clone());
        let present = vec![DashboardWorkspaceRow {
            id: workspace,
            name: "SiteGPT".into(),
            connected: true,
        }];
        assert!(!reconcile_workspace_filter(&mut filter, &present));
        assert!(reconcile_workspace_filter(&mut filter, &[]));
        assert_eq!(filter, WorkspaceFilter::All);
    }

    #[test]
    fn dashboard_focus_cycles_available_panes_in_both_directions() {
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::Workspaces, true, true, false),
            DashboardFocus::Logs
        );
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::Logs, true, true, false),
            DashboardFocus::ShellCommands
        );
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::ShellCommands, true, true, false),
            DashboardFocus::Workspaces
        );
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::Workspaces, true, true, true),
            DashboardFocus::ShellCommands
        );
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::Logs, false, true, true),
            DashboardFocus::ShellCommands
        );
        assert_eq!(
            cycle_dashboard_focus(DashboardFocus::Logs, false, false, false),
            DashboardFocus::Logs
        );
    }

    #[test]
    fn filter_change_reset_restores_tail_and_collapses_expanded_rows() {
        let mut log_scroll = 8;
        let mut log_follow_tail = false;
        let mut command_scroll = 6;
        let mut command_follow_tail = false;
        let mut selected_log = Some(7);
        let mut selected_command = Some(4);
        let mut expanded_log = Some(7);
        let mut expanded_command = Some(4);
        reset_filtered_navigation(
            (&mut log_scroll, &mut log_follow_tail),
            (&mut command_scroll, &mut command_follow_tail),
            (&mut selected_log, &mut selected_command),
            (&mut expanded_log, &mut expanded_command),
        );
        assert_eq!(log_scroll, 0);
        assert!(log_follow_tail);
        assert_eq!(command_scroll, 0);
        assert!(command_follow_tail);
        assert_eq!(selected_log, None);
        assert_eq!(selected_command, None);
        assert_eq!(expanded_log, None);
        assert_eq!(expanded_command, None);
    }

    #[test]
    fn gpt_5_6_sol_cost_estimate_prices_tool_direction_and_cache_bounds() {
        let usage = super::UsageTotals {
            tool_input_tokens: 1_000_000,
            tool_output_tokens: 1_000_000,
            total_tokens: 2_000_000,
            tool_call_count: 1,
        };

        let estimate = super::estimate_gpt_5_6_sol_tool_cost(&usage);
        assert!((estimate.standard_usd - 24.0).abs() < f64::EPSILON);
        assert!((estimate.cached_read_usd - 20.4).abs() < f64::EPSILON);
        assert!((estimate.cache_write_usd - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compact_status_labels_fit_the_shared_alignment_width() {
        for label in [
            "Version",
            "Mode",
            "Tool mode",
            "Server",
            "ngrok",
            "MCP URL",
            "Remote",
            "Session",
            "All-time",
            "Workspaces",
            "Browser",
        ] {
            assert!(label.len() <= super::STATUS_LABEL_WIDTH, "{label}");
        }
        assert_eq!("Workspaces".len(), super::STATUS_LABEL_WIDTH);
        assert_eq!(
            super::STATUS_WORKSPACES_INSERT_INDEX,
            super::STATUS_PRIMARY_MCP_URL_LINE + 1
        );
    }

    #[test]
    fn wide_dashboard_renders_status_workspaces_and_clippymoon_without_shrinking_bottom() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-wide-test");
        let workspace_a = test_workspace_id(40);
        let workspace_b = test_workspace_id(41);
        configure_dashboard_workspaces(
            &mut snapshot,
            &[
                (workspace_a.clone(), "SiteGPT", true),
                (workspace_b.clone(), "KUBA", false),
            ],
        );
        snapshot.request_count = 1_234;
        snapshot.flows = vec![
            test_flow(
                workspace_a.clone(),
                "workspace-a:stateless",
                "tools/call:run_command",
                true,
            ),
            test_flow(
                workspace_b.clone(),
                "workspace-b:stateless",
                "tools/call:poll_command",
                true,
            ),
        ];
        let rendered = render_dashboard(
            &snapshot,
            120,
            44,
            DashboardFocus::Workspaces,
            &WorkspaceFilter::All,
            None,
            None,
        );
        assert!(rendered.text.contains("Status"));
        assert!(rendered.text.contains("Workspaces [focused]"));
        assert!(rendered.text.contains("ClippyMoon"));
        assert!(rendered.text.contains("● SiteGPT connected"));
        assert!(rendered.text.contains("○ KUBA idle"));
        assert!(rendered.dashboard_hits.workspaces.is_some());
        assert_eq!(rendered.dashboard_hits.workspace_rows.len(), 3);
        assert!(rendered.bottom_areas.shell_commands.is_some());
        assert!(rendered.bottom_areas.logs.height >= 15);
        assert!(!rendered.text.contains("tool input, llm output"));
        assert!(rendered.text.contains("PC "));
        assert!(rendered.text.contains("call run_command"));
        assert!(!rendered.text.contains("call poll_command"));
        assert!(rendered.text.contains("~$"));
        assert_eq!(rendered.text.matches("Web  Requests 1.2K").count(), 1);
        assert!(rendered.text.contains("[c] Clear View"));
    }

    #[test]
    fn remote_connection_does_not_keep_idle_flow_visible() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-idle-flow-test");
        let workspace = test_workspace_id(45);
        configure_dashboard_workspaces(&mut snapshot, &[(workspace.clone(), "SiteGPT", true)]);
        snapshot.flows = vec![test_flow(
            workspace,
            "workspace:stateless",
            "tools/call:stale_tool",
            false,
        )];

        let rendered = render_dashboard(
            &snapshot,
            120,
            44,
            DashboardFocus::Workspaces,
            &WorkspaceFilter::All,
            None,
            None,
        );

        assert!(rendered.text.contains("awaiting request"));
        assert!(!rendered.text.contains("call stale_tool"));
    }

    #[test]
    fn narrow_dashboard_hides_inline_workspace_pane_and_keeps_status_summary() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-narrow-test");
        let workspace_a = test_workspace_id(50);
        let workspace_b = test_workspace_id(51);
        configure_dashboard_workspaces(
            &mut snapshot,
            &[(workspace_a, "SiteGPT", true), (workspace_b, "KUBA", false)],
        );
        let rendered = render_dashboard(
            &snapshot,
            110,
            44,
            DashboardFocus::Logs,
            &WorkspaceFilter::All,
            None,
            None,
        );
        assert!(rendered.dashboard_hits.workspaces.is_none());
        assert_eq!(rendered.workspace_visible_count, 0);
        assert!(
            rendered
                .text
                .contains("2 registered · 1 connected · [w] manage")
        );
        assert!(!rendered.text.contains("Workspaces [focused]"));
        assert!(rendered.bottom_areas.shell_commands.is_some());
        assert!(rendered.bottom_areas.logs.height >= 15);
    }

    #[test]
    fn inline_workspace_pane_scrolls_many_rows_and_hit_map_keeps_stable_workspace_ids() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-workspace-scroll-test");
        let rows = (1..=25)
            .map(|index| {
                (
                    test_workspace_id(100 + index),
                    format!("workspace-{index:02}"),
                    index % 2 == 0,
                )
            })
            .collect::<Vec<_>>();
        snapshot.workspaces = rows
            .iter()
            .map(|(id, name, connected)| DashboardWorkspaceRow {
                id: id.clone(),
                name: name.clone(),
                connected: *connected,
            })
            .collect();
        snapshot.workspace_names = rows
            .iter()
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .collect();
        snapshot.workspace_count = rows.len();
        snapshot.connected_workspace_count =
            rows.iter().filter(|(_, _, connected)| *connected).count();
        let last_id = rows.last().expect("last workspace").0.clone();
        let filter = WorkspaceFilter::Workspace(last_id.clone());
        let rendered = render_dashboard(
            &snapshot,
            140,
            44,
            DashboardFocus::Workspaces,
            &filter,
            None,
            None,
        );
        assert!(rendered.workspace_scroll > 0);
        assert!(rendered.workspace_visible_count < rows.len() + 1);
        assert!(rendered.text.contains("/ 26"));
        let last_hit = rendered
            .dashboard_hits
            .workspace_rows
            .last()
            .expect("visible workspace hit");
        assert_eq!(
            workspace_filter_from_index(last_hit.index, &snapshot.workspaces),
            WorkspaceFilter::Workspace(last_id)
        );
        assert_eq!(workspace_filter_index(&filter, &snapshot.workspaces), 25);
    }

    #[test]
    fn filtered_rendering_uses_filtered_indexes_for_selection_expansion_and_mouse_hits() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-filtered-hit-test");
        let workspace_a = test_workspace_id(60);
        let workspace_b = test_workspace_id(61);
        configure_dashboard_workspaces(
            &mut snapshot,
            &[
                (workspace_a.clone(), "SiteGPT", true),
                (workspace_b.clone(), "KUBA", true),
            ],
        );
        snapshot.logs = vec![
            test_log(1, None, "global-hidden"),
            test_log(2, Some(workspace_b), "kuba-hidden"),
            test_log(
                3,
                Some(workspace_a.clone()),
                "sitegpt-visible-entry-that-wraps-when-expanded-for-index-testing",
            ),
            test_log(4, Some(workspace_a.clone()), "sitegpt-visible-second"),
        ];
        let filter = WorkspaceFilter::Workspace(workspace_a);
        apply_workspace_observability_filter(&mut snapshot, &filter, &HashMap::new());
        let rendered = render_dashboard(
            &snapshot,
            140,
            44,
            DashboardFocus::Logs,
            &filter,
            Some(0),
            Some(0),
        );
        assert_eq!(snapshot.logs.len(), 2);
        assert_eq!(rendered.bottom_hits.logs.len(), 2);
        assert_eq!(rendered.bottom_hits.logs[0].index, 0);
        assert_eq!(rendered.bottom_hits.logs[1].index, 1);
        assert_eq!(
            item_under_cursor(&rendered.bottom_hits.logs, rendered.bottom_hits.logs[0].top),
            Some(0)
        );
        assert!(rendered.text.contains("sitegpt-visible-entry"));
        assert!(rendered.text.contains("sitegpt-visible-second"));
        assert!(!rendered.text.contains("global-hidden"));
        assert!(!rendered.text.contains("kuba-hidden"));
    }

    #[test]
    fn browser_status_is_hidden_in_computer_mode_and_compact_in_browser_mode() {
        let mut snapshot = test_dashboard_snapshot("moondesk-dashboard-browser-status-test");
        snapshot.mode = super::Mode::Computer;
        let computer = render_dashboard(
            &snapshot,
            140,
            44,
            DashboardFocus::Logs,
            &WorkspaceFilter::All,
            None,
            None,
        );
        assert!(!computer.text.contains("Local browsers"));
        assert!(!computer.text.contains("Remote dbg support"));
        assert!(!computer.text.contains("Remote dbg active"));
        assert!(!computer.text.contains("Selected browser"));
        assert!(!computer.text.contains("Selected target"));
        assert!(!computer.text.contains("Isolated agent browser"));

        snapshot.mode = super::Mode::Browser;
        let browser = render_dashboard(
            &snapshot,
            140,
            44,
            DashboardFocus::Logs,
            &WorkspaceFilter::All,
            None,
            None,
        );
        assert!(browser.text.contains("Browser"));
        assert!(browser.text.contains("Isolated agent browser"));
        assert!(browser.text.contains("starts on demand"));
        assert!(!browser.text.contains("Local browsers"));
        assert!(!browser.text.contains("Remote dbg support"));
    }

    #[test]
    fn update_confirmation_only_accepts_enter_or_escape() {
        assert_eq!(update_confirm_action(KeyCode::Enter), Some(true));
        assert_eq!(update_confirm_action(KeyCode::Esc), Some(false));
        assert_eq!(update_confirm_action(KeyCode::Char('q')), None);
        assert_eq!(update_confirm_action(KeyCode::Char('u')), None);
    }

    #[test]
    fn update_confirmation_keeps_enter_and_escape_visible_on_narrow_terminals() {
        let backend = TestBackend::new(52, 16);
        let mut terminal =
            Terminal::new(backend).expect("create compact update confirmation terminal");
        let theme = super::theme::resolve(super::theme::DEFAULT_THEME_ID);
        let update_info = super::update::UpdateInfo {
            current_version: "1.2.3".into(),
            latest_version: "1.2.4".into(),
            release_notes: vec!["Add a neat update changelog".into()],
            release_url: Some("https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into()),
        };

        terminal
            .draw(|frame| draw_update_confirm(frame, theme, &update_info, 2))
            .expect("render compact update confirmation");

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for row in 0..16 {
            for column in 0..52 {
                rendered.push_str(buffer[(column, row)].symbol());
            }
            rendered.push('\n');
        }
        assert!(rendered.contains("What's new"));
        assert!(rendered.contains("Add a neat update changelog"));
        assert!(rendered.contains("Finish active work before updating."));
        assert!(rendered.contains("Restart disconnects MCP."));
        assert!(rendered.contains("2 active commands will stop."));
        assert!(rendered.contains("[Enter] Update & Restart"));
        assert!(rendered.contains("[Esc] Abort"));
    }

    #[test]
    fn update_confirmation_warns_that_sessions_disconnect_before_proceeding() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("create update confirmation terminal");
        let theme = super::theme::resolve(super::theme::DEFAULT_THEME_ID);
        let update_info = super::update::UpdateInfo {
            current_version: "1.2.3".into(),
            latest_version: "1.2.4".into(),
            release_notes: vec!["Add a neat update changelog".into()],
            release_url: Some("https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into()),
        };

        terminal
            .draw(|frame| draw_update_confirm(frame, theme, &update_info, 2))
            .expect("render update confirmation");

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for row in 0..30 {
            for column in 0..120 {
                rendered.push_str(buffer[(column, row)].symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("What's new"));
        assert!(rendered.contains("Add a neat update changelog"));
        assert!(
            rendered
                .contains("Release: https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4")
        );
        assert!(rendered.contains("Make sure no ChatGPT/MCP session or command is currently"));
        assert!(
            rendered
                .contains("Updating restarts MoonDesk, so the current connection will be lost.")
        );
        assert!(rendered.contains("Detected now: 2 active commands will be stopped."));
        assert!(rendered.contains("[Enter] Continue with Update & Restart"));
        assert!(rendered.contains("[Esc] Abort"));
    }

    #[test]
    fn update_and_changelog_controls_stay_visible_at_standard_compact_size() {
        let theme = super::theme::resolve(super::theme::DEFAULT_THEME_ID);
        let update_info = super::update::UpdateInfo {
            current_version: "1.2.3".into(),
            latest_version: "1.2.4".into(),
            release_notes: (1..=6)
                .map(|index| format!("Release change {index}"))
                .collect(),
            release_url: Some("https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into()),
        };
        let notice = super::update::ChangelogNotice {
            from_version: "1.2.3".into(),
            to_version: "1.2.4".into(),
            release_notes: vec![],
            release_url: None,
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("create standard compact terminal");
        terminal
            .draw(|frame| draw_update_confirm(frame, theme, &update_info, 1))
            .expect("render standard compact update confirmation");
        let mut update_rendered = String::new();
        for row in 0..24 {
            for column in 0..80 {
                update_rendered.push_str(terminal.backend().buffer()[(column, row)].symbol());
            }
            update_rendered.push('\n');
        }
        assert!(update_rendered.contains("MoonDesk 1.2.3  →  1.2.4"));
        assert!(update_rendered.contains("Release change 1"));
        assert!(update_rendered.contains("Make sure no ChatGPT/MCP session"));
        assert!(update_rendered.contains("Updating restarts MoonDesk"));
        assert!(update_rendered.contains("After the exact new version"));
        assert!(update_rendered.contains("Detected now: 1 active command will be stopped."));
        assert!(update_rendered.contains("[Enter] Continue with Update & Restart"));
        assert!(update_rendered.contains("[Esc] Abort"));

        terminal
            .draw(|frame| draw_changelog_notice(frame, theme, &notice))
            .expect("render standard compact post-update changelog");
        let mut notice_rendered = String::new();
        for row in 0..24 {
            for column in 0..80 {
                notice_rendered.push_str(terminal.backend().buffer()[(column, row)].symbol());
            }
            notice_rendered.push('\n');
        }
        assert!(notice_rendered.contains("Updated successfully  1.2.3  →  1.2.4"));
        assert!(notice_rendered.contains("Release notes are unavailable for this update."));
        assert!(notice_rendered.contains("[Enter] Got it"));
        assert!(notice_rendered.contains("[Esc] Close"));
    }

    #[test]
    fn post_update_changelog_stays_readable_and_closeable_on_narrow_terminals() {
        let backend = TestBackend::new(52, 16);
        let mut terminal = Terminal::new(backend).expect("create compact changelog terminal");
        let theme = super::theme::resolve(super::theme::DEFAULT_THEME_ID);
        let notice = super::update::ChangelogNotice {
            from_version: "1.2.3".into(),
            to_version: "1.3.0".into(),
            release_notes: vec![
                "Add a polished changelog before and after updates".into(),
                "Preserve the new workspace-focused dashboard".into(),
                "Keep update failures non-fatal".into(),
                "Fourth change".into(),
                "Fifth change".into(),
                "Sixth change".into(),
                "Seventh change".into(),
                "Eighth change".into(),
                "Ninth change".into(),
            ],
            release_url: Some("https://github.com/Shattermoon/moondesk/releases/tag/v1.3.0".into()),
        };

        terminal
            .draw(|frame| draw_changelog_notice(frame, theme, &notice))
            .expect("render compact post-update changelog");

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for row in 0..16 {
            for column in 0..52 {
                rendered.push_str(buffer[(column, row)].symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("MoonDesk Updated"));
        assert!(rendered.contains("Updated successfully"));
        assert!(rendered.contains("What's new"));
        assert!(rendered.contains("Add a polished changelog"));
        assert!(rendered.contains("more change"));
        assert!(rendered.contains("[Enter] Got it"));
        assert!(rendered.contains("[Esc] Close"));
    }

    #[test]
    fn settings_renders_every_registered_theme() {
        let usage = super::UsageTotals::default();
        let tool_mode = super::ToolMode::all()[0];

        for (selected_row, theme) in super::theme::all().iter().enumerate() {
            let backend = TestBackend::new(100, 32);
            let mut terminal = Terminal::new(backend).expect("create theme settings terminal");
            terminal
                .draw(|frame| {
                    super::draw_settings(
                        frame,
                        super::SettingsView {
                            current_theme: theme,
                            current_tool_mode: tool_mode,
                            set_moondesk_as_co_author: false,
                            ngrok_domain: None,
                            usage_totals: &usage,
                            selected_row,
                            confirm_reset_token_billing: false,
                        },
                    )
                })
                .unwrap_or_else(|error| panic!("render theme {}: {error}", theme.id));
        }
    }

    #[test]
    fn full_background_theme_sets_readable_default_without_changing_reset_themes() {
        for theme in super::theme::all() {
            let backend = TestBackend::new(8, 3);
            let mut terminal = Terminal::new(backend).expect("create theme background terminal");
            terminal
                .draw(|frame| {
                    super::render_theme_background(frame, theme.palette);
                    frame.render_widget(Paragraph::new("X"), Rect::new(0, 0, 1, 1));
                })
                .unwrap_or_else(|error| panic!("render theme background {}: {error}", theme.id));

            let cell = &terminal.backend().buffer()[(0, 0)];
            if theme.palette.background_bg == Color::Reset {
                assert_eq!(cell.bg, Color::Reset, "theme {} background", theme.id);
                assert_eq!(cell.fg, Color::Reset, "theme {} foreground", theme.id);
            } else {
                assert_eq!(
                    cell.bg, theme.palette.background_bg,
                    "theme {} background",
                    theme.id
                );
                assert_eq!(
                    cell.fg, theme.palette.primary_fg,
                    "theme {} foreground",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn paper_settings_render_keeps_background_under_borders_and_text() {
        let usage = super::UsageTotals::default();
        let tool_mode = super::ToolMode::all()[0];
        let theme = super::theme::resolve("paper");
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper settings terminal");

        terminal
            .draw(|frame| {
                super::draw_settings(
                    frame,
                    super::SettingsView {
                        current_theme: theme,
                        current_tool_mode: tool_mode,
                        set_moondesk_as_co_author: false,
                        ngrok_domain: None,
                        usage_totals: &usage,
                        selected_row: super::theme::all().len() - 1,
                        confirm_reset_token_billing: false,
                    },
                )
            })
            .expect("render paper settings");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, theme.palette.background_bg);
        assert_eq!(buffer[(5, 1)].bg, theme.palette.background_bg);
        assert_eq!(buffer[(99, 31)].bg, theme.palette.background_bg);
        assert_paper_frame_has_no_reset_colors(buffer);
    }

    #[test]
    fn paper_ngrok_overlays_keep_modal_colors_after_clear() {
        let theme = super::theme::resolve("paper");

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper ngrok domain terminal");
        terminal
            .draw(|frame| {
                super::draw_mode_select(frame, theme, super::ToolMode::MultiTools);
                super::draw_ngrok_domain_setup(
                    frame,
                    theme,
                    frame.area(),
                    "example.ngrok-free.app",
                    None,
                );
            })
            .expect("render paper ngrok domain overlay");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper ngrok auth terminal");
        terminal
            .draw(|frame| {
                super::draw_mode_select(frame, theme, super::ToolMode::MultiTools);
                super::draw_ngrok_auth_setup(
                    frame,
                    theme,
                    frame.area(),
                    "unused-config-path",
                    "ngrok_••••••••",
                    None,
                );
            })
            .expect("render paper ngrok auth overlay");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());
    }

    #[test]
    fn paper_standalone_dialogs_keep_full_frame_and_visible_text_themed() {
        let theme = super::theme::resolve("paper");
        let update_info = super::update::UpdateInfo {
            current_version: "1.2.3".into(),
            latest_version: "1.2.4".into(),
            release_notes: vec!["Keep Paper readable everywhere".into()],
            release_url: Some("https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into()),
        };
        let notice = super::update::ChangelogNotice {
            from_version: "1.2.3".into(),
            to_version: "1.2.4".into(),
            release_notes: vec!["Keep Paper readable everywhere".into()],
            release_url: None,
        };

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper prompt terminal");
        terminal
            .draw(|frame| draw_prompt(frame, theme.palette, "Rename workspace:", "MoonDesk"))
            .expect("render paper prompt");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper quit terminal");
        terminal
            .draw(|frame| draw_quit_confirm(frame, theme, 2, 1, 0, 0))
            .expect("render paper quit confirmation");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper update terminal");
        terminal
            .draw(|frame| draw_update_confirm(frame, theme, &update_info, 0))
            .expect("render paper update confirmation");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("create paper changelog terminal");
        terminal
            .draw(|frame| draw_changelog_notice(frame, theme, &notice))
            .expect("render paper changelog");
        assert_paper_frame_has_no_reset_colors(terminal.backend().buffer());
    }
}
