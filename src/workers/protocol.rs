use super::broker::{
    FinishTaskRequest, MessageWorkerRequest, ReportWorkerRequest, ReuseWorkerRequest,
    SpawnWorkerRequest, WorkerBroker, WorkerBrokerError,
};
use super::prompt;
use super::types::{
    ChatIdentity, OperationId, TaskId, WorkerExecutionProfile, WorkerId, WorkerMessageId,
    WorkerResult, WorkerState,
};
use crate::managed_chat::broker::{EnqueueManagedChatRequest, ManagedChatBroker};
use crate::managed_chat::types::{ManagedChatLaunch, ManagedChatOpenMode, ManagedChatPurpose};
use crate::workspaces::WorkspaceId;
use serde_json::{Value, json};

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing or invalid required parameter: {name}"))
}

fn parse_operation_id(arguments: &Value) -> Result<OperationId, String> {
    OperationId::parse(required_string(arguments, "operation_id")?)
}

fn parse_worker_id(arguments: &Value) -> Result<WorkerId, String> {
    WorkerId::parse(required_string(arguments, "worker_id")?)
}

fn parse_task_id(arguments: &Value) -> Result<TaskId, String> {
    TaskId::parse(required_string(arguments, "task_id")?)
}

fn parse_message_id(arguments: &Value) -> Result<WorkerMessageId, String> {
    WorkerMessageId::parse(required_string(arguments, "message_id")?)
}

fn optional_u64(arguments: &Value, name: &str, default: u64) -> Result<u64, String> {
    match arguments.get(name) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("Parameter {name} must be a non-negative integer")),
        None => Ok(default),
    }
}

fn blockers(arguments: &Value) -> Result<Vec<String>, String> {
    let Some(value) = arguments.get("blockers") else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "Parameter blockers must be an array of strings".to_string())?;
    if values.len() > 100 {
        return Err("Parameter blockers contains more than 100 items".into());
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("Parameter blockers[{index}] must be a string"))
        })
        .collect()
}

fn broker_error(error: WorkerBrokerError) -> String {
    error.to_string()
}

pub async fn handle(
    arguments: &Value,
    workspace_id: &WorkspaceId,
    workspace_name: &str,
    caller_identity: &ChatIdentity,
    execution_profile: &WorkerExecutionProfile,
    broker: &WorkerBroker,
    managed_chat_broker: &ManagedChatBroker,
) -> Result<Value, String> {
    let action = required_string(arguments, "action")?;
    match action {
        "spawn" => {
            let assignment = required_string(arguments, "task")?.to_string();
            let execution_profile = execution_profile.clone();
            let receipt = broker
                .spawn_worker(SpawnWorkerRequest {
                    operation_id: parse_operation_id(arguments)?,
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    label: required_string(arguments, "label")?.to_string(),
                    assignment: assignment.clone(),
                    execution_profile: execution_profile.clone(),
                })
                .await
                .map_err(broker_error)?;

            let opening_message = prompt::bootstrap_message(
                workspace_name,
                &assignment,
                &receipt,
                &execution_profile,
            );
            let launch = managed_chat_broker
                .enqueue(EnqueueManagedChatRequest {
                    dedupe_key: format!("worker:{}:task:{}", receipt.worker_id, receipt.task_id),
                    launch: ManagedChatLaunch {
                        workspace_id: workspace_id.clone(),
                        purpose: ManagedChatPurpose::Worker,
                        execution_profile: execution_profile.clone(),
                        opening_message,
                        task_marker: format!("moondesk-worker-task:{}", receipt.task_id),
                        thread_key: Some(format!("worker:{}", receipt.worker_id)),
                        open_mode: ManagedChatOpenMode::NewThread,
                    },
                })
                .await
                .map_err(|error| {
                    format!("worker was persisted but browser launch was not accepted: {error}")
                })?;

            Ok(json!({
                "action": "spawn",
                "familyId": receipt.family_id,
                "workerId": receipt.worker_id,
                "taskId": receipt.task_id,
                "displayId": receipt.display_id,
                "claimToken": receipt.claim_token,
                "executionProfile": execution_profile,
                "launchCommandId": launch.id,
                "state": "provisioning"
            }))
        }
        "reuse" => {
            let assignment = required_string(arguments, "task")?.to_string();
            let receipt = broker
                .reuse_worker(ReuseWorkerRequest {
                    operation_id: parse_operation_id(arguments)?,
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    worker_id: parse_worker_id(arguments)?,
                    assignment: assignment.clone(),
                })
                .await
                .map_err(broker_error)?;
            let opening_message = prompt::reuse_message(
                workspace_name,
                &assignment,
                &receipt.display_id,
                &receipt.worker_id,
                &receipt.task_id,
                &receipt.execution_profile,
            );
            let launch = managed_chat_broker
                .enqueue(EnqueueManagedChatRequest {
                    dedupe_key: format!("worker:{}:task:{}", receipt.worker_id, receipt.task_id),
                    launch: ManagedChatLaunch {
                        workspace_id: workspace_id.clone(),
                        purpose: ManagedChatPurpose::Worker,
                        execution_profile: receipt.execution_profile.clone(),
                        opening_message,
                        task_marker: format!("moondesk-worker-task:{}", receipt.task_id),
                        thread_key: Some(format!("worker:{}", receipt.worker_id)),
                        open_mode: ManagedChatOpenMode::ExistingThread,
                    },
                })
                .await
                .map_err(|error| {
                    format!("worker reuse was persisted but browser wake was not accepted: {error}")
                })?;
            Ok(json!({
                "action": "reuse",
                "workerId": receipt.worker_id,
                "taskId": receipt.task_id,
                "displayId": receipt.display_id,
                "executionProfile": receipt.execution_profile,
                "launchCommandId": launch.id,
                "state": "waking"
            }))
        }
        "status" => {
            let family = broker
                .family_for_anchor(workspace_id, caller_identity)
                .await
                .map_err(broker_error)?;
            let Some(family) = family else {
                return Ok(json!({ "action": "status", "family": null, "workers": [] }));
            };
            let workers: Vec<Value> = family
                .workers
                .values()
                .map(|worker| {
                    json!({
                        "workerId": worker.id,
                        "displayId": worker.display_id,
                        "label": worker.label,
                        "state": worker.state,
                        "attachmentState": worker.attachment_state,
                        "currentTaskId": worker.current_task_id,
                        "claimed": worker.chat_identity.is_some(),
                        "executionProfile": worker.execution_profile,
                        "pendingMessages": worker.messages.len()
                    })
                })
                .collect();
            Ok(json!({
                "action": "status",
                "familyId": family.id,
                "workers": workers
            }))
        }
        "retire" => {
            let worker_id = parse_worker_id(arguments)?;
            let family = broker
                .family_for_anchor(workspace_id, caller_identity)
                .await
                .map_err(broker_error)?
                .ok_or_else(|| "worker was not found".to_string())?;
            let worker = family
                .workers
                .get(&worker_id)
                .cloned()
                .ok_or_else(|| "worker was not found".to_string())?;
            let pending_task = match worker.state {
                WorkerState::Retired | WorkerState::Idle => None,
                WorkerState::Provisioning | WorkerState::Waking => {
                    let task_id = worker
                        .current_task_id
                        .clone()
                        .ok_or_else(|| "worker pending launch has no current task".to_string())?;
                    let dedupe_key = format!("worker:{}:task:{}", worker_id, task_id);
                    managed_chat_broker
                        .cancel_pre_send_by_dedupe(&dedupe_key)
                        .await
                        .map_err(|error| format!("worker cannot be retired safely: {error}"))?;
                    Some(task_id)
                }
                WorkerState::Running => {
                    return Err(
                        "running worker cannot be retired; finish or resolve its active task first"
                            .into(),
                    );
                }
            };
            let retired = broker
                .retire_worker(
                    workspace_id,
                    caller_identity,
                    &worker_id,
                    pending_task.as_ref(),
                )
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "retire",
                "workerId": retired.id,
                "displayId": retired.display_id,
                "state": retired.state
            }))
        }
        "send" => {
            let receipt = broker
                .message_worker(MessageWorkerRequest {
                    operation_id: parse_operation_id(arguments)?,
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    worker_id: parse_worker_id(arguments)?,
                    body: required_string(arguments, "message")?.to_string(),
                })
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "send",
                "workerId": receipt.worker_id,
                "messageId": receipt.message_id,
                "state": "accepted"
            }))
        }
        "collect" => {
            let updates = broker
                .collect_updates_wait(
                    workspace_id,
                    caller_identity,
                    optional_u64(arguments, "wait_ms", 0)?,
                )
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "collect",
                "reports": updates.reports,
                "completed": updates.completed.into_iter().map(|update| json!({
                    "workerId": update.worker_id,
                    "displayId": update.display_id,
                    "taskId": update.task_id,
                    "result": update.result
                })).collect::<Vec<_>>()
            }))
        }
        "claim" => {
            let worker = broker
                .claim_worker(
                    workspace_id,
                    &parse_worker_id(arguments)?,
                    &parse_task_id(arguments)?,
                    required_string(arguments, "claim_token")?,
                    caller_identity.clone(),
                )
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "claim",
                "workerId": worker.id,
                "displayId": worker.display_id,
                "state": worker.state,
                "taskId": worker.current_task_id,
                "executionProfile": worker.execution_profile
            }))
        }
        "start" => {
            let worker = broker
                .start_task(
                    workspace_id,
                    caller_identity,
                    &parse_worker_id(arguments)?,
                    &parse_task_id(arguments)?,
                )
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "start",
                "workerId": worker.id,
                "displayId": worker.display_id,
                "state": worker.state,
                "taskId": worker.current_task_id,
                "executionProfile": worker.execution_profile
            }))
        }
        "inbox" => {
            let worker_id = parse_worker_id(arguments)?;
            let messages = broker
                .pending_messages(workspace_id, caller_identity, &worker_id)
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "inbox",
                "workerId": worker_id,
                "messages": messages
            }))
        }
        "ack" => {
            let worker_id = parse_worker_id(arguments)?;
            let message_id = parse_message_id(arguments)?;
            broker
                .acknowledge_message(workspace_id, caller_identity, &worker_id, &message_id)
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "ack",
                "workerId": worker_id,
                "messageId": message_id,
                "state": "acknowledged"
            }))
        }
        "report" => {
            let receipt = broker
                .report_worker(ReportWorkerRequest {
                    operation_id: parse_operation_id(arguments)?,
                    workspace_id: workspace_id.clone(),
                    worker_identity: caller_identity.clone(),
                    worker_id: parse_worker_id(arguments)?,
                    task_id: parse_task_id(arguments)?,
                    body: required_string(arguments, "message")?.to_string(),
                })
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "report",
                "workerId": receipt.worker_id,
                "reportId": receipt.report_id,
                "state": "accepted"
            }))
        }
        "finish" => {
            let worker_id = parse_worker_id(arguments)?;
            let task_id = parse_task_id(arguments)?;
            let result = broker
                .finish_task(FinishTaskRequest {
                    workspace_id: workspace_id.clone(),
                    worker_identity: caller_identity.clone(),
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    result: WorkerResult {
                        result: required_string(arguments, "result")?.to_string(),
                        changes: required_string(arguments, "changes")?.to_string(),
                        validation: required_string(arguments, "validation")?.to_string(),
                        blockers: blockers(arguments)?,
                    },
                })
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "finish",
                "workerId": worker_id,
                "taskId": task_id,
                "state": "completed",
                "result": result
            }))
        }
        _ => Err(format!("Unknown workers action: {action}")),
    }
}
