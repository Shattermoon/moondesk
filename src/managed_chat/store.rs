use super::{
    MANAGED_CHAT_STORE_FILE_NAME, MAX_MANAGED_CHAT_STORE_BYTES,
    types::{
        ManagedChatCommand, ManagedChatCommandState, ManagedChatLaunch, ManagedChatOpenMode,
        ManagedChatPurpose, ManagedChatStoreData, ManagedChatTerminalResult,
        canonical_chatgpt_conversation_id,
    },
};
use serde::Serialize;
use sha2::{Digest, Sha256};
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueFingerprint<'a> {
    dedupe_key: &'a str,
    launch: &'a ManagedChatLaunch,
}

fn fingerprint_after_launch_migration(command: &ManagedChatCommand) -> std::io::Result<String> {
    let bytes = serde_json::to_vec(&EnqueueFingerprint {
        dedupe_key: &command.dedupe_key,
        launch: &command.launch,
    })
    .map_err(|error| {
        std::io::Error::other(format!(
            "failed to fingerprint migrated managed chat command: {error}"
        ))
    })?;
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    Ok(output)
}

fn legacy_binding_from_history(
    commands: &[ManagedChatCommand],
    command: &ManagedChatCommand,
    worker_store_path: Option<&Path>,
) -> std::io::Result<Option<String>> {
    if let Some(url) = command
        .terminal
        .as_ref()
        .filter(|terminal| terminal.succeeded)
        .and_then(|terminal| terminal.conversation_url.as_ref())
        .filter(|url| canonical_chatgpt_conversation_id(url).is_some())
    {
        return Ok(Some(url.clone()));
    }
    let Some(thread_key) = command.launch.thread_key.as_deref() else {
        return Ok(None);
    };
    let history_url = commands
        .iter()
        .filter(|candidate| {
            candidate.sequence < command.sequence
                && candidate.launch.workspace_id == command.launch.workspace_id
                && candidate.launch.thread_key.as_deref() == Some(thread_key)
                && candidate.state == ManagedChatCommandState::Succeeded
        })
        .filter_map(|candidate| {
            candidate
                .terminal
                .as_ref()
                .filter(|terminal| terminal.succeeded)
                .and_then(|terminal| terminal.conversation_url.as_ref())
                .filter(|url| canonical_chatgpt_conversation_id(url).is_some())
                .map(|url| (candidate.sequence, url.clone()))
        })
        .max_by_key(|(sequence, _)| *sequence)
        .map(|(_, url)| url);
    if history_url.is_some() {
        return Ok(history_url);
    }
    let Some(worker_store_path) = worker_store_path else {
        return Ok(None);
    };
    let durable = crate::workers::durable_conversation_url_for_legacy_thread(
        worker_store_path,
        &command.launch.workspace_id,
        thread_key,
    )?;
    Ok(durable.filter(|url| canonical_chatgpt_conversation_id(url).is_some()))
}

fn migrate_legacy_existing_thread_commands(
    data: &mut ManagedChatStoreData,
    worker_store_path: Option<&Path>,
) -> std::io::Result<bool> {
    let snapshot = data.commands.values().cloned().collect::<Vec<_>>();
    let mut changed = false;
    for command in data.commands.values_mut() {
        if command.launch.purpose != ManagedChatPurpose::Worker
            || command.launch.open_mode != ManagedChatOpenMode::ExistingThread
            || command.launch.existing_conversation_url.is_some()
        {
            continue;
        }
        if let Some(url) = legacy_binding_from_history(&snapshot, command, worker_store_path)? {
            command.launch.existing_conversation_url = Some(url);
            command.request_fingerprint = fingerprint_after_launch_migration(command)?;
            changed = true;
            continue;
        }
        if !matches!(
            command.state,
            ManagedChatCommandState::Paused
                | ManagedChatCommandState::Succeeded
                | ManagedChatCommandState::Failed
        ) {
            command.state = ManagedChatCommandState::Paused;
            command.dispatch_ready = true;
            command.reconcile_history = true;
            command.lease = None;
            command.terminal = Some(ManagedChatTerminalResult {
                succeeded: false,
                details: Some(
                    "Legacy ExistingThread command has no durable canonical conversation URL; retry reuse from Core after MoonDesk starts"
                        .into(),
                ),
                conversation_url: None,
            });
            changed = true;
        }
    }
    Ok(changed)
}

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

    let mut data = serde_json::from_slice::<ManagedChatStoreData>(&bytes).map_err(|error| {
        std::io::Error::other(format!("failed to parse managed chat state: {error}"))
    })?;
    let worker_store_path = path
        .parent()
        .map(|parent| parent.join(crate::workers::WORKER_STORE_FILE_NAME));
    migrate_legacy_existing_thread_commands(&mut data, worker_store_path.as_deref())?;
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
    use crate::managed_chat::types::{
        ChatExecutionProfile, ManagedChatCommand, ManagedChatCommandId, ManagedChatCommandState,
        ManagedChatLaunch, ManagedChatLease, ManagedChatLeaseId, ManagedChatOpenMode,
        ManagedChatPurpose, ManagedChatTerminalResult,
    };
    use crate::workers::broker::{SpawnWorkerRequest, WorkerBroker};
    use crate::workers::types::{ChatIdentity, OperationId, WorkerLaunchState};
    use crate::workspaces::WorkspaceId;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()))
    }

    fn legacy_launch(
        workspace_id: WorkspaceId,
        marker: &str,
        open_mode: ManagedChatOpenMode,
        thread_key: &str,
    ) -> ManagedChatLaunch {
        ManagedChatLaunch {
            workspace_id,
            purpose: ManagedChatPurpose::Worker,
            execution_profile: ChatExecutionProfile::default(),
            opening_message: format!("legacy worker launch {marker}"),
            task_marker: marker.into(),
            thread_key: Some(thread_key.into()),
            open_mode,
            existing_conversation_url: None,
            anchor_session_digest: None,
        }
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
    fn load_migrates_legacy_existing_thread_binding_from_prior_success() {
        let root = temp_root("moondesk-managed-chat-store-legacy-binding");
        fs::create_dir_all(&root).expect("create legacy migration root");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let workspace = WorkspaceId::new();
        let thread_key = format!("worker:{}", Uuid::new_v4());
        let project = "g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk";
        let conversation = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let conversation_url = format!("https://chatgpt.com/g/{project}/c/{conversation}");
        let first_id = ManagedChatCommandId::new();
        let reuse_id = ManagedChatCommandId::new();

        let first = ManagedChatCommand {
            id: first_id.clone(),
            sequence: 0,
            dedupe_key: "legacy-first".into(),
            request_fingerprint: "a".repeat(64),
            launch: legacy_launch(
                workspace.clone(),
                "legacy-first",
                ManagedChatOpenMode::NewThread,
                &thread_key,
            ),
            state: ManagedChatCommandState::Succeeded,
            dispatch_ready: true,
            reconcile_history: true,
            lease: None,
            terminal: Some(ManagedChatTerminalResult {
                succeeded: true,
                details: Some("sent".into()),
                conversation_url: Some(conversation_url.clone()),
            }),
            target_client_id: Some("legacy-browser".into()),
            target_client_pinned_by_request: true,
            anchor_context: None,
        };
        let reuse = ManagedChatCommand {
            id: reuse_id.clone(),
            sequence: 1,
            dedupe_key: "legacy-reuse".into(),
            request_fingerprint: "b".repeat(64),
            launch: legacy_launch(
                workspace,
                "legacy-reuse",
                ManagedChatOpenMode::ExistingThread,
                &thread_key,
            ),
            state: ManagedChatCommandState::Queued,
            dispatch_ready: true,
            reconcile_history: false,
            lease: None,
            terminal: None,
            target_client_id: Some("legacy-browser".into()),
            target_client_pinned_by_request: true,
            anchor_context: None,
        };
        let mut data = ManagedChatStoreData {
            next_sequence: 2,
            ..Default::default()
        };
        data.dedupe
            .insert(first.dedupe_key.clone(), first_id.clone());
        data.dedupe
            .insert(reuse.dedupe_key.clone(), reuse_id.clone());
        data.commands.insert(first_id, first);
        data.commands.insert(reuse_id.clone(), reuse);
        let fixture_bytes =
            serde_json::to_vec_pretty(&data).expect("serialize prior-version fixture");
        fs::write(&path, &fixture_bytes).expect("write prior-version fixture");

        let loaded = load(&path).expect("legacy ExistingThread store must migrate on open");
        let migrated = loaded
            .commands
            .get(&reuse_id)
            .expect("migrated reuse command");
        assert_eq!(
            migrated.launch.existing_conversation_url.as_deref(),
            Some(conversation_url.as_str())
        );
        assert_eq!(migrated.state, ManagedChatCommandState::Queued);
        assert_ne!(migrated.request_fingerprint, "b".repeat(64));
        assert_eq!(
            fs::read(&path).expect("read prior-version bytes after migration"),
            fixture_bytes,
            "load-time migration must not mutate shared durable state before host ownership"
        );
        let reopened = load(&path).expect("migrated state must remain loadable on a later startup");
        assert_eq!(reopened, loaded);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn load_migrates_legacy_existing_thread_from_durable_worker_binding() {
        let root = temp_root("moondesk-managed-chat-store-legacy-worker-binding");
        fs::create_dir_all(&root).expect("create legacy worker migration root");
        let managed_path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let worker_path = root.join(crate::workers::WORKER_STORE_FILE_NAME);
        let workspace = WorkspaceId::new();
        let anchor = ChatIdentity::from_openai_meta(Some("legacy-subject"), "legacy-core");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let receipt = worker
            .spawn_worker(SpawnWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: anchor.clone(),
                label: "legacy worker binding".into(),
                assignment: "preserve durable project conversation".into(),
                context: String::new(),
                execution_profile: ChatExecutionProfile::default(),
            })
            .await
            .expect("spawn durable worker");
        let launch_command_id = Uuid::new_v4().to_string();
        worker
            .link_launch_command(
                &workspace,
                &anchor,
                &receipt.worker_id,
                &receipt.task_id,
                &launch_command_id,
            )
            .await
            .expect("link launch command");
        let project = "g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk";
        let conversation = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let conversation_url = format!("https://chatgpt.com/g/{project}/c/{conversation}");
        worker
            .update_launch_by_command(
                &launch_command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(conversation_url.clone()),
            )
            .await
            .expect("persist durable project conversation");
        drop(worker);

        let command_id = ManagedChatCommandId::new();
        let command = ManagedChatCommand {
            id: command_id.clone(),
            sequence: 0,
            dedupe_key: format!("worker:{}:task:legacy-reuse", receipt.worker_id),
            request_fingerprint: "d".repeat(64),
            launch: legacy_launch(
                workspace,
                "legacy-reuse-from-worker-store",
                ManagedChatOpenMode::ExistingThread,
                &format!("worker:{}", receipt.worker_id),
            ),
            state: ManagedChatCommandState::Queued,
            dispatch_ready: true,
            reconcile_history: false,
            lease: None,
            terminal: None,
            target_client_id: Some("legacy-browser".into()),
            target_client_pinned_by_request: true,
            anchor_context: None,
        };
        let mut data = ManagedChatStoreData {
            next_sequence: 1,
            ..Default::default()
        };
        data.dedupe
            .insert(command.dedupe_key.clone(), command_id.clone());
        data.commands.insert(command_id.clone(), command);
        fs::write(
            &managed_path,
            serde_json::to_vec_pretty(&data).expect("serialize worker-backed fixture"),
        )
        .expect("write worker-backed fixture");

        let loaded = load(&managed_path).expect("legacy command must use durable worker binding");
        let migrated = loaded
            .commands
            .get(&command_id)
            .expect("worker-backed command retained");
        assert_eq!(
            migrated.launch.existing_conversation_url.as_deref(),
            Some(conversation_url.as_str())
        );
        assert_eq!(migrated.state, ManagedChatCommandState::Queued);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_binding_migration_propagates_worker_store_read_failure() {
        let root = temp_root("moondesk-managed-chat-store-legacy-worker-error");
        fs::create_dir_all(&root).expect("create worker error migration root");
        let managed_path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let worker_path = root.join(crate::workers::WORKER_STORE_FILE_NAME);
        let command_id = ManagedChatCommandId::new();
        let command = ManagedChatCommand {
            id: command_id.clone(),
            sequence: 0,
            dedupe_key: "legacy-worker-store-error".into(),
            request_fingerprint: "e".repeat(64),
            launch: legacy_launch(
                WorkspaceId::new(),
                "legacy-worker-store-error",
                ManagedChatOpenMode::ExistingThread,
                &format!("worker:{}", Uuid::new_v4()),
            ),
            state: ManagedChatCommandState::Queued,
            dispatch_ready: true,
            reconcile_history: false,
            lease: None,
            terminal: None,
            target_client_id: Some("legacy-browser".into()),
            target_client_pinned_by_request: true,
            anchor_context: None,
        };
        let mut data = ManagedChatStoreData {
            next_sequence: 1,
            ..Default::default()
        };
        data.dedupe
            .insert(command.dedupe_key.clone(), command_id.clone());
        data.commands.insert(command_id, command);
        fs::write(
            &managed_path,
            serde_json::to_vec_pretty(&data).expect("serialize worker error fixture"),
        )
        .expect("write worker error fixture");
        fs::write(&worker_path, b"{not-valid-worker-json")
            .expect("write corrupt authoritative worker store");

        let error = load(&managed_path).expect_err(
            "authoritative worker-store read failure must not be treated as no binding",
        );
        assert!(error.to_string().contains("failed to parse worker state"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_pauses_unresolvable_legacy_existing_thread_without_resending() {
        let root = temp_root("moondesk-managed-chat-store-legacy-unresolved");
        fs::create_dir_all(&root).expect("create unresolved migration root");
        let path = root.join(MANAGED_CHAT_STORE_FILE_NAME);
        let command_id = ManagedChatCommandId::new();
        let lease_id = ManagedChatLeaseId::new();
        let command = ManagedChatCommand {
            id: command_id.clone(),
            sequence: 0,
            dedupe_key: "legacy-ambiguous-reuse".into(),
            request_fingerprint: "c".repeat(64),
            launch: legacy_launch(
                WorkspaceId::new(),
                "legacy-ambiguous-reuse",
                ManagedChatOpenMode::ExistingThread,
                &format!("worker:{}", Uuid::new_v4()),
            ),
            state: ManagedChatCommandState::SendStarted,
            dispatch_ready: true,
            reconcile_history: true,
            lease: Some(ManagedChatLease {
                lease_id,
                client_id: "legacy-browser".into(),
                expires_at_ms: 123,
            }),
            terminal: None,
            target_client_id: Some("legacy-browser".into()),
            target_client_pinned_by_request: true,
            anchor_context: None,
        };
        let mut data = ManagedChatStoreData {
            next_sequence: 1,
            ..Default::default()
        };
        data.dedupe
            .insert(command.dedupe_key.clone(), command_id.clone());
        data.commands.insert(command_id.clone(), command);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&data).expect("serialize ambiguous prior-version fixture"),
        )
        .expect("write ambiguous prior-version fixture");

        let loaded = load(&path).expect("unresolvable legacy command must not brick startup");
        let migrated = loaded
            .commands
            .get(&command_id)
            .expect("paused legacy command retained");
        assert_eq!(migrated.state, ManagedChatCommandState::Paused);
        assert!(migrated.lease.is_none());
        assert!(migrated.reconcile_history);
        assert!(migrated.launch.existing_conversation_url.is_none());
        assert!(
            migrated
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.details.as_deref())
                .is_some_and(|details| details.contains("retry reuse from Core"))
        );
        let reopened = load(&path).expect("paused legacy state must remain loadable");
        assert_eq!(reopened, loaded);
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
