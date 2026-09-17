use super::broker::{
    FinishTaskRequest, MessageWorkerRequest, ReportWorkerRequest, SpawnWorkerRequest, WorkerBroker,
    WorkerBrokerError,
};
use super::types::{
    ChatIdentity, OperationId, ReasoningEffort, TaskId, WorkerExecutionProfile, WorkerId,
    WorkerMessageId, WorkerResult,
};
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

fn experimental_profile() -> WorkerExecutionProfile {
    WorkerExecutionProfile {
        model_key: "gpt-5.6-sol".into(),
        model_label: "GPT-5.6 Sol".into(),
        reasoning_effort: ReasoningEffort::High,
    }
}

fn broker_error(error: WorkerBrokerError) -> String {
    error.to_string()
}

pub async fn handle(
    arguments: &Value,
    workspace_id: &WorkspaceId,
    caller_identity: &ChatIdentity,
    broker: &WorkerBroker,
) -> Result<Value, String> {
    let action = required_string(arguments, "action")?;
    match action {
        "spawn" => {
            let receipt = broker
                .spawn_worker(SpawnWorkerRequest {
                    operation_id: parse_operation_id(arguments)?,
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    label: required_string(arguments, "label")?.to_string(),
                    assignment: required_string(arguments, "task")?.to_string(),
                    execution_profile: experimental_profile(),
                })
                .await
                .map_err(broker_error)?;
            Ok(json!({
                "action": "spawn",
                "familyId": receipt.family_id,
                "workerId": receipt.worker_id,
                "taskId": receipt.task_id,
                "displayId": receipt.display_id,
                "claimToken": receipt.claim_token,
                "executionProfile": experimental_profile(),
                "state": "provisioning"
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
                .collect_updates(workspace_id, caller_identity)
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
