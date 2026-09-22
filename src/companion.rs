use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use uuid::Uuid;

pub const COMPANION_AUTH_SCHEMA_VERSION: u32 = 2;
pub const COMPANION_AUTH_FILE_NAME: &str = "companion-auth-v1.json";
pub const MAX_PAIRED_COMPANION_CLIENTS: usize = 8;
pub const COMPANION_PRESENCE_MAX_AGE_MS: u64 = 15_000;
const MAX_COMPANION_AUTH_BYTES: u64 = 32 * 1024;
const MAX_COMPANION_CLIENT_ID_BYTES: usize = 128;
const MAX_COMPANION_ORIGIN_BYTES: usize = 512;
const MAX_COMPANION_PRESENCE_TABS: usize = 32;
const MAX_COMPANION_BROWSER_LABEL_BYTES: usize = 64;
const MAX_CHAT_URL_BYTES: usize = 2048;
const MAX_COMPANION_CORRELATIONS: usize = 4096;
const MAX_COMPANION_ANCHOR_AFFINITIES: usize = 512;
const MAX_COMPANION_CORRELATION_REQUEST_IDS: usize = 32;
const MAX_COMPANION_REQUEST_ID_BYTES: usize = 100;

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
struct CompanionClientCredential {
    credential_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extension_origin: Option<String>,
    #[serde(default)]
    paired_at_ms: u64,
}

impl CompanionClientCredential {
    fn validate(&self) -> Result<(), String> {
        validate_credential_hash(&self.credential_hash)?;
        if let Some(origin) = self.extension_origin.as_deref() {
            validate_extension_origin(origin)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompanionAuthState {
    schema_version: u32,
    #[serde(default)]
    clients: BTreeMap<String, CompanionClientCredential>,
}

impl Default for CompanionAuthState {
    fn default() -> Self {
        Self {
            schema_version: COMPANION_AUTH_SCHEMA_VERSION,
            clients: BTreeMap::new(),
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
        if self.clients.len() > MAX_PAIRED_COMPANION_CLIENTS {
            return Err(format!(
                "companion auth exceeds the paired browser limit of {MAX_PAIRED_COMPANION_CLIENTS}"
            ));
        }
        for (client_id, client) in &self.clients {
            validate_client_id(client_id)?;
            client.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyCompanionAuthState {
    schema_version: u32,
    #[serde(default)]
    credential_hash: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionTabPresence {
    pub conversation_id: String,
    pub conversation_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_url: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub window_focused: bool,
    #[serde(default)]
    pub generating: bool,
}

impl CompanionTabPresence {
    fn validate(&self) -> Result<(), String> {
        validate_conversation_id(&self.conversation_id)?;
        validate_chatgpt_conversation_url(&self.conversation_url, &self.conversation_id)?;
        let url = reqwest::Url::parse(&self.conversation_url)
            .map_err(|_| "companion conversation URL is invalid".to_string())?;
        let url_project = project_id_from_path(url.path());
        match self.project_id.as_deref() {
            Some(project_id) => {
                validate_project_id(project_id)?;
                if url_project.as_deref() != Some(project_id) {
                    return Err(
                        "companion conversation Project id does not match its ChatGPT URL".into(),
                    );
                }
                let project_url = self.project_url.as_deref().ok_or_else(|| {
                    "companion Project conversation is missing a Project entry URL".to_string()
                })?;
                validate_chatgpt_project_url(project_url, project_id)?;
            }
            None => {
                if url_project.is_some() || self.project_url.is_some() {
                    return Err(
                        "companion normal conversation cannot carry ChatGPT Project metadata"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionPresenceUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_label: Option<String>,
    #[serde(default)]
    pub tabs: Vec<CompanionTabPresence>,
}

impl CompanionPresenceUpdate {
    fn validate(&self) -> Result<(), String> {
        if self.tabs.len() > MAX_COMPANION_PRESENCE_TABS {
            return Err(format!(
                "companion presence may report at most {MAX_COMPANION_PRESENCE_TABS} ChatGPT tabs"
            ));
        }
        if self.browser_label.as_deref().is_some_and(|label| {
            label.trim().is_empty() || label.len() > MAX_COMPANION_BROWSER_LABEL_BYTES
        }) {
            return Err("companion browser label is invalid".into());
        }
        let mut conversations = BTreeSet::new();
        for tab in &self.tabs {
            tab.validate()?;
            if !conversations.insert(tab.conversation_id.clone()) {
                return Err("companion presence contains duplicate conversation ids".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionClientPresence {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_label: Option<String>,
    pub last_seen_ms: u64,
    pub tabs: Vec<CompanionTabPresence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionAnchorRoute {
    pub client_id: String,
    pub tab: CompanionTabPresence,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCorrelationUpdate {
    pub conversation_id: String,
    pub conversation_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_url: Option<String>,
    pub request_ids: Vec<String>,
}

impl CompanionCorrelationUpdate {
    fn validate(&self) -> Result<(), String> {
        let tab = CompanionTabPresence {
            conversation_id: self.conversation_id.clone(),
            conversation_url: self.conversation_url.clone(),
            project_id: self.project_id.clone(),
            project_url: self.project_url.clone(),
            active: true,
            window_focused: true,
            generating: false,
        };
        tab.validate()?;
        if self.request_ids.is_empty()
            || self.request_ids.len() > MAX_COMPANION_CORRELATION_REQUEST_IDS
        {
            return Err(format!(
                "companion correlation must contain 1..={MAX_COMPANION_CORRELATION_REQUEST_IDS} request ids"
            ));
        }
        let mut seen = BTreeSet::new();
        for request_id in &self.request_ids {
            let normalized = normalize_request_id(request_id)
                .ok_or_else(|| "companion correlation request id is invalid".to_string())?;
            if normalized != request_id.as_str() {
                return Err("companion correlation request id must already be normalized".into());
            }
            if !seen.insert(request_id) {
                return Err("companion correlation contains duplicate request ids".into());
            }
        }
        Ok(())
    }

    fn route(&self, client_id: &str) -> CompanionAnchorRoute {
        CompanionAnchorRoute {
            client_id: client_id.to_string(),
            tab: CompanionTabPresence {
                conversation_id: self.conversation_id.clone(),
                conversation_url: self.conversation_url.clone(),
                project_id: self.project_id.clone(),
                project_url: self.project_url.clone(),
                active: true,
                window_focused: true,
                generating: false,
            },
        }
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
    Limit(String),
    Storage(String),
}

impl std::fmt::Display for CompanionAutoPairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::Conflict(message)
            | Self::Limit(message)
            | Self::Storage(message) => formatter.write_str(message),
        }
    }
}

pub struct CompanionAuth {
    path: PathBuf,
    state: Mutex<CompanionAuthState>,
    presence: Mutex<BTreeMap<String, CompanionClientPresence>>,
    correlations: Mutex<BTreeMap<String, (CompanionAnchorRoute, u64)>>,
    anchor_affinities: Mutex<BTreeMap<String, (CompanionAnchorRoute, u64)>>,
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
            presence: Mutex::new(BTreeMap::new()),
            correlations: Mutex::new(BTreeMap::new()),
            anchor_affinities: Mutex::new(BTreeMap::new()),
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
        extension_origin: Option<&str>,
    ) -> Result<CompanionPairReceipt, String> {
        validate_client_id(client_id)?;
        if let Some(origin) = extension_origin {
            validate_extension_origin(origin)?;
        }
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
        if !guard.clients.contains_key(client_id)
            && guard.clients.len() >= MAX_PAIRED_COMPANION_CLIENTS
        {
            return Err(format!(
                "MoonDesk already has {MAX_PAIRED_COMPANION_CLIENTS} paired browser installations; revoke a stale browser before pairing another"
            ));
        }
        let mut candidate = guard.clone();
        candidate.clients.insert(
            client_id.to_string(),
            CompanionClientCredential {
                credential_hash: secret_hash(&credential),
                extension_origin: extension_origin.map(str::to_string),
                paired_at_ms: unix_time_ms(),
            },
        );
        candidate.validate()?;
        persist_state(&self.path, &candidate).await?;
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
        extension_origin: Option<&str>,
    ) -> Result<CompanionPairReceipt, CompanionAutoPairError> {
        validate_client_id(client_id).map_err(CompanionAutoPairError::Invalid)?;
        validate_credential(credential).map_err(CompanionAutoPairError::Invalid)?;
        if let Some(origin) = extension_origin {
            validate_extension_origin(origin).map_err(CompanionAutoPairError::Invalid)?;
        }

        let actual_hash = secret_hash(credential);
        let mut guard = self.state.lock().await;
        if let Some(existing) = guard.clients.get(client_id) {
            if !constant_time_equal(existing.credential_hash.as_bytes(), actual_hash.as_bytes()) {
                return Err(CompanionAutoPairError::Conflict(
                    "MoonDesk companion credential no longer matches this browser installation"
                        .into(),
                ));
            }
            if let (Some(expected), Some(actual_origin)) =
                (existing.extension_origin.as_deref(), extension_origin)
                && expected != actual_origin
            {
                return Err(CompanionAutoPairError::Conflict(
                    "MoonDesk companion credential is bound to a different browser extension installation"
                        .into(),
                ));
            }
            if existing.extension_origin.is_none() && extension_origin.is_some() {
                let mut candidate = guard.clone();
                if let Some(client) = candidate.clients.get_mut(client_id) {
                    client.extension_origin = extension_origin.map(str::to_string);
                }
                candidate
                    .validate()
                    .map_err(CompanionAutoPairError::Invalid)?;
                persist_state(&self.path, &candidate)
                    .await
                    .map_err(CompanionAutoPairError::Storage)?;
                *guard = candidate;
            }
            return Ok(CompanionPairReceipt {
                client_id: client_id.to_string(),
                credential: credential.to_string(),
            });
        }

        if guard.clients.len() >= MAX_PAIRED_COMPANION_CLIENTS {
            return Err(CompanionAutoPairError::Limit(format!(
                "MoonDesk already has {MAX_PAIRED_COMPANION_CLIENTS} paired browser installations; revoke a stale browser before pairing another"
            )));
        }

        let mut candidate = guard.clone();
        candidate.clients.insert(
            client_id.to_string(),
            CompanionClientCredential {
                credential_hash: actual_hash,
                extension_origin: extension_origin.map(str::to_string),
                paired_at_ms: unix_time_ms(),
            },
        );
        candidate
            .validate()
            .map_err(CompanionAutoPairError::Invalid)?;
        persist_state(&self.path, &candidate)
            .await
            .map_err(CompanionAutoPairError::Storage)?;
        *guard = candidate;

        Ok(CompanionPairReceipt {
            client_id: client_id.to_string(),
            credential: credential.to_string(),
        })
    }

    pub async fn paired_client_id(&self) -> Option<String> {
        self.state.lock().await.clients.keys().next().cloned()
    }

    pub async fn paired_client_ids(&self) -> Vec<String> {
        self.state.lock().await.clients.keys().cloned().collect()
    }

    pub async fn paired_client_count(&self) -> usize {
        self.state.lock().await.clients.len()
    }

    pub async fn authorize(
        &self,
        credential: &str,
        extension_origin: Option<&str>,
    ) -> Option<String> {
        let actual = secret_hash(credential);
        let mut guard = self.state.lock().await;
        let matched = guard.clients.iter().find_map(|(client_id, client)| {
            constant_time_equal(client.credential_hash.as_bytes(), actual.as_bytes())
                .then(|| (client_id.clone(), client.extension_origin.clone()))
        })?;
        let (client_id, expected_origin) = matched;
        if let (Some(expected), Some(actual_origin)) =
            (expected_origin.as_deref(), extension_origin)
            && expected != actual_origin
        {
            return None;
        }

        if expected_origin.is_none() && extension_origin.is_some() {
            let mut candidate = guard.clone();
            if let Some(client) = candidate.clients.get_mut(&client_id) {
                client.extension_origin = extension_origin.map(str::to_string);
            }
            if candidate.validate().is_err() || persist_state(&self.path, &candidate).await.is_err()
            {
                return None;
            }
            *guard = candidate;
        }
        Some(client_id)
    }

    pub async fn revoke_client(&self, client_id: &str) -> Result<bool, String> {
        validate_client_id(client_id)?;
        let mut guard = self.state.lock().await;
        if !guard.clients.contains_key(client_id) {
            return Ok(false);
        }
        let mut candidate = guard.clone();
        candidate.clients.remove(client_id);
        candidate.validate()?;
        persist_state(&self.path, &candidate).await?;
        *guard = candidate;
        self.presence.lock().await.remove(client_id);
        self.correlations
            .lock()
            .await
            .retain(|_, (route, _)| route.client_id != client_id);
        self.anchor_affinities
            .lock()
            .await
            .retain(|_, (route, _)| route.client_id != client_id);
        Ok(true)
    }

    pub async fn update_presence(
        &self,
        client_id: &str,
        update: CompanionPresenceUpdate,
        now_ms: u64,
    ) -> Result<CompanionClientPresence, String> {
        validate_client_id(client_id)?;
        update.validate()?;
        if !self.state.lock().await.clients.contains_key(client_id) {
            return Err("companion browser is not paired".into());
        }
        let presence = CompanionClientPresence {
            client_id: client_id.to_string(),
            browser_label: update.browser_label,
            last_seen_ms: now_ms,
            tabs: update.tabs,
        };
        self.presence
            .lock()
            .await
            .insert(client_id.to_string(), presence.clone());
        Ok(presence)
    }

    #[cfg(test)]
    pub async fn presence_for(
        &self,
        client_id: &str,
        now_ms: u64,
    ) -> Option<CompanionClientPresence> {
        let guard = self.presence.lock().await;
        let presence = guard.get(client_id)?;
        (now_ms.saturating_sub(presence.last_seen_ms) <= COMPANION_PRESENCE_MAX_AGE_MS)
            .then(|| presence.clone())
    }

    pub async fn fresh_presence(&self, now_ms: u64) -> Vec<CompanionClientPresence> {
        self.presence
            .lock()
            .await
            .values()
            .filter(|presence| {
                now_ms.saturating_sub(presence.last_seen_ms) <= COMPANION_PRESENCE_MAX_AGE_MS
            })
            .cloned()
            .collect()
    }

    pub async fn remember_anchor_affinity(
        &self,
        session_digest: &str,
        route: &CompanionAnchorRoute,
    ) -> Result<(), String> {
        if session_digest.len() != 64
            || !session_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("companion Anchor session digest is invalid".into());
        }
        if !self
            .state
            .lock()
            .await
            .clients
            .contains_key(&route.client_id)
        {
            return Err("companion Anchor browser is not paired".into());
        }
        let mut guard = self.anchor_affinities.lock().await;
        if guard.len() >= MAX_COMPANION_ANCHOR_AFFINITIES
            && !guard.contains_key(session_digest)
            && let Some(oldest) = guard
                .iter()
                .min_by_key(|(_, (_, observed_at))| *observed_at)
                .map(|(digest, _)| digest.clone())
        {
            guard.remove(&oldest);
        }
        guard.insert(session_digest.to_string(), (route.clone(), unix_time_ms()));
        Ok(())
    }

    pub async fn anchor_affinity_for(&self, session_digest: &str) -> Option<CompanionAnchorRoute> {
        let cached = self
            .anchor_affinities
            .lock()
            .await
            .get(session_digest)
            .map(|(route, _)| route.clone())?;
        let fresh = self.fresh_presence(unix_time_ms()).await;
        for client in fresh {
            if client.client_id != cached.client_id {
                continue;
            }
            if let Some(tab) = client
                .tabs
                .into_iter()
                .find(|tab| tab.conversation_id == cached.tab.conversation_id)
            {
                return Some(CompanionAnchorRoute {
                    client_id: client.client_id,
                    tab,
                });
            }
        }
        self.anchor_affinities.lock().await.remove(session_digest);
        None
    }

    pub async fn wait_for_generating_anchor_after(
        &self,
        started_ms: u64,
        timeout: std::time::Duration,
    ) -> Result<Option<CompanionAnchorRoute>, String> {
        let started = tokio::time::Instant::now();
        let settle_after = started + std::time::Duration::from_millis(1_700);
        let deadline = started + timeout;

        loop {
            let mut generating = self
                .fresh_presence(unix_time_ms())
                .await
                .into_iter()
                .filter(|presence| presence.last_seen_ms >= started_ms)
                .flat_map(|client| {
                    client.tabs.into_iter().filter_map(move |tab| {
                        tab.generating.then_some(CompanionAnchorRoute {
                            client_id: client.client_id.clone(),
                            tab,
                        })
                    })
                })
                .collect::<Vec<_>>();
            if generating.len() > 1 {
                return Err(
                    "multiple paired ChatGPT conversations are generating; wait for the other response to finish and retry from the Anchor chat".into(),
                );
            }
            if tokio::time::Instant::now() >= settle_after
                && let Some(route) = generating.pop()
            {
                return Ok(Some(route));
            }

            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        }
    }

    #[cfg(test)]
    pub async fn focused_anchor_route(&self) -> Result<Option<CompanionAnchorRoute>, String> {
        let presence = self.fresh_presence(unix_time_ms()).await;
        let mut candidates = presence
            .into_iter()
            .flat_map(|client| {
                client.tabs.into_iter().filter_map(move |tab| {
                    (tab.active && tab.window_focused).then_some(CompanionAnchorRoute {
                        client_id: client.client_id.clone(),
                        tab,
                    })
                })
            })
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            return Err(
                "multiple paired browsers report a focused ChatGPT Anchor; focus only the Anchor browser and retry".into(),
            );
        }
        Ok(candidates.pop())
    }

    pub async fn observe_correlations(
        &self,
        client_id: &str,
        update: CompanionCorrelationUpdate,
        now_ms: u64,
    ) -> Result<usize, String> {
        validate_client_id(client_id)?;
        update.validate()?;
        if !self.state.lock().await.clients.contains_key(client_id) {
            return Err("companion browser is not paired".into());
        }

        let route = update.route(client_id);
        let mut guard = self.correlations.lock().await;
        let mut stored = 0usize;
        for request_id in update.request_ids {
            if let Some((existing, observed_at)) = guard.get_mut(&request_id) {
                if existing.tab.conversation_id != route.tab.conversation_id {
                    return Err(
                        "companion request id was already proved by a different ChatGPT conversation"
                            .into(),
                    );
                }
                if existing.client_id == route.client_id {
                    *observed_at = (*observed_at).max(now_ms);
                } else if route.tab.window_focused && !existing.tab.window_focused {
                    *existing = route.clone();
                    *observed_at = now_ms;
                }
                continue;
            }

            if guard.len() >= MAX_COMPANION_CORRELATIONS
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, (_, observed_at))| *observed_at)
                    .map(|(request_id, _)| request_id.clone())
            {
                guard.remove(&oldest);
            }
            guard.insert(request_id, (route.clone(), now_ms));
            stored += 1;
        }
        Ok(stored)
    }

    pub async fn correlation_for(&self, request_id: &str) -> Option<CompanionAnchorRoute> {
        let normalized = normalize_request_id(request_id)?;
        self.correlations
            .lock()
            .await
            .get(normalized)
            .map(|(route, _)| route.clone())
    }

    #[cfg(test)]
    pub async fn wait_for_correlation(
        &self,
        request_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<CompanionAnchorRoute>, String> {
        let normalized = normalize_request_id(request_id)
            .ok_or_else(|| "OpenAI request id is invalid".to_string())?
            .to_string();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(route) = self.correlation_for(&normalized).await {
                return Ok(Some(route));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        }
    }
}

pub(crate) fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

async fn persist_state(path: &Path, state: &CompanionAuthState) -> Result<(), String> {
    let path = path.to_path_buf();
    let persisted = state.clone();
    tokio::task::spawn_blocking(move || save_state(&path, &persisted))
        .await
        .map_err(|error| format!("companion auth persistence task failed: {error}"))?
        .map_err(|error| format!("failed to persist companion auth: {error}"))
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

fn validate_credential_hash(hash: &str) -> Result<(), String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("companion credential hash is invalid".into());
    }
    Ok(())
}

pub fn normalize_request_id(value: &str) -> Option<&str> {
    let id = value.split('/').next()?.trim();
    if id.is_empty()
        || id.len() > MAX_COMPANION_REQUEST_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }
    Some(id)
}

pub fn validate_extension_origin(origin: &str) -> Result<(), String> {
    if origin.len() > MAX_COMPANION_ORIGIN_BYTES {
        return Err("companion extension origin is too long".into());
    }
    let url = reqwest::Url::parse(origin)
        .map_err(|_| "companion extension origin is invalid".to_string())?;
    if url.scheme() != "chrome-extension"
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("companion extension origin must be chrome-extension://<extension-id>".into());
    }
    Ok(())
}

fn validate_conversation_id(value: &str) -> Result<(), String> {
    if !(16..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err("companion conversation id is invalid".into());
    }
    Ok(())
}

fn validate_project_id(value: &str) -> Result<(), String> {
    let suffix = value
        .strip_prefix("g-p-")
        .ok_or_else(|| "companion ChatGPT Project id must use the g-p-<32 hex> form".to_string())?;
    if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("companion ChatGPT Project id is invalid".into());
    }
    Ok(())
}

fn project_id_from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/g/")?;
    let first = rest.split('/').next()?;
    let raw = first.get(..36)?;
    validate_project_id(raw).ok()?;
    Some(raw.to_ascii_lowercase())
}

fn validate_chatgpt_conversation_url(value: &str, conversation_id: &str) -> Result<(), String> {
    if value.len() > MAX_CHAT_URL_BYTES {
        return Err("companion conversation URL is too long".into());
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| "companion conversation URL is invalid".to_string())?;
    if url.origin().ascii_serialization() != "https://chatgpt.com" {
        return Err("companion conversation URL must use https://chatgpt.com".into());
    }
    let suffix = format!("/c/{conversation_id}");
    if !(url.path() == suffix || url.path().ends_with(&suffix)) {
        return Err("companion conversation URL does not match conversation id".into());
    }
    Ok(())
}

fn validate_chatgpt_project_url(value: &str, project_id: &str) -> Result<(), String> {
    if value.len() > MAX_CHAT_URL_BYTES {
        return Err("companion Project URL is too long".into());
    }
    let url =
        reqwest::Url::parse(value).map_err(|_| "companion Project URL is invalid".to_string())?;
    if url.origin().ascii_serialization() != "https://chatgpt.com"
        || project_id_from_path(url.path()).as_deref() != Some(project_id)
        || !url.path().ends_with("/project")
    {
        return Err("companion Project URL does not match the exact ChatGPT Project id".into());
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

    let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::other(format!("failed to parse companion auth state: {error}"))
    })?;
    let schema = raw
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;

    let (state, migrated) = if schema == COMPANION_AUTH_SCHEMA_VERSION {
        (
            serde_json::from_value::<CompanionAuthState>(raw).map_err(|error| {
                std::io::Error::other(format!("failed to parse companion auth state: {error}"))
            })?,
            false,
        )
    } else if schema == 1 {
        let legacy = serde_json::from_value::<LegacyCompanionAuthState>(raw).map_err(|error| {
            std::io::Error::other(format!(
                "failed to parse legacy companion auth state: {error}"
            ))
        })?;
        if legacy.schema_version != 1
            || legacy.credential_hash.is_some() != legacy.client_id.is_some()
        {
            return Err(std::io::Error::other(
                "legacy companion auth credential/client binding is invalid",
            ));
        }
        let mut clients = BTreeMap::new();
        if let (Some(client_id), Some(credential_hash)) = (legacy.client_id, legacy.credential_hash)
        {
            validate_client_id(&client_id).map_err(std::io::Error::other)?;
            validate_credential_hash(&credential_hash).map_err(std::io::Error::other)?;
            clients.insert(
                client_id,
                CompanionClientCredential {
                    credential_hash,
                    extension_origin: None,
                    paired_at_ms: 0,
                },
            );
        }
        (
            CompanionAuthState {
                schema_version: COMPANION_AUTH_SCHEMA_VERSION,
                clients,
            },
            true,
        )
    } else {
        return Err(std::io::Error::other(format!(
            "unsupported companion auth schema version: {schema}"
        )));
    };

    state.validate().map_err(std::io::Error::other)?;
    if migrated {
        save_state(path, &state)?;
    }
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

    const ORIGIN_A: &str = "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ORIGIN_B: &str = "chrome-extension://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[tokio::test]
    async fn pairing_persists_only_hash_and_survives_restart() {
        let root = temp_root("moondesk-companion-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let pairing = auth.pairing_token();
        let receipt = auth
            .pair(&pairing, "extension-install-a", Some(ORIGIN_A))
            .await
            .expect("pair companion");
        assert_ne!(
            pairing,
            auth.pairing_token(),
            "pair token rotates after use"
        );
        assert_eq!(
            auth.authorize(&receipt.credential, Some(ORIGIN_A))
                .await
                .as_deref(),
            Some("extension-install-a")
        );
        let persisted = fs::read_to_string(&path).expect("read persisted companion auth");
        assert!(!persisted.contains(&receipt.credential));
        assert!(!persisted.contains(&pairing));

        drop(auth);
        let reopened = CompanionAuth::open(&path).expect("reopen companion auth");
        assert_eq!(
            reopened
                .authorize(&receipt.credential, Some(ORIGIN_A))
                .await
                .as_deref(),
            Some("extension-install-a")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn automatic_pairing_supports_multiple_browser_installations() {
        let root = temp_root("moondesk-companion-auto-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let first_credential = "a".repeat(64);
        let second_credential = "b".repeat(64);

        auth.auto_pair("extension-install-a", &first_credential, Some(ORIGIN_A))
            .await
            .expect("pair first companion");
        auth.auto_pair("extension-install-b", &second_credential, Some(ORIGIN_B))
            .await
            .expect("pair second companion");

        assert_eq!(auth.paired_client_count().await, 2);
        assert_eq!(
            auth.authorize(&first_credential, Some(ORIGIN_A))
                .await
                .as_deref(),
            Some("extension-install-a")
        );
        assert_eq!(
            auth.authorize(&second_credential, Some(ORIGIN_B))
                .await
                .as_deref(),
            Some("extension-install-b")
        );
        assert!(
            auth.authorize(&first_credential, Some(ORIGIN_B))
                .await
                .is_none()
        );

        let retry = auth
            .auto_pair("extension-install-a", &first_credential, Some(ORIGIN_A))
            .await
            .expect("auto pair retry is idempotent");
        assert_eq!(retry.client_id, "extension-install-a");

        let wrong_credential = auth
            .auto_pair("extension-install-a", &"c".repeat(64), Some(ORIGIN_A))
            .await
            .expect_err("same install cannot silently rotate credential");
        assert!(matches!(
            wrong_credential,
            CompanionAutoPairError::Conflict(_)
        ));

        drop(auth);
        let reopened = CompanionAuth::open(&path).expect("reopen companion auth");
        assert_eq!(reopened.paired_client_count().await, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn repair_rotates_only_the_requested_browser() {
        let root = temp_root("moondesk-companion-repair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let first_credential = "a".repeat(64);
        let second_credential = "b".repeat(64);
        auth.auto_pair("extension-install-a", &first_credential, Some(ORIGIN_A))
            .await
            .expect("pair first");
        auth.auto_pair("extension-install-b", &second_credential, Some(ORIGIN_B))
            .await
            .expect("pair second");

        let repaired = auth
            .pair(&auth.pairing_token(), "extension-install-b", Some(ORIGIN_B))
            .await
            .expect("repair second");

        assert_eq!(
            auth.authorize(&first_credential, Some(ORIGIN_A))
                .await
                .as_deref(),
            Some("extension-install-a")
        );
        assert!(
            auth.authorize(&second_credential, Some(ORIGIN_B))
                .await
                .is_none()
        );
        assert_eq!(
            auth.authorize(&repaired.credential, Some(ORIGIN_B))
                .await
                .as_deref(),
            Some("extension-install-b")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_single_browser_auth_migrates_without_losing_credential() {
        let root = temp_root("moondesk-companion-migrate");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        fs::create_dir_all(&root).expect("create root");
        let credential = "a".repeat(64);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 1,
                "credentialHash": secret_hash(&credential),
                "clientId": "legacy-install"
            }))
            .expect("serialize legacy"),
        )
        .expect("write legacy");

        let auth = CompanionAuth::open(&path).expect("open migrated state");
        assert_eq!(
            auth.authorize(&credential, Some(ORIGIN_A)).await.as_deref(),
            Some("legacy-install")
        );
        assert!(auth.authorize(&credential, Some(ORIGIN_B)).await.is_none());

        let persisted = fs::read_to_string(&path).expect("read migrated state");
        assert!(persisted.contains("\"schemaVersion\": 2"));
        assert!(!persisted.contains(&credential));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn presence_is_ephemeral_bounded_and_exact_id_based() {
        let root = temp_root("moondesk-companion-presence");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        auth.auto_pair("extension-install-a", &"a".repeat(64), Some(ORIGIN_A))
            .await
            .expect("pair companion");

        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        let project_id = "g-p-6a97a1377b908191a3bc962c27b10fc3";
        auth.update_presence(
            "extension-install-a",
            CompanionPresenceUpdate {
                browser_label: Some("Edge".into()),
                tabs: vec![CompanionTabPresence {
                    conversation_id: conversation_id.into(),
                    conversation_url: format!(
                        "https://chatgpt.com/g/{project_id}-anything/c/{conversation_id}"
                    ),
                    project_id: Some(project_id.into()),
                    project_url: Some(format!(
                        "https://chatgpt.com/g/{project_id}-anything/project"
                    )),
                    active: true,
                    window_focused: true,
                    generating: false,
                }],
            },
            100,
        )
        .await
        .expect("update presence");

        let presence = auth
            .presence_for("extension-install-a", 101)
            .await
            .expect("fresh presence");
        assert_eq!(presence.tabs[0].project_id.as_deref(), Some(project_id));
        assert!(
            auth.presence_for(
                "extension-install-a",
                100 + COMPANION_PRESENCE_MAX_AGE_MS + 1,
            )
            .await
            .is_none()
        );

        let persisted = fs::read_to_string(&path).expect("read auth state");
        assert!(!persisted.contains(conversation_id));
        assert!(!persisted.contains(project_id));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn request_id_normalization_matches_openai_header_and_page_join_key() {
        assert_eq!(
            normalize_request_id("wfr_exact_request/attempt-7"),
            Some("wfr_exact_request")
        );
        assert_eq!(
            normalize_request_id("wfr_exact_request"),
            Some("wfr_exact_request")
        );
        assert_eq!(normalize_request_id("bad request"), None);
        assert_eq!(normalize_request_id(""), None);
    }

    #[tokio::test]
    async fn request_correlation_is_first_proof_wins_and_not_persisted() {
        let root = temp_root("moondesk-companion-correlation");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        auth.auto_pair("edge-install", &"a".repeat(64), Some(ORIGIN_A))
            .await
            .expect("pair edge");
        auth.auto_pair("chrome-install", &"b".repeat(64), Some(ORIGIN_B))
            .await
            .expect("pair chrome");

        let request_id = "wfr_exact_request";
        let edge_conversation = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        let stored = auth
            .observe_correlations(
                "edge-install",
                CompanionCorrelationUpdate {
                    conversation_id: edge_conversation.into(),
                    conversation_url: format!("https://chatgpt.com/c/{edge_conversation}"),
                    project_id: None,
                    project_url: None,
                    request_ids: vec![request_id.into()],
                },
                100,
            )
            .await
            .expect("store exact correlation");
        assert_eq!(stored, 1);
        let route = auth
            .correlation_for(request_id)
            .await
            .expect("exact correlation");
        assert_eq!(route.client_id, "edge-install");
        assert_eq!(route.tab.conversation_id, edge_conversation);

        let chrome_conversation = "7bbd8fc2-5c21-94ff-a8ce-e09c449975ef";
        let conflict = auth
            .observe_correlations(
                "chrome-install",
                CompanionCorrelationUpdate {
                    conversation_id: chrome_conversation.into(),
                    conversation_url: format!("https://chatgpt.com/c/{chrome_conversation}"),
                    project_id: None,
                    project_url: None,
                    request_ids: vec![request_id.into()],
                },
                200,
            )
            .await
            .expect_err("different conversation cannot steal request id");
        assert!(conflict.contains("different ChatGPT conversation"));
        assert_eq!(
            auth.correlation_for(request_id)
                .await
                .expect("first owner retained")
                .client_id,
            "edge-install"
        );

        let persisted = fs::read_to_string(&path).expect("read auth state");
        assert!(!persisted.contains(request_id));
        assert!(!persisted.contains(edge_conversation));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn wait_for_correlation_accepts_late_exact_page_evidence() {
        let root = temp_root("moondesk-companion-correlation-wait");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = std::sync::Arc::new(CompanionAuth::open(&path).expect("open companion auth"));
        auth.auto_pair("edge-install", &"a".repeat(64), Some(ORIGIN_A))
            .await
            .expect("pair edge");
        let request_id = "wfr_late_request";
        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";

        let writer = auth.clone();
        let request_id_owned = request_id.to_string();
        let conversation_id_owned = conversation_id.to_string();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            writer
                .observe_correlations(
                    "edge-install",
                    CompanionCorrelationUpdate {
                        conversation_id: conversation_id_owned.clone(),
                        conversation_url: format!("https://chatgpt.com/c/{conversation_id_owned}"),
                        project_id: None,
                        project_url: None,
                        request_ids: vec![request_id_owned],
                    },
                    unix_time_ms(),
                )
                .await
                .expect("publish late evidence");
        });

        let route = auth
            .wait_for_correlation(request_id, std::time::Duration::from_secs(1))
            .await
            .expect("wait for exact evidence")
            .expect("late evidence route");
        assert_eq!(route.client_id, "edge-install");
        assert_eq!(route.tab.conversation_id, conversation_id);
        task.await.expect("correlation writer");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn focused_anchor_route_uses_exact_focused_browser_tab() {
        let root = temp_root("moondesk-companion-focused-route");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        auth.auto_pair("chrome-install", &"a".repeat(64), Some(ORIGIN_A))
            .await
            .expect("pair chrome");
        auth.auto_pair("edge-install", &"b".repeat(64), Some(ORIGIN_B))
            .await
            .expect("pair edge");

        let chrome_conversation = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        auth.update_presence(
            "chrome-install",
            CompanionPresenceUpdate {
                browser_label: Some("Chrome".into()),
                tabs: vec![CompanionTabPresence {
                    conversation_id: chrome_conversation.into(),
                    conversation_url: format!("https://chatgpt.com/c/{chrome_conversation}"),
                    project_id: None,
                    project_url: None,
                    active: true,
                    window_focused: false,
                    generating: false,
                }],
            },
            unix_time_ms(),
        )
        .await
        .expect("chrome presence");

        let edge_conversation = "7bbd8fc2-5c21-94ff-a8ce-e09c449975ef";
        auth.update_presence(
            "edge-install",
            CompanionPresenceUpdate {
                browser_label: Some("Edge".into()),
                tabs: vec![CompanionTabPresence {
                    conversation_id: edge_conversation.into(),
                    conversation_url: format!("https://chatgpt.com/c/{edge_conversation}"),
                    project_id: None,
                    project_url: None,
                    active: true,
                    window_focused: true,
                    generating: false,
                }],
            },
            unix_time_ms(),
        )
        .await
        .expect("edge presence");

        let route = auth
            .focused_anchor_route()
            .await
            .expect("resolve focused route")
            .expect("focused route");
        assert_eq!(route.client_id, "edge-install");
        assert_eq!(route.tab.conversation_id, edge_conversation);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn focused_anchor_route_fails_closed_when_multiple_browsers_claim_focus() {
        let root = temp_root("moondesk-companion-ambiguous-route");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        auth.auto_pair("chrome-install", &"a".repeat(64), Some(ORIGIN_A))
            .await
            .expect("pair chrome");
        auth.auto_pair("edge-install", &"b".repeat(64), Some(ORIGIN_B))
            .await
            .expect("pair edge");

        for (client_id, conversation_id) in [
            ("chrome-install", "6aad7eb1-4b10-83ee-97bd-d98b338864de"),
            ("edge-install", "7bbd8fc2-5c21-94ff-a8ce-e09c449975ef"),
        ] {
            auth.update_presence(
                client_id,
                CompanionPresenceUpdate {
                    browser_label: None,
                    tabs: vec![CompanionTabPresence {
                        conversation_id: conversation_id.into(),
                        conversation_url: format!("https://chatgpt.com/c/{conversation_id}"),
                        project_id: None,
                        project_url: None,
                        active: true,
                        window_focused: true,
                        generating: false,
                    }],
                },
                unix_time_ms(),
            )
            .await
            .expect("presence");
        }

        let error = auth
            .focused_anchor_route()
            .await
            .expect_err("ambiguous focused browsers must fail");
        assert!(error.contains("multiple paired browsers"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn invalid_pairing_token_cannot_rotate_existing_browser() {
        let root = temp_root("moondesk-companion-invalid-pair");
        let path = root.join(COMPANION_AUTH_FILE_NAME);
        let auth = CompanionAuth::open(&path).expect("open companion auth");
        let credential = "a".repeat(64);
        auth.auto_pair("extension-install-a", &credential, Some(ORIGIN_A))
            .await
            .expect("pair companion");
        let error = auth
            .pair(&"0".repeat(64), "extension-install-b", Some(ORIGIN_B))
            .await
            .expect_err("wrong token must fail");
        assert!(error.contains("invalid companion pairing token"));
        assert_eq!(
            auth.authorize(&credential, Some(ORIGIN_A)).await.as_deref(),
            Some("extension-install-a")
        );
        let _ = fs::remove_dir_all(root);
    }
}
