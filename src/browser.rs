use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct DetectedBrowser {
    pub name: String,
    pub binary: String,
    pub path: String,
    pub mcp_supported: bool,
    pub support_note: String,
}

struct BrowserCandidate {
    name: &'static str,
    binary: &'static str,
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
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "msedge.exe",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Brave",
        binary: "brave.exe",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Vivaldi",
        binary: "vivaldi.exe",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Opera",
        binary: "opera.exe",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Firefox",
        binary: "firefox.exe",
        mcp_supported: false,
        support_note: "Not supported yet (Chromium required)",
    },
    BrowserCandidate {
        name: "Google Chrome",
        binary: "google-chrome-stable",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Google Chrome",
        binary: "google-chrome",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Chromium",
        binary: "chromium",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Chromium",
        binary: "chromium-browser",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "microsoft-edge-stable",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Microsoft Edge",
        binary: "microsoft-edge",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Brave",
        binary: "brave-browser",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Vivaldi",
        binary: "vivaldi",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Opera",
        binary: "opera",
        mcp_supported: true,
        support_note: "Chromium (supported)",
    },
    BrowserCandidate {
        name: "Firefox",
        binary: "firefox",
        mcp_supported: false,
        support_note: "Not supported yet (Chromium required)",
    },
];

pub fn detect_browsers() -> Vec<DetectedBrowser> {
    let mut found = Vec::new();
    let mut seen_names: HashSet<&'static str> = HashSet::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for candidate in CANDIDATES {
        let Some(path) = resolve_binary(candidate.binary) else {
            continue;
        };
        if !seen_names.insert(candidate.name) {
            continue;
        }
        if !seen_paths.insert(normalize_path(&path)) {
            continue;
        }
        found.push(DetectedBrowser {
            name: candidate.name.to_string(),
            binary: candidate.binary.to_string(),
            path: path.display().to_string(),
            mcp_supported: candidate.mcp_supported,
            support_note: candidate.support_note.to_string(),
        });
    }

    // Keep candidate priority instead of alphabetizing. BrowserRuntime uses this list only as a
    // fallback when chrome-devtools-mcp cannot resolve its normal browser automatically, so prefer
    // Chrome, then Edge, then the other supported Chromium installs in the order above.
    found
}

fn resolve_binary(binary: &str) -> Option<PathBuf> {
    let input = Path::new(binary);
    if input.is_absolute() || binary.contains('/') || binary.contains('\\') {
        return input.is_file().then(|| input.to_path_buf());
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

#[cfg(all(test, windows))]
pub fn format_browser_names(browsers: &[DetectedBrowser]) -> String {
    if browsers.is_empty() {
        return "--".into();
    }
    browsers
        .iter()
        .map(|browser| browser.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_remote_debug_config_fields_are_ignored() {
        let value = serde_json::json!({
            "name": "Chrome",
            "binary": "chrome",
            "path": "/chrome",
            "mcp_supported": true,
            "support_note": "Chromium (supported)",
            "remote_debugging": true,
            "remote_debug_hint": "--remote-debugging-port=<port>",
            "remote_debug_active": true,
            "remote_debug_target": "127.0.0.1:9222",
            "remote_debug_pid": 42,
            "remote_debug_page_count": 64
        });
        let restored: DetectedBrowser =
            serde_json::from_value(value).expect("deserialize legacy browser config");
        assert_eq!(restored.name, "Chrome");
        assert_eq!(restored.path, "/chrome");
        assert!(restored.mcp_supported);

        let serialized = serde_json::to_value(&restored).expect("serialize browser");
        assert!(serialized.get("remote_debug_target").is_none());
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
