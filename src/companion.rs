use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use uuid::Uuid;

pub const COMPANION_AUTH_SCHEMA_VERSION: u32 = 1;
pub const COMPANION_AUTH_FILE_NAME: &str = "companion-auth-v1.json";
const MAX_COMPANION_AUTH_BYTES: u64 = 16 * 1024;
const MAX_COMPANION_CLIENT_ID_BYTES: usize = 128;

#[cfg(windows)]
const WINDOWS_STORE_RETRY_DELAYS: [std::time::Duration; 5] = [
    std::time::Duration::from_millis(25),
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(100),
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(400),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompanionAuthState {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
}

impl Default for CompanionAuthState {
    fn default() -> Self {
        Self {
            schema_version: COMPANION_AUTH_SCHEMA_VERSION,
            credential_hash: None,
            client_id: None,
        }
    }
}

impl CompanionAuthState {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != COMPANION_AUTH_SCHEMA_VERSION {
            return Err(format!(
                "unsupported companion auth schema version: {}",
                self.schema_version
            ));
        }
        if self.credential_hash.is_some() != self.client_id.is_some() {
            return Err("companion auth credential/client binding is incomplete".into());
        }
        if let Some(hash) = &self.credential_hash
            && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("companion credential hash is invalid".into());
        }
        if let Some(client_id) = &self.client_id
            && (client_id.trim().is_empty() || client_id.len() > MAX_COMPANION_CLIENT_ID_BYTES)
        {
            return Err("companion client id is invalid".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionPairReceipt {
    pub client_id: String,
    pub credential: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompanionAutoPairError {
    Invalid(String),
    Conflict(String),
    Storage(String),
}

impl std::fmt::Display for CompanionAutoPairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Conflict(message) | Self::Storage(message) => {
                formatter.write_str(message)
            }
        }
    }
}

pub struct CompanionAuth {
    path: PathBuf,
    state: Mutex<CompanionAuthState>,
    pairing_token: RwLock<String>,
}

impl CompanionAuth {
    pub fn open_for_config(config_path: &Path) -> std::io::Result<Self> {
        let parent = config_path.parent().ok_or_else(|| {
            std::io::Error::other("failed to resolve MoonDesk data directory for companion auth")
        })?;
        Self::open(parent.join(COMPANION_AUTH_FILE_NAME))
    }

    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let state = load_state(&path)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
            pairing_token: RwLock::new(random_secret()),
        })
    }

    pub fn pairing_token(&self) -> String {
        self.pairing_token
            .read()
            .map(|token| token.clone())
            .unwrap_or_default()
    }

    pub async fn pair(
        &self,
        supplied_pairing_token: &str,
        client_id: &str,
    ) -> Result<CompanionPairReceipt, String> {
        validate_client_id(client_id)?;
        let current_pairing_token = self.pairing_token();
        if current_pairing_token.is_empty()
            || !constant_time_equal(
                current_pairing_token.as_bytes(),
                supplied_pairing_token.as_bytes(),
            )
        {
            return Err("invalid companion pairing token".into());
        }

        let credential = random_secret();
        let mut guard = self.state.lock().await;
        let mut candidate = guard.clone();
        candidate.credential_hash = Some(secret_hash(&credential));
        candidate.client_id = Some(client_id.to_string());
        candidate.validate()?;

        let path = self.path.clone();
        let persisted = candidate.clone();
        tokio::task::spawn_blocking(move || save_state(&path, &persisted))
            .await
            .map_err(|error| format!("companion auth persistence task failed: {error}"))?
            .map_err(|error| format!("failed to persist companion auth: {error}"))?;
        *guard = candidate;
        drop(guard);

        if let Ok(mut pairing_token) = self.pairing_token.write() {
            *pairing_token = random_secret();
        }

        Ok(CompanionPairReceipt {
            client_id: client_id.to_string(),
            credential,
        })
    }

    pub async fn auto_pair(
        &self,
        client_id: &str,
        credential: &str,
    ) -> Result<CompanionPairReceipt, CompanionAutoPairError> {
        validate_client_id(client_id).map_err(CompanionAutoPairError::Invalid)?;
        validate_credential(credential).map_err(CompanionAutoPairError::Invalid)?;

        let actual_hash = secret_hash(credential);
        let mut guard = self.state.lock().await;
        if let Some(existing_client_id) = guard.client_id.as_deref() {
            if existing_client_id != client_id {
                return Err(CompanionAutoPairError::Conflict(
                    "MoonDesk companion is already paired to another browser installation".into(),
                ));
            }
            let Some(expected_hash) = guard.credential_hash.as_deref() else {
                return Err(CompanionAutoPairError::Conflict(
                    "MoonDesk companion credential binding is incomplete".into(),
                ));
            };
            if !constant_time_equal(expected_hash.as_bytes(), actual_hash.as_bytes()) {
                return Err(CompanionAutoPairError::Conflict(
                    "MoonDesk companion credential no longer matches this browser installation"
                        .into(),
                ));
            }
            return Ok(CompanionPairReceipt {
                client_id: client_id.to_string(),
                credential: credential.to_string(),
            });
        }

        let mut candidate = guard.clone();
        candidate.credential_hash = Some(actual_hash);
        candidate.client_id = Some(client_id.to_string());
        candidate
            .validate()
            .map_err(CompanionAutoPairError::Invalid)?;

        let path = self.path.clone();
        let persisted = candidate.clone();
        tokio::task::spawn_blocking(move || save_state(&path, &persisted))
            .await
            .map_err(|error| {
                CompanionAutoPairError::Storage(format!(
                    "companion auth persistence task failed: {error}"
                ))
            })?
            .map_err(|error| {
                CompanionAutoPairError::Storage(format!(
                    "failed to persist companion auth: {error}"
                ))
            })?;
        *guard = candidate;

        Ok(CompanionPairReceipt {
            client_id: client_id.to_string(),
            credential: credential.to_string(),
        })
    }

    pub async fn paired_client_id(&self) -> Option<String> {
        self.state.lock().await.client_id.clone()
    }

    pub async fn authorize(&self, credential: &str) -> Option<String> {
        let guard = self.state.lock().await;
        let expected = guard.credential_hash.as_deref()?;
        let actual = secret_hash(credential);
        if !constant_time_equal(expected.as_bytes(), actual.as_bytes()) {
            return None;
        }
        guard.client_id.clone()
    }
}

fn validate_client_id(client_id: &str) -> Result<(), String> {
    if client_id.trim().is_empty() || client_id.len() > MAX_COMPANION_CLIENT_ID_BYTES {
        return Err(format!(
            "companion client id must contain 1..={MAX_COMPANION_CLIENT_ID_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_credential(credential: &str) -> Result<(), String> {
    if credential.len() != 64 || !credential.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("companion credential must be exactly 64 hexadecimal characters".into());
    }
    Ok(())
}

fn random_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn secret_hash(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn load_state(path: &Path) -> std::io::Result<CompanionAuthState> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CompanionAuthState::default());
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "companion auth path is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_COMPANION_AUTH_BYTES {
        return Err(std::io::Error::other(
            "companion auth state exceeds safety limit",
        ));
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_COMPANION_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_COMPANION_AUTH_BYTES {
        return Err(std::io::Error::other(
            "companion auth state exceeds safety limit",
        ));
    }
    let state = serde_json::from_slice::<CompanionAuthState>(&bytes).map_err(|error| {
        std::io::Error::other(format!("failed to parse companion auth state: {error}"))
    })?;
    state.validate().map_err(std::io::Error::other)?;
    Ok(state)
}

fn save_state(path: &Path, state: &CompanionAuthState) -> std::io::Result<()> {
    state.validate().map_err(std::io::Error::other)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
        std::io::Error::other(format!("failed to serialize companion auth state: {error}"))
    })?;
    if bytes.len() as u64 > MAX_COMPANION_AUTH_BYTES {
        return Err(std::io::Error::other(
            "companion auth state exceeds safety limit",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::other("failed to resolve companion auth parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(
        ".{COMPANION_AUTH_FILE_NAME}.{}.tmp",
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

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn pairing_persists_only_hash_and_survives_restart() {
        let root = temp_root("moondesk-companion-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let pairing = auth.pairing_token();
        let receipt = auth
            .pair(&pairing, "extension-install-a")
            .await
            .expect("pair companion");
        assert_ne!(
            pairing,
            auth.pairing_token(),
            "pair token rotates after use"
        );
        assert_eq!(
            auth.authorize(&receipt.credential).await.as_deref(),
            Some("extension-install-a")
        );
        let persisted = fs::read_to_string(&path).expect("read persisted companion auth");
        assert!(!persisted.contains(&receipt.credential));
        assert!(!persisted.contains(&pairing));

        drop(auth);
        let reopened = CompanionAuth::open(&path).expect("reopen companion auth");
        assert_eq!(
            reopened.authorize(&receipt.credential).await.as_deref(),
            Some("extension-install-a")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn automatic_pairing_is_idempotent_for_one_installation_and_rejects_takeover() {
        let root = temp_root("moondesk-companion-auto-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let credential = "a".repeat(64);

        let first = auth
            .auto_pair("extension-install-a", &credential)
            .await
            .expect("auto pair companion");
        assert_eq!(first.client_id, "extension-install-a");
        assert_eq!(first.credential, credential);
        assert_eq!(
            auth.authorize(&credential).await.as_deref(),
            Some("extension-install-a")
        );

        let retry = auth
            .auto_pair("extension-install-a", &credential)
            .await
            .expect("auto pair retry is idempotent");
        assert_eq!(retry, first);
        let persisted = fs::read_to_string(&path).expect("read persisted companion auth");
        assert!(!persisted.contains(&credential));

        let wrong_credential = auth
            .auto_pair("extension-install-a", &"b".repeat(64))
            .await
            .expect_err("same install cannot silently rotate credential");
        assert!(matches!(
            wrong_credential,
            CompanionAutoPairError::Conflict(_)
        ));

        let takeover = auth
            .auto_pair("extension-install-b", &"c".repeat(64))
            .await
            .expect_err("different install cannot silently take over");
        assert!(matches!(takeover, CompanionAutoPairError::Conflict(_)));

        drop(auth);
        let reopened = CompanionAuth::open(&path).expect("reopen companion auth");
        assert_eq!(
            reopened.authorize(&credential).await.as_deref(),
            Some("extension-install-a")
        );
        let after_restart = reopened
            .auto_pair("extension-install-a", &credential)
            .await
            .expect("auto pair remains idempotent after restart");
        assert_eq!(after_restart, first);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn invalid_pairing_token_cannot_rotate_existing_credential() {
        let root = temp_root("moondesk-companion-invalid-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let receipt = auth
            .pair(&auth.pairing_token(), "extension-install-a")
            .await
            .expect("pair companion");
        let error = auth
            .pair(&"0".repeat(64), "extension-install-b")
            .await
            .expect_err("wrong token must fail");
        assert!(error.contains("invalid companion pairing token"));
        assert_eq!(
            auth.authorize(&receipt.credential).await.as_deref(),
            Some("extension-install-a")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn successful_repair_revokes_old_credential() {
        let root = temp_root("moondesk-companion-repair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let first = auth
            .pair(&auth.pairing_token(), "extension-install-a")
            .await
            .expect("pair first companion");
        let second = auth
            .pair(&auth.pairing_token(), "extension-install-b")
            .await
            .expect("repair companion");
        assert!(auth.authorize(&first.credential).await.is_none());
        assert_eq!(
            auth.authorize(&second.credential).await.as_deref(),
            Some("extension-install-b")
        );
        let _ = fs::remove_dir_all(root);
    }
}
