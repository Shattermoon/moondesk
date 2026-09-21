use crate::workspaces::WorkspaceId;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            #[allow(dead_code)]
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

uuid_id!(WorkerFamilyId);
uuid_id!(WorkerId);
uuid_id!(TaskId);
uuid_id!(OperationId);
uuid_id!(WorkerMessageId);
uuid_id!(WorkerReportId);

fn digest_identity(label: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"moondesk-worker-identity-v1\0");
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatIdentity {
    pub session_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
}

impl ChatIdentity {
    pub fn session_digest_for(session: &str) -> String {
        digest_identity("session", session)
    }

    pub fn from_openai_meta(subject: Option<&str>, session: &str) -> Self {
        Self {
            session_digest: Self::session_digest_for(session),
            subject_digest: subject.map(|value| digest_identity("subject", value)),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        fn valid_digest(value: &str) -> bool {
            value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }

        if !valid_digest(&self.session_digest) {
            return Err("worker session digest is invalid".into());
        }
        if self
            .subject_digest
            .as_deref()
            .is_some_and(|value| !valid_digest(value))
        {
            return Err("worker subject digest is invalid".into());
        }
        Ok(())
    }
}

pub type WorkerExecutionProfile = crate::managed_chat::types::ChatExecutionProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Provisioning,
    Waking,
    Idle,
    Running,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAttachmentState {
    Absent,
    Opening,
    Attached,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLaunchState {
    #[default]
    Unknown,
    Queued,
    Preparing,
    SendStarted,
    Reconciling,
    WaitingClaim,
    Paused,
    Failed,
    Claimed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running,
    Completed,
    Failed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerResult {
    pub result: String,
    pub changes: String,
    pub validation: String,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerTask {
    pub id: TaskId,
    pub assignment: String,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<WorkerResult>,
    #[serde(default)]
    pub collected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMessageState {
    Pending,
    Acknowledged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerMessage {
    pub id: WorkerMessageId,
    pub body: String,
    pub state: WorkerMessageState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerReport {
    pub id: WorkerReportId,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub body: String,
    #[serde(default)]
    pub collected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRecord {
    pub id: WorkerId,
    pub display_id: String,
    pub label: String,
    pub state: WorkerState,
    pub attachment_state: BrowserAttachmentState,
    pub execution_profile: WorkerExecutionProfile,
    #[serde(default)]
    pub launch_state: WorkerLaunchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_identity: Option<ChatIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task_id: Option<TaskId>,
    #[serde(default)]
    pub tasks: BTreeMap<TaskId, WorkerTask>,
    #[serde(default)]
    pub messages: Vec<WorkerMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnReceipt {
    pub request_fingerprint: String,
    pub family_id: WorkerFamilyId,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub display_id: String,
    pub claim_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReuseReceipt {
    pub request_fingerprint: String,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub display_id: String,
    pub execution_profile: WorkerExecutionProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReceipt {
    pub request_fingerprint: String,
    pub worker_id: WorkerId,
    pub message_id: WorkerMessageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportReceipt {
    pub request_fingerprint: String,
    pub worker_id: WorkerId,
    pub report_id: WorkerReportId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerFamily {
    pub id: WorkerFamilyId,
    pub workspace_id: WorkspaceId,
    pub anchor_identity: ChatIdentity,
    #[serde(default)]
    pub workers: BTreeMap<WorkerId, WorkerRecord>,
    #[serde(default)]
    pub reports: Vec<WorkerReport>,
    #[serde(default)]
    pub spawn_requests: BTreeMap<OperationId, SpawnReceipt>,
    #[serde(default)]
    pub reuse_requests: BTreeMap<OperationId, ReuseReceipt>,
    #[serde(default)]
    pub message_requests: BTreeMap<OperationId, MessageReceipt>,
    #[serde(default)]
    pub report_requests: BTreeMap<OperationId, ReportReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerStoreData {
    pub schema_version: u32,
    #[serde(default)]
    pub families: BTreeMap<WorkerFamilyId, WorkerFamily>,
}

impl Default for WorkerStoreData {
    fn default() -> Self {
        Self {
            schema_version: super::WORKER_STORE_SCHEMA_VERSION,
            families: BTreeMap::new(),
        }
    }
}

impl WorkerStoreData {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != super::WORKER_STORE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported worker store schema version: {}",
                self.schema_version
            ));
        }

        for (family_id, family) in &self.families {
            if family_id != &family.id {
                return Err("worker family map key does not match family id".into());
            }
            family.anchor_identity.validate()?;
            if family.workers.len() > super::MAX_WORKER_RECORDS_PER_FAMILY {
                return Err("worker family exceeds configured worker record limit".into());
            }
            if family
                .workers
                .values()
                .filter(|worker| worker.state != WorkerState::Retired)
                .count()
                > super::MAX_WORKERS_PER_FAMILY
            {
                return Err("worker family exceeds configured active worker limit".into());
            }
            for report in &family.reports {
                if report.body.len() > super::MAX_WORKER_MESSAGE_BYTES {
                    return Err("worker report exceeds configured size limit".into());
                }
                if !family.workers.contains_key(&report.worker_id) {
                    return Err("worker report references a missing worker".into());
                }
            }

            for (worker_id, worker) in &family.workers {
                if worker_id != &worker.id {
                    return Err("worker map key does not match worker id".into());
                }
                if worker.label.trim().is_empty() || worker.label.len() > 128 {
                    return Err("worker label is invalid".into());
                }
                worker.execution_profile.validate()?;
                if let Some(identity) = &worker.chat_identity {
                    identity.validate()?;
                }
                if worker.messages.len() > super::MAX_PENDING_MESSAGES_PER_WORKER {
                    return Err("worker message queue exceeds configured limit".into());
                }
                if let Some(current_task_id) = &worker.current_task_id
                    && !worker.tasks.contains_key(current_task_id)
                {
                    return Err("worker current task is missing from task map".into());
                }
                for (task_id, task) in &worker.tasks {
                    if task_id != &task.id {
                        return Err("worker task map key does not match task id".into());
                    }
                    if task.assignment.len() > super::MAX_WORKER_ASSIGNMENT_BYTES {
                        return Err("worker assignment exceeds configured size limit".into());
                    }
                }
                for message in &worker.messages {
                    if message.body.len() > super::MAX_WORKER_MESSAGE_BYTES {
                        return Err("worker message exceeds configured size limit".into());
                    }
                }
            }
        }
        Ok(())
    }
}
