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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatExecutionProfile {
    pub model_key: String,
    pub model_label: String,
    pub reasoning_effort: ReasoningEffort,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChatLaunch {
    pub workspace_id: WorkspaceId,
    pub purpose: ManagedChatPurpose,
    pub execution_profile: ChatExecutionProfile,
    pub opening_message: String,
    pub task_marker: String,
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
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedChatCommandState {
    Queued,
    Leased,
    NeedsReconcile,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<ManagedChatLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ManagedChatTerminalResult>,
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
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.details.as_deref())
                .is_some_and(|details| details.len() > super::MAX_MANAGED_CHAT_DETAIL_BYTES)
            {
                return Err("managed chat terminal details exceed configured size limit".into());
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
                ManagedChatCommandState::NeedsReconcile => {
                    if command.lease.is_some() || command.terminal.is_some() {
                        return Err(
                            "reconcile managed chat command has invalid lease/terminal state"
                                .into(),
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
