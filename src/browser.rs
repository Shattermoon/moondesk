use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::{Path, PathBuf};

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
}

struct BrowserCandidate {
    name: &'static str,
    binary: &'static str,
    remote_debugging: bool,
    remote_debug_hint: &'static str,
    mcp_supported: bool,
    support_note: &'static str,
}

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
            let cmdline = command_line
                .split_whitespace()
                .map(|part| part.trim_matches('"').to_string())
                .collect::<Vec<_>>();
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
}
