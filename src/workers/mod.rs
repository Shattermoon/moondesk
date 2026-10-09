pub(crate) mod broker;
mod prompt;
pub(crate) mod protocol;
mod store;
pub(crate) mod types;

use crate::workspaces::WorkspaceId;
use std::path::{Path, PathBuf};

pub const WORKER_STORE_SCHEMA_VERSION: u32 = 1;
pub const WORKER_STORE_FILE_NAME: &str = "worker-state-v1.json";
// Long-running delivered history is compacted aggressively; this remains a hard final guard for
// active queues and exact serialized state, enforced by store::prepare_save before publication.
pub const MAX_WORKER_STORE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_WORKER_FAMILIES: usize = 64;
pub const MAX_WORKERS_PER_FAMILY: usize = 8;
pub const RECOMMENDED_WORKERS_PER_FAMILY: usize = 4;
pub const MAX_WORKER_RECORDS_PER_FAMILY: usize = 64;
// Serialize the short cross-store worker lifecycle boundaries (recovery, spawn/reuse linking,
// companion command transitions, and destructive maintenance). Browser execution happens after
// these boundaries and is not held behind this lock.
pub(crate) static WORKER_LIFECYCLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub const MAX_WORKER_ASSIGNMENT_BYTES: usize = 64 * 1024;
pub const MAX_WORKER_MESSAGE_BYTES: usize = 32 * 1024;
pub const MAX_WORKER_RESULT_BYTES: usize = 128 * 1024;
pub const MAX_PENDING_MESSAGES_PER_WORKER: usize = 128;
pub const MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY: usize = 256;
pub const MAX_COLLECT_RECEIPTS_PER_FAMILY: usize = 16;
pub const MAX_COLLECTED_TASK_HISTORY_PER_WORKER: usize = 2;
pub const MAX_TASK_RECORDS_PER_WORKER: usize = 8;
pub const MAX_REPORTS_PER_FAMILY: usize = 64;
pub const MAX_UPDATES_PER_COLLECT: usize = 32;

pub(crate) fn store_path_for_config(config_path: &Path) -> std::io::Result<PathBuf> {
    let parent = config_path.parent().ok_or_else(|| {
        std::io::Error::other("failed to resolve MoonDesk data directory for worker state")
    })?;
    Ok(parent.join(WORKER_STORE_FILE_NAME))
}

pub(crate) fn durable_conversation_url_for_legacy_thread(
    worker_store_path: &Path,
    workspace_id: &WorkspaceId,
    thread_key: &str,
) -> std::io::Result<Option<String>> {
    let Some(worker_id) = thread_key.strip_prefix("worker:") else {
        return Ok(None);
    };
    if worker_id.is_empty() {
        return Ok(None);
    }
    let data = store::load(worker_store_path)?;
    Ok(data
        .families
        .values()
        .filter(|family| &family.workspace_id == workspace_id)
        .flat_map(|family| family.workers.values())
        .find(|worker| worker.id.to_string() == worker_id)
        .and_then(|worker| worker.conversation_url.clone()))
}
