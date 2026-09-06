use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::time::{Duration, timeout};

const READ_CHUNK_BYTES: usize = 8 * 1024;
pub const MAX_COMMAND_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct ProcessRunResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub output_archive_truncated: bool,
    pub output_archive_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommandOutputPaths {
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

/// A spawned shell process owned by MoonDesk.
///
/// Dropping this value is intentionally destructive: if the command is still
/// alive, MoonDesk terminates the process tree. This is what keeps a cancelled
/// MCP request from leaving a compiler or build process behind.
pub struct SpawnedProcess {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    tree: ProcessTreeGuard,
}

impl SpawnedProcess {
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Terminate the root process and all descendants owned by this command.
    pub async fn terminate_tree(&mut self) {
        self.tree.terminate().await;
        // Job-object / process-group termination should already include the
        // root, but keep Tokio's direct kill as a best-effort fallback.
        let _ = self.child.start_kill();
    }

    /// Finalize ownership after the root process exits. Any descendants still
    /// alive at that point are terminated so a command cannot silently detach
    /// work that outlives its MoonDesk job.
    pub async fn disarm(&mut self) {
        self.tree.disarm().await;
    }
}

impl Drop for SpawnedProcess {
    fn drop(&mut self) {
        if self.tree.is_armed() {
            self.tree.terminate_blocking();
            let _ = self.child.start_kill();
        }
    }
}

#[derive(Debug)]
struct ProcessTreeGuard {
    pid: u32,
    armed: bool,
    #[cfg(windows)]
    job_handle: Option<usize>,
}

impl ProcessTreeGuard {
    #[cfg(not(windows))]
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    #[cfg(windows)]
    fn with_windows_job(pid: u32, job_handle: usize) -> Self {
        Self {
            pid,
            armed: true,
            job_handle: Some(job_handle),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed
    }

    async fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if self.job_handle.is_some() {
                close_windows_job(&mut self.job_handle);
            } else {
                terminate_process_tree_async(self.pid).await;
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }

    async fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if !terminate_windows_job(&mut self.job_handle) {
                terminate_process_tree_async(self.pid).await;
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }

    fn terminate_blocking(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if !terminate_windows_job(&mut self.job_handle) {
                terminate_process_tree_blocking(self.pid);
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.terminate_blocking();
    }
}

#[cfg(windows)]
fn create_windows_job_for_process(pid: u32) -> io::Result<usize> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }

        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }
        let assigned = AssignProcessToJobObject(job, process) != 0;
        let assign_error = if assigned {
            None
        } else {
            Some(io::Error::last_os_error())
        };
        CloseHandle(process);
        if let Some(error) = assign_error {
            CloseHandle(job);
            return Err(error);
        }

        Ok(job as usize)
    }
}

#[cfg(windows)]
fn resume_windows_process(pid: u32) -> io::Result<()> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                let previous_suspend_count = ResumeThread(thread);
                let resume_error = if previous_suspend_count == u32::MAX {
                    Some(io::Error::last_os_error())
                } else {
                    None
                };
                CloseHandle(thread);
                CloseHandle(snapshot);
                return match resume_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
            }
            found = Thread32Next(snapshot, &mut entry) != 0;
        }

        CloseHandle(snapshot);
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "suspended process did not expose a resumable thread",
        ))
    }
}

#[cfg(windows)]
fn close_windows_job(job_handle: &mut Option<usize>) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    if let Some(raw) = job_handle.take() {
        unsafe {
            CloseHandle(raw as HANDLE);
        }
    }
}

#[cfg(windows)]
fn terminate_windows_job(job_handle: &mut Option<usize>) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let Some(raw) = job_handle.take() else {
        return false;
    };
    let handle = raw as HANDLE;
    unsafe {
        let terminated = TerminateJobObject(handle, 1) != 0;
        CloseHandle(handle);
        terminated
    }
}

#[cfg(windows)]
fn terminate_process_tree_blocking(pid: u32) {
    // `/T` includes descendants and `/F` makes cancellation deterministic.
    // Use the executable directly rather than a shell command so the PID never
    // passes through shell parsing. This synchronous path is reserved for Drop,
    // where Rust cannot await cleanup.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
async fn terminate_process_tree_async(pid: u32) {
    let _ = tokio::task::spawn_blocking(move || terminate_process_tree_blocking(pid)).await;
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let pgid = match i32::try_from(pid) {
        Ok(value) => value,
        Err(_) => return,
    };
    // The shell is placed in its own process group at spawn time. A negative
    // PID targets the complete process group, including compiler descendants.
    unsafe {
        let _ = libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(any(windows, unix)))]
fn terminate_process_tree(_pid: u32) {}

#[cfg(windows)]
pub fn process_tree_size(root_pid: u32) -> Option<usize> {
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

        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut root_found = false;
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = Process32FirstW(snapshot, &mut entry) != 0;
        while found {
            root_found |= entry.th32ProcessID == root_pid;
            children
                .entry(entry.th32ParentProcessID)
                .or_default()
                .push(entry.th32ProcessID);
            found = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if !root_found {
            return None;
        }

        let mut count = 0usize;
        let mut queue = VecDeque::from([root_pid]);
        while let Some(pid) = queue.pop_front() {
            count = count.saturating_add(1);
            if let Some(descendants) = children.get(&pid) {
                queue.extend(descendants.iter().copied());
            }
        }
        Some(count)
    }
}

#[cfg(target_os = "linux")]
pub fn process_tree_size(root_pid: u32) -> Option<usize> {
    use std::collections::{HashMap, VecDeque};

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut root_found = false;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        root_found |= pid == root_pid;
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let parent = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        });
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    if !root_found {
        return None;
    }

    let mut count = 0usize;
    let mut queue = VecDeque::from([root_pid]);
    while let Some(pid) = queue.pop_front() {
        count = count.saturating_add(1);
        if let Some(descendants) = children.get(&pid) {
            queue.extend(descendants.iter().copied());
        }
    }
    Some(count)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_tree_size(_root_pid: u32) -> Option<usize> {
    None
}

#[cfg(windows)]
const WINDOWS_SHELL_WRAPPER: &str = include_str!("windows_shell_wrapper.ps1");

#[cfg(windows)]
const WINDOWS_SHELL_SOURCE_ENV: &str = "MOONDESK_INTERNAL_WINDOWS_COMMAND";

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let script = WINDOWS_SHELL_WRAPPER.replace("__MOONDESK_SUFFIX__", &suffix);
        let mut shell = Command::new("powershell.exe");
        shell
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .env(WINDOWS_SHELL_SOURCE_ENV, command);
        shell
    }

    #[cfg(not(windows))]
    {
        let mut shell = Command::new("/bin/bash");
        shell.arg("-c").arg(command);
        shell
    }
}

fn spawn_owned_command_blocking(mut command: Command, stdin: Stdio) -> io::Result<SpawnedProcess> {
    command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }

    let mut child = command.spawn()?;
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return Err(io::Error::other(
            "spawned command did not expose a process id",
        ));
    };

    #[cfg(windows)]
    let tree = {
        let job_handle = match create_windows_job_for_process(pid) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = child.start_kill();
                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to assign suspended command to Windows Job Object: {error}"),
                ));
            }
        };
        if let Err(error) = resume_windows_process(pid) {
            let mut job_handle = Some(job_handle);
            close_windows_job(&mut job_handle);
            let _ = child.start_kill();
            return Err(io::Error::new(
                error.kind(),
                format!("failed to resume suspended command process: {error}"),
            ));
        }
        ProcessTreeGuard::with_windows_job(pid, job_handle)
    };

    #[cfg(not(windows))]
    let tree = ProcessTreeGuard::new(pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(SpawnedProcess {
        child,
        stdout,
        stderr,
        tree,
    })
}

pub fn spawn_owned_program(command: Command) -> io::Result<SpawnedProcess> {
    spawn_owned_command_blocking(command, Stdio::piped())
}

fn spawn_shell_command_blocking(command: &str, cwd: &Path) -> io::Result<SpawnedProcess> {
    let mut shell = shell_command(command);
    shell.current_dir(cwd);
    spawn_owned_command_blocking(shell, Stdio::null())
}

pub async fn spawn_shell_command(command: &str, cwd: &Path) -> io::Result<SpawnedProcess> {
    let command = command.to_owned();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || spawn_shell_command_blocking(&command, &cwd))
        .await
        .map_err(|error| io::Error::other(format!("command spawn task failed: {error}")))?
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl BoundedBytes {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
            max_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.max_bytes == 0 {
            self.truncated |= !chunk.is_empty();
            return;
        }
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if chunk.len() <= remaining {
            self.bytes.extend_from_slice(chunk);
            return;
        }
        self.bytes.extend_from_slice(&chunk[..remaining]);
        self.truncated = true;
    }

    fn into_text(self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.truncated,
        )
    }
}

#[derive(Debug)]
struct ArchiveBudget {
    used: AtomicU64,
    truncated: AtomicBool,
    max_bytes: u64,
}

impl ArchiveBudget {
    fn new(max_bytes: u64) -> Self {
        Self {
            used: AtomicU64::new(0),
            truncated: AtomicBool::new(false),
            max_bytes,
        }
    }

    fn reserve(&self, requested: usize) -> usize {
        loop {
            let current = self.used.load(Ordering::Acquire);
            if current >= self.max_bytes {
                self.truncated.store(true, Ordering::Release);
                return 0;
            }
            let remaining = self.max_bytes - current;
            let allowed = remaining.min(requested as u64) as usize;
            match self.used.compare_exchange(
                current,
                current.saturating_add(allowed as u64),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if allowed < requested {
                        self.truncated.store(true, Ordering::Release);
                    }
                    return allowed;
                }
                Err(_) => continue,
            }
        }
    }
}

#[derive(Debug, Default)]
struct CapturedOutput {
    text: String,
    truncated: bool,
    read_error: Option<String>,
    archive_error: Option<String>,
}

async fn capture_reader<R>(
    mut reader: R,
    max_bytes: usize,
    archive_path: Option<PathBuf>,
    archive_budget: Option<Arc<ArchiveBudget>>,
) -> CapturedOutput
where
    R: AsyncRead + Unpin,
{
    let (mut archive, mut archive_error) = if let Some(path) = archive_path {
        match tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(file) => (Some(file), None),
            Err(error) => (
                None,
                Some(format!(
                    "failed to open command output archive {}: {error}",
                    path.display()
                )),
            ),
        }
    } else {
        (None, None)
    };

    let mut output = BoundedBytes::new(max_bytes);
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    let mut read_error = None;
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let chunk = &buffer[..read];
                if let Some(file) = archive.as_mut() {
                    let archive_len = archive_budget
                        .as_ref()
                        .map_or(chunk.len(), |budget| budget.reserve(chunk.len()));
                    if archive_len > 0
                        && let Err(error) = file.write_all(&chunk[..archive_len]).await
                    {
                        archive_error = Some(format!("failed to preserve command output: {error}"));
                        archive = None;
                    }
                }
                output.push(chunk);
            }
            Err(error) => {
                read_error = Some(error.to_string());
                break;
            }
        }
    }
    if let Some(file) = archive.as_mut()
        && let Err(error) = file.flush().await
    {
        archive_error = Some(format!("failed to flush complete command output: {error}"));
    }
    let (text, truncated) = output.into_text();
    CapturedOutput {
        text,
        truncated,
        read_error,
        archive_error,
    }
}

async fn finish_capture(
    task: Option<tokio::task::JoinHandle<CapturedOutput>>,
    stream: &str,
) -> CapturedOutput {
    let Some(task) = task else {
        return CapturedOutput::default();
    };
    match task.await {
        Ok(captured) => captured,
        Err(error) => CapturedOutput {
            read_error: Some(format!("{stream} capture task failed: {error}")),
            ..CapturedOutput::default()
        },
    }
}

fn append_stderr_diagnostic(stderr: &mut String, message: &str) {
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(message);
}

pub async fn run_shell_command(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
    output_paths: Option<&CommandOutputPaths>,
) -> ProcessRunResult {
    let mut process = match spawn_shell_command(command, cwd).await {
        Ok(process) => process,
        Err(error) => {
            return ProcessRunResult {
                stdout: String::new(),
                stderr: format!("Failed to execute: {error}"),
                success: false,
                exit_code: None,
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                output_archive_truncated: false,
                output_archive_error: None,
            };
        }
    };

    let stdout_archive = output_paths.map(|paths| paths.stdout.clone());
    let stderr_archive = output_paths.map(|paths| paths.stderr.clone());
    let archive_budget = output_paths
        .is_some()
        .then(|| Arc::new(ArchiveBudget::new(MAX_COMMAND_ARCHIVE_BYTES)));
    let stdout_budget = archive_budget.clone();
    let stderr_budget = archive_budget.clone();
    let stdout_task = process.take_stdout().map(|stdout| {
        tokio::spawn(capture_reader(
            stdout,
            max_capture_bytes,
            stdout_archive,
            stdout_budget,
        ))
    });
    let stderr_task = process.take_stderr().map(|stderr| {
        tokio::spawn(capture_reader(
            stderr,
            max_capture_bytes,
            stderr_archive,
            stderr_budget,
        ))
    });

    let mut timed_out = false;
    let mut wait_error = None;
    let status = match timeout(Duration::from_millis(timeout_ms), process.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            wait_error = Some(error.to_string());
            process.terminate_tree().await;
            process.wait().await.ok()
        }
        Err(_) => {
            timed_out = true;
            process.terminate_tree().await;
            process.wait().await.ok()
        }
    };
    process.disarm().await;

    let stdout_capture = finish_capture(stdout_task, "stdout").await;
    let stderr_capture = finish_capture(stderr_task, "stderr").await;
    let archive_errors = [
        stdout_capture.archive_error.as_deref(),
        stderr_capture.archive_error.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let output_archive_error = (!archive_errors.is_empty()).then(|| archive_errors.join("; "));
    let stdout = stdout_capture.text;
    let mut stderr = stderr_capture.text;

    if let Some(error) = wait_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("Failed while waiting for command: {error}"),
        );
    }
    if let Some(error) = stdout_capture.read_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("MoonDesk failed to read stdout: {error}"),
        );
    }
    if let Some(error) = stderr_capture.read_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("MoonDesk failed to read stderr: {error}"),
        );
    }
    if timed_out {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("Command timed out after {timeout_ms} ms"),
        );
    }

    let exit_code = status.as_ref().and_then(std::process::ExitStatus::code);
    let success = wait_error.is_none()
        && !timed_out
        && status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success);

    ProcessRunResult {
        stdout,
        stderr,
        success,
        exit_code,
        timed_out,
        stdout_truncated: stdout_capture.truncated,
        stderr_truncated: stderr_capture.truncated,
        output_archive_truncated: archive_budget
            .as_ref()
            .is_some_and(|budget| budget.truncated.load(Ordering::Acquire)),
        output_archive_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;
    use uuid::Uuid;

    struct PartialThenError {
        emitted: bool,
    }

    impl AsyncRead for PartialThenError {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if !self.emitted {
                self.emitted = true;
                buf.put_slice(b"partial-output");
                Poll::Ready(Ok(()))
            } else {
                Poll::Ready(Err(io::Error::other("synthetic read failure")))
            }
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("moondesk-process-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn process_tree_size_requires_a_live_root_process() {
        assert!(process_tree_size(std::process::id()).is_some_and(|count| count >= 1));
        assert_eq!(process_tree_size(u32::MAX), None);
    }

    #[tokio::test]
    async fn capture_reader_preserves_partial_output_on_read_error() {
        let captured = capture_reader(PartialThenError { emitted: false }, 1024, None, None).await;
        assert_eq!(captured.text, "partial-output");
        assert!(!captured.truncated);
        assert_eq!(
            captured.read_error.as_deref(),
            Some("synthetic read failure")
        );
    }

    #[tokio::test]
    async fn capture_reader_archives_bytes_beyond_inline_limit() {
        let root = workspace("archive-overflow");
        let archive = root.join("stdout.log");
        let payload = vec![b'x'; 16 * 1024];
        let expected_len = payload.len();
        let (mut writer, reader) = tokio::io::duplex(32 * 1024);
        let writer_task = tokio::spawn(async move {
            writer.write_all(&payload).await.expect("write payload");
        });

        let captured = capture_reader(reader, 1024, Some(archive.clone()), None).await;
        writer_task.await.expect("writer task");
        assert_eq!(captured.text.len(), 1024);
        assert!(captured.truncated);
        assert!(captured.archive_error.is_none());
        assert_eq!(
            std::fs::metadata(&archive).expect("archive metadata").len(),
            expected_len as u64
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn developer_shell_preserves_host_path_and_home() {
        let root = workspace("developer-env");
        let command = if cfg!(windows) {
            "if ([string]::IsNullOrWhiteSpace($env:PATH) -or [string]::IsNullOrWhiteSpace($env:USERPROFILE)) { exit 19 }; cargo --version"
        } else {
            r#"test -n "$PATH" && test -n "$HOME" && cargo --version"#
        };
        let result = run_shell_command(command, &root, 5_000, 8 * 1024, None).await;
        assert!(
            result.success,
            "developer environment was not preserved: {}",
            result.stderr
        );
        assert!(result.stdout.to_ascii_lowercase().contains("cargo"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "local Windows developer-tool compatibility smoke"]
    async fn windows_developer_toolchain_smoke_uses_normal_host_environment() {
        let root = workspace("developer-toolchain");
        let command = r#"
$requiredChecks = [ordered]@{
    git = @('--version')
    cargo = @('--version')
    rustc = @('--version')
    node = @('--version')
    npm = @('--version')
}
foreach ($tool in $requiredChecks.Keys) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { exit 20 }
    $toolArgs = $requiredChecks[$tool]
    & $tool @toolArgs
    if ($LASTEXITCODE -ne 0) { exit 21 }
}

$optionalTools = @(
    'python', 'pnpm', 'bun', 'deno', 'uv', 'java', 'javac',
    'dotnet', 'docker', 'kubectl', 'gcc'
)
foreach ($tool in $optionalTools) {
    if (Get-Command $tool -ErrorAction SilentlyContinue) {
        Write-Output "OPTIONAL_TOOL_VISIBLE=$tool"
    }
}

git init -q
if ($LASTEXITCODE -ne 0) { exit 22 }
git status --short
if ($LASTEXITCODE -ne 0) { exit 23 }

node -e "require('fs').writeFileSync('node-smoke.txt','ok')"
if ($LASTEXITCODE -ne 0) { exit 24 }
if (Test-Path Env:CUDA_PATH) { Write-Output "CUDA_PATH_PRESENT" }
"#;
        let result = run_shell_command(command, &root, 30_000, 128 * 1024, None).await;
        let git_created = root.join(".git").is_dir();
        let expected_cuda_path = std::env::var_os("CUDA_PATH").is_some();
        let node_created = root.join("node-smoke.txt").is_file();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            result.success,
            "developer toolchain smoke failed (exit {:?}): stdout={} stderr={}",
            result.exit_code, result.stdout, result.stderr
        );
        assert!(git_created, "git init did not create .git");
        if expected_cuda_path {
            assert!(
                result.stdout.contains("CUDA_PATH_PRESENT"),
                "host CUDA_PATH was not inherited by the developer shell"
            );
        }
        assert!(
            node_created,
            "Node was available but could not write in the workspace"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn developer_shell_can_bind_localhost_for_dev_servers() {
        let root = workspace("localhost-bind");
        let command = r#"
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
Write-Output $port
$listener.Stop()
"#;
        let result = run_shell_command(command, &root, 5_000, 8 * 1024, None).await;
        let _ = std::fs::remove_dir_all(root);
        assert!(result.success, "localhost bind failed: {}", result.stderr);
        assert!(result.stdout.trim().parse::<u16>().is_ok());
    }

    #[tokio::test]
    async fn run_shell_command_captures_output_and_exit_status() {
        let root = workspace("success");
        let command = if cfg!(windows) {
            "Write-Output 'hello'"
        } else {
            "printf 'hello\\n'"
        };
        let result = run_shell_command(command, &root, 5_000, 1024, None).await;
        assert!(result.success, "stderr: {}", result.stderr);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello");
        assert!(!result.timed_out);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_supports_common_agent_boolean_chains_and_env_prefixes() {
        let root = workspace("windows-shell-compat");

        let and_result = run_shell_command(
            "Write-Output first && Write-Output second",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            and_result.success,
            "and-chain failed: {}",
            and_result.stderr
        );
        assert!(and_result.stdout.contains("first"));
        assert!(and_result.stdout.contains("second"));

        let or_result = run_shell_command(
            "cmd /c exit 7 || Write-Output recovered",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(or_result.success, "or-chain failed: {}", or_result.stderr);
        assert!(or_result.stdout.contains("recovered"));

        let short_circuit = run_shell_command(
            "cmd /c exit 7 && Write-Output should-not-run",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(!short_circuit.success);
        assert_eq!(short_circuit.exit_code, Some(7));
        assert!(!short_circuit.stdout.contains("should-not-run"));

        let native_exit = run_shell_command("cmd /c exit 11", &root, 15_000, 8 * 1024, None).await;
        assert!(!native_exit.success);
        assert_eq!(native_exit.exit_code, Some(11));

        let commented_native_exit = run_shell_command(
            "cmd /c exit 13 # expected failure",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(!commented_native_exit.success);
        assert_eq!(commented_native_exit.exit_code, Some(13));

        let commented_chain = run_shell_command(
            "Write-Output before-comment && cmd /c exit 17 # expected failure",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            !commented_chain.success,
            "commented chain unexpectedly succeeded"
        );
        assert_eq!(commented_chain.exit_code, Some(17));
        assert!(commented_chain.stdout.contains("before-comment"));

        let operators_in_comment = run_shell_command(
            "Write-Output before-only # && Write-Output should-not-run || Write-Output neither",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            operators_in_comment.success,
            "operators inside comment changed execution: {}",
            operators_in_comment.stderr
        );
        assert_eq!(operators_in_comment.stdout.trim(), "before-only");
        assert!(!operators_in_comment.stdout.contains("should-not-run"));
        assert!(!operators_in_comment.stdout.contains("neither"));

        let hash_in_bareword = run_shell_command(
            "Write-Output file#name.txt && Write-Output after-hash",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            hash_in_bareword.success,
            "hash in bareword broke chain parsing: {}",
            hash_in_bareword.stderr
        );
        assert!(hash_in_bareword.stdout.contains("file#name.txt"));
        assert!(hash_in_bareword.stdout.contains("after-hash"));

        let block_comment = run_shell_command(
            "Write-Output before-block <# block\n&& Write-Output hidden\n#> && Write-Output after-block",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            block_comment.success,
            "block comment broke chain parsing: {}",
            block_comment.stderr
        );
        assert!(block_comment.stdout.contains("before-block"));
        assert!(block_comment.stdout.contains("after-block"));
        assert!(!block_comment.stdout.contains("hidden"));

        let single_here_string = run_shell_command(
            r#"@'
it's && literal
'@ && Write-Output after-single-here"#,
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            single_here_string.success,
            "single-quoted here-string broke chain parsing: {}",
            single_here_string.stderr
        );
        assert!(single_here_string.stdout.contains("it's && literal"));
        assert!(single_here_string.stdout.contains("after-single-here"));

        let double_here_string = run_shell_command(
            r#"@"
value && literal
"@ && Write-Output after-double-here"#,
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            double_here_string.success,
            "double-quoted here-string broke chain parsing: {}",
            double_here_string.stderr
        );
        assert!(double_here_string.stdout.contains("value && literal"));
        assert!(double_here_string.stdout.contains("after-double-here"));

        let env_result = run_shell_command(
            "MOONDESK_SHELL_COMPAT=visible Write-Output $env:MOONDESK_SHELL_COMPAT",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            env_result.success,
            "env-prefix failed: {}",
            env_result.stderr
        );
        assert_eq!(env_result.stdout.trim(), "visible");

        let internal_transport_env = run_shell_command(
            "if ($null -eq $env:MOONDESK_INTERNAL_WINDOWS_COMMAND) { Write-Output hidden } else { Write-Output leaked }",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            internal_transport_env.success,
            "internal transport env check failed: {}",
            internal_transport_env.stderr
        );
        assert_eq!(internal_transport_env.stdout.trim(), "hidden");

        let spaced_env = run_shell_command(
            "MOONDESK_SPACE='hello world' MOONDESK_TWO=second Write-Output \"$env:MOONDESK_SPACE|$env:MOONDESK_TWO\"",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            spaced_env.success,
            "spaced env prefix failed: {}",
            spaced_env.stderr
        );
        assert_eq!(spaced_env.stdout.trim(), "hello world|second");

        let escaped_single_env = run_shell_command(
            "MOONDESK_APOSTROPHE='don''t' Write-Output $env:MOONDESK_APOSTROPHE",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            escaped_single_env.success,
            "single-quoted env escape failed: {}",
            escaped_single_env.stderr
        );
        assert_eq!(escaped_single_env.stdout.trim(), "don't");

        let escaped_double_env = run_shell_command(
            "MOONDESK_BACKTICK=\"left`tvalue\" Write-Output ($env:MOONDESK_BACKTICK -replace \"`t\", \"|\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            escaped_double_env.success,
            "double-quoted env escape failed: {}",
            escaped_double_env.stderr
        );
        assert_eq!(escaped_double_env.stdout.trim(), "left|value");

        let scoped_env = run_shell_command(
            "MOONDESK_SCOPED=visible Write-Output (\"prefixed=$env:MOONDESK_SCOPED\"); Write-Output (\"later=$([string]$env:MOONDESK_SCOPED)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            scoped_env.success,
            "scoped env prefix failed: {}",
            scoped_env.stderr
        );
        assert!(scoped_env.stdout.contains("prefixed=visible"));
        assert!(scoped_env.stdout.contains("later="));
        assert!(!scoped_env.stdout.contains("later=visible"));

        let restored_env = run_shell_command(
            "$env:MOONDESK_RESTORE='original'; MOONDESK_RESTORE=temporary Write-Output (\"inside=$env:MOONDESK_RESTORE\"); Write-Output (\"after=$env:MOONDESK_RESTORE\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            restored_env.success,
            "existing env restoration failed: {}",
            restored_env.stderr
        );
        assert!(restored_env.stdout.contains("inside=temporary"));
        assert!(restored_env.stdout.contains("after=original"));

        let case_insensitive_restore = run_shell_command(
            "$env:MoOnDeSk_CaSe_ReStOrE='original'; MOONDESK_CASE_RESTORE=temporary Write-Output (\"case-inside=$env:MOONDESK_CASE_RESTORE\"); Write-Output (\"case-after=$env:moondesk_case_restore\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            case_insensitive_restore.success,
            "case-insensitive env restoration failed: {}",
            case_insensitive_restore.stderr
        );
        assert!(
            case_insensitive_restore
                .stdout
                .contains("case-inside=temporary")
        );
        assert!(
            case_insensitive_restore
                .stdout
                .contains("case-after=original")
        );

        let empty_env = run_shell_command(
            "MOONDESK_EMPTY_PREFIX='' Write-Output (\"empty-prefix=$([Environment]::GetEnvironmentVariables('Process').Contains('MOONDESK_EMPTY_PREFIX'))\"); Write-Output (\"empty-after=$([Environment]::GetEnvironmentVariables('Process').Contains('MOONDESK_EMPTY_PREFIX'))\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            empty_env.success,
            "empty env prefix failed: {}",
            empty_env.stderr
        );
        assert!(empty_env.stdout.contains("empty-prefix=True"));
        assert!(empty_env.stdout.contains("empty-after=False"));

        let restored_empty_env = run_shell_command(
            "$__native=[System.Object].Assembly.GetType('Microsoft.Win32.Win32Native').GetMethod('SetEnvironmentVariable',[System.Reflection.BindingFlags]'NonPublic,Static'); [void]$__native.Invoke($null,@('MOONDESK_EMPTY_RESTORE','')); Write-Output (\"empty-before=$([Environment]::GetEnvironmentVariables('Process').Contains('MOONDESK_EMPTY_RESTORE'))|$(([string][Environment]::GetEnvironmentVariable('MOONDESK_EMPTY_RESTORE',[EnvironmentVariableTarget]::Process)).Length)\"); MOONDESK_EMPTY_RESTORE=temporary Write-Output (\"empty-inside=$env:MOONDESK_EMPTY_RESTORE\"); Write-Output (\"empty-restored=$([Environment]::GetEnvironmentVariables('Process').Contains('MOONDESK_EMPTY_RESTORE'))|$(([string][Environment]::GetEnvironmentVariable('MOONDESK_EMPTY_RESTORE',[EnvironmentVariableTarget]::Process)).Length)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            restored_empty_env.success,
            "empty env restoration failed: {}",
            restored_empty_env.stderr
        );
        assert!(
            restored_empty_env.stdout.contains("empty-before=True|0"),
            "failed to create pre-existing empty env value: {}",
            restored_empty_env.stdout
        );
        assert!(restored_empty_env.stdout.contains("empty-inside=temporary"));
        assert!(
            restored_empty_env.stdout.contains("empty-restored=True|0"),
            "empty env value was not restored: {}",
            restored_empty_env.stdout
        );

        let helper_name_collision = run_shell_command(
            "function Set-MoonDeskProcessEnvironment { param($name,$value) Write-Output (\"hijacked=$name\") }; MOONDESK_HELPER_COLLISION=visible Write-Output (\"collision-inside=$env:MOONDESK_HELPER_COLLISION\"); Write-Output (\"collision-after=$([string]$env:MOONDESK_HELPER_COLLISION)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            helper_name_collision.success,
            "user-defined helper name intercepted env scoping: {}",
            helper_name_collision.stderr
        );
        assert!(
            helper_name_collision
                .stdout
                .contains("collision-inside=visible")
        );
        assert!(helper_name_collision.stdout.contains("collision-after="));
        assert!(!helper_name_collision.stdout.contains("hijacked="));

        let chain_scoped_env = run_shell_command(
            "MOONDESK_CHAIN_SCOPE=visible Write-Output (\"chain-first=$env:MOONDESK_CHAIN_SCOPE\") && Write-Output (\"chain-second=$([string]$env:MOONDESK_CHAIN_SCOPE)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            chain_scoped_env.success,
            "chain env scoping failed: {}",
            chain_scoped_env.stderr
        );
        assert!(chain_scoped_env.stdout.contains("chain-first=visible"));
        assert!(chain_scoped_env.stdout.contains("chain-second="));
        assert!(!chain_scoped_env.stdout.contains("chain-second=visible"));

        let prefixed_failure = run_shell_command(
            "MOONDESK_FAILURE_SCOPE=visible cmd /c exit 41 || Write-Output (\"recovered=$([string]$env:MOONDESK_FAILURE_SCOPE)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            prefixed_failure.success,
            "prefixed failure did not preserve chain status: {}",
            prefixed_failure.stderr
        );
        assert_eq!(prefixed_failure.stdout.trim(), "recovered=");

        let restored_after_failure = run_shell_command(
            "$env:MOONDESK_FAILURE_RESTORE='original'; MOONDESK_FAILURE_RESTORE=temporary cmd /c exit 43 || Write-Output (\"failure-after=$env:MOONDESK_FAILURE_RESTORE\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            restored_after_failure.success,
            "failed env-prefixed command did not restore before fallback: {}",
            restored_after_failure.stderr
        );
        assert_eq!(
            restored_after_failure.stdout.trim(),
            "failure-after=original"
        );

        let direct_prefixed_failure = run_shell_command(
            "MOONDESK_DIRECT_FAILURE=visible cmd /c exit 42",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(!direct_prefixed_failure.success);
        assert_eq!(direct_prefixed_failure.exit_code, Some(42));

        let nested_env = run_shell_command(
            "MOONDESK_NEST=outer & { MOONDESK_NEST=inner Write-Output (\"inner=$env:MOONDESK_NEST\"); Write-Output (\"outer-restored=$env:MOONDESK_NEST\") }; Write-Output (\"nested-after=$([string]$env:MOONDESK_NEST)\")",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            nested_env.success,
            "nested env prefix failed: {}",
            nested_env.stderr
        );
        assert!(nested_env.stdout.contains("inner=inner"));
        assert!(nested_env.stdout.contains("outer-restored=outer"));
        assert!(nested_env.stdout.contains("nested-after="));
        assert!(!nested_env.stdout.contains("nested-after=outer"));
        assert!(!nested_env.stdout.contains("nested-after=inner"));

        for (command, expected_code) in [
            (
                "$null = (cmd /c exit 31 && Write-Output should-not-run)",
                31,
            ),
            (
                "$null = $(cmd /c exit 32 && Write-Output should-not-run)",
                32,
            ),
            (
                "$null = @(cmd /c exit 33 && Write-Output should-not-run)",
                33,
            ),
        ] {
            let expression_failure =
                run_shell_command(command, &root, 15_000, 8 * 1024, None).await;
            assert!(
                !expression_failure.success,
                "failing expression chain unexpectedly succeeded: {command}"
            );
            assert_eq!(
                expression_failure.exit_code,
                Some(expected_code),
                "wrong expression-chain exit code: {command}"
            );
            assert!(!expression_failure.stdout.contains("should-not-run"));
        }

        let later_statement_wins = run_shell_command(
            "if ($true) { cmd /c exit 34 && Write-Output should-not-run; Write-Output recovered-inside-if }",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            later_statement_wins.success,
            "later successful statement did not supersede earlier chain failure: {}",
            later_statement_wins.stderr
        );
        assert!(later_statement_wins.stdout.contains("recovered-inside-if"));
        assert!(!later_statement_wins.stdout.contains("should-not-run"));

        let quoted_operator = run_shell_command(
            "Write-Output 'literal && operator || text' && Write-Output quoted-done",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            quoted_operator.success,
            "quoted operator failed: {}",
            quoted_operator.stderr
        );
        assert!(
            quoted_operator
                .stdout
                .contains("literal && operator || text")
        );
        assert!(quoted_operator.stdout.contains("quoted-done"));

        let nested_block = run_shell_command(
            "& { Write-Output nested-one && Write-Output nested-two }",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            nested_block.success,
            "nested chain failed: {}",
            nested_block.stderr
        );
        assert!(nested_block.stdout.contains("nested-one"));
        assert!(nested_block.stdout.contains("nested-two"));

        let unicode_result = run_shell_command(
            "Write-Output 'こんにちは🙂'; [Console]::Error.WriteLine('错误🙂')",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            unicode_result.success,
            "unicode command failed: {}",
            unicode_result.stderr
        );
        assert!(unicode_result.stdout.contains("こんにちは🙂"));
        assert!(unicode_result.stderr.contains("错误🙂"));

        let mixed_result = run_shell_command(
            "Write-Output begin && cmd /c exit 9 || Write-Output fallback && Write-Output end",
            &root,
            15_000,
            8 * 1024,
            None,
        )
        .await;
        assert!(
            mixed_result.success,
            "mixed chain failed: {}",
            mixed_result.stderr
        );
        for expected in ["begin", "fallback", "end"] {
            assert!(
                mixed_result.stdout.contains(expected),
                "missing {expected}: {}",
                mixed_result.stdout
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn large_stdout_and_stderr_are_drained_without_deadlock_and_bounded() {
        let root = workspace("bounded-output");
        let command = if cfg!(windows) {
            "[Console]::Out.Write(('x' * 200000)); [Console]::Error.Write(('y' * 200000))"
        } else {
            "printf '%*s' 200000 ''; printf '%*s' 200000 '' >&2"
        };
        let result = run_shell_command(command, &root, 5_000, 4_096, None).await;
        assert!(
            result.success,
            "large-output command failed: {}",
            result.stderr
        );
        assert!(result.stdout.len() <= 4_096);
        assert!(result.stderr.len() <= 4_096);
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timed_out_command_cannot_continue_after_return() {
        let root = workspace("timeout");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 700; Set-Content -Path sentinel.txt -Value survived"
        } else {
            "sleep 0.7; printf survived > sentinel.txt"
        };
        let result = run_shell_command(command, &root, 100, 1024, None).await;
        assert!(result.timed_out);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "timed-out process survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timeout_terminates_descendant_process_tree() {
        let root = workspace("descendant-timeout");
        let sentinel = root.join("descendant.txt");
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path descendant.txt -Value survived' -WorkingDirectory .; Start-Sleep -Seconds 5"
        } else {
            "(sleep 0.8; printf survived > descendant.txt) & sleep 5"
        };
        let result = run_shell_command(command, &root, 150, 1024, None).await;
        assert!(result.timed_out);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "timed-out root shell left a descendant process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn successful_root_exit_cannot_leave_detached_descendant_alive() {
        let root = workspace("detached-success");
        let sentinel = root.join("detached.txt");
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path detached.txt -Value survived' -WorkingDirectory .; Write-Output root-done"
        } else {
            "(sleep 0.8; printf survived > detached.txt) & printf 'root-done\\n'"
        };
        let result = run_shell_command(command, &root, 5_000, 1024, None).await;
        assert!(result.success, "root command failed: {}", result.stderr);
        assert!(result.stdout.contains("root-done"));
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "successful root shell detached a descendant outside MoonDesk ownership"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dropping_run_future_terminates_the_process() {
        let root = workspace("drop");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 700; Set-Content -Path sentinel.txt -Value survived"
        } else {
            "sleep 0.7; printf survived > sentinel.txt"
        };
        let root_for_task = root.clone();
        let task = tokio::spawn(async move {
            run_shell_command(command, &root_for_task, 5_000, 1024, None).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "dropped command future left the process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
