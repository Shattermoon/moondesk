use super::types::ManagedChatStoreData;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

const MANAGED_CHAT_STORE_FILE_NAME: &str = "managed-chat-state-v1.json";
const MAX_MANAGED_CHAT_STORE_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(windows)]
const WINDOWS_STORE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];

pub(crate) fn load(path: &Path) -> std::io::Result<ManagedChatStoreData> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagedChatStoreData::default());
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "managed chat state path is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_MANAGED_CHAT_STORE_BYTES {
        return Err(std::io::Error::other(format!(
            "managed chat state exceeds {} byte safety limit",
            MAX_MANAGED_CHAT_STORE_BYTES
        )));
    }

    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MANAGED_CHAT_STORE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_CHAT_STORE_BYTES {
        return Err(std::io::Error::other(format!(
            "managed chat state exceeds {} byte safety limit",
            MAX_MANAGED_CHAT_STORE_BYTES
        )));
    }

    let data = serde_json::from_slice::<ManagedChatStoreData>(&bytes).map_err(|error| {
        std::io::Error::other(format!("failed to parse managed chat state: {error}"))
    })?;
    data.validate().map_err(std::io::Error::other)?;
    Ok(data)
}

pub(crate) fn save(path: &Path, data: &ManagedChatStoreData) -> std::io::Result<()> {
    data.validate().map_err(std::io::Error::other)?;
    let bytes = serde_json::to_vec_pretty(data).map_err(|error| {
        std::io::Error::other(format!("failed to serialize managed chat state: {error}"))
    })?;
    if bytes.len() as u64 > MAX_MANAGED_CHAT_STORE_BYTES {
        return Err(std::io::Error::other(format!(
            "managed chat state exceeds {} byte safety limit",
            MAX_MANAGED_CHAT_STORE_BYTES
        )));
    }

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::other("failed to resolve managed chat state parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(
        ".{MANAGED_CHAT_STORE_FILE_NAME}.{}.tmp",
        Uuid::new_v4()
    ));

    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }

        let mut file = options.open(&temp_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);

        replace_store_file(&temp_path, path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            if let Ok(directory) = fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(not(windows))]
fn replace_store_file(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    fs::rename(temp_path, target_path)
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
fn replace_store_file(temp_path: &Path, target_path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temp_wide = wide_path(temp_path);
    let target_wide = wide_path(target_path);
    let mut delays = WINDOWS_STORE_RETRY_DELAYS.iter();

    loop {
        let committed = unsafe {
            MoveFileExW(
                temp_wide.as_ptr(),
                target_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if committed != 0 {
            return Ok(());
        }

        let error = std::io::Error::last_os_error();
        let retryable = matches!(error.raw_os_error(), Some(5 | 32 | 33));
        let Some(delay) = retryable.then(|| delays.next()).flatten() else {
            return Err(error);
        };
        std::thread::sleep(*delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_chat::MANAGED_CHAT_STORE_SCHEMA_VERSION;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()))
    }

    #[test]
    fn missing_store_loads_empty_versioned_state() {
        let root = temp_root("moondesk-managed-chat-store-missing");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let loaded = load(&path).expect("missing store should load as empty state");
        assert_eq!(loaded.schema_version, MANAGED_CHAT_STORE_SCHEMA_VERSION);
        assert!(loaded.commands.is_empty());
    }

    #[test]
    fn unsupported_schema_fails_closed() {
        let root = temp_root("moondesk-managed-chat-store-schema");
        fs::create_dir_all(&root).expect("create store test root");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        fs::write(&path, br#"{"schemaVersion":999,"commands":{},"dedupe":{}}"#)
            .expect("write unsupported store");
        let error = load(&path).expect_err("unsupported schema must fail closed");
        assert!(error.to_string().contains("managed chat store schema"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_replaces_existing_store_atomically() {
        let root = temp_root("moondesk-managed-chat-store-save");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let data = ManagedChatStoreData::default();
        save(&path, &data).expect("persist initial store");
        save(&path, &data).expect("replace store");
        let loaded = load(&path).expect("reload store");
        assert_eq!(loaded, data);
        let leftovers = fs::read_dir(&root)
            .expect("read store root")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(root);
    }
}
