use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

const COMPANION_DIR_NAME: &str = "worker-companion";
const FINGERPRINT_FILE_NAME: &str = ".moondesk-companion-source";
pub const COMPANION_BOOTSTRAP_FILE_NAME: &str = "moondesk-bootstrap.json";

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
    replace_file_atomic_using(target, bytes, replace_published_file)
}

fn replace_file_atomic_using<F>(target: &Path, bytes: &[u8], replacer: F) -> io::Result<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let parent = target.parent().ok_or_else(|| {
        io::Error::other("failed to resolve Worker Companion file parent directory")
    })?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("Worker Companion file name is invalid"))?;
    let temporary = parent.join(format!(".{file_name}.moondesk-next-{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = file
        .write_all(bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);

    let result = replacer(&temporary, target);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_published_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn replace_published_file(temporary: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temporary = wide_path(temporary);
    let target = wide_path(target);
    let replaced = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

fn metadata_is_redirected(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

fn ensure_safe_directory(config_path: &Path) -> io::Result<PathBuf> {
    let parent = config_path.parent().ok_or_else(|| {
        io::Error::other("failed to resolve MoonDesk data directory for Worker Companion")
    })?;
    fs::create_dir_all(parent)?;
    let directory = directory_for_config(config_path)?;

    match fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            if metadata_is_redirected(&metadata) {
                return Err(io::Error::other(
                    "Worker Companion folder must not be a symlink, junction, or reparse point",
                ));
            }
            if !metadata.is_dir() {
                return Err(io::Error::other(
                    "Worker Companion path exists but is not a directory",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&directory)?,
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(&directory)?;
    if metadata_is_redirected(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(
            "Worker Companion folder changed to an unsafe filesystem target",
        ));
    }
    let canonical_parent = fs::canonicalize(parent)?;
    let canonical_directory = fs::canonicalize(&directory)?;
    if canonical_directory.parent() != Some(canonical_parent.as_path())
        || canonical_directory
            .file_name()
            .and_then(|name| name.to_str())
            != Some(COMPANION_DIR_NAME)
    {
        return Err(io::Error::other(
            "Worker Companion folder resolves outside the MoonDesk data directory",
        ));
    }
    Ok(directory)
}

fn bootstrap_document(pairing_token: &str) -> io::Result<Vec<u8>> {
    if pairing_token.len() != 64 || !pairing_token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "Worker Companion bootstrap credential is invalid",
        ));
    }
    Ok(format!(r#"{{"pairingToken":"{pairing_token}"}}"#).into_bytes())
}

fn cleanup_owned_temporary_files(directory: &Path) -> io::Result<()> {
    let owned_names = RUNTIME_FILES
        .iter()
        .map(|(name, _)| *name)
        .chain([FINGERPRINT_FILE_NAME, COMPANION_BOOTSTRAP_FILE_NAME]);
    let prefixes = owned_names
        .map(|name| format!(".{name}.moondesk-next-"))
        .collect::<Vec<_>>();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

pub fn materialize_for_config(
    config_path: &Path,
    pairing_token: &str,
) -> io::Result<CompanionInstall> {
    let directory = ensure_safe_directory(config_path)?;
    let bootstrap = bootstrap_document(pairing_token)?;
    let bootstrap_path = directory.join(COMPANION_BOOTSTRAP_FILE_NAME);
    if fs::read(&bootstrap_path).ok().as_deref() != Some(bootstrap.as_slice()) {
        replace_file_atomic(&bootstrap_path, &bootstrap)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&bootstrap_path, fs::Permissions::from_mode(0o600))?;
        }
    }

    let fingerprint = embedded_fingerprint();
    if published_tree_matches(&directory)
        && read_fingerprint(&directory).as_deref() == Some(fingerprint.as_str())
    {
        cleanup_owned_temporary_files(&directory)?;
        return Ok(CompanionInstall { directory });
    }

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
    cleanup_owned_temporary_files(&directory)?;

    if !published_tree_matches(&directory) {
        return Err(io::Error::other(
            "Worker Companion materialization did not produce a complete extension",
        ));
    }

    Ok(CompanionInstall { directory })
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

    fn temp_root() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("moondesk-companion-install-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }

    fn pairing_token() -> String {
        "a".repeat(64)
    }

    #[test]
    fn startup_sync_is_stable_and_refreshes_existing_files() {
        let root = temp_root();
        let config = root.join("config.toml");
        let token = pairing_token();
        let first = materialize_for_config(&config, &token).expect("materialize companion");
        assert!(first.directory.join("manifest.json").is_file());
        assert_eq!(
            fs::read_to_string(first.directory.join(COMPANION_BOOTSTRAP_FILE_NAME))
                .expect("read bootstrap"),
            format!(r#"{{"pairingToken":"{token}"}}"#)
        );

        let second = materialize_for_config(&config, &token).expect("reuse stable companion");
        assert_eq!(first.directory, second.directory);

        fs::write(first.directory.join("popup.js"), b"tampered").expect("tamper popup");
        let third = materialize_for_config(&config, &token).expect("refresh companion");
        assert_eq!(first.directory, third.directory);
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
    fn materialization_preserves_unrelated_files_and_cleans_only_owned_temps() {
        let root = temp_root();
        let config = root.join("config.toml");
        let token = pairing_token();
        let first = materialize_for_config(&config, &token).expect("materialize companion");
        let sentinel = first.directory.join("user-sentinel.txt");
        let user_temp = first.directory.join(".notes.moondesk-next-user-owned");
        let stale_temp = first.directory.join(".popup.js.moondesk-next-abandoned");
        fs::write(&sentinel, b"keep me").expect("write unrelated sentinel");
        fs::write(&user_temp, b"keep this too").expect("write unrelated temp-looking file");
        fs::write(&stale_temp, b"old temp").expect("write stale owned temp");
        fs::remove_file(first.directory.join(FINGERPRINT_FILE_NAME)).expect("remove fingerprint");

        let second = materialize_for_config(&config, &token).expect("refresh companion");
        assert_eq!(first.directory, second.directory);
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), b"keep me");
        assert_eq!(
            fs::read(&user_temp).expect("read unrelated temp-looking file"),
            b"keep this too"
        );
        assert!(!stale_temp.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_atomic_replacement_preserves_the_previous_published_file() {
        let root = temp_root();
        let target = root.join("background.js");
        fs::write(&target, b"old bytes").expect("write old target");

        let error = replace_file_atomic_using(&target, b"new bytes", |_temporary, _target| {
            Err(io::Error::other("injected replacement failure"))
        })
        .expect_err("replacement failure must propagate");
        assert!(error.to_string().contains("injected replacement failure"));
        assert_eq!(
            fs::read(&target).expect("read preserved target"),
            b"old bytes"
        );
        assert!(
            fs::read_dir(&root)
                .expect("read temp root")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".moondesk-next-"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replacement_temp_file_is_private_before_secret_bytes_are_published() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_root();
        let target = root.join(COMPANION_BOOTSTRAP_FILE_NAME);
        replace_file_atomic_using(
            &target,
            br#"{"pairingToken":"secret"}"#,
            |temporary, target| {
                let mode = fs::metadata(temporary)?.permissions().mode() & 0o777;
                if mode != 0o600 {
                    return Err(io::Error::other(format!(
                        "temporary bootstrap mode was {mode:o}, expected 600"
                    )));
                }
                fs::rename(temporary, target)
            },
        )
        .expect("publish private bootstrap temp");
        assert_eq!(
            fs::metadata(&target)
                .expect("bootstrap metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn materialization_rejects_redirected_companion_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let target = root.join("elsewhere");
        fs::create_dir_all(&target).expect("create redirect target");
        let sentinel = target.join("sentinel.txt");
        fs::write(&sentinel, b"keep me").expect("write target sentinel");
        symlink(&target, root.join(COMPANION_DIR_NAME)).expect("create companion symlink");

        let error = materialize_for_config(&root.join("config.toml"), &pairing_token())
            .expect_err("redirected companion path must be rejected");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), b"keep me");

        let _ = fs::remove_file(root.join(COMPANION_DIR_NAME));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn materialization_rejects_redirected_companion_junction_without_touching_target() {
        let root = temp_root();
        let target = root.join("elsewhere");
        let link = root.join(COMPANION_DIR_NAME);
        fs::create_dir_all(&target).expect("create junction target");
        let sentinel = target.join("sentinel.txt");
        fs::write(&sentinel, b"keep me").expect("write target sentinel");
        let output = Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .expect("run mklink junction command");
        assert!(
            output.status.success(),
            "failed to create test junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let error = materialize_for_config(&root.join("config.toml"), &pairing_token())
            .expect_err("redirected companion path must be rejected");
        assert!(error.to_string().contains("junction") || error.to_string().contains("unsafe"));
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), b"keep me");

        let _ = fs::remove_dir(&link);
        let _ = fs::remove_dir_all(root);
    }
}
