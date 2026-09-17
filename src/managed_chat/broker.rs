use super::store;
use super::types::{
    ManagedChatCommand, ManagedChatCommandId, ManagedChatCommandState, ManagedChatLaunch,
    ManagedChatLease, ManagedChatLeaseId, ManagedChatStoreData, ManagedChatTerminalResult,
};
use super::{DEFAULT_COMMAND_LEASE_MS, MAX_MANAGED_CHAT_COMMANDS, MAX_MANAGED_CHAT_DETAIL_BYTES};
use crate::workspaces::WorkspaceId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedChatError {
    Invalid(String),
    NotFound,
    Conflict(String),
    Limit(String),
    Storage(String),
}

impl fmt::Display for ManagedChatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::Conflict(message)
            | Self::Limit(message)
            | Self::Storage(message) => formatter.write_str(message),
            Self::NotFound => formatter.write_str("managed chat command was not found"),
        }
    }
}

impl std::error::Error for ManagedChatError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueManagedChatRequest {
    pub dedupe_key: String,
    pub launch: ManagedChatLaunch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedChatLeaseOffer {
    pub command: ManagedChatCommand,
    pub reconcile_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedChatAckOutcome {
    Succeeded { details: Option<String> },
    Failed { details: Option<String> },
    NeedsReconcile,
}

pub struct ManagedChatBroker {
    path: PathBuf,
    data: Mutex<ManagedChatStoreData>,
}

impl ManagedChatBroker {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ManagedChatError> {
        let path = path.into();
        let data = store::load(&path).map_err(storage_error)?;
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    #[cfg(test)]
    pub async fn snapshot(&self) -> ManagedChatStoreData {
        self.data.lock().await.clone()
    }

    pub async fn purge_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<usize, ManagedChatError> {
        let mut guard = self.data.lock().await;
        let removed_ids = guard
            .commands
            .values()
            .filter(|command| &command.launch.workspace_id == workspace_id)
            .map(|command| command.id.clone())
            .collect::<Vec<_>>();
        if removed_ids.is_empty() {
            return Ok(0);
        }

        let mut candidate = guard.clone();
        for command_id in &removed_ids {
            if let Some(command) = candidate.commands.remove(command_id) {
                candidate.dedupe.remove(&command.dedupe_key);
            }
        }
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(removed_ids.len())
    }

    pub async fn enqueue(
        &self,
        request: EnqueueManagedChatRequest,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        validate_enqueue_request(&request)?;
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;
        if let Some(command_id) = guard.dedupe.get(&request.dedupe_key) {
            let command = guard.commands.get(command_id).ok_or_else(|| {
                ManagedChatError::Storage("managed chat dedupe index is corrupt".into())
            })?;
            if command.request_fingerprint == fingerprint {
                return Ok(command.clone());
            }
            return Err(ManagedChatError::Conflict(
                "managed chat dedupe key was reused with different launch input".into(),
            ));
        }
        if guard.commands.len() >= MAX_MANAGED_CHAT_COMMANDS {
            return Err(ManagedChatError::Limit(format!(
                "managed chat command store is limited to {MAX_MANAGED_CHAT_COMMANDS} commands"
            )));
        }

        let mut candidate = guard.clone();
        let sequence = candidate.next_sequence;
        candidate.next_sequence = candidate.next_sequence.checked_add(1).ok_or_else(|| {
            ManagedChatError::Limit("managed chat command sequence is exhausted".into())
        })?;
        let command_id = ManagedChatCommandId::new();
        let command = ManagedChatCommand {
            id: command_id.clone(),
            sequence,
            dedupe_key: request.dedupe_key.clone(),
            request_fingerprint: fingerprint,
            launch: request.launch,
            state: ManagedChatCommandState::Queued,
            reconcile_history: false,
            lease: None,
            terminal: None,
        };
        candidate
            .commands
            .insert(command_id.clone(), command.clone());
        candidate.dedupe.insert(request.dedupe_key, command_id);
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(command)
    }

    pub async fn redeem(
        &self,
        client_id: &str,
        now_ms: u64,
    ) -> Result<Option<ManagedChatLeaseOffer>, ManagedChatError> {
        if client_id.trim().is_empty() || client_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat client id must contain 1..=128 bytes".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();

        for command in candidate.commands.values_mut() {
            if command.state == ManagedChatCommandState::Leased
                && command
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at_ms <= now_ms)
            {
                command.state = ManagedChatCommandState::NeedsReconcile;
                command.reconcile_history = true;
                command.lease = None;
            }
        }

        let selected_id = candidate
            .commands
            .values()
            .filter(|command| command.state == ManagedChatCommandState::NeedsReconcile)
            .min_by_key(|command| command.sequence)
            .or_else(|| {
                candidate
                    .commands
                    .values()
                    .filter(|command| command.state == ManagedChatCommandState::Queued)
                    .min_by_key(|command| command.sequence)
            })
            .map(|command| command.id.clone());

        let Some(command_id) = selected_id else {
            if candidate != *guard {
                self.commit_candidate(&mut guard, candidate).await?;
            }
            return Ok(None);
        };
        let command = candidate
            .commands
            .get_mut(&command_id)
            .ok_or(ManagedChatError::NotFound)?;
        let reconcile_required = command.reconcile_history
            || command.state == ManagedChatCommandState::NeedsReconcile;
        let lease = ManagedChatLease {
            lease_id: ManagedChatLeaseId::new(),
            client_id: client_id.to_string(),
            expires_at_ms: now_ms.saturating_add(DEFAULT_COMMAND_LEASE_MS),
        };
        command.state = ManagedChatCommandState::Leased;
        command.lease = Some(lease);
        let offered = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(Some(ManagedChatLeaseOffer {
            command: offered,
            reconcile_required,
        }))
    }

    pub async fn acknowledge(
        &self,
        command_id: &ManagedChatCommandId,
        lease_id: &ManagedChatLeaseId,
        client_id: &str,
        outcome: ManagedChatAckOutcome,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        if client_id.trim().is_empty() || client_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat client id must contain 1..=128 bytes".into(),
            ));
        }
        let details = match &outcome {
            ManagedChatAckOutcome::Succeeded { details }
            | ManagedChatAckOutcome::Failed { details } => details.as_deref(),
            ManagedChatAckOutcome::NeedsReconcile => None,
        };
        if details.is_some_and(|value| value.len() > MAX_MANAGED_CHAT_DETAIL_BYTES) {
            return Err(ManagedChatError::Invalid(format!(
                "managed chat ack details exceed {MAX_MANAGED_CHAT_DETAIL_BYTES} bytes"
            )));
        }
        let mut guard = self.data.lock().await;
        let command = guard
            .commands
            .get(command_id)
            .ok_or(ManagedChatError::NotFound)?;

        if matches!(
            command.state,
            ManagedChatCommandState::Succeeded | ManagedChatCommandState::Failed
        ) {
            if terminal_matches(command, &outcome) {
                return Ok(command.clone());
            }
            return Err(ManagedChatError::Conflict(
                "managed chat command is already terminal with a different outcome".into(),
            ));
        }
        let active_lease = command.lease.as_ref().ok_or_else(|| {
            ManagedChatError::Conflict("managed chat command has no active lease".into())
        })?;
        if &active_lease.lease_id != lease_id || active_lease.client_id != client_id {
            return Err(ManagedChatError::Conflict(
                "managed chat lease is stale or belongs to another redemption".into(),
            ));
        }

        let mut candidate = guard.clone();
        let command = candidate
            .commands
            .get_mut(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        command.lease = None;
        match outcome {
            ManagedChatAckOutcome::Succeeded { details } => {
                command.state = ManagedChatCommandState::Succeeded;
                command.terminal = Some(ManagedChatTerminalResult {
                    succeeded: true,
                    details,
                });
            }
            ManagedChatAckOutcome::Failed { details } => {
                command.state = ManagedChatCommandState::Failed;
                command.terminal = Some(ManagedChatTerminalResult {
                    succeeded: false,
                    details,
                });
            }
            ManagedChatAckOutcome::NeedsReconcile => {
                command.state = ManagedChatCommandState::NeedsReconcile;
                command.reconcile_history = true;
                command.terminal = None;
            }
        }
        let acknowledged = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(acknowledged)
    }

    pub async fn retry_failed(
        &self,
        command_id: &ManagedChatCommandId,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        let mut guard = self.data.lock().await;
        let command = guard
            .commands
            .get(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        if command.reconcile_history {
            return Err(ManagedChatError::Conflict(
                "managed chat command cannot be fresh-retried after send ambiguity; reconciliation is required"
                    .into(),
            ));
        }
        match command.state {
            ManagedChatCommandState::Queued | ManagedChatCommandState::Leased => {
                return Ok(command.clone());
            }
            ManagedChatCommandState::Failed => {}
            ManagedChatCommandState::Succeeded => {
                return Err(ManagedChatError::Conflict(
                    "succeeded managed chat command cannot be retried".into(),
                ));
            }
            ManagedChatCommandState::NeedsReconcile => {
                return Err(ManagedChatError::Conflict(
                    "managed chat command requires reconciliation and cannot be fresh-retried"
                        .into(),
                ));
            }
        }

        let mut candidate = guard.clone();
        let command = candidate
            .commands
            .get_mut(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        command.state = ManagedChatCommandState::Queued;
        command.lease = None;
        command.terminal = None;
        let retried = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(retried)
    }

    async fn commit_candidate(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, ManagedChatStoreData>,
        candidate: ManagedChatStoreData,
    ) -> Result<(), ManagedChatError> {
        let path = self.path.clone();
        let persisted = candidate.clone();
        tokio::task::spawn_blocking(move || store::save(&path, &persisted))
            .await
            .map_err(|error| {
                ManagedChatError::Storage(format!("managed chat persistence task failed: {error}"))
            })?
            .map_err(storage_error)?;
        **guard = candidate;
        Ok(())
    }
}

fn terminal_matches(command: &ManagedChatCommand, outcome: &ManagedChatAckOutcome) -> bool {
    let Some(terminal) = &command.terminal else {
        return false;
    };
    match outcome {
        ManagedChatAckOutcome::Succeeded { details } => {
            terminal.succeeded && &terminal.details == details
        }
        ManagedChatAckOutcome::Failed { details } => {
            !terminal.succeeded && &terminal.details == details
        }
        ManagedChatAckOutcome::NeedsReconcile => false,
    }
}

fn validate_enqueue_request(request: &EnqueueManagedChatRequest) -> Result<(), ManagedChatError> {
    if request.dedupe_key.trim().is_empty() || request.dedupe_key.len() > 256 {
        return Err(ManagedChatError::Invalid(
            "managed chat dedupe key must contain 1..=256 bytes".into(),
        ));
    }
    request.launch.validate().map_err(ManagedChatError::Invalid)
}

fn request_fingerprint<T: Serialize>(request: &T) -> Result<String, ManagedChatError> {
    let bytes = serde_json::to_vec(request).map_err(|error| {
        ManagedChatError::Invalid(format!(
            "failed to fingerprint managed chat request: {error}"
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

fn storage_error(error: std::io::Error) -> ManagedChatError {
    ManagedChatError::Storage(format!("failed to persist managed chat state: {error}"))
}

#[cfg(test)]
mod tests {
    use super::super::types::{
        ChatExecutionProfile, ManagedChatOpenMode, ManagedChatPurpose, ReasoningEffort,
    };
    use super::*;
    use crate::workspaces::WorkspaceId;
    use uuid::Uuid;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()))
    }

    fn launch(marker: &str) -> ManagedChatLaunch {
        ManagedChatLaunch {
            workspace_id: WorkspaceId::new(),
            purpose: ManagedChatPurpose::Worker,
            execution_profile: ChatExecutionProfile {
                model_key: "gpt-5.6-sol".into(),
                model_label: "GPT-5.6 Sol".into(),
                reasoning_effort: ReasoningEffort::High,
            },
            opening_message: format!("worker assignment\nTask marker: {marker}"),
            task_marker: marker.into(),
            thread_key: Some("worker:test-thread".into()),
            open_mode: ManagedChatOpenMode::NewThread,
        }
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_and_rejects_changed_input() {
        let root = temp_root("moondesk-managed-chat-idempotent");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let request = EnqueueManagedChatRequest {
            dedupe_key: "worker:one:task:one".into(),
            launch: launch("task-one"),
        };
        let first = broker
            .enqueue(request.clone())
            .await
            .expect("enqueue command");
        let retry = broker.enqueue(request).await.expect("retry command");
        assert_eq!(first, retry);
        assert_eq!(broker.snapshot().await.commands.len(), 1);

        let conflict = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:one".into(),
                launch: launch("different-task"),
            })
            .await
            .expect_err("changed input under same dedupe key must fail");
        assert!(matches!(conflict, ManagedChatError::Conflict(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn purge_workspace_removes_only_owned_managed_chat_commands() {
        let root = temp_root("moondesk-managed-chat-purge-workspace");
        let path = root.join("state.json");
        let broker = ManagedChatBroker::open(&path).expect("open broker");
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let mut launch_a = launch("task-a");
        launch_a.workspace_id = workspace_a.clone();
        let mut launch_b = launch("task-b");
        launch_b.workspace_id = workspace_b.clone();
        let command_a = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "workspace-a-command".into(),
                launch: launch_a,
            })
            .await
            .expect("enqueue A");
        broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "workspace-b-command".into(),
                launch: launch_b,
            })
            .await
            .expect("enqueue B");

        assert_eq!(broker.purge_workspace(&workspace_b).await.expect("purge B"), 1);
        assert_eq!(broker.purge_workspace(&workspace_b).await.expect("repeat purge B"), 0);
        let snapshot = broker.snapshot().await;
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(snapshot.dedupe.len(), 1);
        assert_eq!(snapshot.commands.get(&command_a.id), Some(&command_a));
        assert_eq!(
            snapshot.dedupe.get("workspace-a-command"),
            Some(&command_a.id)
        );
        assert!(!snapshot.dedupe.contains_key("workspace-b-command"));
        drop(broker);

        let reopened = ManagedChatBroker::open(&path).expect("reopen broker");
        let snapshot = reopened.snapshot().await;
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(snapshot.commands.get(&command_a.id), Some(&command_a));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn expired_lease_is_redeemed_as_reconciliation_not_fresh_send() {
        let root = temp_root("moondesk-managed-chat-reconcile");
        let path = root.join("state.json");
        let broker = ManagedChatBroker::open(&path).expect("open broker");
        let command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:reconcile".into(),
                launch: launch("task-reconcile"),
            })
            .await
            .expect("enqueue command");
        let first = broker
            .redeem("extension-a", 1_000)
            .await
            .expect("redeem command")
            .expect("leased command");
        assert!(!first.reconcile_required);
        assert_eq!(first.command.id, command.id);
        drop(broker);

        let reopened = ManagedChatBroker::open(&path).expect("reopen broker");
        let second = reopened
            .redeem("extension-a", 1_000 + DEFAULT_COMMAND_LEASE_MS + 1)
            .await
            .expect("redeem expired command")
            .expect("reconciliation command");
        assert!(second.reconcile_required);
        assert_eq!(second.command.id, command.id);
        assert_ne!(
            second
                .command
                .lease
                .as_ref()
                .expect("second lease")
                .lease_id,
            first.command.lease.as_ref().expect("first lease").lease_id
        );
        assert_eq!(reopened.snapshot().await.commands.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_ack_cannot_override_new_lease_and_terminal_ack_is_idempotent() {
        let root = temp_root("moondesk-managed-chat-stale-ack");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:ack".into(),
                launch: launch("task-ack"),
            })
            .await
            .expect("enqueue command");
        let first = broker
            .redeem("extension-a", 10)
            .await
            .expect("redeem")
            .expect("first lease");
        let second = broker
            .redeem("extension-a", 10 + DEFAULT_COMMAND_LEASE_MS + 1)
            .await
            .expect("redeem after expiry")
            .expect("second lease");
        let stale = broker
            .acknowledge(
                &command.id,
                &first.command.lease.as_ref().expect("first lease").lease_id,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                },
            )
            .await
            .expect_err("stale lease ack must fail");
        assert!(matches!(stale, ManagedChatError::Conflict(_)));

        let lease_id = second
            .command
            .lease
            .as_ref()
            .expect("second active lease")
            .lease_id
            .clone();
        let succeeded = broker
            .acknowledge(
                &command.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                },
            )
            .await
            .expect("ack success");
        assert_eq!(succeeded.state, ManagedChatCommandState::Succeeded);
        let retry = broker
            .acknowledge(
                &command.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                },
            )
            .await
            .expect("repeat same terminal ack");
        assert_eq!(retry, succeeded);
        assert!(
            broker
                .redeem("extension-a", 999_999)
                .await
                .expect("redeem after terminal")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_pre_send_command_can_retry_but_reconciled_command_cannot() {
        let root = temp_root("moondesk-managed-chat-safe-retry");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");

        let pre_send = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:pre-send-fail".into(),
                launch: launch("task-pre-send-fail"),
            })
            .await
            .expect("enqueue pre-send command");
        let leased = broker
            .redeem("extension-a", 100)
            .await
            .expect("redeem pre-send command")
            .expect("leased pre-send command");
        let lease_id = leased
            .command
            .lease
            .as_ref()
            .expect("pre-send lease")
            .lease_id
            .clone();
        let failed = broker
            .acknowledge(
                &pre_send.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("model_unavailable".into()),
                },
            )
            .await
            .expect("ack pre-send failure");
        assert_eq!(failed.state, ManagedChatCommandState::Failed);
        assert!(!failed.reconcile_history);

        let retried = broker
            .retry_failed(&pre_send.id)
            .await
            .expect("fresh retry pre-send failure");
        assert_eq!(retried.state, ManagedChatCommandState::Queued);
        assert!(retried.terminal.is_none());
        let retry_offer = broker
            .redeem("extension-a", 200)
            .await
            .expect("redeem retried command")
            .expect("retried command offer");
        assert!(!retry_offer.reconcile_required);

        let ambiguous = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:ambiguous".into(),
                launch: launch("task-ambiguous"),
            })
            .await
            .expect("enqueue ambiguous command");
        let first = broker
            .redeem("extension-a", 300)
            .await
            .expect("redeem ambiguous command")
            .expect("ambiguous first lease");
        let first_lease = first
            .command
            .lease
            .as_ref()
            .expect("ambiguous first lease")
            .lease_id
            .clone();
        broker
            .acknowledge(
                &ambiguous.id,
                &first_lease,
                "extension-a",
                ManagedChatAckOutcome::NeedsReconcile,
            )
            .await
            .expect("mark ambiguous");
        let reconcile = broker
            .redeem("extension-a", 400)
            .await
            .expect("redeem reconciliation")
            .expect("reconciliation lease");
        assert!(reconcile.reconcile_required);
        let reconcile_lease = reconcile
            .command
            .lease
            .as_ref()
            .expect("reconcile lease")
            .lease_id
            .clone();
        let terminal_after_reconcile = broker
            .acknowledge(
                &ambiguous.id,
                &reconcile_lease,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("reconcile_payload_invalid".into()),
                },
            )
            .await
            .expect("terminal failure after reconcile");
        assert!(terminal_after_reconcile.reconcile_history);
        let unsafe_retry = broker
            .retry_failed(&ambiguous.id)
            .await
            .expect_err("fresh retry after ambiguity must fail");
        assert!(matches!(unsafe_retry, ManagedChatError::Conflict(_)));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistence_failure_does_not_publish_command() {
        let root = temp_root("moondesk-managed-chat-persist-failure");
        std::fs::create_dir_all(&root).expect("create test root");
        let blocked = root.join("blocked");
        std::fs::write(&blocked, "not a directory").expect("create blocked parent");
        let broker = ManagedChatBroker::open(blocked.join("state.json"))
            .expect("open broker before state exists");
        let error = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:one:task:persist".into(),
                launch: launch("task-persist"),
            })
            .await
            .expect_err("persistence failure must reject enqueue");
        assert!(matches!(error, ManagedChatError::Storage(_)));
        assert!(broker.snapshot().await.commands.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
