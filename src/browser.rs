use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
pub struct DetectedBrowser {
    pub name: String,
    pub binary: String,
    pub path: String,
    pub remote_debugging: bool,
    pub remote_debug_hint: String,
    pub mcp_supported: bool,
    pub support_note: String,
    pub remote_debug_active: bool,
    pub remote_debug_target: Option<String>,
    pub remote_debug_pid: Option<u32>,
    #[serde(skip)]
    pub remote_debug_page_count: Option<usize>,
}

struct BrowserCandidate {
    name: &'static str,
    binary: &'static str,
    remote_debugging: bool,
    remote_debug_hint: &'static str,
    mcp_supported: bool,
    support_note: &'static str,
}

pub const LARGE_REMOTE_DEBUG_PAGE_COUNT: usize = 25;
const REMOTE_DEBUG_PROBE_TIMEOUT: Duration = Duration::from_millis(350);
const MAX_REMOTE_DEBUG_RESPONSE_BYTES: u64 = 512 * 1024;

const CANDIDATES: &[BrowserCandidate] = &[
    // Native Windows executable names come first. They simply do not resolve on
    // Unix, while the Unix aliases below do not resolve on a normal Windows
    // install, so the shared candidate list stays deterministic across platforms.
    BrowserCandidate {
        name: "Google Chrome",
        binary: "chrome.exe",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "msedge.exe",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Brave",
        binary: "brave.exe",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Vivaldi",
        binary: "vivaldi.exe",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Opera",
        binary: "opera.exe",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Firefox",
        binary: "firefox.exe",
        remote_debugging: false,
        remote_debug_hint: "--remote-debugging-port <port>",
        mcp_supported: false,
        support_note: "Not supported yet (CDP bridge for Firefox not wired)",
    },
    BrowserCandidate {
        name: "Google Chrome",
        binary: "google-chrome-stable",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Google Chrome",
        binary: "google-chrome",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Chromium",
        binary: "chromium",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Chromium",
        binary: "chromium-browser",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "microsoft-edge-stable",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "microsoft-edge",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Brave",
        binary: "brave-browser",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Vivaldi",
        binary: "vivaldi",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Opera",
        binary: "opera",
        remote_debugging: true,
        remote_debug_hint: "--remote-debugging-port=<port>",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Firefox",
        binary: "firefox",
        remote_debugging: false,
        remote_debug_hint: "--remote-debugging-port <port>",
        mcp_supported: false,
        support_note: "Not supported yet (CDP bridge for Firefox not wired)",
    },
];

pub fn detect_browsers() -> Vec<DetectedBrowser> {
    let mut found: Vec<DetectedBrowser> = Vec::new();
    let mut seen_names: HashSet<&'static str> = HashSet::new();
    let mut seen_paths: HashSet<String> = HashSet::new();
    let processes = collect_processes();

    for candidate in CANDIDATES {
        let Some(path) = resolve_binary(candidate.binary) else {
            continue;
        };

        if !seen_names.insert(candidate.name) {
            continue;
        }

        let normalized = normalize_path(&path);
        if !seen_paths.insert(normalized) {
            continue;
        }

        let active_remote = find_active_remote_debug_for_binary(candidate.binary, &processes);
        let remote_debug_page_count = active_remote
            .as_ref()
            .and_then(|remote| probe_remote_debug_page_count(&remote.target));

        found.push(DetectedBrowser {
            name: candidate.name.to_string(),
            binary: candidate.binary.to_string(),
            path: path.display().to_string(),
            remote_debugging: candidate.remote_debugging,
            remote_debug_hint: candidate.remote_debug_hint.to_string(),
            mcp_supported: candidate.mcp_supported,
            support_note: candidate.support_note.to_string(),
            remote_debug_active: active_remote.is_some(),
            remote_debug_target: active_remote.as_ref().map(|r| r.target.clone()),
            remote_debug_pid: active_remote.as_ref().map(|r| r.pid),
            remote_debug_page_count,
        });
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn resolve_binary(binary: &str) -> Option<PathBuf> {
    let input = Path::new(binary);
    if input.is_absolute() || binary.contains('/') || binary.contains('\\') {
        if input.is_file() {
            return Some(input.to_path_buf());
        }
        return None;
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    #[cfg(windows)]
    return windows_known_install_paths(binary)
        .into_iter()
        .find(|candidate| candidate.is_file());

    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
fn windows_known_install_paths(binary: &str) -> Vec<PathBuf> {
    fn under(env_name: &str, relative: &str) -> Option<PathBuf> {
        std::env::var_os(env_name).map(|root| PathBuf::from(root).join(relative))
    }

    let mut paths = Vec::new();
    let mut push = |env_name: &str, relative: &str| {
        if let Some(path) = under(env_name, relative) {
            paths.push(path);
        }
    };

    match binary.to_ascii_lowercase().as_str() {
        "chrome.exe" => {
            push("ProgramFiles", r"Google\Chrome\Application\chrome.exe");
            push("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe");
            push("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe");
        }
        "msedge.exe" => {
            push("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe");
            push(
                "ProgramFiles(x86)",
                r"Microsoft\Edge\Application\msedge.exe",
            );
            push("LOCALAPPDATA", r"Microsoft\Edge\Application\msedge.exe");
        }
        "brave.exe" => {
            push(
                "ProgramFiles",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            );
            push(
                "ProgramFiles(x86)",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            );
            push(
                "LOCALAPPDATA",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            );
        }
        "vivaldi.exe" => {
            push("LOCALAPPDATA", r"Vivaldi\Application\vivaldi.exe");
            push("ProgramFiles", r"Vivaldi\Application\vivaldi.exe");
            push("ProgramFiles(x86)", r"Vivaldi\Application\vivaldi.exe");
        }
        "opera.exe" => {
            push("LOCALAPPDATA", r"Programs\Opera\opera.exe");
            push("LOCALAPPDATA", r"Programs\Opera GX\opera.exe");
            push("ProgramFiles", r"Opera\opera.exe");
            push("ProgramFiles(x86)", r"Opera\opera.exe");
        }
        "firefox.exe" => {
            push("ProgramFiles", r"Mozilla Firefox\firefox.exe");
            push("ProgramFiles(x86)", r"Mozilla Firefox\firefox.exe");
            push("LOCALAPPDATA", r"Mozilla Firefox\firefox.exe");
        }
        _ => {}
    }

    paths
}

fn normalize_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

struct ProcessInfo {
    pid: u32,
    binary: String,
    cmdline: Vec<String>,
}

struct ActiveRemoteDebug {
    pid: u32,
    target: String,
}

fn loopback_remote_debug_addr(target: &str) -> Option<SocketAddr> {
    if target == "pipe" {
        return None;
    }

    let parsed = target.parse::<SocketAddr>().ok().or_else(|| {
        let (host, port) = target.rsplit_once(':')?;
        let port = port.parse::<u16>().ok()?;
        let ip = if host.eq_ignore_ascii_case("localhost") {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            host.trim_matches(['[', ']']).parse::<IpAddr>().ok()?
        };
        Some(SocketAddr::new(ip, port))
    })?;

    if parsed.ip().is_loopback() {
        return Some(parsed);
    }
    if parsed.ip().is_unspecified() {
        let loopback = match parsed.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        return Some(SocketAddr::new(loopback, parsed.port()));
    }
    None
}

fn http_body_bounds(response: &[u8]) -> Option<(usize, usize)> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let status_ok = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        == Some("200");
    if !status_ok
        || headers.lines().any(|line| {
            line.to_ascii_lowercase()
                .starts_with("transfer-encoding: chunked")
        })
    {
        return None;
    }

    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let body_start = header_end.checked_add(4)?;
    let body_end = body_start.checked_add(content_length)?;
    (body_end <= MAX_REMOTE_DEBUG_RESPONSE_BYTES as usize).then_some((body_start, body_end))
}

fn read_http_body(stream: &mut TcpStream, deadline: Instant) -> Option<Vec<u8>> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if let Some((body_start, body_end)) = http_body_bounds(&response)
            && response.len() >= body_end
        {
            return Some(response[body_start..body_end].to_vec());
        }

        let remaining = (MAX_REMOTE_DEBUG_RESPONSE_BYTES as usize).saturating_sub(response.len());
        if remaining == 0 {
            return None;
        }
        let time_left = deadline.saturating_duration_since(Instant::now());
        if time_left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(time_left)).ok()?;

        let read_limit = remaining.min(buffer.len());
        let read = stream.read(&mut buffer[..read_limit]).ok()?;
        if read == 0 {
            let (body_start, body_end) = http_body_bounds(&response)?;
            return (response.len() >= body_end).then(|| response[body_start..body_end].to_vec());
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

fn probe_remote_debug_page_count(target: &str) -> Option<usize> {
    let addr = loopback_remote_debug_addr(target)?;
    let deadline = Instant::now() + REMOTE_DEBUG_PROBE_TIMEOUT;
    let mut stream = TcpStream::connect_timeout(&addr, REMOTE_DEBUG_PROBE_TIMEOUT).ok()?;
    let write_time_left = deadline.saturating_duration_since(Instant::now());
    if write_time_left.is_zero() {
        return None;
    }
    stream.set_write_timeout(Some(write_time_left)).ok()?;
    let request = format!("GET /json/list HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;

    let body = read_http_body(&mut stream, deadline)?;
    let targets = serde_json::from_slice::<Vec<serde_json::Value>>(&body).ok()?;
    Some(
        targets
            .iter()
            .filter(|target| target.get("type").and_then(serde_json::Value::as_str) == Some("page"))
            .count(),
    )
}

#[cfg(target_os = "linux")]
fn collect_processes() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return processes;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let cmdline_path = entry.path().join("cmdline");
        let Ok(bytes) = fs::read(cmdline_path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let args: Vec<String> = bytes
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if args.is_empty() {
            continue;
        }
        processes.push(ProcessInfo {
            pid,
            binary: args[0].clone(),
            cmdline: args,
        });
    }

    processes
}

#[cfg(windows)]
fn parse_windows_command_line(command_line: &str) -> Vec<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let mut wide = command_line.encode_utf16().collect::<Vec<_>>();
    wide.push(0);
    let mut argc = 0i32;
    let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
    if argv.is_null() || argc <= 0 {
        return Vec::new();
    }

    let args = unsafe {
        std::slice::from_raw_parts(argv, argc as usize)
            .iter()
            .map(|arg| {
                let ptr = *arg;
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
            })
            .collect::<Vec<_>>()
    };
    unsafe {
        let _ = LocalFree(argv.cast());
    }
    args
}

#[cfg(windows)]
fn collect_processes() -> Vec<ProcessInfo> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct WindowsProcess {
        process_id: u32,
        name: String,
        command_line: Option<String>,
    }

    const SCRIPT: &str = r#"
$names = @('chrome.exe','msedge.exe','brave.exe','vivaldi.exe','opera.exe','firefox.exe')
$rows = @(
    Get-CimInstance Win32_Process |
        Where-Object { $_.Name -in $names } |
        Select-Object ProcessId, Name, CommandLine
)
ConvertTo-Json -InputObject $rows -Compress
"#;

    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(rows) = serde_json::from_slice::<Vec<WindowsProcess>>(&output.stdout) else {
        return Vec::new();
    };

    rows.into_iter()
        .filter_map(|row| {
            let command_line = row.command_line?;
            let cmdline = parse_windows_command_line(&command_line);
            Some(ProcessInfo {
                pid: row.process_id,
                binary: row.name,
                cmdline,
            })
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn collect_processes() -> Vec<ProcessInfo> {
    Vec::new()
}

fn find_active_remote_debug_for_binary(
    binary: &str,
    processes: &[ProcessInfo],
) -> Option<ActiveRemoteDebug> {
    for p in processes {
        if !process_matches_binary(p, binary) {
            continue;
        }
        let Some(target) = extract_remote_debug_target(&p.cmdline) else {
            continue;
        };
        return Some(ActiveRemoteDebug { pid: p.pid, target });
    }
    None
}

fn process_matches_binary(process: &ProcessInfo, binary: &str) -> bool {
    if executable_name_matches(&process.binary, binary) {
        return true;
    }
    process
        .cmdline
        .iter()
        .any(|arg| command_matches_binary(arg, binary))
}

fn executable_name_matches(actual: &str, expected: &str) -> bool {
    #[cfg(windows)]
    return actual.eq_ignore_ascii_case(expected);

    #[cfg(not(windows))]
    return actual == expected;
}

fn command_matches_binary(arg: &str, binary: &str) -> bool {
    if executable_name_matches(arg, binary) {
        return true;
    }
    Path::new(arg)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| executable_name_matches(name, binary))
}

fn extract_remote_debug_target(args: &[String]) -> Option<String> {
    let mut address = "127.0.0.1".to_string();
    let mut port: Option<String> = None;

    for (idx, arg) in args.iter().enumerate() {
        if arg == "--remote-debugging-pipe" {
            return Some("pipe".into());
        }

        if let Some(v) = arg.strip_prefix("--remote-debugging-address=") {
            if !v.is_empty() {
                address = v.to_string();
            }
        } else if arg == "--remote-debugging-address"
            && let Some(v) = args.get(idx + 1)
            && !v.is_empty()
        {
            address = v.clone();
        }

        if let Some(v) = arg.strip_prefix("--remote-debugging-port=") {
            if !v.is_empty() {
                port = Some(v.to_string());
            }
        } else if arg == "--remote-debugging-port"
            && let Some(v) = args.get(idx + 1)
            && !v.is_empty()
        {
            port = Some(v.clone());
        }

        if let Some(v) = arg.strip_prefix("--start-debugger-server=") {
            if !v.is_empty() {
                port = Some(v.to_string());
            }
        } else if arg == "--start-debugger-server"
            && let Some(v) = args.get(idx + 1)
            && !v.is_empty()
        {
            port = Some(v.clone());
        }
    }

    port.map(|p| format!("{address}:{p}"))
}

pub fn format_browser_names(browsers: &[DetectedBrowser]) -> String {
    if browsers.is_empty() {
        return "--".into();
    }
    browsers
        .iter()
        .map(|b| b.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn format_remote_debug_names(browsers: &[DetectedBrowser]) -> String {
    let remote: Vec<&str> = browsers
        .iter()
        .filter(|b| b.mcp_supported && b.remote_debugging)
        .map(|b| b.name.as_str())
        .collect();
    if remote.is_empty() {
        return "--".into();
    }
    remote.join(", ")
}

pub fn format_active_remote_debug_names(browsers: &[DetectedBrowser]) -> String {
    let active: Vec<String> = browsers
        .iter()
        .filter(|b| b.mcp_supported && b.remote_debug_active)
        .map(|b| {
            if let Some(target) = &b.remote_debug_target {
                format!("{} ({target})", b.name)
            } else {
                b.name.clone()
            }
        })
        .collect();
    if active.is_empty() {
        return "--".into();
    }
    active.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_debug_target_parses_port_address_and_pipe_forms() {
        assert_eq!(
            extract_remote_debug_target(&["chrome".into(), "--remote-debugging-port=9222".into(),]),
            Some("127.0.0.1:9222".into())
        );
        assert_eq!(
            extract_remote_debug_target(&[
                "chrome".into(),
                "--remote-debugging-address".into(),
                "0.0.0.0".into(),
                "--remote-debugging-port".into(),
                "9333".into(),
            ]),
            Some("0.0.0.0:9333".into())
        );
        assert_eq!(
            extract_remote_debug_target(&["chrome".into(), "--remote-debugging-pipe".into()]),
            Some("pipe".into())
        );
    }

    #[test]
    fn remote_debug_page_probe_counts_pages_without_attaching_devtools() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind probe");
        let port = listener.local_addr().expect("probe address").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept probe");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).expect("read probe request");
            let body = r#"[{"type":"page"},{"type":"service_worker"},{"type":"page"}]"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write probe response");
            // Real Chrome can keep this HTTP connection open after the complete
            // Content-Length body arrives. The probe must not wait for EOF.
            std::thread::sleep(REMOTE_DEBUG_PROBE_TIMEOUT + Duration::from_millis(250));
        });

        assert_eq!(
            probe_remote_debug_page_count(&format!("0.0.0.0:{port}")),
            Some(2)
        );
        server.join().expect("probe server");
        assert_eq!(probe_remote_debug_page_count("192.0.2.1:9222"), None);
        assert_eq!(probe_remote_debug_page_count("pipe"), None);
        assert_eq!(
            loopback_remote_debug_addr("localhost:9333"),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9333))
        );
        assert_eq!(
            loopback_remote_debug_addr("[::]:9444"),
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9444))
        );
        assert_eq!(loopback_remote_debug_addr("example.com:9222"), None);
    }

    #[test]
    fn remote_debug_page_count_is_runtime_only_config_metadata() {
        let browser = DetectedBrowser {
            name: "Chrome".into(),
            binary: "chrome".into(),
            path: "/chrome".into(),
            remote_debugging: true,
            remote_debug_hint: "--remote-debugging-port=<port>".into(),
            mcp_supported: true,
            support_note: "Chromium (supported)".into(),
            remote_debug_active: true,
            remote_debug_target: Some("127.0.0.1:9222".into()),
            remote_debug_pid: Some(42),
            remote_debug_page_count: Some(64),
        };
        let mut value = serde_json::to_value(&browser).expect("serialize browser");
        assert!(value.get("remote_debug_page_count").is_none());

        value
            .as_object_mut()
            .expect("browser JSON is an object")
            .insert("remote_debug_page_count".into(), serde_json::json!(999));
        let restored: DetectedBrowser = serde_json::from_value(value).expect("deserialize browser");
        assert_eq!(restored.remote_debug_page_count, None);
    }

    #[test]
    fn process_matching_uses_the_recorded_executable_name() {
        let process = ProcessInfo {
            pid: 42,
            binary: if cfg!(windows) {
                "CHROME.EXE".into()
            } else {
                "google-chrome".into()
            },
            cmdline: Vec::new(),
        };
        let expected = if cfg!(windows) {
            "chrome.exe"
        } else {
            "google-chrome"
        };
        assert!(process_matches_binary(&process, expected));
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_line_parser_preserves_quoted_paths_and_unquotes_flag_values() {
        let args = parse_windows_command_line(
            r#""C:\Program Files\Google\Chrome\Application\chrome.exe" --remote-debugging-port="9222" --user-data-dir="C:\Temp\Moon Desk""#,
        );
        assert_eq!(
            args,
            vec![
                r#"C:\Program Files\Google\Chrome\Application\chrome.exe"#.to_string(),
                "--remote-debugging-port=9222".to_string(),
                r#"--user-data-dir=C:\Temp\Moon Desk"#.to_string(),
            ]
        );
        assert_eq!(
            extract_remote_debug_target(&args),
            Some("127.0.0.1:9222".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_hints_cover_standard_chrome_edge_and_brave_locations() {
        for (binary, suffix) in [
            ("chrome.exe", r"Google\Chrome\Application\chrome.exe"),
            ("msedge.exe", r"Microsoft\Edge\Application\msedge.exe"),
            (
                "brave.exe",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ),
        ] {
            let paths = windows_known_install_paths(binary);
            assert!(
                paths.iter().any(|path| path.ends_with(suffix)),
                "missing standard install hint for {binary}: {paths:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_collection_can_read_browser_command_lines() {
        let processes = collect_processes();
        assert!(processes.iter().all(|process| !process.binary.is_empty()));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "local Windows browser-detection smoke"]
    fn windows_detect_browsers_finds_a_standard_chromium_install() {
        let browsers = detect_browsers();
        assert!(
            browsers.iter().any(|browser| browser.mcp_supported),
            "no supported Chromium browser detected: {}",
            format_browser_names(&browsers)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a locally running Chromium remote-debug endpoint"]
    fn windows_active_remote_debug_browser_reports_page_count() {
        let browsers = detect_browsers();
        let active = browsers
            .iter()
            .find(|browser| browser.mcp_supported && browser.remote_debug_active)
            .expect("no active Chromium remote-debug endpoint detected");
        assert!(
            active
                .remote_debug_page_count
                .is_some_and(|count| count >= 1),
            "active remote-debug browser did not report pages: {}",
            active.name
        );
    }
}
