pub(crate) mod broker;
mod prompt;
pub(crate) mod protocol;
mod store;
pub(crate) mod types;

use std::path::{Path, PathBuf};

pub const WORKER_STORE_SCHEMA_VERSION: u32 = 1;
pub const WORKER_STORE_FILE_NAME: &str = "worker-state-v1.json";
// Long-running delivered history is compacted aggressively; this remains a hard final guard for
// active queues and exact serialized state, enforced again by WorkerStoreData::validate.
pub const MAX_WORKER_STORE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_WORKERS_PER_FAMILY: usize = 8;
pub const RECOMMENDED_WORKERS_PER_FAMILY: usize = 4;
pub const MAX_WORKER_RECORDS_PER_FAMILY: usize = 64;
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
