use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration as StdDuration, Instant};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, RwLock, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::process_runner;
use crate::workspaces::WorkspaceId;

pub const DEFAULT_JOB_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
pub const MAX_JOB_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
pub const MAX_POLL_WAIT_MS: u64 = 30_000;
pub const DEFAULT_POLL_WAIT_MS: u64 = MAX_POLL_WAIT_MS;
const MAX_ACTIVE_JOBS: usize = 8;
const MAX_RETAINED_JOBS: usize = 64;
const TERMINAL_JOB_TTL: StdDuration = StdDuration::from_secs(60 * 60);
const IDEMPOTENCY_WINDOW: StdDuration = StdDuration::from_secs(30);
const MAX_OUTPUT_BYTES_PER_JOB: usize = 4 * 1024 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const STALE_OUTPUT_ROOT_TTL: StdDuration = StdDuration::from_secs(24 * 60 * 60);
const MAX_POLL_OUTPUT_BYTES: usize = 128 * 1024;
pub const MAX_COMMAND_OUTPUT_READ_BYTES: usize = 128 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const CLEANUP_INTERVAL: StdDuration = StdDuration::from_secs(1);
const OUTPUT_ROOT_PREFIX: &str = "moondesk-command-output-";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl CommandJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CommandOutputEvent {
    pub seq: u64,
    pub stream: &'static str,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobSnapshot {
    pub job_id: String,
    pub command: String,
    pub cwd: String,
    pub state: CommandJobState,
    pub elapsed_ms: u64,
    pub since_last_output_ms: u64,
    pub exit_code: Option<i32>,
    pub events: Vec<CommandOutputEvent>,
    pub next_cursor: u64,
    pub has_more_output: bool,
    pub output_truncated: bool,
    pub output_archive_truncated: bool,
    pub output_archive_error: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobSummary {
    pub job_id: String,
    pub command: String,
    pub cwd: String,
    pub state: CommandJobState,
    pub elapsed_ms: u64,
    pub since_last_output_ms: u64,
    pub exit_code: Option<i32>,
    pub timeout_ms: u64,
    pub root_pid: Option<u32>,
    pub process_count: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct StartCommandResult {
    pub snapshot: CommandJobSnapshot,
    pub reused_existing: bool,
}

#[derive(Clone, Debug)]
pub struct CommandOutputChunk {
    pub start_byte: u64,
    pub end_byte: u64,
    pub text: String,
    pub next_start_byte: Option<u64>,
}

#[derive(Debug)]
struct OutputRootGuard {
    path: PathBuf,
}

impl Drop for OutputRootGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct JobRuntime {
    state: CommandJobState,
    exit_code: Option<i32>,
    root_pid: Option<u32>,
    finished_at: Option<Instant>,
    last_output_at: Option<Instant>,
    events: VecDeque<CommandOutputEvent>,
    retained_output_bytes: usize,
    next_seq: u64,
    output_archive_error: Option<String>,
}

impl Default for JobRuntime {
    fn default() -> Self {
        Self {
            state: CommandJobState::Running,
            exit_code: None,
            root_pid: None,
            finished_at: None,
            last_output_at: None,
            events: VecDeque::new(),
            retained_output_bytes: 0,
            next_seq: 1,
            output_archive_error: None,
        }
    }
}

impl JobRuntime {
    /// Preserve the newest observed output time even if asynchronous archive writes
    /// for stdout and stderr complete in a different order than the reads occurred.
    fn record_output_at(&mut self, output_at: Instant) {
        self.last_output_at = self.last_output_at.max(Some(output_at));
    }
}

#[derive(Debug)]
struct CommandJob {
    id: String,
    workspace_id: WorkspaceId,
    command: String,
    cwd: PathBuf,
    started_at: Instant,
    timeout_ms: u64,
    runtime: Mutex<JobRuntime>,
    stdout_archive: Mutex<tokio::fs::File>,
    stderr_archive: Mutex<tokio::fs::File>,
    archive_bytes: AtomicU64,
    archive_truncated: AtomicBool,
    changed: Notify,
    cancel_tx: watch::Sender<bool>,
}

impl CommandJob {
    fn new_with_output(
        id: String,
        workspace_id: WorkspaceId,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
        output_paths: &process_runner::CommandOutputPaths,
    ) -> Result<(Arc<Self>, watch::Receiver<bool>), String> {
        let stdout_archive = tokio::fs::File::from_std(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_paths.stdout)
                .map_err(|error| format!("failed to open stdout archive: {error}"))?,
        );
        let stderr_archive = tokio::fs::File::from_std(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_paths.stderr)
                .map_err(|error| format!("failed to open stderr archive: {error}"))?,
        );
        let (cancel_tx, cancel_rx) = watch::channel(false);
        Ok((
            Arc::new(Self {
                id,
                workspace_id,
                command,
                cwd,
                started_at: Instant::now(),
                timeout_ms,
                runtime: Mutex::new(JobRuntime::default()),
                stdout_archive: Mutex::new(stdout_archive),
                stderr_archive: Mutex::new(stderr_archive),
                archive_bytes: AtomicU64::new(0),
                archive_truncated: AtomicBool::new(false),
                changed: Notify::new(),
                cancel_tx,
            }),
            cancel_rx,
        ))
    }

    #[cfg(test)]
    fn new(command: String, cwd: PathBuf, timeout_ms: u64) -> (Arc<Self>, watch::Receiver<bool>) {
        Self::new_for_workspace(WorkspaceId::test_default(), command, cwd, timeout_ms)
    }

    #[cfg(test)]
    fn new_for_workspace(
        workspace_id: WorkspaceId,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
    ) -> (Arc<Self>, watch::Receiver<bool>) {
        let id = Uuid::new_v4().to_string();
        let output_dir = cwd.join(format!(".moondesk-command-output-{id}"));
        fs::create_dir_all(&output_dir).expect("create test command output dir");
        let output_paths = process_runner::CommandOutputPaths {
            stdout: output_dir.join("stdout.log"),
            stderr: output_dir.join("stderr.log"),
        };
        Self::new_with_output(id, workspace_id, command, cwd, timeout_ms, &output_paths)
            .expect("create test command output archive")
    }

    fn reserve_archive_bytes(&self, requested: usize) -> usize {
        loop {
            let current = self.archive_bytes.load(Ordering::Acquire);
            if current >= process_runner::MAX_COMMAND_ARCHIVE_BYTES {
                self.archive_truncated.store(true, Ordering::Release);
                return 0;
            }
            let remaining = process_runner::MAX_COMMAND_ARCHIVE_BYTES - current;
            let allowed = remaining.min(requested as u64) as usize;
            match self.archive_bytes.compare_exchange(
                current,
                current.saturating_add(allowed as u64),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if allowed < requested {
                        self.archive_truncated.store(true, Ordering::Release);
                    }
                    return allowed;
                }
                Err(_) => continue,
            }
        }
    }

    async fn append_output(&self, stream: &'static str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let output_at = Instant::now();
        let archive_write_len = self.reserve_archive_bytes(bytes.len());
        let archive_error = if archive_write_len == 0 {
            None
        } else {
            let archive = if stream == "stderr" {
                &self.stderr_archive
            } else {
                &self.stdout_archive
            };
            let mut file = archive.lock().await;
            file.write_all(&bytes[..archive_write_len])
                .await
                .err()
                .map(|error| format!("failed to preserve {stream} output: {error}"))
        };

        let text = String::from_utf8_lossy(bytes).into_owned();
        let event_bytes = text.len();
        let mut runtime = self.runtime.lock().await;
        if runtime.output_archive_error.is_none() {
            runtime.output_archive_error = archive_error;
        }
        runtime.record_output_at(output_at);
        let seq = runtime.next_seq;
        runtime.next_seq = runtime.next_seq.saturating_add(1);
        runtime
            .events
            .push_back(CommandOutputEvent { seq, stream, text });
        runtime.retained_output_bytes = runtime.retained_output_bytes.saturating_add(event_bytes);

        // Keep the in-memory event buffer bounded even if the on-disk archive fails.
        // Archive failure means old output may no longer be recoverable, not that a
        // noisy command is allowed to grow MoonDesk's memory without limit.
        while runtime.retained_output_bytes > MAX_OUTPUT_BYTES_PER_JOB {
            let Some(removed) = runtime.events.pop_front() else {
                break;
            };
            runtime.retained_output_bytes = runtime
                .retained_output_bytes
                .saturating_sub(removed.text.len());
        }
        drop(runtime);
        self.changed.notify_waiters();
    }

    async fn flush_archives(&self) -> Option<String> {
        for (stream, archive) in [
            ("stdout", &self.stdout_archive),
            ("stderr", &self.stderr_archive),
        ] {
            let mut file = archive.lock().await;
            if let Err(error) = file.flush().await {
                return Some(format!(
                    "failed to flush preserved {stream} output: {error}"
                ));
            }
        }
        None
    }

    async fn set_root_pid(&self, root_pid: u32) {
        self.runtime.lock().await.root_pid = Some(root_pid);
    }

    async fn summary(&self) -> CommandJobSummary {
        let runtime = self.runtime.lock().await;
        let state = runtime.state;
        let root_pid = runtime.root_pid;
        let exit_code = runtime.exit_code;
        let finished_at = runtime.finished_at;
        let last_output_at = runtime.last_output_at;
        drop(runtime);
        let observed_at = finished_at.unwrap_or_else(Instant::now);
        let elapsed_ms = observed_at
            .saturating_duration_since(self.started_at)
            .as_millis() as u64;
        let since_last_output_ms = last_output_at
            .map(|last_output_at| {
                observed_at
                    .saturating_duration_since(last_output_at)
                    .as_millis() as u64
            })
            .unwrap_or(elapsed_ms);
        let process_count = if state == CommandJobState::Running {
            root_pid.and_then(process_runner::process_tree_size)
        } else {
            None
        };
        CommandJobSummary {
            job_id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            state,
            elapsed_ms,
            since_last_output_ms,
            exit_code,
            timeout_ms: self.timeout_ms,
            root_pid,
            process_count,
        }
    }

    async fn finish(&self, state: CommandJobState, exit_code: Option<i32>) {
        let flush_error = self.flush_archives().await;
        let mut runtime = self.runtime.lock().await;
        if runtime.state.is_terminal() {
            return;
        }
        if runtime.output_archive_error.is_none() {
            runtime.output_archive_error = flush_error;
        }
        runtime.state = state;
        runtime.exit_code = exit_code;
        runtime.finished_at = Some(Instant::now());
        drop(runtime);
        self.changed.notify_waiters();
    }

    async fn snapshot(&self, after: u64) -> CommandJobSnapshot {
        let runtime = self.runtime.lock().await;
        let first_retained_seq = runtime
            .events
            .front()
            .map(|event| event.seq)
            .unwrap_or(runtime.next_seq);
        let cursor_fell_behind = after.saturating_add(1) < first_retained_seq;
        let latest_cursor = runtime.next_seq.saturating_sub(1);
        let mut events = Vec::new();
        let mut response_bytes = 0usize;
        for event in runtime.events.iter().filter(|event| event.seq > after) {
            let event_bytes = event.text.len();
            if !events.is_empty()
                && response_bytes.saturating_add(event_bytes) > MAX_POLL_OUTPUT_BYTES
            {
                break;
            }
            response_bytes = response_bytes.saturating_add(event_bytes);
            events.push(event.clone());
        }
        let next_cursor = events
            .last()
            .map(|event| event.seq)
            .unwrap_or(latest_cursor);
        let has_more_output = next_cursor < latest_cursor;
        let observed_at = runtime.finished_at.unwrap_or_else(Instant::now);
        let elapsed_ms = observed_at
            .saturating_duration_since(self.started_at)
            .as_millis() as u64;
        let since_last_output_ms = runtime
            .last_output_at
            .map(|last_output_at| {
                observed_at
                    .saturating_duration_since(last_output_at)
                    .as_millis() as u64
            })
            .unwrap_or(elapsed_ms);
        CommandJobSnapshot {
            job_id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            state: runtime.state,
            elapsed_ms,
            since_last_output_ms,
            exit_code: runtime.exit_code,
            events,
            next_cursor,
            has_more_output,
            output_truncated: cursor_fell_behind,
            output_archive_truncated: self.archive_truncated.load(Ordering::Acquire),
            output_archive_error: runtime.output_archive_error.clone(),
            timeout_ms: self.timeout_ms,
        }
    }
}

#[derive(Clone)]
struct RunOutputRecord {
    workspace_id: WorkspaceId,
    created_at: Instant,
}

#[derive(Default)]
struct ManagerState {
    jobs: HashMap<String, Arc<CommandJob>>,
    run_outputs: HashMap<String, RunOutputRecord>,
    // Retry dedupe is intentionally short-lived. JSON-RPC request IDs are only
    // correlation IDs and may be reused later by a stateless client.
    request_jobs: HashMap<(WorkspaceId, String), (String, Instant)>,
    // A workspace being removed must stop admitting new background jobs before
    // its request leases drain. This marker is reversible until config removal
    // has been persisted successfully.
    closing_workspaces: HashSet<WorkspaceId>,
    last_cleanup: Option<Instant>,
}

#[derive(Clone)]
pub struct CommandJobManager {
    inner: Arc<RwLock<ManagerState>>,
    output_root: Arc<OutputRootGuard>,
    // Starting a job performs a dedupe lookup, active-job capacity check, and
    // registry insertion. Serialize that short critical section so concurrent
    // MCP requests cannot both pass the checks and create duplicate/overflow jobs.
    start_lock: Arc<Mutex<()>>,
    // App shutdown is terminal for this manager. Once set, no new background
    // command may be created even if an MCP request races with shutdown.
    shutting_down: Arc<AtomicBool>,
}

fn output_root_owner_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let suffix = name.strip_prefix(OUTPUT_ROOT_PREFIX)?;
    let (pid, instance_id) = suffix.split_once('-')?;
    Uuid::parse_str(instance_id).ok()?;
    let pid = pid.parse::<u32>().ok()?;
    (pid != 0).then_some(pid)
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_live(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;

    if pid == std::process::id() {
        return true;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied;
    }
    let mut exit_code = 0_u32;
    let query_succeeded = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(process);
    }
    query_succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
}

fn cleanup_stale_output_roots() {
    let temp_dir = std::env::temp_dir();
    let Ok(entries) = fs::read_dir(&temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(owner_pid) = output_root_owner_pid(&path) else {
            continue;
        };
        let is_directory = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
        if !is_directory || owner_pid == std::process::id() || process_is_live(owner_pid) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_OUTPUT_ROOT_TTL);
        if old_enough {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn cleanup_stale_output_roots_once() {
    static CLEANUP: Once = Once::new();
    CLEANUP.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("moondesk-output-cleanup".into())
            .spawn(cleanup_stale_output_roots);
    });
}

impl Default for CommandJobManager {
    fn default() -> Self {
        cleanup_stale_output_roots_once();
        let output_root = std::env::temp_dir().join(format!(
            "{OUTPUT_ROOT_PREFIX}{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        Self {
            inner: Arc::new(RwLock::new(ManagerState::default())),
            output_root: Arc::new(OutputRootGuard { path: output_root }),
            start_lock: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }
}

async fn signal_running_jobs(jobs: &[Arc<CommandJob>]) {
    for job in jobs {
        if job.runtime.lock().await.state == CommandJobState::Running {
            let _ = job.cancel_tx.send(true);
        }
    }
}

async fn wait_until_not_running(jobs: &[Arc<CommandJob>], timeout: StdDuration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut any_running = false;
        for job in jobs {
            if job.runtime.lock().await.state == CommandJobState::Running {
                any_running = true;
                break;
            }
        }
        if !any_running {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl CommandJobManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn output_paths(&self, output_id: &str) -> Result<process_runner::CommandOutputPaths, String> {
        Uuid::parse_str(output_id).map_err(|_| "invalid command output id".to_string())?;
        let dir = self.output_root.path.join(output_id);
        Ok(process_runner::CommandOutputPaths {
            stdout: dir.join("stdout.log"),
            stderr: dir.join("stderr.log"),
        })
    }

    fn prepare_output(
        &self,
        output_id: &str,
    ) -> Result<process_runner::CommandOutputPaths, String> {
        let paths = self.output_paths(output_id)?;
        let dir = paths
            .stdout
            .parent()
            .ok_or_else(|| "invalid command output path".to_string())?;
        fs::create_dir_all(dir)
            .map_err(|error| format!("failed to create command output archive: {error}"))?;
        File::create(&paths.stdout)
            .map_err(|error| format!("failed to create stdout archive: {error}"))?;
        File::create(&paths.stderr)
            .map_err(|error| format!("failed to create stderr archive: {error}"))?;
        Ok(paths)
    }

    pub async fn create_run_output_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<(String, process_runner::CommandOutputPaths), String> {
        let output_id = Uuid::new_v4().to_string();
        let paths = self.prepare_output(&output_id)?;
        self.inner.write().await.run_outputs.insert(
            output_id.clone(),
            RunOutputRecord {
                workspace_id: workspace_id.clone(),
                created_at: Instant::now(),
            },
        );
        Ok((output_id, paths))
    }

    #[cfg(test)]
    pub async fn create_run_output(
        &self,
    ) -> Result<(String, process_runner::CommandOutputPaths), String> {
        self.create_run_output_for_workspace(&WorkspaceId::test_default())
            .await
    }

    fn discard_output_dir(&self, output_id: &str) {
        if let Ok(paths) = self.output_paths(output_id)
            && let Some(dir) = paths.stdout.parent()
        {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub async fn discard_output_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        output_id: &str,
    ) -> Result<(), String> {
        let removed = {
            let mut manager = self.inner.write().await;
            let Some(record) = manager.run_outputs.get(output_id) else {
                return Err("command output archive not found".to_string());
            };
            if &record.workspace_id != workspace_id {
                return Err("command output archive not found".to_string());
            }
            manager.run_outputs.remove(output_id).is_some()
        };
        if removed {
            self.discard_output_dir(output_id);
        }
        Ok(())
    }

    pub async fn read_output_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        output_id: &str,
        stream: &str,
        start_byte: u64,
        max_bytes: usize,
    ) -> Result<CommandOutputChunk, String> {
        let owned = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .get(output_id)
                .is_some_and(|job| &job.workspace_id == workspace_id)
                || manager
                    .run_outputs
                    .get(output_id)
                    .is_some_and(|record| &record.workspace_id == workspace_id)
        };
        if !owned {
            return Err("command output archive not found".to_string());
        }
        self.read_output_unchecked(output_id, stream, start_byte, max_bytes)
    }

    #[cfg(test)]
    pub fn read_output(
        &self,
        output_id: &str,
        stream: &str,
        start_byte: u64,
        max_bytes: usize,
    ) -> Result<CommandOutputChunk, String> {
        self.read_output_unchecked(output_id, stream, start_byte, max_bytes)
    }

    fn read_output_unchecked(
        &self,
        output_id: &str,
        stream: &str,
        start_byte: u64,
        max_bytes: usize,
    ) -> Result<CommandOutputChunk, String> {
        if !(4..=MAX_COMMAND_OUTPUT_READ_BYTES).contains(&max_bytes) {
            return Err(format!(
                "max_bytes must be between 4 and {MAX_COMMAND_OUTPUT_READ_BYTES}"
            ));
        }
        let paths = self.output_paths(output_id)?;
        let path = match stream {
            "stdout" => paths.stdout,
            "stderr" => paths.stderr,
            _ => return Err("stream must be stdout or stderr".to_string()),
        };
        if !path.exists() {
            return Err("command output archive not found".to_string());
        }
        let size = path.metadata().map_err(|error| error.to_string())?.len();
        if start_byte > size {
            return Err(format!(
                "start_byte {start_byte} is past the end of the {stream} log ({size} bytes)"
            ));
        }

        let mut file = File::open(&path).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::Start(start_byte))
            .map_err(|error| error.to_string())?;
        let to_read = max_bytes.min(size.saturating_sub(start_byte) as usize);
        let mut data = vec![0_u8; to_read];
        let read = file.read(&mut data).map_err(|error| error.to_string())?;
        data.truncate(read);

        let consumed = match std::str::from_utf8(&data) {
            Ok(_) => data.len(),
            Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
                error.valid_up_to()
            }
            Err(_) => data.len(),
        };
        let text = String::from_utf8_lossy(&data[..consumed]).into_owned();
        let end_byte = start_byte.saturating_add(consumed as u64);
        let next_start_byte = (end_byte < size).then_some(end_byte);
        Ok(CommandOutputChunk {
            start_byte,
            end_byte,
            text,
            next_start_byte,
        })
    }

    pub fn normalize_timeout(timeout_ms: Option<u64>) -> Result<u64, String> {
        match timeout_ms {
            None => Ok(DEFAULT_JOB_TIMEOUT_MS),
            Some(0) => Err("timeout must be at least 1 ms".to_string()),
            Some(value) if value > MAX_JOB_TIMEOUT_MS => Err(format!(
                "timeout exceeds the maximum background command runtime of {MAX_JOB_TIMEOUT_MS} ms"
            )),
            Some(value) => Ok(value),
        }
    }

    #[cfg(test)]
    pub async fn start_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
        request_key: Option<String>,
    ) -> Result<StartCommandResult, String> {
        self.start_for_workspace_with_options(
            workspace_id,
            command,
            cwd,
            timeout_ms,
            true,
            request_key,
        )
        .await
    }

    pub async fn start_for_workspace_with_options(
        &self,
        workspace_id: &WorkspaceId,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
        allow_duplicate: bool,
        request_key: Option<String>,
    ) -> Result<StartCommandResult, String> {
        // Cleanup may scan retained jobs and remove archived output from disk. Do
        // it before taking the short host-wide start lock so one workspace's
        // retention maintenance cannot stall job admission for every workspace.
        self.cleanup().await;
        let _start_guard = self.start_lock.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(
                "command job manager is shutting down; new commands are not accepted".to_string(),
            );
        }
        if self
            .inner
            .read()
            .await
            .closing_workspaces
            .contains(workspace_id)
        {
            return Err(
                "workspace removal is in progress; new command jobs are not accepted".to_string(),
            );
        }
        let request_map_key = request_key.map(|key| (workspace_id.clone(), key));
        if let Some(key) = request_map_key.as_ref() {
            let existing = {
                let manager = self.inner.read().await;
                manager
                    .request_jobs
                    .get(key)
                    .and_then(|(job_id, created_at)| {
                        if created_at.elapsed() <= IDEMPOTENCY_WINDOW {
                            manager.jobs.get(job_id).cloned()
                        } else {
                            None
                        }
                    })
            };
            if let Some(job) = existing {
                if job.command != command || job.cwd != cwd || job.timeout_ms != timeout_ms {
                    return Err(
                        "the same MCP request id was reused with different start_command arguments"
                            .to_string(),
                    );
                }
                return Ok(StartCommandResult {
                    snapshot: job.snapshot(0).await,
                    reused_existing: true,
                });
            }
        }

        if !allow_duplicate {
            let jobs = {
                let manager = self.inner.read().await;
                manager
                    .jobs
                    .values()
                    .filter(|job| {
                        &job.workspace_id == workspace_id
                            && job.command == command
                            && job.cwd == cwd
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for job in jobs {
                let snapshot = job.snapshot(0).await;
                if snapshot.state == CommandJobState::Running {
                    return Ok(StartCommandResult {
                        snapshot,
                        reused_existing: true,
                    });
                }
            }
        }

        let active_count = {
            let jobs = {
                let manager = self.inner.read().await;
                manager
                    .jobs
                    .values()
                    .filter(|job| &job.workspace_id == workspace_id)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let mut active = 0usize;
            for job in jobs {
                if job.runtime.lock().await.state == CommandJobState::Running {
                    active += 1;
                }
            }
            active
        };
        if active_count >= MAX_ACTIVE_JOBS {
            return Err(format!(
                "too many active command jobs ({active_count}); maximum is {MAX_ACTIVE_JOBS}. Poll or cancel an existing job before starting another"
            ));
        }

        let job_id = Uuid::new_v4().to_string();
        let output_paths = self.prepare_output(&job_id)?;
        let (job, cancel_rx) = CommandJob::new_with_output(
            job_id.clone(),
            workspace_id.clone(),
            command,
            cwd,
            timeout_ms,
            &output_paths,
        )?;
        {
            let mut manager = self.inner.write().await;
            manager.jobs.insert(job_id.clone(), job.clone());
            if let Some(key) = request_map_key {
                manager
                    .request_jobs
                    .insert(key, (job_id.clone(), Instant::now()));
            }
        }

        tokio::spawn(run_job(job.clone(), cancel_rx));
        Ok(StartCommandResult {
            snapshot: job.snapshot(0).await,
            reused_existing: false,
        })
    }

    pub async fn list_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        include_completed: bool,
    ) -> Vec<CommandJobSummary> {
        self.cleanup().await;
        let jobs = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .values()
                .filter(|job| &job.workspace_id == workspace_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut summaries = Vec::with_capacity(jobs.len());
        for job in jobs {
            let summary = job.summary().await;
            if include_completed || summary.state == CommandJobState::Running {
                summaries.push(summary);
            }
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.elapsed_ms));
        summaries
    }

    pub async fn poll_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        job_id: &str,
        after: u64,
        wait_ms: u64,
    ) -> Result<CommandJobSnapshot, String> {
        self.cleanup().await;
        let job = self.get_job_for_workspace(workspace_id, job_id).await?;
        let wait_ms = wait_ms.min(MAX_POLL_WAIT_MS);

        // `Notify::notified()` does not register with `notify_waiters()` until
        // the future is polled or explicitly enabled. Pin and enable it before
        // taking the snapshot so a change in the check/wait gap is retained.
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let snapshot = job.snapshot(after).await;
        if snapshot.state.is_terminal() || !snapshot.events.is_empty() || wait_ms == 0 {
            return Ok(snapshot);
        }

        let _ = timeout(Duration::from_millis(wait_ms), &mut notified).await;
        Ok(job.snapshot(after).await)
    }

    pub async fn cancel_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        job_id: &str,
    ) -> Result<CommandJobSnapshot, String> {
        self.cleanup().await;
        let job = self.get_job_for_workspace(workspace_id, job_id).await?;

        let current = job.snapshot(0).await;
        if current.state.is_terminal() {
            return Ok(current);
        }
        let _ = job.cancel_tx.send(true);

        // Cancellation itself remains a short MCP operation, but ordinary
        // stdout/stderr notifications must not make cancel_command return a
        // misleading Running state. Wait until terminal or the bounded deadline.
        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            // Pin and explicitly enable the waiter before checking state so a
            // terminal transition cannot be lost in the check/wait gap.
            let notified = job.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = job.snapshot(0).await;
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(snapshot);
            }
            if timeout(remaining, &mut notified).await.is_err() {
                return Ok(job.snapshot(0).await);
            }
        }
    }

    async fn get_job_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        job_id: &str,
    ) -> Result<Arc<CommandJob>, String> {
        self.inner
            .read()
            .await
            .jobs
            .get(job_id)
            .filter(|job| &job.workspace_id == workspace_id)
            .cloned()
            .ok_or_else(|| format!("unknown or expired command job: {job_id}"))
    }

    pub async fn begin_workspace_removal(&self, workspace_id: &WorkspaceId) -> Result<(), String> {
        let _start_guard = self.start_lock.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(
                "command job manager is shutting down; workspace removal cannot start".to_string(),
            );
        }
        let inserted = self
            .inner
            .write()
            .await
            .closing_workspaces
            .insert(workspace_id.clone());
        if !inserted {
            return Err("workspace removal is already in progress".to_string());
        }
        Ok(())
    }

    pub async fn abort_workspace_removal(&self, workspace_id: &WorkspaceId) {
        let _start_guard = self.start_lock.lock().await;
        self.inner
            .write()
            .await
            .closing_workspaces
            .remove(workspace_id);
    }

    pub async fn finalize_workspace_removal(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<(), String> {
        let prepared = self
            .inner
            .read()
            .await
            .closing_workspaces
            .contains(workspace_id);
        if !prepared {
            return Err("workspace removal was not prepared".to_string());
        }
        self.cancel_workspace(workspace_id).await?;
        self.purge_workspace_state(workspace_id).await
    }

    pub async fn cancel_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), String> {
        // Hold the global start lock only long enough to snapshot the workspace's
        // jobs and signal cancellation; unrelated workspaces must not lose their
        // start capacity while process trees wind down. Removal callers install a
        // closing marker first, so no new job can appear after this snapshot.
        let jobs = {
            let _start_guard = self.start_lock.lock().await;
            let jobs = {
                let manager = self.inner.read().await;
                manager
                    .jobs
                    .values()
                    .filter(|job| &job.workspace_id == workspace_id)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            signal_running_jobs(&jobs).await;
            jobs
        };

        if wait_until_not_running(&jobs, StdDuration::from_secs(5)).await {
            Ok(())
        } else {
            Err("timed out waiting for workspace command jobs to stop".to_string())
        }
    }

    pub async fn purge_workspace_state(&self, workspace_id: &WorkspaceId) -> Result<(), String> {
        let _start_guard = self.start_lock.lock().await;
        let jobs = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .values()
                .filter(|job| &job.workspace_id == workspace_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        for job in &jobs {
            if job.runtime.lock().await.state == CommandJobState::Running {
                return Err("workspace still has active command jobs".to_string());
            }
        }

        let (job_ids, output_ids) = {
            let mut manager = self.inner.write().await;
            let job_ids = manager
                .jobs
                .iter()
                .filter(|(_, job)| &job.workspace_id == workspace_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &job_ids {
                manager.jobs.remove(id);
            }
            manager
                .request_jobs
                .retain(|(owner, _), _| owner != workspace_id);
            let output_ids = manager
                .run_outputs
                .iter()
                .filter(|(_, record)| &record.workspace_id == workspace_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &output_ids {
                manager.run_outputs.remove(id);
            }
            manager.closing_workspaces.remove(workspace_id);
            (job_ids, output_ids)
        };
        for id in job_ids.into_iter().chain(output_ids) {
            self.discard_output_dir(&id);
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn start(
        &self,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
        request_key: Option<String>,
    ) -> Result<StartCommandResult, String> {
        self.start_for_workspace(
            &WorkspaceId::test_default(),
            command,
            cwd,
            timeout_ms,
            request_key,
        )
        .await
    }

    #[cfg(test)]
    pub async fn poll(
        &self,
        job_id: &str,
        after: u64,
        wait_ms: u64,
    ) -> Result<CommandJobSnapshot, String> {
        self.poll_for_workspace(&WorkspaceId::test_default(), job_id, after, wait_ms)
            .await
    }

    #[cfg(test)]
    pub async fn cancel(&self, job_id: &str) -> Result<CommandJobSnapshot, String> {
        self.cancel_for_workspace(&WorkspaceId::test_default(), job_id)
            .await
    }

    /// Cancel every command still owned by MoonDesk and wait briefly for the
    /// runners to terminate their process trees. Used during application exit;
    /// ordinary MCP request completion deliberately does not call this.
    pub async fn cancel_all(&self) {
        // Serialize with start(): either a start completes before this guard and
        // is included below, or shutdown wins and that start is rejected.
        let _start_guard = self.start_lock.lock().await;
        self.shutting_down.store(true, Ordering::Release);

        let jobs = {
            let manager = self.inner.read().await;
            manager.jobs.values().cloned().collect::<Vec<_>>()
        };
        signal_running_jobs(&jobs).await;
        let _ = wait_until_not_running(&jobs, StdDuration::from_secs(5)).await;
    }

    pub async fn cleanup(&self) {
        {
            let mut manager = self.inner.write().await;
            if manager
                .last_cleanup
                .is_some_and(|last| last.elapsed() < CLEANUP_INTERVAL)
            {
                return;
            }
            manager.last_cleanup = Some(Instant::now());
        }

        let jobs = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .iter()
                .map(|(id, job)| (id.clone(), job.clone()))
                .collect::<Vec<_>>()
        };

        let mut expired = HashSet::new();
        let mut retained_by_workspace: HashMap<WorkspaceId, usize> = HashMap::new();
        let mut terminal_by_workspace: HashMap<WorkspaceId, Vec<(String, StdDuration, usize)>> =
            HashMap::new();

        for (id, job) in jobs {
            *retained_by_workspace
                .entry(job.workspace_id.clone())
                .or_default() += 1;
            let runtime = job.runtime.lock().await;
            let Some(finished_at) = runtime.finished_at else {
                continue;
            };
            let age = finished_at.elapsed();
            if age >= TERMINAL_JOB_TTL {
                expired.insert(id);
                if let Some(count) = retained_by_workspace.get_mut(&job.workspace_id) {
                    *count = count.saturating_sub(1);
                }
            } else {
                terminal_by_workspace
                    .entry(job.workspace_id.clone())
                    .or_default()
                    .push((id, age, runtime.retained_output_bytes));
            }
        }

        for (workspace_id, terminal) in &mut terminal_by_workspace {
            // Oldest terminal jobs are evicted first. Both retention count and
            // decoded-output memory budgets are independent per workspace.
            terminal.sort_by_key(|(_, age, _)| std::cmp::Reverse(*age));

            let retained_count = retained_by_workspace
                .entry(workspace_id.clone())
                .or_default();
            if *retained_count > MAX_RETAINED_JOBS {
                let mut overflow = *retained_count - MAX_RETAINED_JOBS;
                for (id, _, _) in terminal.iter() {
                    if overflow == 0 {
                        break;
                    }
                    if expired.insert(id.clone()) {
                        *retained_count = retained_count.saturating_sub(1);
                        overflow -= 1;
                    }
                }
            }

            let mut terminal_output_bytes = terminal
                .iter()
                .filter(|(id, _, _)| !expired.contains(id))
                .map(|(_, _, bytes)| *bytes)
                .sum::<usize>();
            if terminal_output_bytes > MAX_TERMINAL_OUTPUT_BYTES {
                for (id, _, bytes) in terminal.iter() {
                    if terminal_output_bytes <= MAX_TERMINAL_OUTPUT_BYTES {
                        break;
                    }
                    if expired.insert(id.clone()) {
                        terminal_output_bytes = terminal_output_bytes.saturating_sub(*bytes);
                        *retained_count = retained_count.saturating_sub(1);
                    }
                }
            }
        }

        let expired = expired.into_iter().collect::<Vec<_>>();
        let expired_run_outputs = {
            let mut manager = self.inner.write().await;
            for id in &expired {
                manager.jobs.remove(id);
            }
            let live_job_ids = manager.jobs.keys().cloned().collect::<HashSet<_>>();
            manager.request_jobs.retain(|_, (job_id, created_at)| {
                created_at.elapsed() <= IDEMPOTENCY_WINDOW && live_job_ids.contains(job_id)
            });
            let expired_outputs = manager
                .run_outputs
                .iter()
                .filter(|(_, record)| record.created_at.elapsed() >= TERMINAL_JOB_TTL)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &expired_outputs {
                manager.run_outputs.remove(id);
            }
            expired_outputs
        };
        for id in expired.into_iter().chain(expired_run_outputs) {
            self.discard_output_dir(&id);
        }
    }
}

fn decode_utf8_incremental(
    pending: &mut Vec<u8>,
    chunk: &[u8],
    end_of_stream: bool,
) -> Vec<String> {
    pending.extend_from_slice(chunk);
    let mut decoded = Vec::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                if !text.is_empty() {
                    decoded.push(text.to_string());
                }
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let text = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
                    decoded.push(text);
                    pending.drain(..valid_up_to);
                    continue;
                }
                match error.error_len() {
                    Some(invalid_len) => {
                        decoded.push("�".to_string());
                        pending.drain(..invalid_len.min(pending.len()));
                    }
                    None => break, // incomplete UTF-8 sequence; keep it for the next read
                }
            }
        }
    }

    if end_of_stream && !pending.is_empty() {
        decoded.push(String::from_utf8_lossy(pending).into_owned());
        pending.clear();
    }
    decoded
}

async fn read_job_output<R>(job: Arc<CommandJob>, stream: &'static str, mut reader: R)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    let mut pending_utf8 = Vec::with_capacity(4);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &[], true) {
                    job.append_output(stream, text.as_bytes()).await;
                }
                break;
            }
            Ok(read) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &buffer[..read], false) {
                    job.append_output(stream, text.as_bytes()).await;
                }
            }
            Err(error) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &[], true) {
                    job.append_output(stream, text.as_bytes()).await;
                }
                job.append_output(
                    "stderr",
                    format!("MoonDesk failed to read {stream}: {error}\n").as_bytes(),
                )
                .await;
                break;
            }
        }
    }
}

async fn run_job(job: Arc<CommandJob>, mut cancel_rx: watch::Receiver<bool>) {
    let cancelled_before_spawn = *cancel_rx.borrow();
    if cancelled_before_spawn {
        job.finish(CommandJobState::Cancelled, None).await;
        return;
    }

    let mut process = match process_runner::spawn_shell_command(&job.command, &job.cwd).await {
        Ok(process) => process,
        Err(error) => {
            job.append_output("stderr", format!("Failed to execute: {error}\n").as_bytes())
                .await;
            job.finish(CommandJobState::Failed, None).await;
            return;
        }
    };
    if let Some(root_pid) = process.pid() {
        job.set_root_pid(root_pid).await;
    }

    let stdout_task = process
        .take_stdout()
        .map(|stdout| tokio::spawn(read_job_output(job.clone(), "stdout", stdout)));
    let stderr_task = process
        .take_stderr()
        .map(|stderr| tokio::spawn(read_job_output(job.clone(), "stderr", stderr)));

    enum Completion {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }

    let completion = tokio::select! {
        status = process.wait() => Completion::Exited(status),
        _ = cancel_rx.changed() => Completion::Cancelled,
        _ = tokio::time::sleep(Duration::from_millis(job.timeout_ms)) => Completion::TimedOut,
    };

    let (state, exit_code) = match completion {
        Completion::Exited(Ok(status)) => {
            process.disarm().await;
            if status.success() {
                (CommandJobState::Succeeded, status.code())
            } else {
                (CommandJobState::Failed, status.code())
            }
        }
        Completion::Exited(Err(error)) => {
            process.terminate_tree().await;
            let _ = process.wait().await;
            job.append_output(
                "stderr",
                format!("MoonDesk failed while waiting for command: {error}\n").as_bytes(),
            )
            .await;
            (CommandJobState::Failed, None)
        }
        Completion::Cancelled => {
            process.terminate_tree().await;
            let status = process.wait().await.ok();
            (
                CommandJobState::Cancelled,
                status.and_then(|value| value.code()),
            )
        }
        Completion::TimedOut => {
            process.terminate_tree().await;
            let status = process.wait().await.ok();
            job.append_output(
                "stderr",
                format!("Command timed out after {} ms\n", job.timeout_ms).as_bytes(),
            )
            .await;
            (
                CommandJobState::TimedOut,
                status.and_then(|value| value.code()),
            )
        }
    };

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    job.finish(state, exit_code).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("moondesk-jobs-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    #[test]
    fn output_root_names_track_a_live_owner_pid() {
        let instance_id = Uuid::new_v4();
        let path = std::env::temp_dir().join(format!(
            "{OUTPUT_ROOT_PREFIX}{}-{instance_id}",
            std::process::id()
        ));
        assert_eq!(output_root_owner_pid(&path), Some(std::process::id()));
        assert!(process_is_live(std::process::id()));
        assert_eq!(
            output_root_owner_pid(&std::env::temp_dir().join("moondesk-command-output-invalid")),
            None
        );
    }

    async fn wait_terminal(manager: &CommandJobManager, job_id: &str) -> CommandJobSnapshot {
        let mut cursor = 0;
        for _ in 0..30 {
            let snapshot = manager.poll(job_id, cursor, 250).await.expect("poll job");
            cursor = snapshot.next_cursor;
            if snapshot.state.is_terminal() {
                // Fetch from zero once terminal so callers that assert on output
                // see the complete retained log rather than only the final delta.
                return manager.poll(job_id, 0, 0).await.expect("read terminal job");
            }
        }
        panic!("job did not reach terminal state");
    }

    async fn wait_for_file(path: &std::path::Path) {
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !path.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            path.exists(),
            "command never reached ready state: {}",
            path.display()
        );
    }

    #[test]
    fn last_output_timestamp_stays_monotonic_when_writes_finish_out_of_order() {
        let mut runtime = JobRuntime::default();
        let first = Instant::now();
        let newer = first + StdDuration::from_millis(20);
        let newest = newer + StdDuration::from_millis(20);

        runtime.record_output_at(newer);
        runtime.record_output_at(first);
        assert_eq!(runtime.last_output_at, Some(newer));

        runtime.record_output_at(newest);
        assert_eq!(runtime.last_output_at, Some(newest));
    }

    #[tokio::test]
    async fn background_job_returns_immediately_and_completes() {
        let root = workspace("complete");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300; Write-Output done"
        } else {
            "sleep 0.3; printf 'done\\n'"
        };
        let started = Instant::now();
        let started_job = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        assert!(started.elapsed() < StdDuration::from_millis(250));
        assert_eq!(started_job.snapshot.state, CommandJobState::Running);

        let snapshot = wait_terminal(&manager, &started_job.snapshot.job_id).await;
        assert_eq!(snapshot.state, CommandJobState::Succeeded);
        let text = snapshot
            .events
            .iter()
            .map(|event| event.text.as_str())
            .collect::<String>();
        assert!(text.contains("done"));
        assert!(
            snapshot.since_last_output_ms < snapshot.elapsed_ms,
            "recent command output should reset the idle timer"
        );
        let elapsed_ms = snapshot.elapsed_ms;
        let since_last_output_ms = snapshot.since_last_output_ms;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let later = manager
            .poll(&started_job.snapshot.job_id, snapshot.next_cursor, 0)
            .await
            .expect("re-poll completed job");
        assert_eq!(
            later.elapsed_ms, elapsed_ms,
            "terminal elapsed time must remain the execution duration"
        );
        assert_eq!(
            later.since_last_output_ms, since_last_output_ms,
            "terminal idle time must stop advancing after completion"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn polling_is_incremental_by_cursor() {
        let root = workspace("cursor");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output first; Start-Sleep -Milliseconds 250; Write-Output second"
        } else {
            "printf 'first\\n'; sleep 0.25; printf 'second\\n'"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");

        let deadline = Instant::now() + StdDuration::from_secs(5);
        let first = loop {
            let snapshot = manager
                .poll(&started.snapshot.job_id, 0, 250)
                .await
                .expect("first poll");
            if !snapshot.events.is_empty() {
                break snapshot;
            }
            assert!(
                !snapshot.state.is_terminal(),
                "job completed without producing expected first output"
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for first output"
            );
        };

        let first_cursor = first.next_cursor;
        let deadline = Instant::now() + StdDuration::from_secs(5);
        let second = loop {
            let snapshot = manager
                .poll(&started.snapshot.job_id, first_cursor, 250)
                .await
                .expect("second poll");
            if !snapshot.events.is_empty() || snapshot.state.is_terminal() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for second output"
            );
        };
        assert!(
            second.events.iter().all(|event| event.seq > first_cursor),
            "incremental poll repeated an already-consumed event"
        );
        let _ = wait_terminal(&manager, &started.snapshot.job_id).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_prevents_later_side_effects() {
        let root = workspace("cancel");
        let ready = root.join("ready.txt");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; Start-Sleep -Seconds 3; Set-Content sentinel.txt survived"
        } else {
            "printf ready > ready.txt; sleep 3; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");
        wait_for_file(&ready).await;
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel job");
        assert!(matches!(
            cancelled.state,
            CommandJobState::Cancelled | CommandJobState::Running
        ));
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "cancelled process survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pre_cancelled_job_does_not_spawn_command() {
        let root = workspace("pre-cancel");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Set-Content sentinel.txt spawned; Start-Sleep -Seconds 2"
        } else {
            "printf spawned > sentinel.txt; sleep 2"
        };
        let (job, cancel_rx) = CommandJob::new(command.to_string(), root.clone(), 10_000);

        let _ = job.cancel_tx.send(true);
        run_job(job.clone(), cancel_rx).await;

        let snapshot = job.snapshot(0).await;
        assert_eq!(snapshot.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !sentinel.exists(),
            "a job cancelled before the runner started still spawned its shell"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_waits_for_terminal_state_despite_output_notifications() {
        let root = workspace("cancel-terminal");
        let ready = root.join("ready.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; 1..200 | ForEach-Object { Write-Output $_; Start-Sleep -Milliseconds 5 }; Start-Sleep -Seconds 3"
        } else {
            "printf ready > ready.txt; for i in $(seq 1 200); do printf '%s\\n' \"$i\"; sleep 0.005; done; sleep 3"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start noisy job");
        wait_for_file(&ready).await;
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel noisy job");
        assert_eq!(
            cancelled.state,
            CommandJobState::Cancelled,
            "cancel_command should wait past output notifications for terminal acknowledgement"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_terminates_descendant_process_tree() {
        let root = workspace("descendant-cancel");
        let sentinel = root.join("descendant.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path descendant.txt -Value survived' -WorkingDirectory .; Start-Sleep -Seconds 5"
        } else {
            "(sleep 0.8; printf survived > descendant.txt) & sleep 5"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start descendant job");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel descendant job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "cancelled root shell left a descendant process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn job_timeout_terminates_process_tree() {
        let root = workspace("timeout");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 800; Set-Content sentinel.txt survived"
        } else {
            "sleep 0.8; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 100, None)
            .await
            .expect("start job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::TimedOut);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "timed-out job survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_utf8_decoder_preserves_split_multibyte_characters() {
        let bytes = "build ✓ 🚀".as_bytes();
        let split = bytes.len() - 2;
        let mut pending = Vec::new();
        let first = decode_utf8_incremental(&mut pending, &bytes[..split], false);
        let second = decode_utf8_incremental(&mut pending, &bytes[split..], false);
        let final_chunk = decode_utf8_incremental(&mut pending, &[], true);
        let decoded = first
            .into_iter()
            .chain(second)
            .chain(final_chunk)
            .collect::<String>();
        assert_eq!(decoded, "build ✓ 🚀");
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn cancel_all_terminates_active_jobs() {
        let root = workspace("cancel-all");
        let ready = root.join("ready.txt");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; Start-Sleep -Seconds 3; Set-Content sentinel.txt survived"
        } else {
            "printf ready > ready.txt; sleep 3; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");
        wait_for_file(&ready).await;
        manager.cancel_all().await;
        let terminal = manager
            .poll(&started.snapshot.job_id, 0, 0)
            .await
            .expect("poll cancelled job");
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "shutdown cancellation left process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_all_permanently_rejects_future_starts() {
        let root = workspace("shutdown-reject");
        let manager = CommandJobManager::new();
        manager.cancel_all().await;

        let error = manager
            .start("echo should-not-run".to_string(), root.clone(), 5_000, None)
            .await
            .expect_err("shutdown manager must reject new jobs");
        assert!(error.contains("shutting down"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn start_racing_with_shutdown_cannot_escape_cancellation() {
        use tokio::sync::Barrier;

        let root = workspace("shutdown-race");
        let sentinel = root.join("escaped.txt");
        let manager = CommandJobManager::new();
        let barrier = Arc::new(Barrier::new(3));
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 2; Set-Content escaped.txt survived"
        } else {
            "sleep 2; printf survived > escaped.txt"
        };

        let starter_manager = manager.clone();
        let starter_root = root.clone();
        let starter_barrier = barrier.clone();
        let starter = tokio::spawn(async move {
            starter_barrier.wait().await;
            starter_manager
                .start(command.to_string(), starter_root, 10_000, None)
                .await
        });

        let shutdown_manager = manager.clone();
        let shutdown_barrier = barrier.clone();
        let shutdown = tokio::spawn(async move {
            shutdown_barrier.wait().await;
            shutdown_manager.cancel_all().await;
        });

        barrier.wait().await;
        let start_result = starter.await.expect("starter task");
        shutdown.await.expect("shutdown task");

        match start_result {
            Ok(started) => {
                let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
                assert_eq!(terminal.state, CommandJobState::Cancelled);
            }
            Err(error) => assert!(error.contains("shutting down")),
        }

        tokio::time::sleep(Duration::from_millis(2_200)).await;
        assert!(
            !sentinel.exists(),
            "a start racing with shutdown escaped manager ownership"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cleanup_prunes_expired_idempotency_keys_without_job_eviction() {
        let root = workspace("dedupe-cleanup");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };
        let started = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("expired-request-key".into()),
            )
            .await
            .expect("start job");
        let _ = wait_terminal(&manager, &started.snapshot.job_id).await;

        {
            let mut state = manager.inner.write().await;
            let entry = state
                .request_jobs
                .get_mut(&(
                    WorkspaceId::test_default(),
                    "expired-request-key".to_string(),
                ))
                .expect("request key exists before cleanup");
            entry.1 = Instant::now() - IDEMPOTENCY_WINDOW - StdDuration::from_secs(1);
            state.last_cleanup = None;
            assert!(state.jobs.contains_key(&started.snapshot.job_id));
        }

        manager.cleanup().await;
        let state = manager.inner.read().await;
        assert!(
            !state.request_jobs.contains_key(&(
                WorkspaceId::test_default(),
                "expired-request-key".to_string()
            )),
            "expired idempotency metadata must be pruned even when no job is evicted"
        );
        assert!(
            state.jobs.contains_key(&started.snapshot.job_id),
            "cleanup should retain the still-fresh terminal job"
        );
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_request_key_reuses_existing_job() {
        let root = workspace("dedup");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300"
        } else {
            "sleep 0.3"
        };
        let first = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("request-1".into()),
            )
            .await
            .expect("start first job");
        let second = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("request-1".into()),
            )
            .await
            .expect("deduplicate job");
        assert_eq!(first.snapshot.job_id, second.snapshot.job_id);
        let _ = manager.cancel(&first.snapshot.job_id).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn background_timeout_validation_covers_boundaries() {
        assert_eq!(
            CommandJobManager::normalize_timeout(None).expect("default timeout"),
            DEFAULT_JOB_TIMEOUT_MS
        );
        assert!(CommandJobManager::normalize_timeout(Some(0)).is_err());
        assert_eq!(
            CommandJobManager::normalize_timeout(Some(1)).expect("minimum timeout"),
            1
        );
        assert_eq!(
            CommandJobManager::normalize_timeout(Some(MAX_JOB_TIMEOUT_MS))
                .expect("maximum timeout"),
            MAX_JOB_TIMEOUT_MS
        );
        assert!(CommandJobManager::normalize_timeout(Some(MAX_JOB_TIMEOUT_MS + 1)).is_err());
    }

    #[tokio::test]
    async fn active_job_limit_is_enforced_and_recovers_after_cancel() {
        let root = workspace("capacity");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_ACTIVE_JOBS {
            let started = manager
                .start(command.to_string(), root.clone(), 10_000, None)
                .await
                .expect("start capacity job");
            ids.push(started.snapshot.job_id);
        }

        let overflow = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect_err("ninth active job must be rejected");
        assert!(overflow.contains("too many active command jobs"));

        manager.cancel(&ids[0]).await.expect("cancel one job");
        let _ = wait_terminal(&manager, &ids[0]).await;
        let replacement = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("capacity should recover after cancellation");
        ids.push(replacement.snapshot.job_id);
        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_running_command_is_reused_unless_explicitly_allowed() {
        let root = workspace("duplicate-awareness");
        let manager = CommandJobManager::new();
        let workspace_id = WorkspaceId::test_default();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };

        let first = manager
            .start_for_workspace_with_options(
                &workspace_id,
                command.to_string(),
                root.clone(),
                10_000,
                false,
                None,
            )
            .await
            .expect("start first job");
        assert!(!first.reused_existing);

        let reused = manager
            .start_for_workspace_with_options(
                &workspace_id,
                command.to_string(),
                root.clone(),
                20_000,
                false,
                None,
            )
            .await
            .expect("reuse duplicate job");
        assert!(reused.reused_existing);
        assert_eq!(reused.snapshot.job_id, first.snapshot.job_id);

        let duplicate = manager
            .start_for_workspace_with_options(
                &workspace_id,
                command.to_string(),
                root.clone(),
                10_000,
                true,
                None,
            )
            .await
            .expect("start intentional duplicate");
        assert!(!duplicate.reused_existing);
        assert_ne!(duplicate.snapshot.job_id, first.snapshot.job_id);

        let jobs = manager.list_for_workspace(&workspace_id, false).await;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| job.state == CommandJobState::Running));

        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn job_summary_reports_live_root_process_metadata() {
        let root = workspace("process-metadata");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        job.set_root_pid(std::process::id()).await;

        let summary = job.summary().await;
        assert_eq!(summary.root_pid, Some(std::process::id()));
        assert!(summary.process_count.is_some_and(|count| count >= 1));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_duplicate_falls_through_to_a_new_job() {
        let root = workspace("terminal-duplicate");
        let manager = CommandJobManager::new();
        let workspace_id = WorkspaceId::test_default();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };

        let first = manager
            .start_for_workspace_with_options(
                &workspace_id,
                command.to_string(),
                root.clone(),
                5_000,
                false,
                None,
            )
            .await
            .expect("start first job");
        let terminal = wait_terminal(&manager, &first.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Succeeded);

        let second = manager
            .start_for_workspace_with_options(
                &workspace_id,
                command.to_string(),
                root.clone(),
                5_000,
                false,
                None,
            )
            .await
            .expect("start second job after terminal match");
        assert!(!second.reused_existing);
        assert_ne!(second.snapshot.job_id, first.snapshot.job_id);

        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_output_is_bounded_and_marks_old_cursor_truncated() {
        let root = workspace("output-limit");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = (MAX_OUTPUT_BYTES_PER_JOB / READ_CHUNK_BYTES) + 8;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }

        let snapshot = job.snapshot(0).await;
        let retained = snapshot
            .events
            .iter()
            .map(|event| event.text.len())
            .sum::<usize>();
        assert!(retained <= MAX_OUTPUT_BYTES_PER_JOB);
        assert!(snapshot.output_truncated);
        assert!(snapshot.events.first().is_some_and(|event| event.seq > 1));
        assert_eq!(
            snapshot.next_cursor,
            snapshot.events.last().expect("retained poll events").seq
        );
        assert!(snapshot.has_more_output);
        assert!(snapshot.next_cursor < chunks as u64);

        let caught_up = job.snapshot(chunks as u64).await;
        assert!(!caught_up.output_truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn overflowed_background_output_remains_fully_recoverable_from_archive() {
        let root = workspace("output-archive-recovery");
        let manager = CommandJobManager::new();
        let job_id = Uuid::new_v4().to_string();
        let paths = manager
            .prepare_output(&job_id)
            .expect("prepare output archive");
        let (job, _cancel_rx) = CommandJob::new_with_output(
            job_id.clone(),
            WorkspaceId::test_default(),
            "synthetic".into(),
            root.clone(),
            5_000,
            &paths,
        )
        .expect("create archived job");
        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = (MAX_OUTPUT_BYTES_PER_JOB / READ_CHUNK_BYTES) + 8;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }

        let snapshot = job.snapshot(0).await;
        assert!(snapshot.output_truncated);
        assert!(snapshot.output_archive_error.is_none());

        let mut start_byte = 0u64;
        let mut recovered_bytes = 0usize;
        loop {
            let output = manager
                .read_output(&job_id, "stdout", start_byte, MAX_COMMAND_OUTPUT_READ_BYTES)
                .expect("read archived output");
            recovered_bytes += output.text.len();
            match output.next_start_byte {
                Some(next) => start_byte = next,
                None => break,
            }
        }
        assert_eq!(recovered_bytes, chunks * READ_CHUNK_BYTES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn archive_budget_caps_preserved_output_and_marks_truncation() {
        let root = workspace("archive-budget");
        let manager = CommandJobManager::new();
        let job_id = Uuid::new_v4().to_string();
        let paths = manager
            .prepare_output(&job_id)
            .expect("prepare output archive");
        let (job, _cancel_rx) = CommandJob::new_with_output(
            job_id,
            WorkspaceId::test_default(),
            "synthetic".into(),
            root.clone(),
            5_000,
            &paths,
        )
        .expect("create archived job");

        job.archive_bytes.store(
            process_runner::MAX_COMMAND_ARCHIVE_BYTES - 4,
            Ordering::Release,
        );
        assert_eq!(job.reserve_archive_bytes(8), 4);
        assert_eq!(
            job.archive_bytes.load(Ordering::Acquire),
            process_runner::MAX_COMMAND_ARCHIVE_BYTES
        );
        assert!(job.archive_truncated.load(Ordering::Acquire));
        assert_eq!(job.reserve_archive_bytes(1), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn archive_failure_still_keeps_background_output_memory_bounded() {
        let root = workspace("output-archive-failure-bounded");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        {
            let mut runtime = job.runtime.lock().await;
            runtime.output_archive_error = Some("simulated archive failure".into());
        }

        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = (MAX_OUTPUT_BYTES_PER_JOB / READ_CHUNK_BYTES) + 8;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }

        let snapshot = job.snapshot(0).await;
        let retained = snapshot
            .events
            .iter()
            .map(|event| event.text.len())
            .sum::<usize>();
        assert!(retained <= MAX_OUTPUT_BYTES_PER_JOB);
        assert!(snapshot.output_truncated);
        assert_eq!(
            snapshot.output_archive_error.as_deref(),
            Some("simulated archive failure")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn poll_output_is_bounded_and_cursor_drains_terminal_logs_without_gaps() {
        let root = workspace("poll-output-limit");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = 40usize;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }
        job.finish(CommandJobState::Succeeded, Some(0)).await;

        let mut after = 0u64;
        let mut seen = Vec::new();
        let mut polls = 0usize;
        loop {
            let snapshot = job.snapshot(after).await;
            polls += 1;
            assert_eq!(snapshot.state, CommandJobState::Succeeded);
            let returned_bytes = snapshot
                .events
                .iter()
                .map(|event| event.text.len())
                .sum::<usize>();
            assert!(
                returned_bytes <= MAX_POLL_OUTPUT_BYTES,
                "poll returned {returned_bytes} bytes, limit is {MAX_POLL_OUTPUT_BYTES}"
            );
            for event in &snapshot.events {
                assert_eq!(event.seq, after + 1, "cursor skipped or repeated an event");
                after = event.seq;
                seen.push(event.seq);
            }
            assert_eq!(snapshot.next_cursor, after);
            if !snapshot.has_more_output {
                break;
            }
            assert!(
                !snapshot.events.is_empty(),
                "hasMoreOutput must make progress"
            );
        }

        assert!(
            polls > 1,
            "test must exercise multiple bounded poll responses"
        );
        assert_eq!(seen.len(), chunks);
        assert_eq!(seen, (1..=chunks as u64).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_job_ids_are_rejected_for_poll_and_cancel() {
        let manager = CommandJobManager::new();
        let poll_error = manager
            .poll("definitely-not-a-job", 0, 0)
            .await
            .expect_err("unknown poll must fail");
        assert!(poll_error.contains("unknown or expired command job"));
        let cancel_error = manager
            .cancel("definitely-not-a-job")
            .await
            .expect_err("unknown cancel must fail");
        assert!(cancel_error.contains("unknown or expired command job"));
    }

    #[tokio::test]
    async fn cancelling_terminal_job_is_idempotent() {
        let root = workspace("terminal-cancel");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Succeeded);
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel terminal job");
        assert_eq!(cancelled.state, CommandJobState::Succeeded);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn poll_wait_returns_near_requested_deadline_when_nothing_changes() {
        let root = workspace("poll-wait");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 500"
        } else {
            "sleep 0.5"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        let started_wait = Instant::now();
        let snapshot = manager
            .poll(&started.snapshot.job_id, 0, 100)
            .await
            .expect("poll job");
        let elapsed = started_wait.elapsed();
        assert_eq!(snapshot.state, CommandJobState::Running);
        assert!(snapshot.events.is_empty());
        assert!(
            elapsed >= StdDuration::from_millis(70),
            "poll returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_millis(400),
            "poll waited too long: {elapsed:?}"
        );
        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cleanup_enforces_terminal_output_budget_for_single_workspace() {
        let root = workspace("single-workspace-output-budget");
        let manager = CommandJobManager::new();

        for index in 0..9u64 {
            let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
            {
                let mut runtime = job.runtime.lock().await;
                runtime.state = CommandJobState::Succeeded;
                runtime.finished_at = Some(Instant::now() - StdDuration::from_millis(index));
                runtime.retained_output_bytes = MAX_OUTPUT_BYTES_PER_JOB;
            }
            manager.inner.write().await.jobs.insert(job.id.clone(), job);
        }

        manager.inner.write().await.last_cleanup = None;
        manager.cleanup().await;

        let jobs = {
            let state = manager.inner.read().await;
            state.jobs.values().cloned().collect::<Vec<_>>()
        };
        let mut retained_bytes = 0usize;
        for job in jobs {
            retained_bytes =
                retained_bytes.saturating_add(job.runtime.lock().await.retained_output_bytes);
        }
        assert!(retained_bytes <= MAX_TERMINAL_OUTPUT_BYTES);
        let _ = std::fs::remove_dir_all(root);
    }
    #[tokio::test]
    async fn workspace_ownership_blocks_cross_workspace_job_and_output_access() {
        let root = workspace("workspace-ownership");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let started = manager
            .start_for_workspace(
                &workspace_a,
                command.to_string(),
                root.clone(),
                10_000,
                None,
            )
            .await
            .expect("start workspace A job");

        assert!(
            manager
                .poll_for_workspace(&workspace_b, &started.snapshot.job_id, 0, 0)
                .await
                .expect_err("workspace B must not poll workspace A job")
                .contains("unknown or expired command job")
        );
        assert!(
            manager
                .cancel_for_workspace(&workspace_b, &started.snapshot.job_id)
                .await
                .expect_err("workspace B must not cancel workspace A job")
                .contains("unknown or expired command job")
        );

        let (output_id, paths) = manager
            .create_run_output_for_workspace(&workspace_a)
            .await
            .expect("create workspace A output");
        std::fs::write(&paths.stdout, "workspace-a-output").expect("write output");
        assert_eq!(
            manager
                .read_output_for_workspace(&workspace_b, &output_id, "stdout", 0, 128)
                .await
                .expect_err("workspace B must not read workspace A output"),
            "command output archive not found"
        );

        manager
            .cancel_for_workspace(&workspace_a, &started.snapshot.job_id)
            .await
            .expect("cancel workspace A job");
        manager
            .discard_output_for_workspace(&workspace_a, &output_id)
            .await
            .expect("discard workspace A output");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn active_job_quota_is_independent_per_workspace() {
        let root = workspace("workspace-capacity");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };

        for _ in 0..MAX_ACTIVE_JOBS {
            manager
                .start_for_workspace(
                    &workspace_a,
                    command.to_string(),
                    root.clone(),
                    10_000,
                    None,
                )
                .await
                .expect("workspace A should receive its full active-job allowance");
        }
        let started_b = manager
            .start_for_workspace(
                &workspace_b,
                command.to_string(),
                root.clone(),
                10_000,
                None,
            )
            .await
            .expect("workspace B must retain its independent allowance");
        assert!(
            manager
                .start_for_workspace(
                    &workspace_a,
                    command.to_string(),
                    root.clone(),
                    10_000,
                    None,
                )
                .await
                .expect_err("workspace A ninth job must still be rejected")
                .contains("too many active command jobs")
        );

        manager
            .cancel_workspace(&workspace_a)
            .await
            .expect("cancel workspace A jobs");
        let workspace_b_snapshot = manager
            .poll_for_workspace(&workspace_b, &started_b.snapshot.job_id, 0, 0)
            .await
            .expect("poll workspace B after cancelling A");
        assert_eq!(workspace_b_snapshot.state, CommandJobState::Running);
        assert_eq!(
            manager
                .purge_workspace_state(&workspace_b)
                .await
                .expect_err("active workspace B job must block purge"),
            "workspace still has active command jobs"
        );
        manager
            .cancel_workspace(&workspace_b)
            .await
            .expect("cancel workspace B jobs");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_output_budget_is_independent_per_workspace() {
        let root = workspace("workspace-output-budget");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();

        for workspace_id in [&workspace_a, &workspace_b] {
            for index in 0..9u64 {
                let (job, _cancel_rx) = CommandJob::new_for_workspace(
                    workspace_id.clone(),
                    "synthetic".into(),
                    root.clone(),
                    5_000,
                );
                {
                    let mut runtime = job.runtime.lock().await;
                    runtime.state = CommandJobState::Succeeded;
                    runtime.finished_at = Some(Instant::now() - StdDuration::from_millis(index));
                    runtime.retained_output_bytes = MAX_OUTPUT_BYTES_PER_JOB;
                }
                manager.inner.write().await.jobs.insert(job.id.clone(), job);
            }
        }

        manager.inner.write().await.last_cleanup = None;
        manager.cleanup().await;

        let jobs = {
            let state = manager.inner.read().await;
            state.jobs.values().cloned().collect::<Vec<_>>()
        };
        let mut bytes_a = 0usize;
        let mut bytes_b = 0usize;
        for job in jobs {
            let bytes = job.runtime.lock().await.retained_output_bytes;
            if job.workspace_id == workspace_a {
                bytes_a = bytes_a.saturating_add(bytes);
            } else if job.workspace_id == workspace_b {
                bytes_b = bytes_b.saturating_add(bytes);
            }
        }
        assert!(bytes_a <= MAX_TERMINAL_OUTPUT_BYTES);
        assert!(bytes_b <= MAX_TERMINAL_OUTPUT_BYTES);
        assert!(bytes_a > 0 && bytes_b > 0);
        assert!(
            bytes_a.saturating_add(bytes_b) > MAX_TERMINAL_OUTPUT_BYTES,
            "workspace budgets must not be collapsed into one global 32 MiB pool"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn retained_job_limit_is_independent_per_workspace() {
        let root = workspace("workspace-retained-count");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();

        for workspace_id in [&workspace_a, &workspace_b] {
            for index in 0..(MAX_RETAINED_JOBS + 6) {
                let (job, _cancel_rx) = CommandJob::new_for_workspace(
                    workspace_id.clone(),
                    "synthetic".into(),
                    root.clone(),
                    5_000,
                );
                {
                    let mut runtime = job.runtime.lock().await;
                    runtime.state = CommandJobState::Succeeded;
                    runtime.finished_at =
                        Some(Instant::now() - StdDuration::from_millis(index as u64));
                }
                manager.inner.write().await.jobs.insert(job.id.clone(), job);
            }
        }

        manager.inner.write().await.last_cleanup = None;
        manager.cleanup().await;
        let state = manager.inner.read().await;
        let retained_a = state
            .jobs
            .values()
            .filter(|job| job.workspace_id == workspace_a)
            .count();
        let retained_b = state
            .jobs
            .values()
            .filter(|job| job.workspace_id == workspace_b)
            .count();
        assert_eq!(retained_a, MAX_RETAINED_JOBS);
        assert_eq!(retained_b, MAX_RETAINED_JOBS);
        assert_eq!(state.jobs.len(), MAX_RETAINED_JOBS * 2);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_request_keys_are_independent_per_workspace() {
        let root = workspace("workspace-dedupe-key");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };

        let started_a = manager
            .start_for_workspace(
                &workspace_a,
                command.to_string(),
                root.clone(),
                5_000,
                Some("same-json-rpc-id".into()),
            )
            .await
            .expect("start workspace A job");
        let started_b = manager
            .start_for_workspace(
                &workspace_b,
                command.to_string(),
                root.clone(),
                5_000,
                Some("same-json-rpc-id".into()),
            )
            .await
            .expect("start workspace B job");

        assert_ne!(started_a.snapshot.job_id, started_b.snapshot.job_id);
        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_all_terminates_jobs_across_multiple_workspaces() {
        let root = workspace("workspace-shutdown-all");
        let manager = CommandJobManager::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let sentinel_a = root.join("a-survived.txt");
        let sentinel_b = root.join("b-survived.txt");
        let command_a = if cfg!(windows) {
            "Start-Sleep -Milliseconds 800; Set-Content a-survived.txt survived"
        } else {
            "sleep 0.8; printf survived > a-survived.txt"
        };
        let command_b = if cfg!(windows) {
            "Start-Sleep -Milliseconds 800; Set-Content b-survived.txt survived"
        } else {
            "sleep 0.8; printf survived > b-survived.txt"
        };

        let started_a = manager
            .start_for_workspace(
                &workspace_a,
                command_a.to_string(),
                root.clone(),
                5_000,
                None,
            )
            .await
            .expect("start workspace A shutdown job");
        let started_b = manager
            .start_for_workspace(
                &workspace_b,
                command_b.to_string(),
                root.clone(),
                5_000,
                None,
            )
            .await
            .expect("start workspace B shutdown job");

        manager.cancel_all().await;
        let snapshot_a = manager
            .poll_for_workspace(&workspace_a, &started_a.snapshot.job_id, 0, 0)
            .await
            .expect("poll workspace A after shutdown cancellation");
        let snapshot_b = manager
            .poll_for_workspace(&workspace_b, &started_b.snapshot.job_id, 0, 0)
            .await
            .expect("poll workspace B after shutdown cancellation");
        assert_eq!(snapshot_a.state, CommandJobState::Cancelled);
        assert_eq!(snapshot_b.state, CommandJobState::Cancelled);
        tokio::time::sleep(StdDuration::from_millis(1_000)).await;
        assert!(!sentinel_a.exists());
        assert!(!sentinel_b.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
