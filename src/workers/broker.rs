use super::store;
use super::types::{
    BrowserAttachmentState, ChatIdentity, CollectReceipt, CollectedTaskFailureReceipt,
    CollectedTaskReceipt, MessageReceipt, OperationId, ReportReceipt, RetiredWorkerFence,
    ReuseReceipt, SpawnReceipt, TaskId, TaskState, WorkerExecutionProfile, WorkerFamily,
    WorkerFamilyId, WorkerId, WorkerLaunchState, WorkerMessage, WorkerMessageId,
    WorkerMessageState, WorkerRecord, WorkerRecoveryGuard, WorkerReport, WorkerReportId,
    WorkerResult, WorkerState, WorkerStoreData, WorkerTask, WorkerTaskFailure,
};
use super::{
    MAX_COLLECT_RECEIPTS_PER_FAMILY, MAX_COLLECTED_TASK_HISTORY_PER_WORKER,
    MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY, MAX_PENDING_MESSAGES_PER_WORKER, MAX_REPORTS_PER_FAMILY,
    MAX_TASK_RECORDS_PER_WORKER, MAX_UPDATES_PER_COLLECT, MAX_WORKER_ASSIGNMENT_BYTES,
    MAX_WORKER_CONTEXT_BYTES, MAX_WORKER_FAMILIES, MAX_WORKER_MESSAGE_BYTES,
    MAX_WORKER_RECORDS_PER_FAMILY, MAX_WORKERS_PER_FAMILY, RETIRED_WORKER_FENCE_TTL_MS,
    WORKER_CLAIM_TIMEOUT_MS,
};
use crate::managed_chat::types::canonical_chatgpt_conversation_id;
use crate::workspaces::WorkspaceId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBrokerError {
    Invalid(String),
    NotFound,
    Conflict(String),
    Limit(String),
    Storage(String),
}

impl fmt::Display for WorkerBrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::Conflict(message)
            | Self::Limit(message)
            | Self::Storage(message) => formatter.write_str(message),
            Self::NotFound => formatter.write_str("worker state was not found in this workspace"),
        }
    }
}

impl std::error::Error for WorkerBrokerError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnWorkerRequest {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    pub anchor_identity: ChatIdentity,
    pub label: String,
    pub assignment: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub context: String,
    pub execution_profile: WorkerExecutionProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReuseWorkerRequest {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    pub anchor_identity: ChatIdentity,
    pub worker_id: WorkerId,
    pub assignment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageWorkerRequest {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    pub anchor_identity: ChatIdentity,
    pub worker_id: WorkerId,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportWorkerRequest {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    pub worker_identity: ChatIdentity,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishTaskRequest {
    pub workspace_id: WorkspaceId,
    pub worker_identity: ChatIdentity,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub result: WorkerResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedWorkerUpdate {
    pub worker_id: WorkerId,
    pub display_id: String,
    pub task_id: TaskId,
    pub result: WorkerResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedWorkerFailure {
    pub worker_id: WorkerId,
    pub display_id: String,
    pub task_id: TaskId,
    pub failure: WorkerTaskFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedUpdates {
    pub reports: Vec<WorkerReport>,
    pub completed: Vec<CollectedWorkerUpdate>,
    pub failed: Vec<CollectedWorkerFailure>,
}

const MAX_RECENT_WORKER_TOOL_ACTIVITY: usize = 12;
const CLAIM_REGISTRATION_PENDING_MESSAGE: &str = "worker registration is still pending; MoonDesk has not yet confirmed the canonical ChatGPT conversation";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerToolActivity {
    pub at_ms: u64,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerActivitySnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<u64>,
    pub recent_tool_calls: Vec<WorkerToolActivity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InactiveWorkerCleanupSummary {
    pub family_count: usize,
    pub worker_count: usize,
    pub replay_receipt_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InactiveWorkerCleanupFamily {
    pub family_id: WorkerFamilyId,
    pub session_digest: String,
    pub expected_family: WorkerFamily,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InactiveWorkerCleanupPlan {
    pub workspace_id: WorkspaceId,
    pub summary: InactiveWorkerCleanupSummary,
    pub families: Vec<InactiveWorkerCleanupFamily>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerClaimAttestationRequirement {
    pub operation_id: OperationId,
    pub conversation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingWorkerClaimRegistration {
    pub launch_command_id: String,
    pub operation_id: OperationId,
}

pub struct WorkerBroker {
    path: PathBuf,
    data: Mutex<WorkerStoreData>,
    activity: Mutex<BTreeMap<WorkerId, WorkerActivitySnapshot>>,
    updates: Notify,
    #[cfg(test)]
    fail_next_commit: std::sync::atomic::AtomicBool,
}

impl WorkerBroker {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, WorkerBrokerError> {
        let path = path.into();
        let data = store::load(&path).map_err(storage_error)?;
        Ok(Self {
            path,
            data: Mutex::new(data),
            activity: Mutex::new(BTreeMap::new()),
            updates: Notify::new(),
            #[cfg(test)]
            fail_next_commit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) async fn snapshot(&self) -> WorkerStoreData {
        self.data.lock().await.clone()
    }

    pub async fn has_worker_state(&self) -> bool {
        !self.data.lock().await.families.is_empty()
    }

    pub async fn is_retired_worker_identity(&self, identity: &ChatIdentity) -> bool {
        let now_ms = worker_unix_time_ms();
        self.data
            .lock()
            .await
            .retired_workers
            .get(&identity.session_digest)
            .is_some_and(|fence| retired_worker_fence_is_live(fence, now_ms))
    }

    pub async fn clear_all_workers(&self) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        let mut guard = self.data.lock().await;
        let workers = guard
            .families
            .values()
            .flat_map(|family| family.workers.values().cloned())
            .collect::<Vec<_>>();
        if workers.is_empty() && guard.families.is_empty() {
            return Ok(workers);
        }

        let now_ms = worker_unix_time_ms();
        let mut candidate = guard.clone();
        prune_retired_worker_fences(&mut candidate, now_ms);
        for worker in &workers {
            let Some(identity) = worker.chat_identity.clone() else {
                continue;
            };
            candidate.retired_workers.insert(
                identity.session_digest.clone(),
                RetiredWorkerFence {
                    identity,
                    retired_at_ms: now_ms,
                },
            );
        }
        candidate.families.clear();
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(workers)
    }

    pub async fn clear_family_for_anchor(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let Some((family_id, family)) = guard.families.iter().find(|(_, family)| {
            &family.workspace_id == workspace_id && &family.anchor_identity == anchor_identity
        }) else {
            return Ok(Vec::new());
        };
        let family_id = family_id.clone();
        let workers = family.workers.values().cloned().collect::<Vec<_>>();

        let now_ms = worker_unix_time_ms();
        let mut candidate = guard.clone();
        prune_retired_worker_fences(&mut candidate, now_ms);
        for worker in &workers {
            let Some(identity) = worker.chat_identity.clone() else {
                continue;
            };
            candidate.retired_workers.insert(
                identity.session_digest.clone(),
                RetiredWorkerFence {
                    identity,
                    retired_at_ms: now_ms,
                },
            );
        }
        candidate.families.remove(&family_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(workers)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_commit_for_test(&self) {
        self.fail_next_commit
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn spawn_worker(
        &self,
        request: SpawnWorkerRequest,
    ) -> Result<SpawnReceipt, WorkerBrokerError> {
        validate_spawn_request(&request)?;
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;
        if guard
            .retired_workers
            .get(&request.anchor_identity.session_digest)
            .is_some_and(|fence| retired_worker_fence_is_live(fence, worker_unix_time_ms()))
        {
            return Err(WorkerBrokerError::Conflict(
                "this ChatGPT conversation was retired by Clear Workers and cannot become a Core yet; start a new Core conversation"
                    .into(),
            ));
        }

        if let Some(family) = guard.families.values().find(|family| {
            family.workspace_id == request.workspace_id
                && family.anchor_identity == request.anchor_identity
        }) && let Some(receipt) = family.spawn_requests.get(&request.operation_id)
        {
            if receipt.request_fingerprint == fingerprint {
                return Ok(receipt.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "worker spawn operation id was reused with different input".into(),
            ));
        }

        let mut candidate = guard.clone();
        prune_retired_worker_fences(&mut candidate, worker_unix_time_ms());
        let family_id = if let Some(existing) = candidate
            .families
            .values()
            .find(|family| {
                family.workspace_id == request.workspace_id
                    && family.anchor_identity == request.anchor_identity
            })
            .map(|family| family.id.clone())
        {
            existing
        } else {
            compact_inactive_families(&mut candidate);
            if candidate.families.len() >= MAX_WORKER_FAMILIES {
                let cleanup_candidates = candidate
                    .families
                    .values()
                    .filter(|family| family_can_be_freed_by_user_cleanup(family))
                    .count();
                let cleanup_guidance = if cleanup_candidates > 0 {
                    format!(
                        "; {cleanup_candidates} retained Core families are safe to release. Use MoonDesk Settings -> Workers -> Release inactive Worker capacity for the affected workspace; ChatGPT conversations are preserved"
                    )
                } else {
                    "; no retained Core family is currently safe to release; finish active work and collect pending worker results before freeing older Core history"
                        .to_string()
                };
                return Err(WorkerBrokerError::Limit(format!(
                    "worker store is limited to {MAX_WORKER_FAMILIES} Core families with retained live/history state{cleanup_guidance}"
                )));
            }
            let id = WorkerFamilyId::new();
            candidate.families.insert(
                id.clone(),
                WorkerFamily {
                    id: id.clone(),
                    workspace_id: request.workspace_id.clone(),
                    anchor_identity: request.anchor_identity.clone(),
                    next_receipt_sequence: 0,
                    workers: Default::default(),
                    reports: Vec::new(),
                    spawn_requests: Default::default(),
                    reuse_requests: Default::default(),
                    message_requests: Default::default(),
                    report_requests: Default::default(),
                    collect_requests: Default::default(),
                },
            );
            id
        };
        let family = candidate.families.get_mut(&family_id).ok_or_else(|| {
            WorkerBrokerError::Storage("worker family disappeared during mutation".into())
        })?;

        if family.workers.len() >= MAX_WORKER_RECORDS_PER_FAMILY {
            return Err(WorkerBrokerError::Limit(format!(
                "worker family retains at most {MAX_WORKER_RECORDS_PER_FAMILY} worker records"
            )));
        }
        let active_workers = family
            .workers
            .values()
            .filter(|worker| worker.state != WorkerState::Retired)
            .count();
        if active_workers >= MAX_WORKERS_PER_FAMILY {
            return Err(WorkerBrokerError::Limit(format!(
                "worker family is limited to {MAX_WORKERS_PER_FAMILY} active workers"
            )));
        }

        let display_id = next_display_id(family).ok_or_else(|| {
            WorkerBrokerError::Limit(format!(
                "worker family is limited to {MAX_WORKERS_PER_FAMILY} workers"
            ))
        })?;
        let worker_id = WorkerId::new();
        let task_id = TaskId::new();
        let claim_token = new_claim_token();
        let claim_operation_id = OperationId::new();
        let task = WorkerTask {
            id: task_id.clone(),
            assignment: request.assignment,
            state: TaskState::Pending,
            result: None,
            failure: None,
            collected: false,
        };
        let worker = WorkerRecord {
            id: worker_id.clone(),
            display_id: display_id.clone(),
            label: request.label,
            state: WorkerState::Provisioning,
            attachment_state: BrowserAttachmentState::Absent,
            execution_profile: request.execution_profile,
            launch_state: crate::workers::types::WorkerLaunchState::Queued,
            launch_command_id: None,
            launch_error: None,
            conversation_url: None,
            chat_identity: None,
            claim_token: Some(claim_token.clone()),
            claim_operation_id: Some(claim_operation_id.clone()),
            waiting_claim_since_ms: None,
            current_task_id: Some(task_id.clone()),
            recovery_guard: None,
            tasks: [(task_id.clone(), task)].into_iter().collect(),
            messages: Vec::new(),
        };
        family.workers.insert(worker_id.clone(), worker);

        let sequence = allocate_receipt_sequence(family)?;
        let receipt = SpawnReceipt {
            request_fingerprint: fingerprint,
            sequence,
            family_id,
            worker_id,
            task_id,
            display_id,
            claim_token,
            claim_operation_id: Some(claim_operation_id),
        };
        family
            .spawn_requests
            .insert(request.operation_id, receipt.clone());
        compact_family_history(family)?;

        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
    }

    pub async fn link_launch_command(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
        command_id: &str,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        if Uuid::parse_str(command_id).is_err() {
            return Err(WorkerBrokerError::Invalid(
                "worker launch command id is invalid".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.current_task_id.as_ref() != Some(task_id) || !worker.tasks.contains_key(task_id) {
            return Err(WorkerBrokerError::Conflict(
                "worker launch task changed before command link was persisted".into(),
            ));
        }
        if let Some(existing) = worker.launch_command_id.as_deref() {
            if existing == command_id {
                return Ok(worker.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "worker task is already linked to a different launch command".into(),
            ));
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.launch_command_id = Some(command_id.to_string());
        worker.launch_state = WorkerLaunchState::Queued;
        worker.launch_error = None;
        if worker.state == WorkerState::Provisioning {
            worker.conversation_url = None;
        } else if worker.state == WorkerState::Waking && worker.conversation_url.is_none() {
            return Err(WorkerBrokerError::Conflict(
                "existing worker wake lost its durable ChatGPT conversation URL".into(),
            ));
        }
        let linked = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(linked)
    }

    pub async fn update_launch_by_command(
        &self,
        command_id: &str,
        launch_state: WorkerLaunchState,
        launch_error: Option<String>,
        conversation_url: Option<String>,
    ) -> Result<Option<WorkerRecord>, WorkerBrokerError> {
        if Uuid::parse_str(command_id).is_err() {
            return Err(WorkerBrokerError::Invalid(
                "worker launch command id is invalid".into(),
            ));
        }
        if launch_error
            .as_deref()
            .is_some_and(|value| value.len() > 1000)
        {
            return Err(WorkerBrokerError::Invalid(
                "worker launch error is too long".into(),
            ));
        }
        if conversation_url
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > 2048)
        {
            return Err(WorkerBrokerError::Invalid(
                "worker conversation URL is invalid".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let mut location = None;
        'families: for (family_id, family) in &guard.families {
            for (worker_id, worker) in &family.workers {
                if worker.launch_command_id.as_deref() == Some(command_id) {
                    location = Some((family_id.clone(), worker_id.clone()));
                    break 'families;
                }
            }
        }
        let Some((family_id, worker_id)) = location else {
            return Ok(None);
        };
        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(&worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.launch_state == WorkerLaunchState::Claimed
            && launch_state != WorkerLaunchState::Claimed
        {
            return Ok(Some(worker.clone()));
        }
        if launch_state == WorkerLaunchState::WaitingClaim
            && conversation_url
                .as_deref()
                .and_then(canonical_conversation_id_from_url)
                .is_none()
        {
            return Err(WorkerBrokerError::Invalid(
                "worker WaitingClaim state requires a canonical ChatGPT conversation URL".into(),
            ));
        }
        worker.launch_state = launch_state;
        worker.launch_error = launch_error;
        if conversation_url.is_some() {
            worker.conversation_url = conversation_url;
        }
        match launch_state {
            WorkerLaunchState::WaitingClaim if worker.chat_identity.is_none() => {
                worker
                    .waiting_claim_since_ms
                    .get_or_insert_with(worker_unix_time_ms);
            }
            WorkerLaunchState::Claimed | WorkerLaunchState::Paused | WorkerLaunchState::Failed => {
                worker.waiting_claim_since_ms = None;
            }
            _ => {}
        }
        worker.attachment_state = match launch_state {
            WorkerLaunchState::Unknown | WorkerLaunchState::Queued => {
                BrowserAttachmentState::Absent
            }
            WorkerLaunchState::Preparing
            | WorkerLaunchState::SendStarted
            | WorkerLaunchState::Reconciling
            | WorkerLaunchState::WaitingClaim => BrowserAttachmentState::Opening,
            WorkerLaunchState::Claimed => BrowserAttachmentState::Attached,
            WorkerLaunchState::Paused | WorkerLaunchState::Failed => {
                BrowserAttachmentState::Unknown
            }
        };
        let updated = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(Some(updated))
    }

    pub async fn sync_browser_presence(
        &self,
        open_conversation_ids: &BTreeSet<String>,
    ) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();
        let now_ms = worker_unix_time_ms();
        let mut changed = expire_waiting_claims_in_data(&mut candidate, now_ms, None);

        for family in candidate.families.values_mut() {
            for worker in family.workers.values_mut() {
                let mut worker_changed = false;
                let Some(conversation_id) = worker
                    .conversation_url
                    .as_deref()
                    .and_then(conversation_id_from_url)
                else {
                    if worker_changed {
                        changed.push(worker.clone());
                    }
                    continue;
                };
                let open = open_conversation_ids.contains(&conversation_id);
                let desired = match worker.state {
                    WorkerState::Running => {
                        if open {
                            BrowserAttachmentState::Attached
                        } else {
                            BrowserAttachmentState::Detached
                        }
                    }
                    WorkerState::Idle => {
                        if open {
                            BrowserAttachmentState::Attached
                        } else {
                            BrowserAttachmentState::Absent
                        }
                    }
                    WorkerState::Retired => BrowserAttachmentState::Absent,
                    WorkerState::Provisioning | WorkerState::Waking => {
                        if worker_changed {
                            changed.push(worker.clone());
                        }
                        continue;
                    }
                };
                if worker.attachment_state != desired {
                    worker.attachment_state = desired;
                    worker_changed = true;
                }
                if worker_changed {
                    changed.push(worker.clone());
                }
            }
        }

        if changed.is_empty() {
            return Ok(changed);
        }
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(changed)
    }

    pub async fn expire_waiting_claim_timeouts_for_anchor(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        workspace_id
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();
        let changed = expire_waiting_claims_in_data(
            &mut candidate,
            worker_unix_time_ms(),
            Some((workspace_id, anchor_identity)),
        );
        if changed.is_empty() {
            return Ok(changed);
        }
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(changed)
    }

    pub async fn cancel_unclaimed_after_send(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Provisioning
            || worker.launch_state != WorkerLaunchState::WaitingClaim
            || worker.chat_identity.is_some()
        {
            return Err(WorkerBrokerError::Conflict(
                "only an unclaimed Worker whose assignment was sent can be cancelled this way"
                    .into(),
            ));
        }
        let task_id = worker.current_task_id.clone().ok_or_else(|| {
            WorkerBrokerError::Conflict("unclaimed Worker has no pending task to cancel".into())
        })?;
        if worker
            .tasks
            .get(&task_id)
            .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "unclaimed Worker task is no longer pending".into(),
            ));
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        let task = worker
            .tasks
            .get_mut(&task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Failed;
        task.result = None;
        task.failure = Some(WorkerTaskFailure {
            stage: "claim".into(),
            error: "worker_registration_cancelled_after_send".into(),
            launch_command_id: worker.launch_command_id.clone(),
        });
        task.collected = false;
        worker.current_task_id = None;
        worker.claim_token = None;
        worker.claim_operation_id = None;
        worker.waiting_claim_since_ms = None;
        worker.state = WorkerState::Retired;
        worker.launch_state = WorkerLaunchState::Failed;
        worker.launch_error = Some("worker_registration_cancelled_after_send".into());
        worker.attachment_state = BrowserAttachmentState::Absent;
        let cancelled = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(cancelled)
    }

    pub async fn settle_pre_send_failure_by_command(
        &self,
        command_id: &str,
        launch_error: Option<String>,
    ) -> Result<Option<WorkerRecord>, WorkerBrokerError> {
        if Uuid::parse_str(command_id).is_err() {
            return Err(WorkerBrokerError::Invalid(
                "worker launch command id is invalid".into(),
            ));
        }
        if launch_error
            .as_deref()
            .is_some_and(|value| value.len() > 1000)
        {
            return Err(WorkerBrokerError::Invalid(
                "worker launch error is too long".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let mut location = None;
        'families: for (family_id, family) in &guard.families {
            for (worker_id, worker) in &family.workers {
                if worker.launch_command_id.as_deref() == Some(command_id) {
                    location = Some((family_id.clone(), worker_id.clone()));
                    break 'families;
                }
            }
        }
        let Some((family_id, worker_id)) = location else {
            return Ok(None);
        };
        let worker = guard
            .families
            .get(&family_id)
            .and_then(|family| family.workers.get(&worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.launch_state == WorkerLaunchState::Claimed
            || matches!(worker.state, WorkerState::Idle | WorkerState::Retired)
        {
            return Ok(Some(worker.clone()));
        }
        let task_id = worker.current_task_id.clone().ok_or_else(|| {
            WorkerBrokerError::Conflict(
                "pre-Send worker failure has no pending task to settle".into(),
            )
        })?;
        if worker
            .tasks
            .get(&task_id)
            .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "pre-Send worker failure no longer owns a pending task".into(),
            ));
        }

        let was_fresh = worker.state == WorkerState::Provisioning && worker.chat_identity.is_none();
        let was_reuse = worker.state == WorkerState::Waking && worker.chat_identity.is_some();
        if !was_fresh && !was_reuse {
            return Err(WorkerBrokerError::Conflict(
                "worker launch cannot be settled as pre-Send failure after claim or task start"
                    .into(),
            ));
        }
        let launch_error = launch_error
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "worker launch failed before Send".into());
        let task_failure = WorkerTaskFailure {
            stage: worker_launch_failure_stage(&launch_error).into(),
            error: launch_error.clone(),
            launch_command_id: Some(command_id.to_string()),
        };

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = family
            .workers
            .get_mut(&worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let task = worker
            .tasks
            .get_mut(&task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Failed;
        task.result = None;
        task.failure = Some(task_failure);
        task.collected = false;
        worker.current_task_id = None;
        worker.launch_error = Some(launch_error);
        worker.waiting_claim_since_ms = None;
        worker.attachment_state = BrowserAttachmentState::Absent;
        if was_fresh {
            worker.claim_token = None;
            worker.claim_operation_id = None;
            worker.state = WorkerState::Retired;
            worker.launch_state = WorkerLaunchState::Failed;
        } else {
            worker.state = WorkerState::Idle;
            worker.launch_state = WorkerLaunchState::Claimed;
            worker.launch_command_id = None;
        }
        let settled = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(Some(settled))
    }

    pub async fn rollback_linked_spawn(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
        command_id: &str,
    ) -> Result<(), WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Provisioning
            || worker.current_task_id.as_ref() != Some(task_id)
            || worker.chat_identity.is_some()
            || worker.launch_command_id.as_deref() != Some(command_id)
            || !matches!(
                worker.launch_state,
                WorkerLaunchState::Unknown
                    | WorkerLaunchState::Queued
                    | WorkerLaunchState::Preparing
            )
            || worker
                .tasks
                .get(task_id)
                .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "linked worker spawn cannot be rolled back after claim or task transition".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        family.workers.remove(worker_id);
        family
            .spawn_requests
            .retain(|_, receipt| receipt.worker_id != *worker_id || receipt.task_id != *task_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(())
    }

    pub async fn rollback_linked_reuse(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
        command_id: &str,
    ) -> Result<(), WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Waking
            || worker.current_task_id.as_ref() != Some(task_id)
            || worker.chat_identity.is_none()
            || worker.launch_command_id.as_deref() != Some(command_id)
            || !matches!(
                worker.launch_state,
                WorkerLaunchState::Unknown
                    | WorkerLaunchState::Queued
                    | WorkerLaunchState::Preparing
            )
            || worker
                .tasks
                .get(task_id)
                .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "linked worker reuse cannot be rolled back after start or task transition".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = family
            .workers
            .get_mut(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.tasks.remove(task_id);
        worker.current_task_id = None;
        worker.state = WorkerState::Idle;
        worker.launch_state = WorkerLaunchState::Claimed;
        worker.launch_command_id = None;
        worker.launch_error = None;
        worker.attachment_state = BrowserAttachmentState::Attached;
        family
            .reuse_requests
            .retain(|_, receipt| receipt.worker_id != *worker_id || receipt.task_id != *task_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(())
    }

    pub async fn rollback_unlinked_spawn(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
    ) -> Result<(), WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Provisioning
            || worker.current_task_id.as_ref() != Some(task_id)
            || worker.chat_identity.is_some()
            || worker.launch_command_id.is_some()
            || worker
                .tasks
                .get(task_id)
                .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "worker spawn cannot be rolled back after browser launch linkage or claim".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        family.workers.remove(worker_id);
        family
            .spawn_requests
            .retain(|_, receipt| receipt.worker_id != *worker_id || receipt.task_id != *task_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(())
    }

    pub async fn rollback_unlinked_reuse(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
    ) -> Result<(), WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Waking
            || worker.current_task_id.as_ref() != Some(task_id)
            || worker.chat_identity.is_none()
            || worker.launch_command_id.is_some()
            || worker
                .tasks
                .get(task_id)
                .is_none_or(|task| task.state != TaskState::Pending)
        {
            return Err(WorkerBrokerError::Conflict(
                "worker reuse cannot be rolled back after browser launch linkage or start".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = family
            .workers
            .get_mut(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.tasks.remove(task_id);
        worker.current_task_id = None;
        worker.state = WorkerState::Idle;
        worker.launch_state = WorkerLaunchState::Claimed;
        worker.launch_error = None;
        worker.attachment_state = BrowserAttachmentState::Attached;
        family
            .reuse_requests
            .retain(|_, receipt| receipt.worker_id != *worker_id || receipt.task_id != *task_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(())
    }

    pub async fn reuse_worker(
        &self,
        request: ReuseWorkerRequest,
    ) -> Result<ReuseReceipt, WorkerBrokerError> {
        request
            .anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        if request.assignment.is_empty() || request.assignment.len() > MAX_WORKER_ASSIGNMENT_BYTES {
            return Err(WorkerBrokerError::Invalid(format!(
                "worker assignment must contain 1..={MAX_WORKER_ASSIGNMENT_BYTES} bytes"
            )));
        }
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, &request.workspace_id, &request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if family.anchor_identity != request.anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        if let Some(receipt) = family.reuse_requests.get(&request.operation_id) {
            if receipt.request_fingerprint == fingerprint {
                let mut receipt = receipt.clone();
                if receipt.conversation_url.is_empty() {
                    receipt.conversation_url = family
                        .workers
                        .get(&request.worker_id)
                        .and_then(|worker| worker.conversation_url.clone())
                        .ok_or_else(|| {
                            WorkerBrokerError::Conflict(
                                "replayed worker reuse has no durable canonical ChatGPT conversation URL"
                                    .into(),
                            )
                        })?;
                }
                return Ok(receipt);
            }
            return Err(WorkerBrokerError::Conflict(
                "worker reuse operation id was reused with different input".into(),
            ));
        }
        let worker = family
            .workers
            .get(&request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.is_none() {
            return Err(WorkerBrokerError::Conflict(
                "worker must complete its initial claim before it can be reused".into(),
            ));
        }
        if worker.state != WorkerState::Idle || worker.current_task_id.is_some() {
            return Err(WorkerBrokerError::Conflict(
                "worker can only be reused while idle".into(),
            ));
        }
        let conversation_url = worker.conversation_url.clone().ok_or_else(|| {
            WorkerBrokerError::Conflict(
                "claimed worker has no durable canonical ChatGPT conversation URL".into(),
            )
        })?;

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let task_id = TaskId::new();
        let (display_id, execution_profile) = {
            let worker = family
                .workers
                .get_mut(&request.worker_id)
                .ok_or(WorkerBrokerError::NotFound)?;
            worker.tasks.insert(
                task_id.clone(),
                WorkerTask {
                    id: task_id.clone(),
                    assignment: request.assignment,
                    state: TaskState::Pending,
                    result: None,
                    failure: None,
                    collected: false,
                },
            );
            worker.current_task_id = Some(task_id.clone());
            worker.recovery_guard = None;
            worker.state = WorkerState::Waking;
            worker.launch_state = WorkerLaunchState::Queued;
            worker.launch_command_id = None;
            worker.launch_error = None;
            worker.attachment_state = BrowserAttachmentState::Opening;
            (worker.display_id.clone(), worker.execution_profile.clone())
        };
        let sequence = allocate_receipt_sequence(family)?;
        let receipt = ReuseReceipt {
            request_fingerprint: fingerprint,
            sequence,
            worker_id: request.worker_id,
            task_id,
            display_id,
            execution_profile,
            conversation_url,
        };
        family
            .reuse_requests
            .insert(request.operation_id, receipt.clone());
        compact_family_history(family)?;
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
    }

    pub async fn ensure_existing_thread_binding(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        conversation_url: &str,
    ) -> Result<(), WorkerBrokerError> {
        let guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let durable_url = worker.conversation_url.as_deref().ok_or_else(|| {
            WorkerBrokerError::Conflict(
                "worker has no durable canonical ChatGPT conversation URL".into(),
            )
        })?;
        if canonical_conversation_id_from_url(durable_url).is_none()
            || durable_url != conversation_url
        {
            return Err(WorkerBrokerError::Conflict(
                "existing-thread URL does not match this workspace/Core/worker durable binding"
                    .into(),
            ));
        }
        Ok(())
    }

    pub async fn start_task(
        &self,
        workspace_id: &WorkspaceId,
        worker_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = guard
            .families
            .get(&family_id)
            .and_then(|family| family.workers.get(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.as_ref() != Some(worker_identity)
            || worker.current_task_id.as_ref() != Some(task_id)
        {
            return Err(WorkerBrokerError::NotFound);
        }
        let task = worker
            .tasks
            .get(task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        match task.state {
            TaskState::Running => return Ok(worker.clone()),
            TaskState::Pending => {}
            _ => {
                return Err(WorkerBrokerError::Conflict(
                    "worker task cannot be started from its current state".into(),
                ));
            }
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        let task = worker
            .tasks
            .get_mut(task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Running;
        worker.state = WorkerState::Running;
        worker.launch_state = WorkerLaunchState::Claimed;
        worker.launch_error = None;
        worker.attachment_state = BrowserAttachmentState::Attached;
        let started = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(started)
    }

    pub async fn family_for_anchor(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<Option<WorkerFamily>, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        Ok(guard
            .families
            .values()
            .find(|family| {
                &family.workspace_id == workspace_id && &family.anchor_identity == anchor_identity
            })
            .cloned())
    }

    pub async fn gateway_workspace_for_worker_identity(
        &self,
        worker_identity: &ChatIdentity,
    ) -> Result<Option<WorkspaceId>, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        let mut matched_workspace: Option<WorkspaceId> = None;
        for family in guard.families.values().filter(|family| {
            family
                .workers
                .values()
                .any(|worker| worker.chat_identity.as_ref() == Some(worker_identity))
        }) {
            if matched_workspace
                .as_ref()
                .is_some_and(|workspace_id| workspace_id != &family.workspace_id)
            {
                return Err(WorkerBrokerError::Conflict(
                    "this Worker conversation is bound to multiple MoonDesk workspaces; Clear Workers before using the generalized Worker connector again"
                        .into(),
                ));
            }
            matched_workspace = Some(family.workspace_id.clone());
        }
        Ok(matched_workspace)
    }

    pub async fn gateway_workspace_for_active_worker_identity(
        &self,
        worker_identity: &ChatIdentity,
    ) -> Result<Option<WorkspaceId>, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        let mut matched: Option<(WorkspaceId, WorkerState)> = None;
        for family in guard.families.values() {
            for worker in family.workers.values() {
                if worker.chat_identity.as_ref() != Some(worker_identity) {
                    continue;
                }
                if matched.is_some() {
                    return Err(WorkerBrokerError::Conflict(
                        "this Worker conversation is bound more than once; Clear Workers before using the generalized Worker connector again"
                            .into(),
                    ));
                }
                matched = Some((family.workspace_id.clone(), worker.state));
            }
        }
        let Some((workspace_id, state)) = matched else {
            return Ok(None);
        };
        if state != WorkerState::Running {
            return Err(WorkerBrokerError::Conflict(format!(
                "this Worker conversation is bound to MoonDesk but has no active running task (state={state:?}); wait for Core reuse and complete workers start before using workspace tools"
            )));
        }
        Ok(Some(workspace_id))
    }

    pub async fn gateway_workspace_for_claim(
        &self,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        claim_operation_id: &OperationId,
        worker_identity: &ChatIdentity,
    ) -> Result<WorkspaceId, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;

        let mut existing_binding = None;
        for family in guard.families.values() {
            for worker in family.workers.values() {
                if worker.chat_identity.as_ref() != Some(worker_identity) {
                    continue;
                }
                if existing_binding.is_some() {
                    return Err(WorkerBrokerError::Conflict(
                        "this Worker conversation is already bound more than once; Clear Workers before retrying"
                            .into(),
                    ));
                }
                existing_binding = Some((
                    family.workspace_id.clone(),
                    worker.id.clone(),
                    worker.current_task_id.clone(),
                ));
            }
        }
        if let Some((workspace_id, existing_worker_id, existing_task_id)) = existing_binding {
            if &existing_worker_id == worker_id && existing_task_id.as_ref() == Some(task_id) {
                return Ok(workspace_id);
            }
            return Err(WorkerBrokerError::Conflict(
                "this ChatGPT conversation is already bound to another MoonDesk Worker; start a fresh Worker conversation"
                    .into(),
            ));
        }

        let mut matched_workspace = None;
        for family in guard.families.values() {
            let Some(worker) = family.workers.get(worker_id) else {
                continue;
            };
            if worker.current_task_id.as_ref() != Some(task_id)
                || worker.claim_operation_id.as_ref() != Some(claim_operation_id)
            {
                continue;
            }
            let Some(expected_token) = worker.claim_token.as_deref() else {
                continue;
            };
            if claim_token.len() != expected_token.len()
                || !bool::from(claim_token.as_bytes().ct_eq(expected_token.as_bytes()))
            {
                continue;
            }
            if matched_workspace.is_some() {
                return Err(WorkerBrokerError::Conflict(
                    "worker claim capability matched multiple workspaces; Clear Workers before retrying"
                        .into(),
                ));
            }
            matched_workspace = Some(family.workspace_id.clone());
        }
        matched_workspace.ok_or(WorkerBrokerError::NotFound)
    }

    pub async fn record_tool_activity(
        &self,
        workspace_id: &WorkspaceId,
        worker_identity: &ChatIdentity,
        tool: &str,
    ) -> Result<Option<WorkerActivitySnapshot>, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let tool = tool.trim();
        if tool.is_empty() || tool.len() > 128 {
            return Err(WorkerBrokerError::Invalid(
                "worker activity tool name must contain 1..=128 bytes".into(),
            ));
        }
        let owned = {
            let guard = self.data.lock().await;
            guard
                .families
                .values()
                .filter(|family| &family.workspace_id == workspace_id)
                .flat_map(|family| family.workers.values())
                .find(|worker| worker.chat_identity.as_ref() == Some(worker_identity))
                .map(|worker| (worker.id.clone(), worker.current_task_id.clone()))
        };
        let Some((worker_id, task_id)) = owned else {
            return Ok(None);
        };
        Ok(Some(
            self.record_worker_activity(&worker_id, task_id, tool).await,
        ))
    }

    async fn record_worker_activity(
        &self,
        worker_id: &WorkerId,
        task_id: Option<TaskId>,
        tool: &str,
    ) -> WorkerActivitySnapshot {
        let now_ms = worker_unix_time_ms();
        let mut activity = self.activity.lock().await;
        let snapshot = activity.entry(worker_id.clone()).or_default();
        snapshot.last_activity_at_ms = Some(now_ms);
        snapshot.recent_tool_calls.push(WorkerToolActivity {
            at_ms: now_ms,
            tool: tool.to_string(),
            task_id,
        });
        if snapshot.recent_tool_calls.len() > MAX_RECENT_WORKER_TOOL_ACTIVITY {
            let remove = snapshot.recent_tool_calls.len() - MAX_RECENT_WORKER_TOOL_ACTIVITY;
            snapshot.recent_tool_calls.drain(0..remove);
        }
        snapshot.clone()
    }

    pub async fn activity_for_family(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<BTreeMap<WorkerId, WorkerActivitySnapshot>, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let worker_ids = {
            let guard = self.data.lock().await;
            guard
                .families
                .values()
                .find(|family| {
                    &family.workspace_id == workspace_id
                        && &family.anchor_identity == anchor_identity
                })
                .map(|family| family.workers.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let activity = self.activity.lock().await;
        Ok(worker_ids
            .into_iter()
            .filter_map(|worker_id| {
                activity
                    .get(&worker_id)
                    .cloned()
                    .map(|snapshot| (worker_id, snapshot))
            })
            .collect())
    }

    pub async fn begin_recovery_guard(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
        guard_ms: u64,
    ) -> Result<WorkerRecoveryGuard, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Running || worker.current_task_id.as_ref() != Some(task_id)
        {
            return Err(WorkerBrokerError::Conflict(
                "worker recovery guard requires the exact currently running task".into(),
            ));
        }
        let now = worker_unix_time_ms();
        if worker.recovery_guard.as_ref().is_some_and(|existing| {
            existing.task_id == *task_id && now.saturating_sub(existing.started_at_ms) < guard_ms
        }) {
            return Err(WorkerBrokerError::Conflict(
                "worker recovery was already attempted recently for this exact task; observe status/collect before another recovery"
                    .into(),
            ));
        }

        let recovery_guard = WorkerRecoveryGuard {
            task_id: task_id.clone(),
            started_at_ms: now,
        };
        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.recovery_guard = Some(recovery_guard.clone());
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(recovery_guard)
    }

    pub async fn clear_recovery_guard(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        task_id: &TaskId,
        started_at_ms: u64,
    ) -> Result<(), WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if !worker.recovery_guard.as_ref().is_some_and(|existing| {
            existing.task_id == *task_id && existing.started_at_ms == started_at_ms
        }) {
            return Ok(());
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.recovery_guard = None;
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(())
    }

    pub async fn retire_worker(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        worker_id: &WorkerId,
        expected_pending_task: Option<&TaskId>,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        anchor_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity != anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state == WorkerState::Retired {
            return Ok(worker.clone());
        }
        match expected_pending_task {
            None => {
                if worker.state != WorkerState::Idle || worker.current_task_id.is_some() {
                    return Err(WorkerBrokerError::Conflict(
                        "worker can only be retired while idle or after a proven pre-send launch cancellation"
                            .into(),
                    ));
                }
            }
            Some(task_id) => {
                if !matches!(
                    worker.state,
                    WorkerState::Provisioning | WorkerState::Waking
                ) || worker.current_task_id.as_ref() != Some(task_id)
                    || worker
                        .tasks
                        .get(task_id)
                        .is_none_or(|task| task.state != TaskState::Pending)
                {
                    return Err(WorkerBrokerError::Conflict(
                        "worker pending task changed before retirement could be committed".into(),
                    ));
                }
            }
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if let Some(task_id) = expected_pending_task {
            let task = worker
                .tasks
                .get_mut(task_id)
                .ok_or(WorkerBrokerError::NotFound)?;
            task.state = TaskState::Failed;
            worker.current_task_id = None;
        }
        worker.claim_token = None;
        worker.claim_operation_id = None;
        worker.state = WorkerState::Retired;
        worker.attachment_state = BrowserAttachmentState::Absent;
        let retired = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(retired)
    }

    pub async fn purge_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<usize, WorkerBrokerError> {
        let mut guard = self.data.lock().await;
        let removed = guard
            .families
            .values()
            .filter(|family| &family.workspace_id == workspace_id)
            .count();
        if removed == 0 {
            return Ok(0);
        }
        let mut candidate = guard.clone();
        candidate
            .families
            .retain(|_, family| &family.workspace_id != workspace_id);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(removed)
    }

    pub async fn inactive_cleanup_preview_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<InactiveWorkerCleanupPlan, WorkerBrokerError> {
        workspace_id
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        let families = guard
            .families
            .iter()
            .filter(|(_, family)| {
                &family.workspace_id == workspace_id
                    && family_is_safe_for_explicit_host_cleanup(family)
            })
            .map(|(family_id, family)| InactiveWorkerCleanupFamily {
                family_id: family_id.clone(),
                session_digest: family.anchor_identity.session_digest.clone(),
                expected_family: family.clone(),
            })
            .collect::<Vec<_>>();
        let summary =
            cleanup_summary_for_families(families.iter().map(|entry| &entry.expected_family));
        Ok(InactiveWorkerCleanupPlan {
            workspace_id: workspace_id.clone(),
            summary,
            families,
        })
    }

    pub async fn ensure_inactive_cleanup_plan_unchanged(
        &self,
        plan: &InactiveWorkerCleanupPlan,
    ) -> Result<(), WorkerBrokerError> {
        plan.workspace_id
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        ensure_cleanup_plan_matches(&guard, plan)
    }

    pub async fn cleanup_inactive_families_for_plan(
        &self,
        plan: &InactiveWorkerCleanupPlan,
    ) -> Result<InactiveWorkerCleanupSummary, WorkerBrokerError> {
        plan.workspace_id
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        ensure_cleanup_plan_matches(&guard, plan)?;
        if plan.families.is_empty() {
            return Ok(InactiveWorkerCleanupSummary::default());
        }

        let mut candidate = guard.clone();
        for entry in &plan.families {
            candidate.families.remove(&entry.family_id);
        }
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(plan.summary.clone())
    }

    #[cfg(test)]
    pub async fn ensure_clearable_session_digest(
        &self,
        session_digest: &str,
    ) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        if session_digest.len() != 64
            || !session_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(WorkerBrokerError::Invalid(
                "worker Core session digest is invalid".into(),
            ));
        }
        let guard = self.data.lock().await;
        let workers = guard
            .families
            .values()
            .filter(|family| family.anchor_identity.session_digest == session_digest)
            .flat_map(|family| family.workers.values().cloned())
            .collect::<Vec<_>>();
        if workers.iter().any(|worker| match worker.state {
            WorkerState::Idle | WorkerState::Retired => false,
            WorkerState::Provisioning | WorkerState::Waking => {
                worker.launch_state != WorkerLaunchState::Failed
                    || worker.conversation_url.is_some()
            }
            _ => true,
        }) {
            return Err(WorkerBrokerError::Conflict(
                "Core workers cannot be cleared while a worker is active or a launch outcome is unresolved"
                    .into(),
            ));
        }
        Ok(workers)
    }

    #[cfg(test)]
    pub async fn clear_session_digest(
        &self,
        session_digest: &str,
    ) -> Result<Vec<WorkerRecord>, WorkerBrokerError> {
        let workers = self.ensure_clearable_session_digest(session_digest).await?;
        if workers.is_empty() {
            return Ok(workers);
        }
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();
        candidate
            .families
            .retain(|_, family| family.anchor_identity.session_digest != session_digest);
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(workers)
    }

    pub(crate) async fn pending_claim_registration(
        &self,
        workspace_id: &WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        claim_operation_id: &OperationId,
        worker_identity: &ChatIdentity,
    ) -> Result<Option<PendingWorkerClaimRegistration>, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity == worker_identity {
            return Err(WorkerBrokerError::Conflict(
                "the Core conversation cannot claim one of its own workers".into(),
            ));
        }
        ensure_worker_identity_unique_binding(&guard, &family_id, worker_id, worker_identity)?;
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.current_task_id.as_ref() != Some(task_id) || !worker.tasks.contains_key(task_id) {
            return Err(WorkerBrokerError::NotFound);
        }
        if let Some(existing) = &worker.chat_identity {
            if existing == worker_identity {
                return Ok(None);
            }
            return Err(WorkerBrokerError::Conflict(
                "worker has already been claimed by another ChatGPT conversation".into(),
            ));
        }
        ensure_worker_family_account_affinity(family, worker_id, worker_identity)?;
        let Some(expected_operation_id) = worker.claim_operation_id.as_ref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim predates browser-attested admission; leave this stale worker untouched and create a fresh worker"
                    .into(),
            ));
        };
        if expected_operation_id != claim_operation_id {
            return Err(WorkerBrokerError::NotFound);
        }
        let Some(expected_claim_token) = worker.claim_token.as_deref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim token has already been consumed".into(),
            ));
        };
        if claim_token.len() != expected_claim_token.len()
            || !bool::from(
                claim_token
                    .as_bytes()
                    .ct_eq(expected_claim_token.as_bytes()),
            )
        {
            return Err(WorkerBrokerError::NotFound);
        }
        match worker.launch_state {
            WorkerLaunchState::WaitingClaim | WorkerLaunchState::Claimed => Ok(None),
            WorkerLaunchState::Unknown
            | WorkerLaunchState::Queued
            | WorkerLaunchState::Preparing
            | WorkerLaunchState::SendStarted
            | WorkerLaunchState::Reconciling => {
                let launch_command_id = worker.launch_command_id.clone().ok_or_else(|| {
                    WorkerBrokerError::Conflict(
                        "worker registration is pending but has no linked browser launch command"
                            .into(),
                    )
                })?;
                Ok(Some(PendingWorkerClaimRegistration {
                    launch_command_id,
                    operation_id: expected_operation_id.clone(),
                }))
            }
            WorkerLaunchState::Paused | WorkerLaunchState::Failed => {
                Err(WorkerBrokerError::Conflict(format!(
                    "worker launch cannot be claimed because registration ended in {:?}",
                    worker.launch_state
                )))
            }
        }
    }

    pub(crate) async fn claim_attestation_requirement(
        &self,
        workspace_id: &WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        claim_operation_id: &OperationId,
        worker_identity: &ChatIdentity,
    ) -> Result<Option<WorkerClaimAttestationRequirement>, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if &family.anchor_identity == worker_identity {
            return Err(WorkerBrokerError::Conflict(
                "the Core conversation cannot claim one of its own workers".into(),
            ));
        }
        ensure_worker_identity_unique_binding(&guard, &family_id, worker_id, worker_identity)?;
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.current_task_id.as_ref() != Some(task_id) || !worker.tasks.contains_key(task_id) {
            return Err(WorkerBrokerError::NotFound);
        }
        if let Some(existing) = &worker.chat_identity {
            if existing == worker_identity {
                return Ok(None);
            }
            return Err(WorkerBrokerError::Conflict(
                "worker has already been claimed by another ChatGPT conversation".into(),
            ));
        }
        ensure_worker_family_account_affinity(family, worker_id, worker_identity)?;
        match worker.launch_state {
            WorkerLaunchState::WaitingClaim => {}
            WorkerLaunchState::Unknown
            | WorkerLaunchState::Queued
            | WorkerLaunchState::Preparing
            | WorkerLaunchState::SendStarted
            | WorkerLaunchState::Reconciling => {
                return Err(WorkerBrokerError::Conflict(
                    CLAIM_REGISTRATION_PENDING_MESSAGE.into(),
                ));
            }
            WorkerLaunchState::Paused | WorkerLaunchState::Failed => {
                return Err(WorkerBrokerError::Conflict(format!(
                    "worker launch cannot be claimed because registration ended in {:?}",
                    worker.launch_state
                )));
            }
            WorkerLaunchState::Claimed => {
                return Err(WorkerBrokerError::Conflict(
                    "worker launch is already claimed but has no bound worker identity".into(),
                ));
            }
        }
        let conversation_id = worker
            .conversation_url
            .as_deref()
            .and_then(canonical_conversation_id_from_url)
            .ok_or_else(|| {
                WorkerBrokerError::Conflict(CLAIM_REGISTRATION_PENDING_MESSAGE.into())
            })?;
        let Some(expected_operation_id) = worker.claim_operation_id.as_ref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim predates browser-attested admission; leave this stale worker untouched and create a fresh worker"
                    .into(),
            ));
        };
        if expected_operation_id != claim_operation_id {
            return Err(WorkerBrokerError::NotFound);
        }
        let Some(expected_claim_token) = worker.claim_token.as_deref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim token has already been consumed".into(),
            ));
        };
        if claim_token.len() != expected_claim_token.len()
            || !bool::from(
                claim_token
                    .as_bytes()
                    .ct_eq(expected_claim_token.as_bytes()),
            )
        {
            return Err(WorkerBrokerError::NotFound);
        }
        Ok(Some(WorkerClaimAttestationRequirement {
            operation_id: expected_operation_id.clone(),
            conversation_id,
        }))
    }

    pub async fn claim_worker_attested(
        &self,
        workspace_id: &WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        claim_operation_id: &OperationId,
        worker_identity: ChatIdentity,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        worker_identity
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if family.anchor_identity == worker_identity {
            return Err(WorkerBrokerError::Conflict(
                "the Core conversation cannot claim one of its own workers".into(),
            ));
        }
        ensure_worker_identity_unique_binding(&guard, &family_id, worker_id, &worker_identity)?;
        let worker = family
            .workers
            .get(worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.current_task_id.as_ref() != Some(task_id) || !worker.tasks.contains_key(task_id) {
            return Err(WorkerBrokerError::NotFound);
        }
        if let Some(existing) = &worker.chat_identity {
            if existing == &worker_identity {
                return Ok(worker.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "worker has already been claimed by another ChatGPT conversation".into(),
            ));
        }
        ensure_worker_family_account_affinity(family, worker_id, &worker_identity)?;
        if worker.launch_state != WorkerLaunchState::WaitingClaim
            || worker
                .conversation_url
                .as_deref()
                .and_then(canonical_conversation_id_from_url)
                .is_none()
        {
            return Err(WorkerBrokerError::Conflict(
                CLAIM_REGISTRATION_PENDING_MESSAGE.into(),
            ));
        }
        let Some(expected_operation_id) = worker.claim_operation_id.as_ref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim predates browser-attested admission; leave this stale worker untouched and create a fresh worker"
                    .into(),
            ));
        };
        if expected_operation_id != claim_operation_id {
            return Err(WorkerBrokerError::NotFound);
        }
        let Some(expected_claim_token) = worker.claim_token.as_deref() else {
            return Err(WorkerBrokerError::Conflict(
                "worker claim token has already been consumed".into(),
            ));
        };
        if claim_token.len() != expected_claim_token.len()
            || !bool::from(
                claim_token
                    .as_bytes()
                    .ct_eq(expected_claim_token.as_bytes()),
            )
        {
            return Err(WorkerBrokerError::NotFound);
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.chat_identity = Some(worker_identity);
        worker.claim_token = None;
        worker.claim_operation_id = None;
        worker.waiting_claim_since_ms = None;
        // Claim binds this exact ChatGPT conversation to the Worker, but it does not mean the
        // assignment has actually started. Keep the task pending until the Worker explicitly calls
        // `workers start`; project tools remain blocked by the Worker gateway until that transition.
        worker.state = WorkerState::Provisioning;
        worker.launch_state = WorkerLaunchState::Claimed;
        worker.launch_error = None;
        worker.attachment_state = BrowserAttachmentState::Attached;
        let task = worker
            .tasks
            .get_mut(task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Pending;
        let claimed = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        self.record_worker_activity(worker_id, Some(task_id.clone()), "workers:claim")
            .await;
        Ok(claimed)
    }

    #[cfg(test)]
    pub async fn claim_worker(
        &self,
        workspace_id: &WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        worker_identity: ChatIdentity,
    ) -> Result<WorkerRecord, WorkerBrokerError> {
        let claim_operation_id = {
            let guard = self.data.lock().await;
            let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
                .ok_or(WorkerBrokerError::NotFound)?;
            guard
                .families
                .get(&family_id)
                .and_then(|family| family.workers.get(worker_id))
                .and_then(|worker| worker.claim_operation_id.clone())
                .unwrap_or_else(OperationId::new)
        };
        let claimed = self
            .claim_worker_attested(
                workspace_id,
                worker_id,
                task_id,
                claim_token,
                &claim_operation_id,
                worker_identity.clone(),
            )
            .await?;
        if claimed.state == WorkerState::Running {
            return Ok(claimed);
        }
        self.start_task(workspace_id, &worker_identity, worker_id, task_id)
            .await
    }

    pub async fn message_worker(
        &self,
        request: MessageWorkerRequest,
    ) -> Result<MessageReceipt, WorkerBrokerError> {
        if request.body.is_empty() || request.body.len() > MAX_WORKER_MESSAGE_BYTES {
            return Err(WorkerBrokerError::Invalid(format!(
                "worker message must contain 1..={MAX_WORKER_MESSAGE_BYTES} bytes"
            )));
        }
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, &request.workspace_id, &request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if family.anchor_identity != request.anchor_identity {
            return Err(WorkerBrokerError::NotFound);
        }
        if let Some(receipt) = family.message_requests.get(&request.operation_id) {
            if receipt.request_fingerprint == fingerprint {
                return Ok(receipt.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "worker message operation id was reused with different input".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = family
            .workers
            .get_mut(&request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state == WorkerState::Retired {
            return Err(WorkerBrokerError::Conflict(
                "cannot message a retired worker".into(),
            ));
        }
        if worker.messages.len() >= MAX_PENDING_MESSAGES_PER_WORKER {
            return Err(WorkerBrokerError::Limit(format!(
                "worker inbox is limited to {MAX_PENDING_MESSAGES_PER_WORKER} pending messages"
            )));
        }

        let message_id = WorkerMessageId::new();
        worker.messages.push(WorkerMessage {
            id: message_id.clone(),
            body: request.body,
            state: WorkerMessageState::Pending,
        });
        let sequence = allocate_receipt_sequence(family)?;
        let receipt = MessageReceipt {
            request_fingerprint: fingerprint,
            sequence,
            worker_id: request.worker_id,
            message_id,
        };
        family
            .message_requests
            .insert(request.operation_id, receipt.clone());
        compact_family_history(family)?;

        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
    }

    pub async fn pending_messages(
        &self,
        workspace_id: &WorkspaceId,
        worker_identity: &ChatIdentity,
        worker_id: &WorkerId,
    ) -> Result<Vec<WorkerMessage>, WorkerBrokerError> {
        let guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = guard
            .families
            .get(&family_id)
            .and_then(|family| family.workers.get(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.as_ref() != Some(worker_identity) {
            return Err(WorkerBrokerError::NotFound);
        }
        Ok(worker
            .messages
            .iter()
            .filter(|message| message.state == WorkerMessageState::Pending)
            .cloned()
            .collect())
    }

    pub async fn acknowledge_message(
        &self,
        workspace_id: &WorkspaceId,
        worker_identity: &ChatIdentity,
        worker_id: &WorkerId,
        message_id: &WorkerMessageId,
    ) -> Result<(), WorkerBrokerError> {
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, workspace_id, worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = guard
            .families
            .get(&family_id)
            .and_then(|family| family.workers.get(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.as_ref() != Some(worker_identity) {
            return Err(WorkerBrokerError::NotFound);
        }
        if !worker
            .messages
            .iter()
            .any(|message| &message.id == message_id)
        {
            return Ok(());
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        worker.messages.retain(|message| &message.id != message_id);
        self.commit_candidate(&mut guard, candidate).await
    }

    pub async fn report_worker(
        &self,
        request: ReportWorkerRequest,
    ) -> Result<ReportReceipt, WorkerBrokerError> {
        if request.body.is_empty() || request.body.len() > MAX_WORKER_MESSAGE_BYTES {
            return Err(WorkerBrokerError::Invalid(format!(
                "worker report must contain 1..={MAX_WORKER_MESSAGE_BYTES} bytes"
            )));
        }
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, &request.workspace_id, &request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = family
            .workers
            .get(&request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.as_ref() != Some(&request.worker_identity)
            || worker.current_task_id.as_ref() != Some(&request.task_id)
        {
            return Err(WorkerBrokerError::NotFound);
        }
        let task = worker
            .tasks
            .get(&request.task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.state != WorkerState::Running || task.state != TaskState::Running {
            return Err(WorkerBrokerError::Conflict(
                "worker task must complete workers start before reporting progress".into(),
            ));
        }
        if let Some(receipt) = family.report_requests.get(&request.operation_id) {
            if receipt.request_fingerprint == fingerprint {
                return Ok(receipt.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "worker report operation id was reused with different input".into(),
            ));
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        compact_family_history(family)?;
        if family.reports.len() >= MAX_REPORTS_PER_FAMILY {
            return Err(WorkerBrokerError::Limit(format!(
                "worker family is limited to {MAX_REPORTS_PER_FAMILY} uncollected reports; collect existing updates before reporting more"
            )));
        }
        let report_id = WorkerReportId::new();
        family.reports.push(WorkerReport {
            id: report_id.clone(),
            worker_id: request.worker_id.clone(),
            task_id: request.task_id,
            body: request.body,
            collected: false,
        });
        let sequence = allocate_receipt_sequence(family)?;
        let receipt = ReportReceipt {
            request_fingerprint: fingerprint,
            sequence,
            worker_id: request.worker_id,
            report_id,
        };
        family
            .report_requests
            .insert(request.operation_id, receipt.clone());
        compact_family_history(family)?;
        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(receipt)
    }

    #[cfg(test)]
    async fn collect_updates(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<CollectedUpdates, WorkerBrokerError> {
        let operation_id = OperationId::new();
        self.collect_updates_for_operation(workspace_id, anchor_identity, &operation_id)
            .await
    }

    async fn collect_updates_for_operation(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        operation_id: &OperationId,
    ) -> Result<CollectedUpdates, WorkerBrokerError> {
        let fingerprint = request_fingerprint(&(
            "collect",
            workspace_id.as_str(),
            anchor_identity.session_digest.as_str(),
        ))?;
        let mut guard = self.data.lock().await;
        let family_id = guard
            .families
            .values()
            .find(|family| {
                &family.workspace_id == workspace_id && &family.anchor_identity == anchor_identity
            })
            .map(|family| family.id.clone())
            .ok_or(WorkerBrokerError::NotFound)?;
        let family = guard
            .families
            .get(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if let Some(receipt) = family.collect_requests.get(operation_id) {
            if receipt.request_fingerprint != fingerprint {
                return Err(WorkerBrokerError::Conflict(
                    "worker collect operation id was reused with different input".into(),
                ));
            }
            return Ok(collected_updates_from_receipt(receipt));
        }

        let reports = family
            .reports
            .iter()
            .filter(|report| !report.collected)
            .take(MAX_UPDATES_PER_COLLECT)
            .cloned()
            .collect::<Vec<_>>();
        let mut completed = Vec::new();
        let mut failed = Vec::new();
        'workers: for worker in family.workers.values() {
            for task in worker.tasks.values() {
                if task.collected {
                    continue;
                }
                if let Some(result) = &task.result {
                    completed.push(CollectedTaskReceipt {
                        worker_id: worker.id.clone(),
                        display_id: worker.display_id.clone(),
                        task_id: task.id.clone(),
                        result: result.clone(),
                    });
                    if completed.len() >= MAX_UPDATES_PER_COLLECT {
                        break 'workers;
                    }
                } else if let Some(failure) = &task.failure {
                    failed.push(CollectedTaskFailureReceipt {
                        worker_id: worker.id.clone(),
                        display_id: worker.display_id.clone(),
                        task_id: task.id.clone(),
                        failure: failure.clone(),
                    });
                    if failed.len() >= MAX_UPDATES_PER_COLLECT {
                        break 'workers;
                    }
                }
            }
        }
        if reports.is_empty() && completed.is_empty() && failed.is_empty() {
            return Ok(CollectedUpdates {
                reports,
                completed: Vec::new(),
                failed: Vec::new(),
            });
        }

        let report_ids = reports
            .iter()
            .map(|report| report.id.clone())
            .collect::<BTreeSet<_>>();
        let task_ids = completed
            .iter()
            .map(|task| task.task_id.clone())
            .chain(failed.iter().map(|task| task.task_id.clone()))
            .collect::<BTreeSet<_>>();
        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        family
            .reports
            .retain(|report| !report_ids.contains(&report.id));
        for worker in family.workers.values_mut() {
            for task in worker.tasks.values_mut() {
                if task_ids.contains(&task.id) {
                    task.collected = true;
                }
            }
        }
        let sequence = allocate_receipt_sequence(family)?;
        let receipt = CollectReceipt {
            request_fingerprint: fingerprint,
            sequence,
            reports,
            completed,
            failed,
        };
        family
            .collect_requests
            .insert(operation_id.clone(), receipt.clone());
        compact_family_history(family)?;
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(collected_updates_from_receipt(&receipt))
    }

    fn enabled_update_waiter(&self) -> impl std::future::Future<Output = ()> + '_ {
        let mut notified = Box::pin(self.updates.notified());
        notified.as_mut().enable();
        notified
    }

    pub async fn collect_updates_wait(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        operation_id: &OperationId,
        wait_ms: u64,
    ) -> Result<CollectedUpdates, WorkerBrokerError> {
        const MAX_WAIT_MS: u64 = 60_000;
        if wait_ms > MAX_WAIT_MS {
            return Err(WorkerBrokerError::Invalid(format!(
                "worker collect wait_ms cannot exceed {MAX_WAIT_MS}"
            )));
        }
        if wait_ms == 0 {
            return self
                .collect_updates_for_operation(workspace_id, anchor_identity, operation_id)
                .await;
        }

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(wait_ms);
        loop {
            // Register and enable the waiter before inspecting durable state. Tokio's Notify can
            // otherwise hand a permit to a later waiter between Notified creation and first poll.
            let notified = self.enabled_update_waiter();
            let updates = self
                .collect_updates_for_operation(workspace_id, anchor_identity, operation_id)
                .await?;
            if !updates.reports.is_empty()
                || !updates.completed.is_empty()
                || !updates.failed.is_empty()
            {
                return Ok(updates);
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(updates);
            }
            if tokio::time::timeout(deadline - now, notified)
                .await
                .is_err()
            {
                return Ok(CollectedUpdates {
                    reports: Vec::new(),
                    completed: Vec::new(),
                    failed: Vec::new(),
                });
            }
        }
    }

    pub async fn finish_task(
        &self,
        request: FinishTaskRequest,
    ) -> Result<WorkerResult, WorkerBrokerError> {
        request
            .result
            .validate()
            .map_err(WorkerBrokerError::Invalid)?;
        let mut guard = self.data.lock().await;
        let family_id = find_family_for_worker(&guard, &request.workspace_id, &request.worker_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        let worker = guard
            .families
            .get(&family_id)
            .and_then(|family| family.workers.get(&request.worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        if worker.chat_identity.as_ref() != Some(&request.worker_identity) {
            return Err(WorkerBrokerError::NotFound);
        }
        let task = worker
            .tasks
            .get(&request.task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        if let Some(existing) = &task.result {
            if existing == &request.result {
                return Ok(existing.clone());
            }
            return Err(WorkerBrokerError::Conflict(
                "completed worker task was finished again with a different result".into(),
            ));
        }
        if task.state == TaskState::Failed || task.failure.is_some() {
            return Err(WorkerBrokerError::Conflict(
                "failed worker task cannot be finished by a late Worker turn".into(),
            ));
        }
        if worker.current_task_id.as_ref() != Some(&request.task_id)
            || worker.state != WorkerState::Running
            || task.state != TaskState::Running
        {
            return Err(WorkerBrokerError::Conflict(
                "worker task must complete workers start before it can finish".into(),
            ));
        }

        let mut candidate = guard.clone();
        let worker = candidate
            .families
            .get_mut(&family_id)
            .and_then(|family| family.workers.get_mut(&request.worker_id))
            .ok_or(WorkerBrokerError::NotFound)?;
        let task = worker
            .tasks
            .get_mut(&request.task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Completed;
        task.result = Some(request.result.clone());
        task.failure = None;
        if worker.current_task_id.as_ref() == Some(&request.task_id) {
            worker.current_task_id = None;
            if worker
                .recovery_guard
                .as_ref()
                .is_some_and(|guard| guard.task_id == request.task_id)
            {
                worker.recovery_guard = None;
            }
            worker.state = WorkerState::Idle;
            if worker.attachment_state == BrowserAttachmentState::Detached {
                worker.attachment_state = BrowserAttachmentState::Absent;
            }
        }

        self.commit_candidate(&mut guard, candidate).await?;
        self.updates.notify_waiters();
        Ok(request.result)
    }

    async fn commit_candidate(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, WorkerStoreData>,
        candidate: WorkerStoreData,
    ) -> Result<(), WorkerBrokerError> {
        #[cfg(test)]
        if self
            .fail_next_commit
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(WorkerBrokerError::Storage(
                "injected worker persistence failure".into(),
            ));
        }
        let path = self.path.clone();
        let persisted = candidate.clone();
        let prepared = tokio::task::spawn_blocking(move || store::prepare_save(&path, &persisted))
            .await
            .map_err(|error| {
                WorkerBrokerError::Storage(format!("worker state persistence task failed: {error}"))
            })?
            .map_err(storage_error)?;
        // The only await is before the canonical store changes. Once execution resumes, commit the
        // prepared temp file and publish the identical in-memory candidate without another
        // cancellation point, so a dropped HTTP/MCP future cannot leave disk ahead of memory.
        store::commit_prepared(prepared).map_err(storage_error)?;
        **guard = candidate;
        Ok(())
    }
}

fn ensure_worker_identity_unique_binding(
    data: &WorkerStoreData,
    current_family_id: &WorkerFamilyId,
    current_worker_id: &WorkerId,
    worker_identity: &ChatIdentity,
) -> Result<(), WorkerBrokerError> {
    let bound_elsewhere = data.families.iter().any(|(family_id, family)| {
        family.workers.iter().any(|(worker_id, worker)| {
            worker.chat_identity.as_ref() == Some(worker_identity)
                && (family_id != current_family_id || worker_id != current_worker_id)
        })
    });
    if bound_elsewhere {
        return Err(WorkerBrokerError::Conflict(
            "this ChatGPT conversation is already bound to another MoonDesk Worker; start a fresh Worker conversation"
                .into(),
        ));
    }
    Ok(())
}

fn ensure_worker_family_account_affinity(
    family: &WorkerFamily,
    claiming_worker_id: &WorkerId,
    worker_identity: &ChatIdentity,
) -> Result<(), WorkerBrokerError> {
    let subjects = family
        .workers
        .iter()
        .filter(|(worker_id, _)| *worker_id != claiming_worker_id)
        .filter_map(|(_, worker)| {
            worker
                .chat_identity
                .as_ref()
                .and_then(|identity| identity.subject_digest.as_deref())
        })
        .collect::<BTreeSet<_>>();
    if subjects.len() > 1 {
        return Err(WorkerBrokerError::Conflict(
            "this Worker family already contains claimed Workers from multiple ChatGPT accounts; Clear Workers before admitting another Worker"
                .into(),
        ));
    }
    let Some(expected_subject) = subjects.into_iter().next() else {
        return Ok(());
    };
    match worker_identity.subject_digest.as_deref() {
        Some(actual_subject) if actual_subject == expected_subject => Ok(()),
        Some(_) => Err(WorkerBrokerError::Conflict(
            "worker claim came from a different ChatGPT account than this Worker family. Restore the fixed Worker browser account or Clear Workers before changing Worker accounts"
                .into(),
        )),
        None => Err(WorkerBrokerError::Conflict(
            "worker claim could not verify the fixed Worker account because OpenAI subject metadata is missing; retry from the existing Worker account"
                .into(),
        )),
    }
}

fn worker_unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn retired_worker_fence_is_live(fence: &RetiredWorkerFence, now_ms: u64) -> bool {
    now_ms.saturating_sub(fence.retired_at_ms) < RETIRED_WORKER_FENCE_TTL_MS
}

fn prune_retired_worker_fences(data: &mut WorkerStoreData, now_ms: u64) {
    data.retired_workers
        .retain(|_, fence| retired_worker_fence_is_live(fence, now_ms));
}

fn collected_updates_from_receipt(receipt: &CollectReceipt) -> CollectedUpdates {
    CollectedUpdates {
        reports: receipt.reports.clone(),
        completed: receipt
            .completed
            .iter()
            .map(|update| CollectedWorkerUpdate {
                worker_id: update.worker_id.clone(),
                display_id: update.display_id.clone(),
                task_id: update.task_id.clone(),
                result: update.result.clone(),
            })
            .collect(),
        failed: receipt
            .failed
            .iter()
            .map(|update| CollectedWorkerFailure {
                worker_id: update.worker_id.clone(),
                display_id: update.display_id.clone(),
                task_id: update.task_id.clone(),
                failure: update.failure.clone(),
            })
            .collect(),
    }
}

fn task_is_settled_and_delivered(task: &WorkerTask) -> bool {
    match task.state {
        TaskState::Completed => task.result.is_some() && task.collected,
        TaskState::Failed => {
            if task.result.is_some() || task.failure.is_some() {
                task.collected
            } else {
                true
            }
        }
        TaskState::Pending | TaskState::Running | TaskState::Blocked => false,
    }
}

fn collect_receipt_has_payload(receipt: &CollectReceipt) -> bool {
    !receipt.reports.is_empty() || !receipt.completed.is_empty() || !receipt.failed.is_empty()
}

fn family_is_safe_for_explicit_host_cleanup(family: &WorkerFamily) -> bool {
    family.reports.is_empty()
        && family.workers.values().all(|worker| {
            matches!(worker.state, WorkerState::Idle | WorkerState::Retired)
                && matches!(
                    worker.launch_state,
                    WorkerLaunchState::Claimed | WorkerLaunchState::Failed
                )
                && worker.current_task_id.is_none()
                && worker.claim_token.is_none()
                && worker.messages.is_empty()
                && worker.tasks.values().all(task_is_settled_and_delivered)
        })
}

fn retired_family_is_reclaimable(family: &WorkerFamily) -> bool {
    // Automatic capacity reclamation must honor the documented collect replay window. An explicit
    // local Settings cleanup may discard those receipts only after showing their count and requiring
    // typed confirmation, but unrelated Core activity must never evict a replayable payload early.
    family_is_safe_for_explicit_host_cleanup(family)
        && family
            .workers
            .values()
            .all(|worker| worker.state == WorkerState::Retired)
        && !family
            .collect_requests
            .values()
            .any(collect_receipt_has_payload)
}

fn family_can_be_freed_by_user_cleanup(family: &WorkerFamily) -> bool {
    family_is_safe_for_explicit_host_cleanup(family)
}

fn cleanup_summary_for_families<'a>(
    families: impl IntoIterator<Item = &'a WorkerFamily>,
) -> InactiveWorkerCleanupSummary {
    let mut summary = InactiveWorkerCleanupSummary::default();
    for family in families {
        summary.family_count += 1;
        summary.worker_count += family.workers.len();
        summary.replay_receipt_count += family.collect_requests.len();
    }
    summary
}

fn ensure_cleanup_plan_matches(
    data: &WorkerStoreData,
    plan: &InactiveWorkerCleanupPlan,
) -> Result<(), WorkerBrokerError> {
    for entry in &plan.families {
        let Some(current) = data.families.get(&entry.family_id) else {
            return Err(WorkerBrokerError::Conflict(
                "inactive Worker cleanup preview changed; review and confirm the cleanup again"
                    .into(),
            ));
        };
        if current != &entry.expected_family
            || current.workspace_id != plan.workspace_id
            || current.anchor_identity.session_digest != entry.session_digest
            || !family_is_safe_for_explicit_host_cleanup(current)
        {
            return Err(WorkerBrokerError::Conflict(
                "inactive Worker cleanup preview changed; review and confirm the cleanup again"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn compact_inactive_families(data: &mut WorkerStoreData) {
    while data.families.len() >= MAX_WORKER_FAMILIES {
        let removable = data
            .families
            .iter()
            .find(|(_, family)| retired_family_is_reclaimable(family))
            .map(|(family_id, _)| family_id.clone());
        let Some(family_id) = removable else {
            break;
        };
        data.families.remove(&family_id);
    }
}

fn allocate_receipt_sequence(family: &mut WorkerFamily) -> Result<u64, WorkerBrokerError> {
    let sequence = family.next_receipt_sequence;
    family.next_receipt_sequence = family
        .next_receipt_sequence
        .checked_add(1)
        .ok_or_else(|| WorkerBrokerError::Limit("worker receipt sequence is exhausted".into()))?;
    Ok(sequence)
}

fn prune_receipt_map<T, F, S>(
    map: &mut std::collections::BTreeMap<OperationId, T>,
    limit: usize,
    is_protected: F,
    sequence: S,
) -> bool
where
    F: Fn(&T) -> bool,
    S: Fn(&T) -> u64,
{
    while map.len() > limit {
        let removable = map
            .iter()
            .filter(|(_, receipt)| !is_protected(receipt))
            .min_by_key(|(_, receipt)| sequence(receipt))
            .map(|(operation_id, _)| operation_id.clone());
        let Some(operation_id) = removable else {
            return false;
        };
        map.remove(&operation_id);
    }
    true
}

fn compact_family_history(family: &mut WorkerFamily) -> Result<(), WorkerBrokerError> {
    // Older experimental stores marked delivered reports instead of removing them. They are no
    // longer replayable obligations, so reclaim them before applying the bounded-history policy.
    family.reports.retain(|report| !report.collected);

    let task_sequences = family
        .spawn_requests
        .values()
        .map(|receipt| (receipt.task_id.clone(), receipt.sequence))
        .chain(
            family
                .reuse_requests
                .values()
                .map(|receipt| (receipt.task_id.clone(), receipt.sequence)),
        )
        .collect::<std::collections::BTreeMap<_, _>>();

    for worker in family.workers.values_mut() {
        let mut collected = worker
            .tasks
            .values()
            .filter(|task| task.collected && worker.current_task_id.as_ref() != Some(&task.id))
            .map(|task| {
                (
                    task.id.clone(),
                    task_sequences.get(&task.id).copied().unwrap_or(0),
                )
            })
            .collect::<Vec<_>>();
        collected.sort_by_key(|(_, sequence)| std::cmp::Reverse(*sequence));
        let keep = collected
            .into_iter()
            .take(MAX_COLLECTED_TASK_HISTORY_PER_WORKER)
            .map(|(task_id, _)| task_id)
            .collect::<BTreeSet<_>>();
        worker.tasks.retain(|task_id, task| {
            !task.collected
                || worker.current_task_id.as_ref() == Some(task_id)
                || keep.contains(task_id)
        });
        if worker.tasks.len() > MAX_TASK_RECORDS_PER_WORKER {
            return Err(WorkerBrokerError::Limit(format!(
                "worker task history is limited to {MAX_TASK_RECORDS_PER_WORKER} records"
            )));
        }
    }

    let task_ids = family
        .workers
        .values()
        .flat_map(|worker| worker.tasks.keys().cloned())
        .collect::<BTreeSet<_>>();
    let message_ids = family
        .workers
        .values()
        .flat_map(|worker| worker.messages.iter().map(|message| message.id.clone()))
        .collect::<BTreeSet<_>>();
    let report_ids = family
        .reports
        .iter()
        .map(|report| report.id.clone())
        .collect::<BTreeSet<_>>();

    if !prune_receipt_map(
        &mut family.spawn_requests,
        MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY,
        |receipt| task_ids.contains(&receipt.task_id),
        |receipt| receipt.sequence,
    ) || !prune_receipt_map(
        &mut family.reuse_requests,
        MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY,
        |receipt| task_ids.contains(&receipt.task_id),
        |receipt| receipt.sequence,
    ) || !prune_receipt_map(
        &mut family.message_requests,
        MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY,
        |receipt| message_ids.contains(&receipt.message_id),
        |receipt| receipt.sequence,
    ) || !prune_receipt_map(
        &mut family.report_requests,
        MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY,
        |receipt| report_ids.contains(&receipt.report_id),
        |receipt| receipt.sequence,
    ) || !prune_receipt_map(
        &mut family.collect_requests,
        MAX_COLLECT_RECEIPTS_PER_FAMILY,
        |_| false,
        |receipt| receipt.sequence,
    ) {
        return Err(WorkerBrokerError::Limit(
            "worker idempotency history cannot be compacted while every retained receipt is still active"
                .into(),
        ));
    }
    Ok(())
}

fn expire_waiting_claims_in_data(
    data: &mut WorkerStoreData,
    now_ms: u64,
    scope: Option<(&WorkspaceId, &ChatIdentity)>,
) -> Vec<WorkerRecord> {
    let mut changed = Vec::new();
    for family in data.families.values_mut() {
        if scope.is_some_and(|(workspace_id, anchor_identity)| {
            &family.workspace_id != workspace_id || &family.anchor_identity != anchor_identity
        }) {
            continue;
        }
        for worker in family.workers.values_mut() {
            if worker.state != WorkerState::Provisioning
                || worker.launch_state != WorkerLaunchState::WaitingClaim
                || worker.chat_identity.is_some()
            {
                continue;
            }
            let started_at_ms = *worker.waiting_claim_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(started_at_ms) < WORKER_CLAIM_TIMEOUT_MS {
                // Persist the deadline anchor even for legacy records that predate this field.
                if started_at_ms == now_ms {
                    changed.push(worker.clone());
                }
                continue;
            }
            let task_id = worker.current_task_id.clone();
            if let Some(task_id) = task_id
                && let Some(task) = worker.tasks.get_mut(&task_id)
                && task.state == TaskState::Pending
            {
                task.state = TaskState::Failed;
                task.result = None;
                task.failure = Some(WorkerTaskFailure {
                    stage: "claim".into(),
                    error: "worker_claim_timeout_after_send".into(),
                    launch_command_id: worker.launch_command_id.clone(),
                });
                task.collected = false;
            }
            worker.current_task_id = None;
            worker.claim_token = None;
            worker.claim_operation_id = None;
            worker.waiting_claim_since_ms = None;
            worker.state = WorkerState::Retired;
            worker.launch_state = WorkerLaunchState::Failed;
            worker.launch_error = Some("worker_claim_timeout_after_send".into());
            worker.attachment_state = BrowserAttachmentState::Absent;
            changed.push(worker.clone());
        }
    }
    changed
}

fn worker_launch_failure_stage(error: &str) -> &'static str {
    if error.starts_with("model_or_effort_unconfirmed:")
        || error.starts_with("worker_prepare")
        || error.contains("composer_not_ready")
    {
        "prepare"
    } else if error.starts_with("worker_browser_launch")
        || error.starts_with("worker_account_")
        || error.starts_with("worker_source_url_")
        || error.starts_with("worker_thread_binding_")
    {
        "browser_launch"
    } else if error.starts_with("reconciliation_") {
        "reconcile"
    } else {
        "launch"
    }
}

fn new_claim_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn storage_error(error: std::io::Error) -> WorkerBrokerError {
    WorkerBrokerError::Storage(format!("failed to persist worker state: {error}"))
}

fn canonical_conversation_id_from_url(value: &str) -> Option<String> {
    canonical_chatgpt_conversation_id(value)
}

fn conversation_id_from_url(value: &str) -> Option<String> {
    canonical_conversation_id_from_url(value)
}

fn validate_spawn_request(request: &SpawnWorkerRequest) -> Result<(), WorkerBrokerError> {
    request
        .anchor_identity
        .validate()
        .map_err(WorkerBrokerError::Invalid)?;
    request
        .execution_profile
        .validate()
        .map_err(WorkerBrokerError::Invalid)?;
    if request.label.trim().is_empty() || request.label.len() > 128 {
        return Err(WorkerBrokerError::Invalid(
            "worker label must contain 1..=128 bytes".into(),
        ));
    }
    if request.assignment.is_empty() || request.assignment.len() > MAX_WORKER_ASSIGNMENT_BYTES {
        return Err(WorkerBrokerError::Invalid(format!(
            "worker assignment must contain 1..={MAX_WORKER_ASSIGNMENT_BYTES} bytes"
        )));
    }
    if request.context.len() > MAX_WORKER_CONTEXT_BYTES {
        return Err(WorkerBrokerError::Invalid(format!(
            "worker shared Core context must contain at most {MAX_WORKER_CONTEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn request_fingerprint<T: Serialize>(request: &T) -> Result<String, WorkerBrokerError> {
    let bytes = serde_json::to_vec(request).map_err(|error| {
        WorkerBrokerError::Invalid(format!("failed to fingerprint worker request: {error}"))
    })?;
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    Ok(output)
}

fn next_display_id(family: &WorkerFamily) -> Option<String> {
    (1..=MAX_WORKERS_PER_FAMILY)
        .map(|index| format!("worker-{index}"))
        .find(|candidate| {
            !family.workers.values().any(|worker| {
                worker.display_id == *candidate && worker.state != WorkerState::Retired
            })
        })
}

fn find_family_for_worker(
    data: &WorkerStoreData,
    workspace_id: &WorkspaceId,
    worker_id: &WorkerId,
) -> Option<WorkerFamilyId> {
    data.families
        .values()
        .find(|family| {
            &family.workspace_id == workspace_id && family.workers.contains_key(worker_id)
        })
        .map(|family| family.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_chat::types::ReasoningEffort;
    use uuid::Uuid;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()))
    }

    fn profile() -> WorkerExecutionProfile {
        WorkerExecutionProfile {
            model_key: "gpt-5.6-sol".into(),
            model_label: "GPT-5.6 Sol".into(),
            reasoning_effort: ReasoningEffort::High,
        }
    }

    fn anchor(name: &str) -> ChatIdentity {
        ChatIdentity::from_openai_meta(Some("test-subject"), name)
    }

    fn spawn_request(
        operation_id: OperationId,
        workspace_id: WorkspaceId,
        anchor_identity: ChatIdentity,
        assignment: &str,
    ) -> SpawnWorkerRequest {
        SpawnWorkerRequest {
            operation_id,
            workspace_id,
            anchor_identity,
            label: "audit".into(),
            assignment: assignment.into(),
            context: String::new(),
            execution_profile: profile(),
        }
    }

    fn finished_result() -> WorkerResult {
        WorkerResult {
            result: "clean".into(),
            changes: "none".into(),
            validation: "tests passed".into(),
            blockers: Vec::new(),
        }
    }

    fn inactive_family(
        workspace_id: WorkspaceId,
        anchor_identity: ChatIdentity,
        worker_state: WorkerState,
        result_collected: bool,
    ) -> WorkerFamily {
        let family_id = WorkerFamilyId::new();
        let worker_id = WorkerId::new();
        let task_id = TaskId::new();
        let task = WorkerTask {
            id: task_id.clone(),
            assignment: "completed history".into(),
            state: TaskState::Completed,
            result: Some(finished_result()),
            failure: None,
            collected: result_collected,
        };
        let worker = WorkerRecord {
            id: worker_id.clone(),
            display_id: "worker-1".into(),
            label: "inactive".into(),
            state: worker_state,
            attachment_state: BrowserAttachmentState::Absent,
            execution_profile: profile(),
            launch_state: WorkerLaunchState::Claimed,
            launch_command_id: None,
            launch_error: None,
            conversation_url: Some(format!("https://chatgpt.com/c/{worker_id}")),
            chat_identity: Some(anchor(&format!("worker-{worker_id}"))),
            claim_token: None,
            claim_operation_id: None,
            waiting_claim_since_ms: None,
            current_task_id: None,
            recovery_guard: None,
            tasks: [(task_id, task)].into_iter().collect(),
            messages: Vec::new(),
        };
        WorkerFamily {
            id: family_id,
            workspace_id,
            anchor_identity,
            next_receipt_sequence: 0,
            workers: [(worker_id, worker)].into_iter().collect(),
            reports: Vec::new(),
            spawn_requests: Default::default(),
            reuse_requests: Default::default(),
            message_requests: Default::default(),
            report_requests: Default::default(),
            collect_requests: Default::default(),
        }
    }

    async fn make_claimable(
        broker: &WorkerBroker,
        workspace: &WorkspaceId,
        anchor_identity: &ChatIdentity,
        receipt: &SpawnReceipt,
    ) {
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                workspace,
                anchor_identity,
                &receipt.worker_id,
                &receipt.task_id,
                &command_id,
            )
            .await
            .expect("link worker launch before claim");
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{}", receipt.worker_id)),
            )
            .await
            .expect("confirm canonical worker conversation before claim");
    }

    #[tokio::test]
    async fn worker_tool_activity_is_ephemeral_and_scoped_to_exact_worker_identity() {
        let root = temp_root("moondesk-worker-tool-activity");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-tool-activity");
        let worker_identity = anchor("worker-tool-activity");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "exercise activity telemetry",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &core, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        assert!(
            broker
                .record_tool_activity(&workspace, &anchor("unrelated-chat"), "read")
                .await
                .expect("ignore unrelated caller")
                .is_none()
        );
        broker
            .record_tool_activity(&workspace, &worker_identity, "read")
            .await
            .expect("record read");
        broker
            .record_tool_activity(&workspace, &worker_identity, "run_command")
            .await
            .expect("record command");

        let activity = broker
            .activity_for_family(&workspace, &core)
            .await
            .expect("read Core activity");
        let snapshot = activity.get(&spawned.worker_id).expect("worker activity");
        assert!(snapshot.last_activity_at_ms.is_some());
        assert_eq!(snapshot.recent_tool_calls.len(), 3);
        assert_eq!(snapshot.recent_tool_calls[0].tool, "workers:claim");
        assert_eq!(snapshot.recent_tool_calls[1].tool, "read");
        assert_eq!(snapshot.recent_tool_calls[2].tool, "run_command");
        assert!(
            snapshot
                .recent_tool_calls
                .iter()
                .all(|event| event.task_id.as_ref() == Some(&spawned.task_id))
        );

        drop(broker);
        let reopened = WorkerBroker::open(&path).expect("reopen worker broker");
        assert!(
            reopened
                .activity_for_family(&workspace, &core)
                .await
                .expect("ephemeral activity resets on restart")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_guard_is_durable_and_blocks_duplicate_same_task_recovery() {
        let root = temp_root("moondesk-worker-recovery-guard");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let core = anchor("core-recovery-guard");
        let worker_identity = anchor("worker-recovery-guard");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "exercise durable recovery fence",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &core, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity,
            )
            .await
            .expect("claim worker");

        let first = broker
            .begin_recovery_guard(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                180_000,
            )
            .await
            .expect("begin recovery guard");
        let duplicate = broker
            .begin_recovery_guard(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                180_000,
            )
            .await
            .expect_err("duplicate recovery must be fenced");
        assert!(duplicate.to_string().contains("already attempted recently"));

        drop(broker);
        let reopened = WorkerBroker::open(&path).expect("reopen recovery-guard broker");
        let family = reopened
            .family_for_anchor(&workspace, &core)
            .await
            .expect("read family after restart")
            .expect("family survives restart");
        assert_eq!(
            family
                .workers
                .get(&spawned.worker_id)
                .and_then(|worker| worker.recovery_guard.as_ref()),
            Some(&first),
            "recovery fence must survive host restart"
        );
        let after_restart = reopened
            .begin_recovery_guard(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                180_000,
            )
            .await
            .expect_err("durable guard must fence a retry after restart");
        assert!(
            after_restart
                .to_string()
                .contains("already attempted recently")
        );
        reopened
            .clear_recovery_guard(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                first.started_at_ms,
            )
            .await
            .expect("clear exact recovery guard");
        let family = reopened
            .family_for_anchor(&workspace, &core)
            .await
            .expect("read family after guard clear")
            .expect("family remains");
        assert!(
            family
                .workers
                .get(&spawned.worker_id)
                .and_then(|worker| worker.recovery_guard.as_ref())
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn browser_presence_detaches_running_worker_without_ending_its_task() {
        let root = temp_root("moondesk-worker-detached-tab");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-detached-tab");
        let worker_identity = anchor("worker-detached-tab");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "keep working after the tab closes",
            ))
            .await
            .expect("spawn worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link worker launch");
        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{conversation_id}")),
            )
            .await
            .expect("bind durable worker conversation");
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        let changed = broker
            .sync_browser_presence(&BTreeSet::new())
            .await
            .expect("publish closed-tab presence");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, WorkerState::Running);
        assert_eq!(
            changed[0].attachment_state,
            BrowserAttachmentState::Detached
        );
        assert_eq!(changed[0].current_task_id.as_ref(), Some(&spawned.task_id));
        assert_eq!(
            changed[0]
                .tasks
                .get(&spawned.task_id)
                .map(|task| task.state),
            Some(TaskState::Running)
        );

        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                body: "server-side work continued after the browser view disappeared".into(),
            })
            .await
            .expect("detached worker report remains valid");

        let open = BTreeSet::from([conversation_id.to_string()]);
        let reattached = broker
            .sync_browser_presence(&open)
            .await
            .expect("publish returning page presence");
        assert_eq!(reattached.len(), 1);
        assert_eq!(
            reattached[0].attachment_state,
            BrowserAttachmentState::Attached
        );
        assert_eq!(reattached[0].state, WorkerState::Running);

        broker
            .sync_browser_presence(&BTreeSet::new())
            .await
            .expect("detach worker again before finish");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("detached worker can finish normally");
        let family = broker
            .family_for_anchor(&workspace, &core)
            .await
            .expect("read worker family")
            .expect("worker family exists");
        let finished = family
            .workers
            .get(&spawned.worker_id)
            .expect("worker remains reusable");
        assert_eq!(finished.state, WorkerState::Idle);
        assert_eq!(finished.attachment_state, BrowserAttachmentState::Absent);
        assert!(finished.current_task_id.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_worker_context_stays_out_of_request_fingerprints_for_upgrade_compatibility() {
        let request = spawn_request(
            OperationId::new(),
            WorkspaceId::new(),
            anchor("anchor-context-compat"),
            "audit auth",
        );
        let serialized = serde_json::to_value(&request).expect("serialize worker request");
        assert!(serialized.get("context").is_none());

        let mut contextual = request;
        contextual.context = "shared context".into();
        let serialized = serde_json::to_value(&contextual).expect("serialize contextual request");
        assert_eq!(
            serialized.get("context"),
            Some(&serde_json::json!("shared context"))
        );
    }

    #[tokio::test]
    async fn spawn_is_idempotent_and_rejects_changed_input_for_same_operation() {
        let root = temp_root("moondesk-worker-spawn-idempotent");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let identity = anchor("anchor-a");
        let operation = OperationId::new();

        let mut request = spawn_request(
            operation.clone(),
            workspace.clone(),
            identity.clone(),
            "audit auth",
        );
        request.context = "shared review context".into();
        let first = broker
            .spawn_worker(request.clone())
            .await
            .expect("spawn worker");
        let retry = broker
            .spawn_worker(request.clone())
            .await
            .expect("retry same spawn");
        assert_eq!(first, retry);
        assert_eq!(broker.snapshot().await.families.len(), 1);
        assert_eq!(
            broker
                .snapshot()
                .await
                .families
                .values()
                .next()
                .expect("worker family")
                .workers
                .len(),
            1
        );

        let mut changed_context = request.clone();
        changed_context.context = "different shared review context".into();
        let changed = broker
            .spawn_worker(changed_context)
            .await
            .expect_err("same operation id with changed context must fail");
        assert!(matches!(changed, WorkerBrokerError::Conflict(_)));

        let changed = broker
            .spawn_worker(spawn_request(
                operation,
                workspace,
                identity,
                "different assignment",
            ))
            .await
            .expect_err("same operation id with changed input must fail");
        assert!(matches!(changed, WorkerBrokerError::Conflict(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unlinked_launch_rollbacks_leave_no_ghost_worker_or_wake_task() {
        let root = temp_root("moondesk-worker-launch-rollback");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-rollback");
        let worker_identity = anchor("worker-rollback");

        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "launch that never enqueues",
            ))
            .await
            .expect("spawn pending worker");
        broker
            .rollback_unlinked_spawn(
                &workspace,
                &anchor_identity,
                &spawned.worker_id,
                &spawned.task_id,
            )
            .await
            .expect("rollback unlinked spawn");
        let family = broker
            .family_for_anchor(&workspace, &anchor_identity)
            .await
            .expect("read family")
            .expect("family remains");
        assert!(family.workers.is_empty());
        assert!(family.spawn_requests.is_empty());

        let live = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "real worker",
            ))
            .await
            .expect("spawn real worker");
        make_claimable(&broker, &workspace, &anchor_identity, &live).await;
        broker
            .claim_worker(
                &workspace,
                &live.worker_id,
                &live.task_id,
                &live.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim real worker");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: live.worker_id.clone(),
                task_id: live.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish real worker");

        let wake = broker
            .reuse_worker(ReuseWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: anchor_identity.clone(),
                worker_id: live.worker_id.clone(),
                assignment: "wake that never enqueues".into(),
            })
            .await
            .expect("create pending reuse");
        broker
            .rollback_unlinked_reuse(&workspace, &anchor_identity, &live.worker_id, &wake.task_id)
            .await
            .expect("rollback unlinked reuse");
        let family = broker
            .family_for_anchor(&workspace, &anchor_identity)
            .await
            .expect("read family after reuse rollback")
            .expect("family remains");
        let worker = family.workers.get(&live.worker_id).expect("worker remains");
        assert_eq!(worker.state, WorkerState::Idle);
        assert!(worker.current_task_id.is_none());
        assert!(!worker.tasks.contains_key(&wake.task_id));
        assert!(family.reuse_requests.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn durable_worker_task_and_message_survive_restart() {
        let root = temp_root("moondesk-worker-restart");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let identity = anchor("anchor-restart");
        let worker_identity = anchor("worker-restart");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                identity.clone(),
                "audit persistence",
            ))
            .await
            .expect("spawn durable worker");
        make_claimable(&broker, &workspace, &identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim durable worker");
        let message = broker
            .message_worker(MessageWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: identity.clone(),
                worker_id: spawned.worker_id.clone(),
                body: "also inspect restart behavior".into(),
            })
            .await
            .expect("queue worker message");
        drop(broker);

        let reopened = WorkerBroker::open(&path).expect("reopen worker broker");
        let pending = reopened
            .pending_messages(&workspace, &worker_identity, &spawned.worker_id)
            .await
            .expect("read pending messages after restart");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, message.message_id);
        assert_eq!(pending[0].body, "also inspect restart behavior");

        reopened
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish durable task");
        drop(reopened);

        let again = WorkerBroker::open(&path).expect("reopen finished worker broker");
        let snapshot = again.snapshot().await;
        let worker = snapshot
            .families
            .values()
            .next()
            .and_then(|family| family.workers.get(&spawned.worker_id))
            .expect("persisted worker");
        assert_eq!(worker.state, WorkerState::Idle);
        assert!(worker.current_task_id.is_none());
        let task = worker.tasks.get(&spawned.task_id).expect("persisted task");
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.result.as_ref(), Some(&finished_result()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn idle_worker_reuse_is_idempotent_durable_and_requires_bound_worker_start() {
        let root = temp_root("moondesk-worker-reuse");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-reuse");
        let worker_identity = anchor("worker-reuse");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "first assignment",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish first task");

        let operation_id = OperationId::new();
        let reuse_request = ReuseWorkerRequest {
            operation_id: operation_id.clone(),
            workspace_id: workspace.clone(),
            anchor_identity: anchor_identity.clone(),
            worker_id: spawned.worker_id.clone(),
            assignment: "second assignment".into(),
        };
        let reused = broker
            .reuse_worker(reuse_request.clone())
            .await
            .expect("reuse idle worker");
        let retry = broker
            .reuse_worker(reuse_request)
            .await
            .expect("retry same reuse");
        assert_eq!(reused, retry);
        assert_ne!(reused.task_id, spawned.task_id);
        assert_eq!(reused.display_id, spawned.display_id);
        assert_eq!(reused.execution_profile, profile());
        drop(broker);

        let reopened = WorkerBroker::open(&path).expect("reopen reused worker broker");
        let snapshot = reopened.snapshot().await;
        let worker = snapshot
            .families
            .values()
            .next()
            .and_then(|family| family.workers.get(&spawned.worker_id))
            .expect("reused worker after restart");
        assert_eq!(worker.state, WorkerState::Waking);
        assert_eq!(worker.current_task_id.as_ref(), Some(&reused.task_id));
        assert_eq!(
            worker.tasks.get(&reused.task_id).map(|task| task.state),
            Some(TaskState::Pending)
        );

        let forged = reopened
            .start_task(
                &workspace,
                &anchor("different-worker-chat"),
                &spawned.worker_id,
                &reused.task_id,
            )
            .await
            .expect_err("different chat must not start reused task");
        assert_eq!(forged, WorkerBrokerError::NotFound);

        let started = reopened
            .start_task(
                &workspace,
                &worker_identity,
                &spawned.worker_id,
                &reused.task_id,
            )
            .await
            .expect("bound worker starts reused task");
        assert_eq!(started.state, WorkerState::Running);
        assert_eq!(started.attachment_state, BrowserAttachmentState::Attached);
        assert_eq!(
            started.tasks.get(&reused.task_id).map(|task| task.state),
            Some(TaskState::Running)
        );
        let start_retry = reopened
            .start_task(
                &workspace,
                &worker_identity,
                &spawned.worker_id,
                &reused.task_id,
            )
            .await
            .expect("repeated start is idempotent");
        assert_eq!(start_retry.state, WorkerState::Running);

        reopened
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: reused.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish reused task");
        let family = reopened
            .family_for_anchor(&workspace, &anchor_identity)
            .await
            .expect("read family")
            .expect("family exists");
        assert_eq!(
            family
                .workers
                .get(&spawned.worker_id)
                .map(|worker| worker.state),
            Some(WorkerState::Idle)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn purge_workspace_removes_only_owned_worker_families_and_is_idempotent() {
        let root = temp_root("moondesk-worker-purge-workspace");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let anchor_a = anchor("anchor-purge-a");
        let anchor_b = anchor("anchor-purge-b");
        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_a.clone(),
                anchor_a.clone(),
                "keep workspace A",
            ))
            .await
            .expect("spawn worker A");
        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_b.clone(),
                anchor_b.clone(),
                "purge workspace B",
            ))
            .await
            .expect("spawn worker B");

        assert_eq!(
            broker.purge_workspace(&workspace_b).await.expect("purge B"),
            1
        );
        assert_eq!(
            broker
                .purge_workspace(&workspace_b)
                .await
                .expect("repeat purge B"),
            0
        );
        assert!(
            broker
                .family_for_anchor(&workspace_a, &anchor_a)
                .await
                .expect("read A")
                .is_some()
        );
        assert!(
            broker
                .family_for_anchor(&workspace_b, &anchor_b)
                .await
                .expect("read B")
                .is_none()
        );
        drop(broker);

        let reopened = WorkerBroker::open(&path).expect("reopen worker broker");
        assert!(
            reopened
                .family_for_anchor(&workspace_a, &anchor_a)
                .await
                .expect("read persisted A")
                .is_some()
        );
        assert!(
            reopened
                .family_for_anchor(&workspace_b, &anchor_b)
                .await
                .expect("read persisted B")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn clear_session_refuses_live_launch_but_removes_proven_failed_worker_family() {
        let root = temp_root("moondesk-worker-clear-session");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let identity = anchor("core-clear-session");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                identity.clone(),
                "safe failed launch",
            ))
            .await
            .expect("spawn worker");

        let active_error = broker
            .ensure_clearable_session_digest(&identity.session_digest)
            .await
            .expect_err("queued provisioning worker is still active");
        assert!(matches!(active_error, WorkerBrokerError::Conflict(_)));

        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &identity,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link launch command");
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::Failed,
                Some("model_unavailable".into()),
                None,
            )
            .await
            .expect("mark launch failed");

        let clearable = broker
            .ensure_clearable_session_digest(&identity.session_digest)
            .await
            .expect("failed pre-send worker is clearable");
        assert_eq!(clearable.len(), 1);
        let cleared = broker
            .clear_session_digest(&identity.session_digest)
            .await
            .expect("clear worker family");
        assert_eq!(cleared.len(), 1);
        assert!(
            broker
                .family_for_anchor(&workspace, &identity)
                .await
                .expect("read cleared family")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pre_send_failure_retires_fresh_worker_and_restores_reuse_to_idle() {
        let root = temp_root("moondesk-worker-pre-send-settle");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-pre-send-settle");

        let fresh = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "fresh failure",
            ))
            .await
            .expect("spawn fresh worker");
        let fresh_command = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &fresh.worker_id,
                &fresh.task_id,
                &fresh_command,
            )
            .await
            .expect("link fresh command");
        let fresh_settled = broker
            .settle_pre_send_failure_by_command(&fresh_command, Some("model unavailable".into()))
            .await
            .expect("settle fresh failure")
            .expect("fresh worker found");
        assert_eq!(fresh_settled.state, WorkerState::Retired);
        assert_eq!(fresh_settled.launch_state, WorkerLaunchState::Failed);
        assert!(fresh_settled.current_task_id.is_none());
        assert_eq!(
            fresh_settled
                .tasks
                .get(&fresh.task_id)
                .map(|task| task.state),
            Some(TaskState::Failed)
        );
        assert_eq!(
            fresh_settled
                .tasks
                .get(&fresh.task_id)
                .and_then(|task| task.failure.as_ref())
                .map(|failure| (failure.stage.as_str(), failure.error.as_str())),
            Some(("launch", "model unavailable"))
        );
        let collected_failure = broker
            .collect_updates_for_operation(&workspace, &core, &OperationId::new())
            .await
            .expect("collect failed launch");
        assert_eq!(collected_failure.failed.len(), 1);
        assert_eq!(collected_failure.failed[0].task_id, fresh.task_id);
        assert_eq!(
            collected_failure.failed[0].failure.error,
            "model unavailable"
        );

        let durable = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "durable worker",
            ))
            .await
            .expect("spawn durable worker");
        make_claimable(&broker, &workspace, &core, &durable).await;
        let worker_identity = anchor("worker-pre-send-settle");
        broker
            .claim_worker(
                &workspace,
                &durable.worker_id,
                &durable.task_id,
                &durable.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim durable worker");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: durable.worker_id.clone(),
                task_id: durable.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish initial durable task");
        let reused = broker
            .reuse_worker(ReuseWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: core.clone(),
                worker_id: durable.worker_id.clone(),
                assignment: "reuse failure".into(),
            })
            .await
            .expect("reuse durable worker");
        let reuse_command = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &reused.worker_id,
                &reused.task_id,
                &reuse_command,
            )
            .await
            .expect("link reuse command");
        let reused_settled = broker
            .settle_pre_send_failure_by_command(&reuse_command, Some("composer unavailable".into()))
            .await
            .expect("settle reuse failure")
            .expect("reused worker found");
        assert_eq!(reused_settled.state, WorkerState::Idle);
        assert_eq!(reused_settled.launch_state, WorkerLaunchState::Claimed);
        assert!(reused_settled.chat_identity.is_some());
        assert!(reused_settled.current_task_id.is_none());
        assert_eq!(
            reused_settled
                .tasks
                .get(&reused.task_id)
                .map(|task| task.state),
            Some(TaskState::Failed)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unclaimed_sent_worker_can_be_cancelled_and_collected_as_failure() {
        let root = temp_root("moondesk-worker-cancel-unclaimed");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-cancel-unclaimed");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "cancel unclaimed",
            ))
            .await
            .expect("spawn worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link command");
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{}", spawned.worker_id)),
            )
            .await
            .expect("mark waiting claim");

        let cancelled = broker
            .cancel_unclaimed_after_send(&workspace, &core, &spawned.worker_id)
            .await
            .expect("cancel unclaimed worker");
        assert_eq!(cancelled.state, WorkerState::Retired);
        assert_eq!(cancelled.launch_state, WorkerLaunchState::Failed);
        assert!(cancelled.claim_token.is_none());
        assert!(cancelled.claim_operation_id.is_none());
        assert!(cancelled.current_task_id.is_none());
        let failure = cancelled
            .tasks
            .get(&spawned.task_id)
            .and_then(|task| task.failure.as_ref())
            .expect("cancel failure payload");
        assert_eq!(failure.stage, "claim");
        assert_eq!(failure.error, "worker_registration_cancelled_after_send");

        let updates = broker
            .collect_updates_for_operation(&workspace, &core, &OperationId::new())
            .await
            .expect("collect cancellation");
        assert_eq!(updates.failed.len(), 1);
        assert_eq!(updates.failed[0].task_id, spawned.task_id);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn waiting_claim_timeout_terminalizes_abandoned_worker_and_reports_failure() {
        let root = temp_root("moondesk-worker-claim-timeout");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-claim-timeout");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "claim timeout",
            ))
            .await
            .expect("spawn worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link command");
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{}", spawned.worker_id)),
            )
            .await
            .expect("mark waiting claim");
        {
            let mut guard = broker.data.lock().await;
            let worker = guard
                .families
                .values_mut()
                .flat_map(|family| family.workers.values_mut())
                .find(|worker| worker.id == spawned.worker_id)
                .expect("waiting worker");
            worker.waiting_claim_since_ms =
                Some(worker_unix_time_ms().saturating_sub(WORKER_CLAIM_TIMEOUT_MS + 1));
        }

        broker
            .expire_waiting_claim_timeouts_for_anchor(&workspace, &core)
            .await
            .expect("expire waiting claim without browser presence");
        let family = broker
            .family_for_anchor(&workspace, &core)
            .await
            .expect("read family")
            .expect("family remains");
        let worker = family
            .workers
            .get(&spawned.worker_id)
            .expect("worker remains");
        assert_eq!(worker.state, WorkerState::Retired);
        assert_eq!(worker.launch_state, WorkerLaunchState::Failed);
        assert_eq!(
            worker.launch_error.as_deref(),
            Some("worker_claim_timeout_after_send")
        );
        assert!(worker.claim_token.is_none());

        let updates = broker
            .collect_updates_for_operation(&workspace, &core, &OperationId::new())
            .await
            .expect("collect claim timeout");
        assert_eq!(updates.failed.len(), 1);
        assert_eq!(updates.failed[0].failure.stage, "claim");
        assert_eq!(
            updates.failed[0].failure.error,
            "worker_claim_timeout_after_send"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn wrong_workspace_cannot_address_worker() {
        let root = temp_root("moondesk-worker-workspace-isolation");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let owner_workspace = WorkspaceId::new();
        let other_workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-isolation");
        let worker_identity = anchor("worker-isolation");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                owner_workspace,
                anchor_identity.clone(),
                "audit isolation",
            ))
            .await
            .expect("spawn worker");

        let message_error = broker
            .message_worker(MessageWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: other_workspace.clone(),
                anchor_identity,
                worker_id: spawned.worker_id.clone(),
                body: "cross workspace message".into(),
            })
            .await
            .expect_err("other workspace must not message worker");
        assert_eq!(message_error, WorkerBrokerError::NotFound);

        let finish_error = broker
            .finish_task(FinishTaskRequest {
                workspace_id: other_workspace,
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                result: finished_result(),
            })
            .await
            .expect_err("other workspace must not finish worker task");
        assert_eq!(finish_error, WorkerBrokerError::NotFound);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_finish_result_is_rejected_without_completing_the_task() {
        let root = temp_root("moondesk-worker-finish-size-limit");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-finish-size-limit");
        let worker_identity = anchor("worker-finish-size-limit");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "finish size limit",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        let error = broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: WorkerResult {
                    result: "x".repeat(crate::workers::MAX_WORKER_RESULT_BYTES),
                    changes: "overflow".into(),
                    validation: "overflow".into(),
                    blockers: Vec::new(),
                },
            })
            .await
            .expect_err("oversized result must fail before durable mutation");
        assert!(matches!(error, WorkerBrokerError::Invalid(_)));

        let snapshot = broker.snapshot().await;
        let worker = snapshot
            .families
            .values()
            .next()
            .and_then(|family| family.workers.get(&spawned.worker_id))
            .expect("worker remains");
        assert_eq!(worker.current_task_id.as_ref(), Some(&spawned.task_id));
        assert_eq!(
            worker.tasks.get(&spawned.task_id).map(|task| task.state),
            Some(TaskState::Running)
        );
        assert!(
            worker
                .tasks
                .get(&spawned.task_id)
                .and_then(|task| task.result.as_ref())
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finish_is_idempotent_but_conflicting_second_result_is_rejected() {
        let root = temp_root("moondesk-worker-finish-idempotent");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-finish");
        let worker_identity = anchor("worker-finish");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "finish task",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");
        let request = FinishTaskRequest {
            workspace_id: workspace.clone(),
            worker_identity: worker_identity.clone(),
            worker_id: spawned.worker_id.clone(),
            task_id: spawned.task_id.clone(),
            result: finished_result(),
        };
        let first = broker
            .finish_task(request.clone())
            .await
            .expect("finish worker task");
        let retry = broker
            .finish_task(request)
            .await
            .expect("repeat same finish");
        assert_eq!(first, retry);

        let conflict = broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace,
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                result: WorkerResult {
                    result: "different".into(),
                    ..finished_result()
                },
            })
            .await
            .expect_err("different second finish must fail");
        assert!(matches!(conflict, WorkerBrokerError::Conflict(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accepted_message_is_at_least_once_until_acknowledged() {
        let root = temp_root("moondesk-worker-message-ack");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-message");
        let worker_identity = anchor("worker-message");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "message task",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");
        let operation = OperationId::new();
        let request = MessageWorkerRequest {
            operation_id: operation.clone(),
            workspace_id: workspace.clone(),
            anchor_identity,
            worker_id: spawned.worker_id.clone(),
            body: "verify logout too".into(),
        };
        let first = broker
            .message_worker(request.clone())
            .await
            .expect("queue worker message");
        let retry = broker
            .message_worker(request)
            .await
            .expect("retry queued worker message");
        assert_eq!(first, retry);
        assert_eq!(
            broker
                .pending_messages(&workspace, &worker_identity, &spawned.worker_id)
                .await
                .expect("first inbox read")
                .len(),
            1
        );
        assert_eq!(
            broker
                .pending_messages(&workspace, &worker_identity, &spawned.worker_id)
                .await
                .expect("second inbox read")
                .len(),
            1
        );

        broker
            .acknowledge_message(
                &workspace,
                &worker_identity,
                &spawned.worker_id,
                &first.message_id,
            )
            .await
            .expect("ack worker message");
        assert!(
            broker
                .pending_messages(&workspace, &worker_identity, &spawned.worker_id)
                .await
                .expect("inbox after ack")
                .is_empty()
        );
        drop(broker);
        let reopened = WorkerBroker::open(&path).expect("reopen acknowledged store");
        assert!(
            reopened
                .pending_messages(&workspace, &worker_identity, &spawned.worker_id)
                .await
                .expect("reopened inbox after ack")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistence_failure_does_not_publish_worker_in_memory() {
        let root = temp_root("moondesk-worker-persist-failure");
        let state_parent = root.join("state-parent");
        std::fs::create_dir_all(&state_parent).expect("create valid worker state parent");
        let broker = WorkerBroker::open(state_parent.join("worker-state-v1.json"))
            .expect("open broker before first worker state file exists");

        broker.fail_next_commit_for_test();
        let error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                WorkspaceId::new(),
                anchor("anchor-persist-failure"),
                "must not publish",
            ))
            .await
            .expect_err("persistence failure must reject spawn");
        assert!(matches!(error, WorkerBrokerError::Storage(_)));
        assert!(broker.snapshot().await.families.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn same_workspace_different_anchor_cannot_message_or_collect_family() {
        let root = temp_root("moondesk-worker-anchor-isolation");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let owner = anchor("anchor-owner");
        let intruder = anchor("anchor-intruder");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                owner,
                "audit ownership",
            ))
            .await
            .expect("spawn worker");

        let message_error = broker
            .message_worker(MessageWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: intruder.clone(),
                worker_id: spawned.worker_id,
                body: "should not be accepted".into(),
            })
            .await
            .expect_err("another chat in the same workspace must not message the worker");
        assert_eq!(message_error, WorkerBrokerError::NotFound);

        let collect_error = broker
            .collect_updates(&workspace, &intruder)
            .await
            .expect_err("another chat in the same workspace must not collect this family");
        assert_eq!(collect_error, WorkerBrokerError::NotFound);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn worker_family_pins_claims_to_first_worker_chatgpt_account_not_core_account() {
        let root = temp_root("moondesk-worker-account-affinity");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = ChatIdentity::from_openai_meta(Some("codex-core-account"), "core-session");
        let first = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "first fixed-account worker",
            ))
            .await
            .expect("spawn first worker");
        make_claimable(&broker, &workspace, &core, &first).await;
        broker
            .claim_worker(
                &workspace,
                &first.worker_id,
                &first.task_id,
                &first.claim_token,
                ChatIdentity::from_openai_meta(Some("fixed-worker-account"), "worker-session-1"),
            )
            .await
            .expect("first Worker account establishes family affinity");

        let second = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "second fixed-account worker",
            ))
            .await
            .expect("spawn second worker");
        make_claimable(&broker, &workspace, &core, &second).await;
        let wrong_account = broker
            .claim_worker(
                &workspace,
                &second.worker_id,
                &second.task_id,
                &second.claim_token,
                ChatIdentity::from_openai_meta(
                    Some("different-worker-account"),
                    "worker-session-2",
                ),
            )
            .await
            .expect_err("different Worker account must not join the family");
        assert!(
            wrong_account
                .to_string()
                .contains("different ChatGPT account")
        );

        let claimed = broker
            .claim_worker(
                &workspace,
                &second.worker_id,
                &second.task_id,
                &second.claim_token,
                ChatIdentity::from_openai_meta(Some("fixed-worker-account"), "worker-session-2"),
            )
            .await
            .expect("different Worker chat on the same Worker account is valid");
        assert_eq!(claimed.state, WorkerState::Running);
        assert_eq!(
            claimed
                .chat_identity
                .as_ref()
                .and_then(|identity| identity.subject_digest.as_deref()),
            ChatIdentity::from_openai_meta(Some("fixed-worker-account"), "worker-session-2")
                .subject_digest
                .as_deref()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn generalized_gateway_claim_capability_resolves_workspace_then_identity_stays_pinned() {
        let root = temp_root("moondesk-generalized-worker-gateway-routing");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let core_a = ChatIdentity::local_core(&workspace_a);
        let core_b = ChatIdentity::local_core(&workspace_b);
        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_a.clone(),
                core_a,
                "unrelated workspace A worker",
            ))
            .await
            .expect("spawn unrelated workspace A worker");
        let target = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_b.clone(),
                core_b.clone(),
                "gateway target in workspace B",
            ))
            .await
            .expect("spawn gateway target");
        let claim_operation_id = target
            .claim_operation_id
            .clone()
            .expect("fresh claim operation id");
        let worker_identity =
            ChatIdentity::from_openai_meta(Some("fixed-worker-account"), "gateway-worker-chat");

        let resolved = broker
            .gateway_workspace_for_claim(
                &target.worker_id,
                &target.task_id,
                &target.claim_token,
                &claim_operation_id,
                &worker_identity,
            )
            .await
            .expect("claim capability resolves exact workspace");
        assert_eq!(resolved, workspace_b);
        assert_eq!(
            broker
                .gateway_workspace_for_worker_identity(&worker_identity)
                .await
                .expect("lookup before claim"),
            None,
            "ordinary tools must not route before claim binds the Worker chat"
        );

        make_claimable(&broker, &workspace_b, &core_b, &target).await;
        let claimed = broker
            .claim_worker_attested(
                &workspace_b,
                &target.worker_id,
                &target.task_id,
                &target.claim_token,
                &claim_operation_id,
                worker_identity.clone(),
            )
            .await
            .expect("claim target Worker");
        assert_eq!(claimed.state, WorkerState::Provisioning);
        assert_eq!(
            claimed.tasks.get(&target.task_id).map(|task| task.state),
            Some(TaskState::Pending),
            "claim must bind the Worker without pretending project work has started"
        );
        assert_eq!(
            broker
                .gateway_workspace_for_worker_identity(&worker_identity)
                .await
                .expect("lookup after claim"),
            Some(workspace_b.clone())
        );
        let before_start = broker
            .gateway_workspace_for_active_worker_identity(&worker_identity)
            .await
            .expect_err(
                "claimed Worker must not receive ordinary project-tool authority before start",
            );
        assert!(before_start.to_string().contains("no active running task"));
        let early_report = broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace_b.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: target.worker_id.clone(),
                task_id: target.task_id.clone(),
                body: "must not report before start".into(),
            })
            .await
            .expect_err("claimed Worker must start before reporting");
        assert!(early_report.to_string().contains("workers start"));
        let early_finish = broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace_b.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: target.worker_id.clone(),
                task_id: target.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect_err("claimed Worker must start before finishing");
        assert!(early_finish.to_string().contains("workers start"));

        let started = broker
            .start_task(
                &workspace_b,
                &worker_identity,
                &target.worker_id,
                &target.task_id,
            )
            .await
            .expect("start claimed target Worker");
        assert_eq!(started.state, WorkerState::Running);
        assert_eq!(
            started.tasks.get(&target.task_id).map(|task| task.state),
            Some(TaskState::Running)
        );
        assert_eq!(
            broker
                .gateway_workspace_for_active_worker_identity(&worker_identity)
                .await
                .expect("active lookup after start"),
            Some(workspace_b.clone())
        );
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace_b.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: target.worker_id.clone(),
                task_id: target.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish gateway target");
        let idle_tool_access = broker
            .gateway_workspace_for_active_worker_identity(&worker_identity)
            .await
            .expect_err("idle Worker chat must not retain ordinary workspace-tool authority");
        assert!(
            idle_tool_access
                .to_string()
                .contains("no active running task")
        );
        assert_eq!(
            broker
                .gateway_workspace_for_worker_identity(&worker_identity)
                .await
                .expect("lifecycle routing remains available while idle"),
            Some(workspace_b.clone())
        );
        let wrong_token = broker
            .gateway_workspace_for_claim(
                &target.worker_id,
                &target.task_id,
                "wrong-token",
                &claim_operation_id,
                &ChatIdentity::from_openai_meta(
                    Some("fixed-worker-account"),
                    "different-worker-chat",
                ),
            )
            .await
            .expect_err("claim capability must remain secret before identity binding");
        assert_eq!(wrong_token, WorkerBrokerError::NotFound);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn claimed_worker_rejects_another_chat_and_reports_are_collectable_once() {
        let root = temp_root("moondesk-worker-session-isolation");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-report");
        let worker_identity = anchor("worker-report");
        let intruder = anchor("worker-intruder");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "audit reports",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        let bad_claim = broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                "definitely-not-the-claim-token",
                intruder.clone(),
            )
            .await
            .expect_err("wrong claim token must not bind worker");
        assert_eq!(bad_claim, WorkerBrokerError::NotFound);

        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        let second_claim = broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                intruder.clone(),
            )
            .await
            .expect_err("second chat must not steal claimed worker");
        assert!(matches!(second_claim, WorkerBrokerError::Conflict(_)));

        let report_error = broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity: intruder,
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                body: "forged report".into(),
            })
            .await
            .expect_err("wrong worker chat must not report");
        assert_eq!(report_error, WorkerBrokerError::NotFound);

        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                body: "found a concrete issue".into(),
            })
            .await
            .expect("store worker report");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                result: finished_result(),
            })
            .await
            .expect("finish worker task");

        let first = broker
            .collect_updates(&workspace, &anchor_identity)
            .await
            .expect("collect updates");
        assert_eq!(first.reports.len(), 1);
        assert_eq!(first.reports[0].body, "found a concrete issue");
        assert_eq!(first.completed.len(), 1);
        assert_eq!(first.completed[0].result, finished_result());
        let second = broker
            .collect_updates(&workspace, &anchor_identity)
            .await
            .expect("collect updates again");
        assert!(second.reports.is_empty());
        assert!(second.completed.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn collect_wait_wakes_on_durable_report_without_polling() {
        let root = temp_root("moondesk-worker-collect-wait");
        let path = root.join("worker-state-v1.json");
        let broker = std::sync::Arc::new(WorkerBroker::open(&path).expect("open worker broker"));
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-collect-wait");
        let worker_identity = anchor("worker-collect-wait");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "wait for report",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        let waiting_broker = broker.clone();
        let waiting_workspace = workspace.clone();
        let waiting_anchor = anchor_identity.clone();
        let waiter = tokio::spawn(async move {
            waiting_broker
                .collect_updates_wait(
                    &waiting_workspace,
                    &waiting_anchor,
                    &OperationId::new(),
                    2_000,
                )
                .await
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                body: "wake the waiting Core".into(),
            })
            .await
            .expect("store report");

        let updates = tokio::time::timeout(tokio::time::Duration::from_secs(2), waiter)
            .await
            .expect("collect waiter should wake before timeout")
            .expect("collect task should join")
            .expect("collect updates");
        assert_eq!(updates.reports.len(), 1);
        assert_eq!(updates.reports[0].body, "wake the waiting Core");
        assert!(updates.completed.is_empty());

        let empty = broker
            .collect_updates_wait(&workspace, &anchor_identity, &OperationId::new(), 25)
            .await
            .expect("bounded empty wait");
        assert!(empty.reports.is_empty());
        assert!(empty.completed.is_empty());

        let too_long = broker
            .collect_updates_wait(&workspace, &anchor_identity, &OperationId::new(), 60_001)
            .await
            .expect_err("oversized wait must fail");
        assert!(matches!(too_long, WorkerBrokerError::Invalid(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn enabled_collect_waiter_catches_report_committed_after_empty_read_before_await() {
        let root = temp_root("moondesk-worker-collect-race");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-collect-race");
        let worker_identity = anchor("worker-collect-race");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "exercise notify gap",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        // This is the exact ordering collect_updates_wait relies on: register the Notify waiter,
        // inspect durable state, then let a report commit before the waiter is awaited.
        let notified = broker.enabled_update_waiter();
        let empty = broker
            .collect_updates(&workspace, &anchor_identity)
            .await
            .expect("initial collection is empty");
        assert!(empty.reports.is_empty());
        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                body: "committed inside the former lost-wakeup gap".into(),
            })
            .await
            .expect("commit report before awaiting notification");
        tokio::time::timeout(tokio::time::Duration::from_millis(100), notified)
            .await
            .expect("enabled waiter must retain notify_waiters wakeup");
        let updates = broker
            .collect_updates(&workspace, &anchor_identity)
            .await
            .expect("collect committed report");
        assert_eq!(updates.reports.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn clear_all_workers_destroys_live_families_and_fences_claimed_worker_chats() {
        let root = temp_root("moondesk-worker-clear-all");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let core_a = anchor("clear-all-core-a");
        let core_b = anchor("clear-all-core-b");
        let worker_identity = anchor("clear-all-running-worker");

        let running = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_a.clone(),
                core_a.clone(),
                "running worker cleared globally",
            ))
            .await
            .expect("spawn running worker");
        make_claimable(&broker, &workspace_a, &core_a, &running).await;
        broker
            .claim_worker(
                &workspace_a,
                &running.worker_id,
                &running.task_id,
                &running.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim running worker");

        let pending = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_b,
                core_b,
                "unclaimed worker cleared globally",
            ))
            .await
            .expect("spawn pending worker");
        assert!(broker.has_worker_state().await);

        let cleared = broker
            .clear_all_workers()
            .await
            .expect("clear every worker family");
        assert_eq!(cleared.len(), 2);
        assert!(cleared.iter().any(|worker| worker.id == running.worker_id));
        assert!(cleared.iter().any(|worker| worker.id == pending.worker_id));
        assert!(!broker.has_worker_state().await);
        assert!(broker.snapshot().await.families.is_empty());
        assert!(broker.is_retired_worker_identity(&worker_identity).await);

        let retired_core_error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace_a,
                worker_identity.clone(),
                "cleared worker chat must not become Core",
            ))
            .await
            .expect_err("retired worker chat must stay fenced");
        assert!(matches!(retired_core_error, WorkerBrokerError::Conflict(_)));

        drop(broker);
        let reopened = WorkerBroker::open(&path).expect("reopen worker broker");
        assert!(reopened.is_retired_worker_identity(&worker_identity).await);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn retiring_idle_worker_frees_display_slot_without_erasing_history() {
        let root = temp_root("moondesk-worker-retire-slot");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-retire-slot");
        let worker_identity = anchor("worker-retire-slot");

        let first = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "first assignment",
            ))
            .await
            .expect("spawn first worker");
        let second = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "second assignment",
            ))
            .await
            .expect("spawn second worker");
        assert_eq!(first.display_id, "worker-1");
        assert_eq!(second.display_id, "worker-2");

        make_claimable(&broker, &workspace, &anchor_identity, &first).await;
        broker
            .claim_worker(
                &workspace,
                &first.worker_id,
                &first.task_id,
                &first.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim first worker");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: first.worker_id.clone(),
                task_id: first.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish first worker");
        let retired = broker
            .retire_worker(&workspace, &anchor_identity, &first.worker_id, None)
            .await
            .expect("retire idle worker");
        assert_eq!(retired.state, WorkerState::Retired);
        assert_eq!(retired.display_id, "worker-1");

        let replacement = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "replacement assignment",
            ))
            .await
            .expect("spawn replacement worker");
        assert_eq!(replacement.display_id, "worker-1");
        assert_ne!(replacement.worker_id, first.worker_id);

        let snapshot = broker.snapshot().await;
        let family = snapshot
            .families
            .values()
            .find(|family| family.workspace_id == workspace)
            .expect("worker family");
        assert_eq!(family.workers.len(), 3);
        assert_eq!(
            family
                .workers
                .values()
                .filter(|worker| worker.state != WorkerState::Retired)
                .count(),
            2
        );
        assert_eq!(
            family
                .workers
                .get(&first.worker_id)
                .expect("retired history")
                .tasks
                .get(&first.task_id)
                .and_then(|task| task.result.as_ref()),
            Some(&finished_result())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn collect_operation_is_replayable_after_a_dropped_response() {
        let root = temp_root("moondesk-worker-collect-replay");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-collect-replay");
        let worker_identity = anchor("worker-collect-replay");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "produce replayable updates",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");
        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                body: "durable progress".into(),
            })
            .await
            .expect("store report");
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id,
                task_id: spawned.task_id,
                result: finished_result(),
            })
            .await
            .expect("finish worker");

        let operation_id = OperationId::new();
        let first = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &operation_id)
            .await
            .expect("first durable collection");
        assert_eq!(first.reports.len(), 1);
        assert_eq!(first.completed.len(), 1);

        let replay = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &operation_id)
            .await
            .expect("replay collection after response loss");
        assert_eq!(replay, first);

        let next = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &OperationId::new())
            .await
            .expect("new collection after replay");
        assert!(next.reports.is_empty());
        assert!(next.completed.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn collect_replay_window_is_bounded_and_old_receipts_eventually_evict() {
        let root = temp_root("moondesk-worker-collect-replay-window");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-collect-window");
        let worker_identity = anchor("worker-collect-window");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "produce one dropped collection",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");
        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                body: "delivery-window".into(),
            })
            .await
            .expect("store report");

        let dropped_operation = OperationId::new();
        let delivered = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &dropped_operation)
            .await
            .expect("persist collection whose transport response is dropped");
        assert_eq!(delivered.reports.len(), 1);
        let replay = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &dropped_operation)
            .await
            .expect("prompt retry replays the durable batch");
        assert_eq!(replay, delivered);

        for index in 0..MAX_COLLECT_RECEIPTS_PER_FAMILY {
            broker
                .report_worker(ReportWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    worker_identity: worker_identity.clone(),
                    worker_id: spawned.worker_id.clone(),
                    task_id: spawned.task_id.clone(),
                    body: format!("advance replay window {index}"),
                })
                .await
                .expect("store report that advances replay history");
            let updates = broker
                .collect_updates_for_operation(&workspace, &anchor_identity, &OperationId::new())
                .await
                .expect("advance bounded collection replay history");
            assert_eq!(updates.reports.len(), 1);
        }
        let snapshot = broker.snapshot().await;
        let family = snapshot
            .families
            .values()
            .find(|family| family.workspace_id == workspace)
            .expect("worker family");
        assert_eq!(
            family.collect_requests.len(),
            MAX_COLLECT_RECEIPTS_PER_FAMILY
        );
        assert!(
            !family.collect_requests.contains_key(&dropped_operation),
            "Workers V1 intentionally guarantees replay only inside the documented bounded window"
        );
        let expired_retry = broker
            .collect_updates_for_operation(&workspace, &anchor_identity, &dropped_operation)
            .await
            .expect("expired operation becomes a new empty collection");
        assert!(expired_retry.reports.is_empty());
        assert!(expired_retry.completed.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn claim_requires_confirmed_canonical_worker_conversation_and_rejects_core_identity() {
        let root = temp_root("moondesk-worker-claim-conversation");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-claim-conversation");
        let worker_identity = anchor("worker-claim-conversation");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "claim exact conversation",
            ))
            .await
            .expect("spawn worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link worker launch");

        for invalid in [
            None,
            Some("https://chatgpt.com/".to_string()),
            Some(format!(
                "https://chatgpt.com/c/{}?temporary=1",
                spawned.worker_id
            )),
            Some(format!("https://chatgpt.com/foo/c/{}", spawned.worker_id)),
        ] {
            let error = broker
                .update_launch_by_command(
                    &command_id,
                    WorkerLaunchState::WaitingClaim,
                    None,
                    invalid,
                )
                .await
                .expect_err("non-canonical success must not become claimable");
            assert!(matches!(error, WorkerBrokerError::Invalid(_)));
        }

        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{}", spawned.worker_id)),
            )
            .await
            .expect("confirm canonical worker conversation");
        let core_claim = broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                core,
            )
            .await
            .expect_err("Core must not claim its own worker");
        assert!(matches!(core_claim, WorkerBrokerError::Conflict(_)));

        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity,
            )
            .await
            .expect("worker conversation claims exact launch");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn claimed_and_unlinked_history_ignore_obsolete_waiting_claim_urls() {
        let root = temp_root("moondesk-worker-claimed-legacy-sync");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("core-claimed-legacy-sync");
        let worker_identity = anchor("worker-claimed-legacy-sync");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "claimed legacy launch sync",
            ))
            .await
            .expect("spawn worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link worker launch");
        let canonical_url = format!("https://chatgpt.com/c/{}", spawned.worker_id);
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(canonical_url.clone()),
            )
            .await
            .expect("confirm canonical worker conversation");
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity,
            )
            .await
            .expect("claim worker");

        let obsolete_placeholder =
            "https://chatgpt.com/c/local-chatgpt%3A422be40f-2871-4c05-9110-62ac6e7854cd";
        let claimed = broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(obsolete_placeholder.into()),
            )
            .await
            .expect("claimed historical launch sync must be a no-op")
            .expect("claimed worker remains linked");
        assert_eq!(claimed.launch_state, WorkerLaunchState::Claimed);
        assert_eq!(
            claimed.conversation_url.as_deref(),
            Some(canonical_url.as_str())
        );

        let unlinked = broker
            .update_launch_by_command(
                &Uuid::new_v4().to_string(),
                WorkerLaunchState::WaitingClaim,
                None,
                Some(obsolete_placeholder.into()),
            )
            .await
            .expect("unlinked historical launch sync must be ignored");
        assert!(unlinked.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_history_and_idempotency_receipts_stay_bounded_across_many_cycles() {
        let root = temp_root("moondesk-worker-history-bounded");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-history-bounded");
        let worker_identity = anchor("worker-history-bounded");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                "initial history task",
            ))
            .await
            .expect("spawn worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim worker");

        let worker_id = spawned.worker_id.clone();
        let mut task_id = spawned.task_id.clone();
        for index in 0..80 {
            let message = broker
                .message_worker(MessageWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    anchor_identity: anchor_identity.clone(),
                    worker_id: worker_id.clone(),
                    body: format!("message-{index}"),
                })
                .await
                .expect("store bounded message");
            broker
                .acknowledge_message(
                    &workspace,
                    &worker_identity,
                    &worker_id,
                    &message.message_id,
                )
                .await
                .expect("ack bounded message");
            broker
                .report_worker(ReportWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    worker_identity: worker_identity.clone(),
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    body: format!("report-{index}"),
                })
                .await
                .expect("store bounded report");
            broker
                .finish_task(FinishTaskRequest {
                    workspace_id: workspace.clone(),
                    worker_identity: worker_identity.clone(),
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    result: finished_result(),
                })
                .await
                .expect("finish bounded task");
            let updates = broker
                .collect_updates_for_operation(&workspace, &anchor_identity, &OperationId::new())
                .await
                .expect("collect bounded updates");
            assert_eq!(updates.reports.len(), 1);
            assert_eq!(updates.completed.len(), 1);

            if index == 79 {
                break;
            }
            let reused = broker
                .reuse_worker(ReuseWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    anchor_identity: anchor_identity.clone(),
                    worker_id: worker_id.clone(),
                    assignment: format!("history-task-{index}"),
                })
                .await
                .expect("reuse bounded worker");
            task_id = reused.task_id.clone();
            broker
                .start_task(&workspace, &worker_identity, &worker_id, &task_id)
                .await
                .expect("start bounded reuse task");
        }

        let snapshot = broker.snapshot().await;
        let family = snapshot.families.values().next().expect("bounded family");
        let worker = family.workers.get(&worker_id).expect("bounded worker");
        assert!(worker.tasks.len() <= MAX_TASK_RECORDS_PER_WORKER);
        assert!(family.reports.is_empty());
        assert!(family.spawn_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.reuse_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.message_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.report_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.collect_requests.len() <= MAX_COLLECT_RECEIPTS_PER_FAMILY);
        assert!(worker.messages.is_empty());
        assert!(
            std::fs::metadata(&path)
                .expect("bounded worker store metadata")
                .len()
                < 4 * 1024 * 1024
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn receipt_compaction_prunes_old_unreferenced_idempotency_history() {
        let root = temp_root("moondesk-worker-receipt-compaction");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-receipt-compaction");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor_identity,
                "receipt compaction seed",
            ))
            .await
            .expect("spawn receipt compaction seed");
        let mut family = broker
            .snapshot()
            .await
            .families
            .into_values()
            .next()
            .expect("seed family");

        for _ in 0..(MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY + 32) {
            let sequence =
                allocate_receipt_sequence(&mut family).expect("allocate spawn receipt sequence");
            family.spawn_requests.insert(
                OperationId::new(),
                SpawnReceipt {
                    request_fingerprint: "a".repeat(64),
                    sequence,
                    family_id: family.id.clone(),
                    worker_id: spawned.worker_id.clone(),
                    task_id: TaskId::new(),
                    display_id: "worker-history".into(),
                    claim_token: "c".repeat(64),
                    claim_operation_id: Some(OperationId::new()),
                },
            );
        }
        for _ in 0..(MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY + 32) {
            let sequence =
                allocate_receipt_sequence(&mut family).expect("allocate reuse receipt sequence");
            family.reuse_requests.insert(
                OperationId::new(),
                ReuseReceipt {
                    request_fingerprint: "b".repeat(64),
                    sequence,
                    worker_id: spawned.worker_id.clone(),
                    task_id: TaskId::new(),
                    display_id: "worker-history".into(),
                    execution_profile: profile(),
                    conversation_url: format!("https://chatgpt.com/c/{}", Uuid::new_v4()),
                },
            );
        }
        for _ in 0..(MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY + 32) {
            let sequence =
                allocate_receipt_sequence(&mut family).expect("allocate message receipt sequence");
            family.message_requests.insert(
                OperationId::new(),
                MessageReceipt {
                    request_fingerprint: "c".repeat(64),
                    sequence,
                    worker_id: spawned.worker_id.clone(),
                    message_id: WorkerMessageId::new(),
                },
            );
        }
        for _ in 0..(MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY + 32) {
            let sequence =
                allocate_receipt_sequence(&mut family).expect("allocate report receipt sequence");
            family.report_requests.insert(
                OperationId::new(),
                ReportReceipt {
                    request_fingerprint: "d".repeat(64),
                    sequence,
                    worker_id: spawned.worker_id.clone(),
                    report_id: WorkerReportId::new(),
                },
            );
        }
        for _ in 0..(MAX_COLLECT_RECEIPTS_PER_FAMILY + 8) {
            let sequence =
                allocate_receipt_sequence(&mut family).expect("allocate collect receipt sequence");
            family.collect_requests.insert(
                OperationId::new(),
                CollectReceipt {
                    request_fingerprint: "e".repeat(64),
                    sequence,
                    reports: Vec::new(),
                    completed: Vec::new(),
                    failed: Vec::new(),
                },
            );
        }

        compact_family_history(&mut family).expect("compact receipt history");
        assert!(family.spawn_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.reuse_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.message_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.report_requests.len() <= MAX_IDEMPOTENCY_RECEIPTS_PER_FAMILY);
        assert!(family.collect_requests.len() <= MAX_COLLECT_RECEIPTS_PER_FAMILY);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn large_delivered_payload_history_stays_within_worker_store_budget() {
        let root = temp_root("moondesk-worker-large-history-budget");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let anchor_identity = anchor("anchor-large-history-budget");
        let worker_identity = anchor("worker-large-history-budget");
        let large_assignment = "a".repeat(MAX_WORKER_ASSIGNMENT_BYTES);
        let large_message = "m".repeat(MAX_WORKER_MESSAGE_BYTES);
        let large_report = "r".repeat(MAX_WORKER_MESSAGE_BYTES);
        let large_result = WorkerResult {
            result: "z".repeat(96 * 1024),
            changes: "large payload regression".into(),
            validation: "persisted and collected".into(),
            blockers: Vec::new(),
        };
        large_result
            .validate()
            .expect("large result stays within result budget");

        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor_identity.clone(),
                &large_assignment,
            ))
            .await
            .expect("spawn large-payload worker");
        make_claimable(&broker, &workspace, &anchor_identity, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim large-payload worker");

        let worker_id = spawned.worker_id.clone();
        let mut task_id = spawned.task_id.clone();
        for index in 0..20 {
            let message = broker
                .message_worker(MessageWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    anchor_identity: anchor_identity.clone(),
                    worker_id: worker_id.clone(),
                    body: large_message.clone(),
                })
                .await
                .expect("store large message");
            broker
                .acknowledge_message(
                    &workspace,
                    &worker_identity,
                    &worker_id,
                    &message.message_id,
                )
                .await
                .expect("ack large message");
            broker
                .report_worker(ReportWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    worker_identity: worker_identity.clone(),
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    body: large_report.clone(),
                })
                .await
                .expect("store large report");
            broker
                .finish_task(FinishTaskRequest {
                    workspace_id: workspace.clone(),
                    worker_identity: worker_identity.clone(),
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    result: large_result.clone(),
                })
                .await
                .expect("finish large result");
            let updates = broker
                .collect_updates_for_operation(&workspace, &anchor_identity, &OperationId::new())
                .await
                .expect("collect large payload batch");
            assert_eq!(updates.reports.len(), 1);
            assert_eq!(updates.completed.len(), 1);

            if index == 19 {
                break;
            }
            let reused = broker
                .reuse_worker(ReuseWorkerRequest {
                    operation_id: OperationId::new(),
                    workspace_id: workspace.clone(),
                    anchor_identity: anchor_identity.clone(),
                    worker_id: worker_id.clone(),
                    assignment: large_assignment.clone(),
                })
                .await
                .expect("reuse large-payload worker");
            task_id = reused.task_id.clone();
            broker
                .start_task(&workspace, &worker_identity, &worker_id, &task_id)
                .await
                .expect("start large-payload task");
        }

        let snapshot = broker.snapshot().await;
        snapshot
            .validate()
            .expect("bounded large worker history validates");
        let persisted_bytes = std::fs::metadata(&path)
            .expect("large worker store metadata")
            .len();
        assert!(persisted_bytes < 4 * 1024 * 1024);
        assert!(persisted_bytes <= crate::workers::MAX_WORKER_STORE_BYTES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn global_family_limit_keeps_idle_history_and_reports_actionable_cleanup() {
        let root = temp_root("moondesk-worker-family-global-idle-limit");
        std::fs::create_dir_all(&root).expect("create worker family root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let mut data = WorkerStoreData::default();
        for index in 0..MAX_WORKER_FAMILIES {
            let family = inactive_family(
                workspace.clone(),
                anchor(&format!("idle-family-{index}")),
                WorkerState::Idle,
                true,
            );
            data.families.insert(family.id.clone(), family);
        }
        store::save(&path, &data).expect("seed maximum idle Core families");
        let broker = WorkerBroker::open(&path).expect("open bounded worker broker");
        let error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor("new-core-at-idle-limit"),
                "new Core waits for explicit old-Core cleanup",
            ))
            .await
            .expect_err("idle reusable workers must not be silently evicted");
        let message = error.to_string();
        assert!(message.contains("64 Core families"));
        assert!(message.contains("safe to release"));
        assert!(message.contains("Release inactive Worker capacity"));
        assert_eq!(broker.snapshot().await.families.len(), MAX_WORKER_FAMILIES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn host_cleanup_reclaims_idle_families_without_touching_other_workspaces_or_uncollected_results()
     {
        let root = temp_root("moondesk-worker-host-cleanup-capacity");
        std::fs::create_dir_all(&root).expect("create worker cleanup root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let other_workspace = WorkspaceId::new();
        let mut data = WorkerStoreData::default();

        for index in 0..(MAX_WORKER_FAMILIES - 2) {
            let mut family = inactive_family(
                workspace.clone(),
                anchor(&format!("stale-idle-family-{index}")),
                WorkerState::Idle,
                true,
            );
            if index == 0 {
                let worker = family.workers.values().next().expect("cleanup worker");
                let task = worker.tasks.values().next().expect("cleanup task");
                family.collect_requests.insert(
                    OperationId::new(),
                    CollectReceipt {
                        request_fingerprint: "a".repeat(64),
                        sequence: 1,
                        reports: Vec::new(),
                        completed: vec![CollectedTaskReceipt {
                            worker_id: worker.id.clone(),
                            display_id: worker.display_id.clone(),
                            task_id: task.id.clone(),
                            result: task.result.clone().expect("cleanup task result"),
                        }],
                        failed: Vec::new(),
                    },
                );
            }
            data.families.insert(family.id.clone(), family);
        }

        let protected = inactive_family(
            workspace.clone(),
            anchor("uncollected-family"),
            WorkerState::Retired,
            false,
        );
        let protected_id = protected.id.clone();
        data.families.insert(protected.id.clone(), protected);

        let other = inactive_family(
            other_workspace.clone(),
            anchor("other-workspace-idle-family"),
            WorkerState::Idle,
            true,
        );
        let other_id = other.id.clone();
        data.families.insert(other.id.clone(), other);
        assert_eq!(data.families.len(), MAX_WORKER_FAMILIES);
        store::save(&path, &data).expect("seed capacity-blocking worker families");

        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let preview = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview host cleanup");
        assert_eq!(preview.summary.family_count, MAX_WORKER_FAMILIES - 2);
        assert_eq!(preview.summary.replay_receipt_count, 1);

        let removed = broker
            .cleanup_inactive_families_for_plan(&preview)
            .await
            .expect("explicit host cleanup");
        assert_eq!(removed, preview.summary);
        let snapshot = broker.snapshot().await;
        assert!(snapshot.families.contains_key(&protected_id));
        assert!(snapshot.families.contains_key(&other_id));
        assert_eq!(snapshot.families.len(), 2);

        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor("new-core-after-host-cleanup"),
                "capacity recovers without reopening old Core chats",
            ))
            .await
            .expect("host cleanup frees capacity for a new Core");
        let snapshot = broker.snapshot().await;
        assert!(snapshot.families.contains_key(&other_id));
        assert_eq!(
            snapshot
                .families
                .values()
                .filter(|family| family.workspace_id == other_workspace)
                .count(),
            1,
            "host cleanup must stay scoped to the selected workspace"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn host_cleanup_recovers_capacity_from_sixty_four_unreachable_idle_cores() {
        let root = temp_root("moondesk-worker-host-cleanup-sixty-four-idle");
        std::fs::create_dir_all(&root).expect("create worker cleanup root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let mut data = WorkerStoreData::default();
        for index in 0..MAX_WORKER_FAMILIES {
            let family = inactive_family(
                workspace.clone(),
                anchor(&format!("unreachable-idle-core-{index}")),
                WorkerState::Idle,
                true,
            );
            data.families.insert(family.id.clone(), family);
        }
        store::save(&path, &data).expect("seed sixty-four unreachable idle Cores");
        let broker = WorkerBroker::open(&path).expect("open worker broker");

        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor("blocked-new-core"),
                "prove the global family ceiling is reached first",
            ))
            .await
            .expect_err("sixty-four retained idle Cores initially block a new Core");
        let preview = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview all unreachable idle Cores");
        assert_eq!(preview.summary.family_count, MAX_WORKER_FAMILIES);

        let removed = broker
            .cleanup_inactive_families_for_plan(&preview)
            .await
            .expect("host cleanup does not require any old Core conversation");
        assert_eq!(removed.family_count, MAX_WORKER_FAMILIES);
        assert!(broker.snapshot().await.families.is_empty());

        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor("new-core-after-sixty-four-cleanup"),
                "capacity is available again",
            ))
            .await
            .expect("supported host cleanup recovers global worker capacity");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn host_cleanup_refuses_active_or_ambiguous_worker_family() {
        let root = temp_root("moondesk-worker-host-cleanup-active");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let core = anchor("host-cleanup-active-core");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                core.clone(),
                "keep this active launch protected",
            ))
            .await
            .expect("spawn active worker");
        let command_id = Uuid::new_v4().to_string();
        broker
            .link_launch_command(
                &workspace,
                &core,
                &spawned.worker_id,
                &spawned.task_id,
                &command_id,
            )
            .await
            .expect("link active launch");
        broker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::Paused,
                Some("ambiguous Send outcome".into()),
                None,
            )
            .await
            .expect("mark launch ambiguous");

        let preview = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview active family cleanup");
        assert_eq!(preview.summary.family_count, 0);
        let removed = broker
            .cleanup_inactive_families_for_plan(&preview)
            .await
            .expect("active cleanup attempt is safely a no-op");
        assert_eq!(removed.family_count, 0);
        assert_eq!(broker.snapshot().await.families.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn existing_thread_binding_rejects_other_worker_workspace_or_core_url() {
        let root = temp_root("moondesk-worker-existing-thread-binding");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let core = anchor("binding-core");
        let family = inactive_family(workspace.clone(), core.clone(), WorkerState::Idle, true);
        let worker = family.workers.values().next().expect("worker").clone();
        let durable_url = worker.conversation_url.clone().expect("durable URL");
        let mut data = WorkerStoreData::default();
        data.families.insert(family.id.clone(), family);
        store::save(&path, &data).expect("seed binding family");
        let broker = WorkerBroker::open(&path).expect("open worker broker");

        broker
            .ensure_existing_thread_binding(&workspace, &core, &worker.id, &durable_url)
            .await
            .expect("exact workspace/Core/worker binding");
        let wrong_url = format!("https://chatgpt.com/c/{}", WorkerId::new());
        assert!(
            broker
                .ensure_existing_thread_binding(&workspace, &core, &worker.id, &wrong_url)
                .await
                .is_err(),
            "another worker conversation URL must be rejected"
        );
        assert!(
            broker
                .ensure_existing_thread_binding(
                    &WorkspaceId::new(),
                    &core,
                    &worker.id,
                    &durable_url,
                )
                .await
                .is_err(),
            "another workspace must be rejected"
        );
        assert!(
            broker
                .ensure_existing_thread_binding(
                    &workspace,
                    &anchor("binding-other-core"),
                    &worker.id,
                    &durable_url,
                )
                .await
                .is_err(),
            "another Core must be rejected"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn confirmed_cleanup_plan_does_not_sweep_newly_eligible_family() {
        let root = temp_root("moondesk-worker-cleanup-exact-plan");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let first = inactive_family(
            workspace.clone(),
            anchor("cleanup-preview-first"),
            WorkerState::Idle,
            true,
        );
        let first_id = first.id.clone();
        let mut data = WorkerStoreData::default();
        data.families.insert(first.id.clone(), first);
        store::save(&path, &data).expect("seed first cleanup family");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let plan = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview exact cleanup plan");
        assert_eq!(plan.summary.family_count, 1);

        let second = inactive_family(
            workspace.clone(),
            anchor("cleanup-preview-second"),
            WorkerState::Idle,
            true,
        );
        let second_id = second.id.clone();
        {
            let mut guard = broker.data.lock().await;
            let mut candidate = guard.clone();
            candidate.families.insert(second.id.clone(), second);
            broker
                .commit_candidate(&mut guard, candidate)
                .await
                .expect("make second family eligible after preview");
        }

        let removed = broker
            .cleanup_inactive_families_for_plan(&plan)
            .await
            .expect("commit exact previewed cleanup plan");
        assert_eq!(removed.family_count, 1);
        let snapshot = broker.snapshot().await;
        assert!(!snapshot.families.contains_key(&first_id));
        assert!(
            snapshot.families.contains_key(&second_id),
            "family that became eligible after confirmation preview must survive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn family_finished_and_collected_after_preview_is_not_swept() {
        let root = temp_root("moondesk-worker-cleanup-finish-collect-newly-eligible");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let first = inactive_family(
            workspace.clone(),
            anchor("cleanup-confirmed-first"),
            WorkerState::Idle,
            true,
        );
        let first_id = first.id.clone();
        let mut data = WorkerStoreData::default();
        data.families.insert(first.id.clone(), first);
        store::save(&path, &data).expect("seed confirmed family");
        let broker = WorkerBroker::open(&path).expect("open worker broker");

        let second_core = anchor("cleanup-later-finished-core");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                second_core.clone(),
                "become eligible after confirmation",
            ))
            .await
            .expect("spawn second family");
        make_claimable(&broker, &workspace, &second_core, &spawned).await;
        let second_identity = anchor("cleanup-later-finished-worker");
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                second_identity.clone(),
            )
            .await
            .expect("claim second worker");

        let plan = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview before second family is eligible");
        assert_eq!(plan.summary.family_count, 1);
        assert_eq!(plan.families[0].family_id, first_id);

        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: second_identity,
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: finished_result(),
            })
            .await
            .expect("finish second family after preview");
        let updates = broker
            .collect_updates(&workspace, &second_core)
            .await
            .expect("collect second family after preview");
        assert_eq!(updates.completed.len(), 1);

        broker
            .cleanup_inactive_families_for_plan(&plan)
            .await
            .expect("delete only confirmed first family");
        let snapshot = broker.snapshot().await;
        assert!(!snapshot.families.contains_key(&first_id));
        assert!(
            snapshot
                .families
                .values()
                .any(|family| family.anchor_identity == second_core),
            "family made eligible by finish+collect after preview must survive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn confirmed_cleanup_plan_aborts_after_report_and_collect_transition() {
        let root = temp_root("moondesk-worker-cleanup-report-collect-race");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let core = anchor("cleanup-report-core");
        let family = inactive_family(workspace.clone(), core.clone(), WorkerState::Idle, true);
        let family_id = family.id.clone();
        let worker = family.workers.values().next().expect("worker").clone();
        let worker_identity = worker.chat_identity.clone().expect("worker identity");
        let mut data = WorkerStoreData::default();
        data.families.insert(family.id.clone(), family);
        store::save(&path, &data).expect("seed cleanup family");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let plan = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview cleanup plan");
        broker
            .ensure_inactive_cleanup_plan_unchanged(&plan)
            .await
            .expect("precheck selected cleanup family");

        let reused = broker
            .reuse_worker(ReuseWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                anchor_identity: core.clone(),
                worker_id: worker.id.clone(),
                assignment: "late work between cleanup precheck and commit".into(),
            })
            .await
            .expect("reuse worker after cleanup precheck");
        broker
            .start_task(&workspace, &worker_identity, &worker.id, &reused.task_id)
            .await
            .expect("start late work before reporting");
        broker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: worker.id.clone(),
                task_id: reused.task_id,
                body: "late report between cleanup precheck and commit".into(),
            })
            .await
            .expect("append late report");
        let collected = broker
            .collect_updates(&workspace, &core)
            .await
            .expect("collect late report");
        assert_eq!(collected.reports.len(), 1);

        let error = broker
            .cleanup_inactive_families_for_plan(&plan)
            .await
            .expect_err("report/collect transition must invalidate confirmed snapshot");
        assert!(error.to_string().contains("preview changed"));
        assert!(broker.snapshot().await.families.contains_key(&family_id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn confirmed_cleanup_plan_aborts_when_selected_family_changes() {
        let root = temp_root("moondesk-worker-cleanup-plan-change");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let family = inactive_family(
            workspace.clone(),
            anchor("cleanup-plan-change"),
            WorkerState::Idle,
            true,
        );
        let family_id = family.id.clone();
        let mut data = WorkerStoreData::default();
        data.families.insert(family.id.clone(), family);
        store::save(&path, &data).expect("seed cleanup family");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let plan = broker
            .inactive_cleanup_preview_for_workspace(&workspace)
            .await
            .expect("preview cleanup plan");

        {
            let mut guard = broker.data.lock().await;
            let mut candidate = guard.clone();
            candidate
                .families
                .get_mut(&family_id)
                .expect("selected family")
                .next_receipt_sequence += 1;
            broker
                .commit_candidate(&mut guard, candidate)
                .await
                .expect("mutate selected family after preview");
        }

        let error = broker
            .cleanup_inactive_families_for_plan(&plan)
            .await
            .expect_err("changed selected family must require reconfirmation");
        assert!(error.to_string().contains("preview changed"));
        assert!(broker.snapshot().await.families.contains_key(&family_id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn automatic_family_compaction_preserves_live_collect_replay_receipt() {
        let root = temp_root("moondesk-worker-family-collect-replay-retention");
        std::fs::create_dir_all(&root).expect("create worker replay root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let original_anchor = anchor("replay-protected-family");
        let worker_identity = anchor("replay-protected-worker");
        let replay_operation = OperationId::new();
        let broker = WorkerBroker::open(&path).expect("open replay source broker");

        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                original_anchor.clone(),
                "produce one replayable collected result",
            ))
            .await
            .expect("spawn replay worker");
        make_claimable(&broker, &workspace, &original_anchor, &spawned).await;
        broker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim replay worker");
        let replay_result = WorkerResult {
            result: "replay-me".into(),
            changes: "collect response is intentionally dropped".into(),
            validation: "same operation id must replay after capacity pressure".into(),
            blockers: Vec::new(),
        };
        broker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: replay_result.clone(),
            })
            .await
            .expect("finish replay worker task");
        let dropped_response = broker
            .collect_updates_for_operation(&workspace, &original_anchor, &replay_operation)
            .await
            .expect("commit collect receipt before simulating a dropped response");
        assert_eq!(dropped_response.completed.len(), 1);
        broker
            .retire_worker(&workspace, &original_anchor, &spawned.worker_id, None)
            .await
            .expect("retire replay worker after the collected result is durable");
        let replay_family_id = broker
            .family_for_anchor(&workspace, &original_anchor)
            .await
            .expect("inspect replay family")
            .expect("replay family remains retained")
            .id;
        drop(broker);

        let mut data = store::load(&path).expect("reload durable dropped-response receipt");
        let replay_family = data
            .families
            .get(&replay_family_id)
            .expect("replay family persisted before capacity pressure");
        assert!(
            replay_family
                .collect_requests
                .contains_key(&replay_operation)
        );
        for index in 0..(MAX_WORKER_FAMILIES - 1) {
            let family = inactive_family(
                workspace.clone(),
                anchor(&format!("reclaimable-retired-family-{index}")),
                WorkerState::Retired,
                true,
            );
            data.families.insert(family.id.clone(), family);
        }
        assert_eq!(data.families.len(), MAX_WORKER_FAMILIES);
        store::save(&path, &data).expect("persist capacity pressure around replay family");

        let broker = WorkerBroker::open(&path).expect("reopen replay-protected broker");
        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor("new-core-for-replay-compaction"),
                "force one automatic family compaction",
            ))
            .await
            .expect("another reclaimable family should free capacity");
        assert!(
            broker
                .snapshot()
                .await
                .families
                .contains_key(&replay_family_id),
            "unrelated Core activity must not evict a still-replayable collect payload"
        );

        let replayed = broker
            .collect_updates_for_operation(&workspace, &original_anchor, &replay_operation)
            .await
            .expect("retry original dropped collect response");
        assert_eq!(replayed.completed.len(), 1);
        assert_eq!(replayed.completed[0].result, replay_result);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn global_family_compaction_preserves_retired_uncollected_results() {
        let root = temp_root("moondesk-worker-family-uncollected-result-limit");
        std::fs::create_dir_all(&root).expect("create worker family root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let mut data = WorkerStoreData::default();
        for index in 0..MAX_WORKER_FAMILIES {
            let family = inactive_family(
                workspace.clone(),
                anchor(&format!("retired-uncollected-family-{index}")),
                WorkerState::Retired,
                false,
            );
            data.families.insert(family.id.clone(), family);
        }
        store::save(&path, &data).expect("seed retired families with uncollected results");
        let broker = WorkerBroker::open(&path).expect("open bounded worker broker");
        let error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor("new-core-before-result-collection"),
                "do not erase undelivered result history",
            ))
            .await
            .expect_err("uncollected results must block automatic family eviction");
        assert!(error.to_string().contains("collect pending worker results"));
        assert_eq!(broker.snapshot().await.families.len(), MAX_WORKER_FAMILIES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn creating_a_new_core_family_compacts_inactive_family_history_at_global_limit() {
        let root = temp_root("moondesk-worker-family-global-limit");
        std::fs::create_dir_all(&root).expect("create worker family root");
        let path = root.join("worker-state-v1.json");
        let workspace = WorkspaceId::new();
        let mut data = WorkerStoreData::default();
        for index in 0..MAX_WORKER_FAMILIES {
            let family_id = WorkerFamilyId::new();
            data.families.insert(
                family_id.clone(),
                WorkerFamily {
                    id: family_id,
                    workspace_id: workspace.clone(),
                    anchor_identity: anchor(&format!("retired-family-{index}")),
                    next_receipt_sequence: 0,
                    workers: Default::default(),
                    reports: Vec::new(),
                    spawn_requests: Default::default(),
                    reuse_requests: Default::default(),
                    message_requests: Default::default(),
                    report_requests: Default::default(),
                    collect_requests: Default::default(),
                },
            );
        }
        store::save(&path, &data).expect("seed maximum inactive Core families");
        let broker = WorkerBroker::open(&path).expect("open bounded worker broker");
        broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                anchor("new-core-after-compaction"),
                "new Core replaces inactive history",
            ))
            .await
            .expect("inactive family history must be compacted before rejecting a new Core");
        assert_eq!(broker.snapshot().await.families.len(), MAX_WORKER_FAMILIES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn worker_family_supports_eight_active_workers_and_rejects_ninth_without_partial_state() {
        let root = temp_root("moondesk-worker-limit");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let identity = anchor("anchor-limit");
        let mut display_ids = Vec::new();
        for index in 1..=MAX_WORKERS_PER_FAMILY {
            let receipt = broker
                .spawn_worker(spawn_request(
                    OperationId::new(),
                    workspace.clone(),
                    identity.clone(),
                    &format!("assignment-{index}"),
                ))
                .await
                .expect("spawn allowed worker");
            display_ids.push(receipt.display_id);
        }
        assert_eq!(MAX_WORKERS_PER_FAMILY, 8);
        assert_eq!(
            display_ids,
            (1..=MAX_WORKERS_PER_FAMILY)
                .map(|index| format!("worker-{index}"))
                .collect::<Vec<_>>()
        );
        let error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                identity,
                "ninth",
            ))
            .await
            .expect_err("ninth worker must be rejected");
        assert!(matches!(error, WorkerBrokerError::Limit(_)));
        let family = broker
            .snapshot()
            .await
            .families
            .into_values()
            .next()
            .expect("worker family");
        assert_eq!(family.workers.len(), MAX_WORKERS_PER_FAMILY);
        assert_eq!(
            family
                .workers
                .values()
                .filter(|worker| worker.state != WorkerState::Retired)
                .count(),
            8
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
