use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMPANION_DIR_NAME: &str = "worker-companion";
const FINGERPRINT_FILE_NAME: &str = ".moondesk-companion-source";

const RUNTIME_FILES: &[(&str, &[u8])] = &[
    (
        "background.js",
        include_bytes!("../extensions/moondesk-worker-companion/background.js"),
    ),
    (
        "chatgpt-dom.js",
        include_bytes!("../extensions/moondesk-worker-companion/chatgpt-dom.js"),
    ),
    (
        "content.js",
        include_bytes!("../extensions/moondesk-worker-companion/content.js"),
    ),
    (
        "manifest.json",
        include_bytes!("../extensions/moondesk-worker-companion/manifest.json"),
    ),
    (
        "model-state-main.js",
        include_bytes!("../extensions/moondesk-worker-companion/model-state-main.js"),
    ),
    (
        "popup.css",
        include_bytes!("../extensions/moondesk-worker-companion/popup.css"),
    ),
    (
        "popup.html",
        include_bytes!("../extensions/moondesk-worker-companion/popup.html"),
    ),
    (
        "popup.js",
        include_bytes!("../extensions/moondesk-worker-companion/popup.js"),
    ),
    (
        "provider-correlation-main.js",
        include_bytes!("../extensions/moondesk-worker-companion/provider-correlation-main.js"),
    ),
];

#[derive(Debug, Clone)]
pub struct CompanionInstall {
    pub directory: PathBuf,
    pub refreshed: bool,
}

fn embedded_fingerprint() -> String {
    let mut hash = Sha256::new();
    for (name, bytes) in RUNTIME_FILES {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

pub fn directory_for_config(config_path: &Path) -> io::Result<PathBuf> {
    let parent = config_path.parent().ok_or_else(|| {
        io::Error::other("failed to resolve MoonDesk data directory for Worker Companion")
    })?;
    Ok(parent.join(COMPANION_DIR_NAME))
}

fn replace_file_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::other("failed to resolve Worker Companion file parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("Worker Companion file name is invalid"))?;
    let temporary = parent.join(format!(".{file_name}.moondesk-next-{}", std::process::id()));
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    fs::write(&temporary, bytes)?;
    if target.exists() {
        fs::remove_file(target)?;
    }
    fs::rename(temporary, target)
}

fn read_fingerprint(directory: &Path) -> Option<String> {
    fs::read_to_string(directory.join(FINGERPRINT_FILE_NAME))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn published_tree_matches(directory: &Path) -> bool {
    RUNTIME_FILES.iter().all(|(name, bytes)| {
        fs::read(directory.join(name))
            .map(|current| current.as_slice() == *bytes)
            .unwrap_or(false)
    })
}

pub fn materialize_for_config(config_path: &Path) -> io::Result<CompanionInstall> {
    let directory = directory_for_config(config_path)?;
    fs::create_dir_all(&directory)?;
    let fingerprint = embedded_fingerprint();
    if published_tree_matches(&directory)
        && read_fingerprint(&directory).as_deref() == Some(fingerprint.as_str())
    {
        return Ok(CompanionInstall {
            directory,
            refreshed: false,
        });
    }

    let allowed = RUNTIME_FILES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .chain(std::iter::once(FINGERPRINT_FILE_NAME.to_string()))
        .collect::<BTreeSet<_>>();

    // Publish scripts/resources first and manifest/fingerprint last. Chrome remembers this exact
    // directory from Load unpacked, so keep the root directory in place across app updates.
    for (name, bytes) in RUNTIME_FILES
        .iter()
        .filter(|(name, _)| *name != "manifest.json")
    {
        replace_file_atomic(&directory.join(name), bytes)?;
    }
    let manifest = RUNTIME_FILES
        .iter()
        .find(|(name, _)| *name == "manifest.json")
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| io::Error::other("embedded Worker Companion manifest is missing"))?;
    replace_file_atomic(&directory.join("manifest.json"), manifest)?;
    replace_file_atomic(
        &directory.join(FINGERPRINT_FILE_NAME),
        fingerprint.as_bytes(),
    )?;

    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !allowed.contains(&name) && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }

    if !published_tree_matches(&directory) {
        return Err(io::Error::other(
            "Worker Companion materialization did not produce a complete extension",
        ));
    }

    Ok(CompanionInstall {
        directory,
        refreshed: true,
    })
}

pub fn open_folder(directory: &Path) -> Result<(), String> {
    if !directory.is_dir() {
        return Err(format!(
            "Worker Companion folder is unavailable: {}",
            directory.display()
        ));
    }

    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(directory);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(directory);
        command
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(directory);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to open Worker Companion folder: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("moondesk-companion-install-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }

    #[test]
    fn materialization_is_stable_and_refreshes_tampered_files() {
        let root = temp_root();
        let config = root.join("config.toml");
        let first = materialize_for_config(&config).expect("materialize companion");
        assert!(first.refreshed);
        assert!(first.directory.join("manifest.json").is_file());

        let second = materialize_for_config(&config).expect("reuse stable companion");
        assert!(!second.refreshed);
        assert_eq!(first.directory, second.directory);

        fs::write(first.directory.join("popup.js"), b"tampered").expect("tamper popup");
        let third = materialize_for_config(&config).expect("refresh companion");
        assert!(third.refreshed);
        assert_eq!(
            fs::read(first.directory.join("popup.js")).expect("read refreshed popup"),
            RUNTIME_FILES
                .iter()
                .find(|(name, _)| *name == "popup.js")
                .map(|(_, bytes)| bytes.to_vec())
                .expect("embedded popup")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn materialization_removes_stale_runtime_files_without_replacing_root() {
        let root = temp_root();
        let config = root.join("config.toml");
        let first = materialize_for_config(&config).expect("materialize companion");
        let stale = first.directory.join("old-runtime.js");
        fs::write(&stale, b"old").expect("write stale file");
        fs::remove_file(first.directory.join(FINGERPRINT_FILE_NAME)).expect("remove fingerprint");

        let second = materialize_for_config(&config).expect("refresh companion");
        assert_eq!(first.directory, second.directory);
        assert!(!stale.exists());

        let _ = fs::remove_dir_all(root);
    }
}
