use crate::workspaces::WorkspaceId;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub fn parse(value: impl AsRef<str>) -> Result<Self, String> {
                let parsed = Uuid::parse_str(value.as_ref().trim())
                    .map_err(|_| format!("invalid {}", stringify!($name)))?;
                Ok(Self(parsed.to_string()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

uuid_id!(ManagedChatCommandId);
uuid_id!(ManagedChatLeaseId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Instant,
    Low,
    Medium,
    High,
    ExtraHigh,
}

impl ReasoningEffort {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Instant => "Instant",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::ExtraHigh => "Extra High",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Instant => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::ExtraHigh,
            Self::ExtraHigh => Self::Instant,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatExecutionProfile {
    pub model_key: String,
    pub model_label: String,
    pub reasoning_effort: ReasoningEffort,
}

impl Default for ChatExecutionProfile {
    fn default() -> Self {
        Self {
            model_key: "gpt-5.6-sol".into(),
            model_label: "GPT-5.6 Sol".into(),
            reasoning_effort: ReasoningEffort::High,
        }
    }
}

impl ChatExecutionProfile {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.model_key.trim().is_empty() || self.model_key.len() > 128 {
            return Err("managed chat model key is invalid".into());
        }
        if self.model_label.trim().is_empty() || self.model_label.len() > 128 {
            return Err("managed chat model label is invalid".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChatPurpose {
    Worker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChatOpenMode {
    #[default]
    NewThread,
    ExistingThread,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatLaunch {
    pub workspace_id: WorkspaceId,
    pub purpose: ManagedChatPurpose,
    pub execution_profile: ChatExecutionProfile,
    pub opening_message: String,
    pub task_marker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
    #[serde(default)]
    pub open_mode: ManagedChatOpenMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_session_digest: Option<String>,
}

impl ManagedChatLaunch {
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.execution_profile.validate()?;
        if self.opening_message.is_empty()
            || self.opening_message.len() > super::MAX_MANAGED_CHAT_OPENING_MESSAGE_BYTES
        {
            return Err(format!(
                "managed chat opening message must contain 1..={} bytes",
                super::MAX_MANAGED_CHAT_OPENING_MESSAGE_BYTES
            ));
        }
        if self.task_marker.trim().is_empty() || self.task_marker.len() > 256 {
            return Err("managed chat task marker is invalid".into());
        }
        if let Some(thread_key) = self.thread_key.as_deref()
            && (thread_key.trim().is_empty() || thread_key.len() > 256)
        {
            return Err("managed chat thread key is invalid".into());
        }
        if self.open_mode == ManagedChatOpenMode::ExistingThread && self.thread_key.is_none() {
            return Err("existing managed chat launch requires a thread key".into());
        }
        if self.anchor_session_digest.as_deref().is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("managed chat anchor session digest is invalid".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatAnchorContext {
    pub conversation_id: String,
    pub conversation_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_url: Option<String>,
}

impl ManagedChatAnchorContext {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.conversation_id.trim().is_empty() || self.conversation_id.len() > 128 {
            return Err("managed chat Anchor conversation id is invalid".into());
        }
        if self.conversation_url.trim().is_empty() || self.conversation_url.len() > 2048 {
            return Err("managed chat Anchor conversation URL is invalid".into());
        }
        if self
            .project_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 128)
        {
            return Err("managed chat Anchor Project id is invalid".into());
        }
        if self
            .project_url
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 2048)
        {
            return Err("managed chat Anchor Project URL is invalid".into());
        }
        if self.project_id.is_some() != self.project_url.is_some() {
            return Err("managed chat Anchor Project metadata is incomplete".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChatCommandState {
    Queued,
    Leased,
    SendStarted,
    NeedsReconcile,
    Paused,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatLease {
    pub lease_id: ManagedChatLeaseId,
    pub client_id: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatTerminalResult {
    pub succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_url: Option<String>,
}

fn default_dispatch_ready() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatCommand {
    pub id: ManagedChatCommandId,
    pub sequence: u64,
    pub dedupe_key: String,
    pub request_fingerprint: String,
    pub launch: ManagedChatLaunch,
    pub state: ManagedChatCommandState,
    #[serde(default = "default_dispatch_ready")]
    pub dispatch_ready: bool,
    #[serde(default)]
    pub reconcile_history: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<ManagedChatLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ManagedChatTerminalResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_context: Option<ManagedChatAnchorContext>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatStoreData {
    pub schema_version: u32,
    #[serde(default)]
    pub next_sequence: u64,
    #[serde(default)]
    pub commands: BTreeMap<ManagedChatCommandId, ManagedChatCommand>,
    #[serde(default)]
    pub dedupe: BTreeMap<String, ManagedChatCommandId>,
}

impl Default for ManagedChatStoreData {
    fn default() -> Self {
        Self {
            schema_version: super::MANAGED_CHAT_STORE_SCHEMA_VERSION,
            next_sequence: 0,
            commands: BTreeMap::new(),
            dedupe: BTreeMap::new(),
        }
    }
}

impl ManagedChatStoreData {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != super::MANAGED_CHAT_STORE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported managed chat store schema version: {}",
                self.schema_version
            ));
        }
        if self.commands.len() > super::MAX_MANAGED_CHAT_COMMANDS {
            return Err("managed chat command store exceeds configured limit".into());
        }
        if self.dedupe.len() != self.commands.len() {
            return Err("managed chat dedupe index does not cover every command".into());
        }
        let mut sequences = BTreeSet::new();
        for (command_id, command) in &self.commands {
            if command.sequence >= self.next_sequence {
                return Err(
                    "managed chat command sequence is outside the store sequence range".into(),
                );
            }
            if !sequences.insert(command.sequence) {
                return Err("managed chat command sequence is duplicated".into());
            }
            if command_id != &command.id {
                return Err("managed chat command map key does not match command id".into());
            }
            command.launch.validate()?;
            if command.dedupe_key.trim().is_empty() || command.dedupe_key.len() > 256 {
                return Err("managed chat dedupe key is invalid".into());
            }
            if command.request_fingerprint.len() != 64
                || !command
                    .request_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("managed chat request fingerprint is invalid".into());
            }
            if let Some(lease) = &command.lease {
                if lease.client_id.trim().is_empty() || lease.client_id.len() > 128 {
                    return Err("managed chat lease client id is invalid".into());
                }
                if lease.expires_at_ms == 0 {
                    return Err("managed chat lease expiry is invalid".into());
                }
            }
            if command
                .target_client_id
                .as_deref()
                .is_some_and(|client_id| client_id.trim().is_empty() || client_id.len() > 128)
            {
                return Err("managed chat target client id is invalid".into());
            }
            if let Some(anchor_context) = command.anchor_context.as_ref() {
                anchor_context.validate()?;
            }
            if command
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.details.as_deref())
                .is_some_and(|details| details.len() > super::MAX_MANAGED_CHAT_DETAIL_BYTES)
            {
                return Err("managed chat terminal details exceed configured size limit".into());
            }
            if command
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.conversation_url.as_deref())
                .is_some_and(|url| url.is_empty() || url.len() > 2048)
            {
                return Err("managed chat terminal conversation URL is invalid".into());
            }
            match command.state {
                ManagedChatCommandState::Queued => {
                    if command.lease.is_some() || command.terminal.is_some() {
                        return Err(
                            "queued managed chat command has invalid lease/terminal state".into(),
                        );
                    }
                }
                ManagedChatCommandState::Leased => {
                    if command.lease.is_none() || command.terminal.is_some() {
                        return Err(
                            "leased managed chat command has invalid lease/terminal state".into(),
                        );
                    }
                }
                ManagedChatCommandState::SendStarted => {
                    if command.lease.is_none()
                        || command.terminal.is_some()
                        || !command.reconcile_history
                    {
                        return Err(
                            "send-started managed chat command has invalid lease/history state"
                                .into(),
                        );
                    }
                }
                ManagedChatCommandState::NeedsReconcile => {
                    if command.lease.is_some()
                        || command.terminal.is_some()
                        || !command.reconcile_history
                    {
                        return Err(
                            "reconcile managed chat command has invalid lease/history state".into(),
                        );
                    }
                }
                ManagedChatCommandState::Paused => {
                    if command.lease.is_some()
                        || command.terminal.is_none()
                        || !command.reconcile_history
                    {
                        return Err(
                            "paused managed chat command has invalid lease/history state".into(),
                        );
                    }
                }
                ManagedChatCommandState::Succeeded | ManagedChatCommandState::Failed => {
                    if command.lease.is_some() || command.terminal.is_none() {
                        return Err(
                            "terminal managed chat command has invalid lease/terminal state".into(),
                        );
                    }
                }
            }
        }
        for (dedupe_key, command_id) in &self.dedupe {
            let command = self.commands.get(command_id).ok_or_else(|| {
                "managed chat dedupe entry references missing command".to_string()
            })?;
            if &command.dedupe_key != dedupe_key {
                return Err("managed chat dedupe entry does not match command".into());
            }
        }
        Ok(())
    }
}
