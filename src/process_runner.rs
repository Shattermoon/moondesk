use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::{Duration, timeout};

const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub struct ProcessRunResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// A spawned shell process owned by CatDesk.
///
/// Dropping this value is intentionally destructive: if the command is still
/// alive, CatDesk terminates the process tree. This is what keeps a cancelled
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
    pub fn terminate_tree(&mut self) {
        self.tree.terminate();
        // `taskkill /T` / process-group termination should already include the
        // root, but keep Tokio's direct kill as a best-effort fallback.
        let _ = self.child.start_kill();
    }

    /// Finalize ownership after the root process exits. Any descendants still
    /// alive at that point are terminated so a command cannot silently detach
    /// work that outlives its CatDesk job.
    pub fn disarm(&mut self) {
        self.tree.disarm();
    }
}

impl Drop for SpawnedProcess {
    fn drop(&mut self) {
        if self.tree.is_armed() {
            self.tree.terminate();
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
    fn new(pid: u32) -> Self {
        Self {
            pid,
            armed: true,
            #[cfg(windows)]
            job_handle: create_windows_job_for_process(pid),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed
    }

    fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if self.job_handle.is_some() {
                close_windows_job(&mut self.job_handle);
            } else {
                // Best effort when Job Object assignment was unavailable.
                terminate_process_tree(self.pid);
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }

    fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if !terminate_windows_job(&mut self.job_handle) {
                terminate_process_tree(self.pid);
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
fn create_windows_job_for_process(pid: u32) -> Option<usize> {
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
            return None;
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
            CloseHandle(job);
            return None;
        }

        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return None;
        }
        let assigned = AssignProcessToJobObject(job, process) != 0;
        CloseHandle(process);
        if !assigned {
            CloseHandle(job);
            return None;
        }

        Some(job as usize)
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
fn terminate_process_tree(pid: u32) {
    // `/T` includes descendants and `/F` makes cancellation deterministic.
    // Use the executable directly rather than a shell command so the PID never
    // passes through shell parsing.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

pub fn spawn_shell_command(command: &str, cwd: &Path) -> io::Result<SpawnedProcess> {
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

    let mut child = shell.spawn()?;
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return Err(io::Error::other(
            "spawned command did not expose a process id",
        ));
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(SpawnedProcess {
        child,
        stdout,
        stderr,
        tree: ProcessTreeGuard::new(pid),
    })
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

async fn capture_reader<R>(mut reader: R, max_bytes: usize) -> io::Result<(String, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut output = BoundedBytes::new(max_bytes);
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        output.push(&buffer[..read]);
    }
    Ok(output.into_text())
}

pub async fn run_shell_command(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
) -> ProcessRunResult {
    let started = Instant::now();
    let mut process = match spawn_shell_command(command, cwd) {
        Ok(process) => process,
        Err(error) => {
            return ProcessRunResult {
                stdout: String::new(),
                stderr: format!("Failed to execute: {error}"),
                success: false,
                exit_code: None,
                elapsed_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            };
        }
    };

    let stdout_task = process
        .take_stdout()
        .map(|stdout| tokio::spawn(capture_reader(stdout, max_capture_bytes)));
    let stderr_task = process
        .take_stderr()
        .map(|stderr| tokio::spawn(capture_reader(stderr, max_capture_bytes)));

    let mut timed_out = false;
    let status = match timeout(Duration::from_millis(timeout_ms), process.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            process.terminate_tree();
            let _ = process.wait().await;
            let mut stderr = format!("Failed while waiting for command: {error}");
            if let Some(task) = stderr_task
                && let Ok(Ok((captured, _))) = task.await
                && !captured.is_empty()
            {
                stderr.push('\n');
                stderr.push_str(&captured);
            }
            let stdout = if let Some(task) = stdout_task {
                task.await
                    .ok()
                    .and_then(Result::ok)
                    .map(|entry| entry.0)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return ProcessRunResult {
                stdout,
                stderr,
                success: false,
                exit_code: None,
                elapsed_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            };
        }
        Err(_) => {
            timed_out = true;
            process.terminate_tree();
            process.wait().await.ok()
        }
    };
    process.disarm();

    let (stdout, stdout_truncated) = match stdout_task {
        Some(task) => task
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| (String::new(), false)),
        None => (String::new(), false),
    };
    let (mut stderr, stderr_truncated) = match stderr_task {
        Some(task) => task
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| (String::new(), false)),
        None => (String::new(), false),
    };

    if timed_out {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!("Command timed out after {timeout_ms} ms"));
    }

    let exit_code = status.as_ref().and_then(std::process::ExitStatus::code);
    let success = !timed_out
        && status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success);

    ProcessRunResult {
        stdout,
        stderr,
        success,
        exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("catdesk-process-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    #[tokio::test]
    async fn run_shell_command_captures_output_and_exit_status() {
        let root = workspace("success");
        let command = if cfg!(windows) {
            "Write-Output 'hello'"
        } else {
            "printf 'hello\\n'"
        };
        let result = run_shell_command(command, &root, 5_000, 1024).await;
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
            "python3 -c \"import sys; sys.stdout.write('x'*200000); sys.stderr.write('y'*200000)\""
        };
        let result = run_shell_command(command, &root, 5_000, 4_096).await;
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
        let result = run_shell_command(command, &root, 100, 1024).await;
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
        let result = run_shell_command(command, &root, 150, 1024).await;
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
        let result = run_shell_command(command, &root, 5_000, 1024).await;
        assert!(result.success, "root command failed: {}", result.stderr);
        assert!(result.stdout.contains("root-done"));
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "successful root shell detached a descendant outside CatDesk ownership"
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
        let task =
            tokio::spawn(
                async move { run_shell_command(command, &root_for_task, 5_000, 1024).await },
            );
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
