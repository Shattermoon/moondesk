use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
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

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut shell = Command::new("powershell.exe");
        shell
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(command);
        shell
    }

    #[cfg(not(windows))]
    {
        let mut shell = Command::new("/bin/bash");
        shell.arg("-c").arg(command);
        shell
    }
}

fn spawn_shell_command_blocking(command: &str, cwd: &Path) -> io::Result<SpawnedProcess> {
    let mut shell = shell_command(command);
    shell
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        shell.as_std_mut().process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        shell.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }

    let mut child = shell.spawn()?;
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
    fn where_first_in_test(name: &str) -> Option<PathBuf> {
        std::process::Command::new("where.exe")
            .arg(name)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| text.lines().next().map(PathBuf::from))
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "local Windows developer-tool compatibility smoke"]
    async fn windows_developer_toolchain_smoke_uses_normal_host_environment() {
        let root = workspace("developer-toolchain");
        let command = r#"
$checks = @{
    git = @('--version')
    cargo = @('--version')
    rustc = @('--version')
    python = @('--version')
    node = @('--version')
    npm = @('--version')
    pnpm = @('--version')
    bun = @('--version')
    deno = @('--version')
    uv = @('--version')
    java = @('-version')
    javac = @('-version')
    dotnet = @('--version')
    docker = @('--version')
    kubectl = @('version','--client')
    gcc = @('--version')
}
$seen = 0
foreach ($tool in $checks.Keys) {
    if (Get-Command $tool -ErrorAction SilentlyContinue) {
        $seen++
        $toolArgs = $checks[$tool]
        & $tool @toolArgs
        if ($LASTEXITCODE -ne 0) { exit 20 }
    }
}
if ($seen -lt 5) { exit 21 }

git init -q
if ($LASTEXITCODE -ne 0) { exit 22 }
git status --short
if ($LASTEXITCODE -ne 0) { exit 23 }

if (Get-Command python -ErrorAction SilentlyContinue) {
    python -c "open('python-smoke.txt','w').write('ok')"
    if ($LASTEXITCODE -ne 0) { exit 24 }
}
if (Get-Command node -ErrorAction SilentlyContinue) {
    node -e "require('fs').writeFileSync('node-smoke.txt','ok')"
    if ($LASTEXITCODE -ne 0) { exit 25 }
}
if (Test-Path Env:CUDA_PATH) { Write-Output "CUDA_PATH_PRESENT" }
"#;
        let result = run_shell_command(command, &root, 30_000, 128 * 1024, None).await;
        let git_created = root.join(".git").is_dir();
        let expected_cuda_path = std::env::var_os("CUDA_PATH").is_some();
        let python_created =
            where_first_in_test("python").is_none() || root.join("python-smoke.txt").is_file();
        let node_created =
            where_first_in_test("node").is_none() || root.join("node-smoke.txt").is_file();
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
            python_created,
            "Python was discoverable but could not write in the workspace"
        );
        assert!(
            node_created,
            "Node was discoverable but could not write in the workspace"
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
