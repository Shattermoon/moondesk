use super::store;
use super::types::{
    BrowserAttachmentState, ChatIdentity, MessageReceipt, OperationId, ReportReceipt, ReuseReceipt,
    SpawnReceipt, TaskId, TaskState, WorkerExecutionProfile, WorkerFamily, WorkerFamilyId,
    WorkerId, WorkerMessage, WorkerMessageId, WorkerMessageState, WorkerRecord, WorkerReport,
    WorkerReportId, WorkerResult, WorkerState, WorkerStoreData, WorkerTask,
};
use super::{
    MAX_PENDING_MESSAGES_PER_WORKER, MAX_WORKER_ASSIGNMENT_BYTES, MAX_WORKER_MESSAGE_BYTES,
    MAX_WORKERS_PER_FAMILY,
};
use crate::workspaces::WorkspaceId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
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
pub struct CollectedUpdates {
    pub reports: Vec<WorkerReport>,
    pub completed: Vec<CollectedWorkerUpdate>,
}

pub struct WorkerBroker {
    path: PathBuf,
    data: Mutex<WorkerStoreData>,
}

impl WorkerBroker {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, WorkerBrokerError> {
        let path = path.into();
        let data = store::load(&path).map_err(storage_error)?;
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    #[cfg(test)]
    pub async fn snapshot(&self) -> WorkerStoreData {
        self.data.lock().await.clone()
    }

    pub async fn spawn_worker(
        &self,
        request: SpawnWorkerRequest,
    ) -> Result<SpawnReceipt, WorkerBrokerError> {
        validate_spawn_request(&request)?;
        let fingerprint = request_fingerprint(&request)?;
        let mut guard = self.data.lock().await;

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
        let family_id = candidate
            .families
            .values()
            .find(|family| {
                family.workspace_id == request.workspace_id
                    && family.anchor_identity == request.anchor_identity
            })
            .map(|family| family.id.clone())
            .unwrap_or_else(|| {
                let id = WorkerFamilyId::new();
                candidate.families.insert(
                    id.clone(),
                    WorkerFamily {
                        id: id.clone(),
                        workspace_id: request.workspace_id.clone(),
                        anchor_identity: request.anchor_identity.clone(),
                        workers: Default::default(),
                        reports: Vec::new(),
                        spawn_requests: Default::default(),
                        reuse_requests: Default::default(),
                        message_requests: Default::default(),
                        report_requests: Default::default(),
                    },
                );
                id
            });
        let family = candidate.families.get_mut(&family_id).ok_or_else(|| {
            WorkerBrokerError::Storage("worker family disappeared during mutation".into())
        })?;

        if family.workers.len() >= MAX_WORKERS_PER_FAMILY {
            return Err(WorkerBrokerError::Limit(format!(
                "worker family is limited to {MAX_WORKERS_PER_FAMILY} workers"
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
        let task = WorkerTask {
            id: task_id.clone(),
            assignment: request.assignment,
            state: TaskState::Pending,
            result: None,
            collected: false,
        };
        let worker = WorkerRecord {
            id: worker_id.clone(),
            display_id: display_id.clone(),
            label: request.label,
            state: WorkerState::Provisioning,
            attachment_state: BrowserAttachmentState::Absent,
            execution_profile: request.execution_profile,
            chat_identity: None,
            claim_token: Some(claim_token.clone()),
            current_task_id: Some(task_id.clone()),
            tasks: [(task_id.clone(), task)].into_iter().collect(),
            messages: Vec::new(),
        };
        family.workers.insert(worker_id.clone(), worker);

        let receipt = SpawnReceipt {
            request_fingerprint: fingerprint,
            family_id,
            worker_id,
            task_id,
            display_id,
            claim_token,
        };
        family
            .spawn_requests
            .insert(request.operation_id, receipt.clone());

        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
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
                return Ok(receipt.clone());
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
                    collected: false,
                },
            );
            worker.current_task_id = Some(task_id.clone());
            worker.state = WorkerState::Waking;
            worker.attachment_state = BrowserAttachmentState::Opening;
            (worker.display_id.clone(), worker.execution_profile.clone())
        };
        let receipt = ReuseReceipt {
            request_fingerprint: fingerprint,
            worker_id: request.worker_id,
            task_id,
            display_id,
            execution_profile,
        };
        family
            .reuse_requests
            .insert(request.operation_id, receipt.clone());
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
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

    pub async fn claim_worker(
        &self,
        workspace_id: &WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
        claim_token: &str,
        worker_identity: ChatIdentity,
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
        worker.state = WorkerState::Running;
        worker.attachment_state = BrowserAttachmentState::Attached;
        let task = worker
            .tasks
            .get_mut(task_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        task.state = TaskState::Running;
        let claimed = worker.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(claimed)
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
        let receipt = MessageReceipt {
            request_fingerprint: fingerprint,
            worker_id: request.worker_id,
            message_id,
        };
        family
            .message_requests
            .insert(request.operation_id, receipt.clone());

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
            || !worker.tasks.contains_key(&request.task_id)
        {
            return Err(WorkerBrokerError::NotFound);
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
        let report_id = WorkerReportId::new();
        family.reports.push(WorkerReport {
            id: report_id.clone(),
            worker_id: request.worker_id.clone(),
            task_id: request.task_id,
            body: request.body,
            collected: false,
        });
        let receipt = ReportReceipt {
            request_fingerprint: fingerprint,
            worker_id: request.worker_id,
            report_id,
        };
        family
            .report_requests
            .insert(request.operation_id, receipt.clone());
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(receipt)
    }

    pub async fn collect_updates(
        &self,
        workspace_id: &WorkspaceId,
        anchor_identity: &ChatIdentity,
    ) -> Result<CollectedUpdates, WorkerBrokerError> {
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
        let reports: Vec<_> = family
            .reports
            .iter()
            .filter(|report| !report.collected)
            .cloned()
            .collect();
        let mut completed = Vec::new();
        for worker in family.workers.values() {
            for task in worker.tasks.values() {
                if task.collected {
                    continue;
                }
                if let Some(result) = &task.result {
                    completed.push(CollectedWorkerUpdate {
                        worker_id: worker.id.clone(),
                        display_id: worker.display_id.clone(),
                        task_id: task.id.clone(),
                        result: result.clone(),
                    });
                }
            }
        }
        if reports.is_empty() && completed.is_empty() {
            return Ok(CollectedUpdates { reports, completed });
        }

        let mut candidate = guard.clone();
        let family = candidate
            .families
            .get_mut(&family_id)
            .ok_or(WorkerBrokerError::NotFound)?;
        for report in &mut family.reports {
            if !report.collected {
                report.collected = true;
            }
        }
        for worker in family.workers.values_mut() {
            for task in worker.tasks.values_mut() {
                if task.result.is_some() {
                    task.collected = true;
                }
            }
        }
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(CollectedUpdates { reports, completed })
    }

    pub async fn finish_task(
        &self,
        request: FinishTaskRequest,
    ) -> Result<WorkerResult, WorkerBrokerError> {
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
        if worker.current_task_id.as_ref() == Some(&request.task_id) {
            worker.current_task_id = None;
            worker.state = WorkerState::Idle;
        }

        self.commit_candidate(&mut guard, candidate).await?;
        Ok(request.result)
    }

    async fn commit_candidate(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, WorkerStoreData>,
        candidate: WorkerStoreData,
    ) -> Result<(), WorkerBrokerError> {
        let path = self.path.clone();
        let persisted = candidate.clone();
        tokio::task::spawn_blocking(move || store::save(&path, &persisted))
            .await
            .map_err(|error| {
                WorkerBrokerError::Storage(format!("worker state persistence task failed: {error}"))
            })?
            .map_err(storage_error)?;
        **guard = candidate;
        Ok(())
    }
}

fn new_claim_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn storage_error(error: std::io::Error) -> WorkerBrokerError {
    WorkerBrokerError::Storage(format!("failed to persist worker state: {error}"))
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

    #[tokio::test]
    async fn spawn_is_idempotent_and_rejects_changed_input_for_same_operation() {
        let root = temp_root("moondesk-worker-spawn-idempotent");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let identity = anchor("anchor-a");
        let operation = OperationId::new();

        let first = broker
            .spawn_worker(spawn_request(
                operation.clone(),
                workspace.clone(),
                identity.clone(),
                "audit auth",
            ))
            .await
            .expect("spawn worker");
        let retry = broker
            .spawn_worker(spawn_request(
                operation.clone(),
                workspace.clone(),
                identity.clone(),
                "audit auth",
            ))
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
            family.workers.get(&spawned.worker_id).map(|worker| worker.state),
            Some(WorkerState::Idle)
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
    async fn finish_is_idempotent_but_conflicting_second_result_is_rejected() {
        let root = temp_root("moondesk-worker-finish-idempotent");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let worker_identity = anchor("worker-finish");
        let spawned = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor("anchor-finish"),
                "finish task",
            ))
            .await
            .expect("spawn worker");
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
        std::fs::create_dir_all(&root).expect("create worker persistence test root");
        let blocked_parent = root.join("blocked-parent");
        std::fs::write(&blocked_parent, "not a directory").expect("create blocked parent file");
        let broker = WorkerBroker::open(blocked_parent.join("worker-state-v1.json"))
            .expect("open broker before first worker state file exists");

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
    async fn worker_family_limit_is_enforced_without_partial_third_worker() {
        let root = temp_root("moondesk-worker-limit");
        let path = root.join("worker-state-v1.json");
        let broker = WorkerBroker::open(&path).expect("open worker broker");
        let workspace = WorkspaceId::new();
        let identity = anchor("anchor-limit");
        for assignment in ["first", "second"] {
            broker
                .spawn_worker(spawn_request(
                    OperationId::new(),
                    workspace.clone(),
                    identity.clone(),
                    assignment,
                ))
                .await
                .expect("spawn allowed worker");
        }
        let error = broker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace,
                identity,
                "third",
            ))
            .await
            .expect_err("third worker must be rejected");
        assert!(matches!(error, WorkerBrokerError::Limit(_)));
        let family = broker
            .snapshot()
            .await
            .families
            .into_values()
            .next()
            .expect("worker family");
        assert_eq!(family.workers.len(), MAX_WORKERS_PER_FAMILY);
        let _ = std::fs::remove_dir_all(root);
    }
}
