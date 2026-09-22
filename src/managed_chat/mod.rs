pub(crate) mod broker;
mod store;
pub(crate) mod types;

use std::path::{Path, PathBuf};

pub const MANAGED_CHAT_STORE_SCHEMA_VERSION: u32 = 1;
pub const MANAGED_CHAT_STORE_FILE_NAME: &str = "managed-chat-state-v1.json";
pub const DEFAULT_COMMAND_LEASE_MS: u64 = 120_000;
pub const MAX_MANAGED_CHAT_OPENING_MESSAGE_BYTES: usize = 128 * 1024;
pub const MAX_MANAGED_CHAT_COMMANDS: usize = 256;
pub const MAX_MANAGED_CHAT_DETAIL_BYTES: usize = 4 * 1024;

pub(crate) fn store_path_for_config(config_path: &Path) -> std::io::Result<PathBuf> {
    let parent = config_path.parent().ok_or_else(|| {
        std::io::Error::other("failed to resolve MoonDesk data directory for managed chat state")
    })?;
    Ok(parent.join(MANAGED_CHAT_STORE_FILE_NAME))
}
