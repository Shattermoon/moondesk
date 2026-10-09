use super::{
    MANAGED_CHAT_STORE_FILE_NAME, MAX_MANAGED_CHAT_STORE_BYTES, types::ManagedChatStoreData,
};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;
use uuid::Uuid;

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

pub(crate) struct PreparedManagedChatStoreWrite {
    temp_path: PathBuf,
    target_path: PathBuf,
    #[cfg(unix)]
    parent: PathBuf,
}

impl Drop for PreparedManagedChatStoreWrite {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp_path);
    }
}

pub(crate) fn prepare_save(
    path: &Path,
    data: &ManagedChatStoreData,
) -> std::io::Result<PreparedManagedChatStoreWrite> {
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
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options.open(&temp_path)?;
    if let Err(error) = file
        .write_all(&bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(file);

    Ok(PreparedManagedChatStoreWrite {
        temp_path,
        target_path: path.to_path_buf(),
        #[cfg(unix)]
        parent: parent.to_path_buf(),
    })
}

pub(crate) fn commit_prepared(mut prepared: PreparedManagedChatStoreWrite) -> std::io::Result<()> {
    replace_store_file(&prepared.temp_path, &prepared.target_path)?;

    #[cfg(unix)]
    {
        // The temp file is created with 0600, and rename preserves that mode. Directory sync is
        // best-effort after the atomic replacement so no post-commit error can leave disk ahead
        // of the broker's in-memory candidate.
        if let Ok(directory) = fs::File::open(&prepared.parent) {
            let _ = directory.sync_all();
        }
    }

    prepared.temp_path = PathBuf::new();
    Ok(())
}

#[cfg(test)]
pub(crate) fn save(path: &Path, data: &ManagedChatStoreData) -> std::io::Result<()> {
    let prepared = prepare_save(path, data)?;
    commit_prepared(prepared)
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
    fn prepared_write_does_not_publish_before_commit() {
        let root = temp_root("moondesk-managed-chat-store-prepared");
        fs::create_dir_all(&root).expect("create managed chat store test root");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        fs::write(&path, b"old canonical bytes").expect("write old canonical managed chat store");

        let prepared = prepare_save(&path, &ManagedChatStoreData::default())
            .expect("prepare managed chat store");
        assert_eq!(
            fs::read(&path).expect("read old canonical managed chat store"),
            b"old canonical bytes"
        );
        drop(prepared);
        assert_eq!(
            fs::read(&path).expect("read preserved canonical managed chat store"),
            b"old canonical bytes"
        );

        let prepared = prepare_save(&path, &ManagedChatStoreData::default())
            .expect("prepare managed chat store");
        commit_prepared(prepared).expect("commit managed chat store");
        assert_eq!(
            load(&path).expect("load committed managed chat store"),
            ManagedChatStoreData::default()
        );
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
