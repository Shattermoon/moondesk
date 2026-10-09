use super::broker::{
    FinishTaskRequest, MessageWorkerRequest, ReportWorkerRequest, ReuseWorkerRequest,
    SpawnWorkerRequest, WorkerBroker, WorkerBrokerError,
};
use super::prompt;
use super::types::{
    ChatIdentity, OperationId, TaskId, WorkerExecutionProfile, WorkerId, WorkerMessageId,
    WorkerResult, WorkerState,
};
use crate::companion::CompanionAnchorRoute;
use crate::managed_chat::broker::{EnqueueManagedChatRequest, ManagedChatBroker};
use crate::managed_chat::types::{
    ManagedChatAnchorContext, ManagedChatCommandState, ManagedChatLaunch, ManagedChatOpenMode,
    ManagedChatPurpose,
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

pub struct WorkerLaunchContext<'a> {
    pub managed_chat_broker: &'a ManagedChatBroker,
    pub anchor_route: Option<&'a CompanionAnchorRoute>,
    pub worker_target_count: usize,
}

async fn recover_incomplete_launch_transactions(
    workspace_id: &WorkspaceId,
    worker_broker: &WorkerBroker,
    managed_chat_broker: &ManagedChatBroker,
) -> Result<(), String> {
    let worker_snapshot = worker_broker.snapshot().await;
    let managed_snapshot = managed_chat_broker.snapshot().await;
    let pending = worker_snapshot
        .families
        .values()
        .filter(|family| &family.workspace_id == workspace_id)
        .flat_map(|family| {
            family.workers.values().filter_map(move |worker| {
                if !matches!(
                    worker.state,
                    WorkerState::Provisioning | WorkerState::Waking
                ) {
                    return None;
                }
                let task_id = worker.current_task_id.clone()?;
                Some((
                    family.anchor_identity.clone(),
                    worker.id.clone(),
                    task_id,
                    worker.state,
                    worker.chat_identity.is_some(),
                    worker.launch_command_id.clone(),
                ))
            })
        })
        .collect::<Vec<_>>();

    for (anchor_identity, worker_id, task_id, worker_state, claimed, linked_command_id) in pending {
        let dedupe_key = format!("worker:{worker_id}:task:{task_id}");
        let command = managed_snapshot
            .dedupe
            .get(&dedupe_key)
            .and_then(|command_id| managed_snapshot.commands.get(command_id))
            .cloned();
        match command {
            Some(command) => {
                let command_id = command.id.to_string();
                if let Some(linked) = linked_command_id.as_deref() {
                    if linked != command_id {
                        return Err(format!(
                            "worker launch recovery found conflicting command links for {worker_id}"
                        ));
                    }
                } else {
                    worker_broker
                        .link_launch_command(
                            workspace_id,
                            &anchor_identity,
                            &worker_id,
                            &task_id,
                            &command_id,
                        )
                        .await
                        .map_err(|error| format!("worker launch recovery could not restore link: {error}"))?;
                }
                if !command.dispatch_ready {
                    if command.state != ManagedChatCommandState::Queued
                        || command.lease.is_some()
                        || command.reconcile_history
                        || command.terminal.is_some()
                    {
                        return Err(format!(
                            "worker launch recovery found a held command in an unsafe state for {worker_id}"
                        ));
                    }
                    managed_chat_broker
                        .activate_dispatch(&command.id)
                        .await
                        .map_err(|error| format!("worker launch recovery could not activate command: {error}"))?;
                }
            }
            None => {
                match (worker_state, claimed, linked_command_id.as_deref()) {
                    (WorkerState::Provisioning, false, None) => worker_broker
                        .rollback_unlinked_spawn(workspace_id, &anchor_identity, &worker_id, &task_id)
                        .await
                        .map_err(|error| format!("worker launch recovery could not roll back orphan spawn: {error}"))?,
                    (WorkerState::Waking, true, None) => worker_broker
                        .rollback_unlinked_reuse(workspace_id, &anchor_identity, &worker_id, &task_id)
                        .await
                        .map_err(|error| format!("worker launch recovery could not roll back orphan reuse: {error}"))?,
                    (WorkerState::Provisioning, false, Some(command_id)) => worker_broker
                        .rollback_linked_spawn(
                            workspace_id,
                            &anchor_identity,
                            &worker_id,
                            &task_id,
                            command_id,
                        )
                        .await
                        .map_err(|error| format!("worker launch recovery could not roll back missing linked spawn command: {error}"))?,
                    (WorkerState::Waking, true, Some(command_id)) => worker_broker
                        .rollback_linked_reuse(
                            workspace_id,
                            &anchor_identity,
                            &worker_id,
                            &task_id,
                            command_id,
                        )
                        .await
                        .map_err(|error| format!("worker launch recovery could not roll back missing linked reuse command: {error}"))?,
                    _ => {
                        return Err(format!(
                            "worker launch recovery found inconsistent pending state for {worker_id}"
                        ));
                    }
                }
            }
        }
    }

    let refreshed_workers = worker_broker.snapshot().await;
    let referenced = refreshed_workers
        .families
        .values()
        .filter(|family| &family.workspace_id == workspace_id)
        .flat_map(|family| family.workers.values())
        .filter_map(|worker| worker.launch_command_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let refreshed_commands = managed_chat_broker.snapshot().await;
    for command in refreshed_commands.commands.values().filter(|command| {
        &command.launch.workspace_id == workspace_id
            && command.launch.purpose == ManagedChatPurpose::Worker
            && !referenced.contains(&command.id.to_string())
    }) {
        if !command.dispatch_ready {
            managed_chat_broker
                .cancel_pre_send_by_dedupe(&command.dedupe_key)
                .await
                .map_err(|error| {
                    format!("worker launch recovery could not remove orphan held command: {error}")
                })?;
        }
    }
    Ok(())
}

pub async fn handle(
    arguments: &Value,
    workspace_id: &WorkspaceId,
    workspace_name: &str,
    caller_identity: &ChatIdentity,
    execution_profile: &WorkerExecutionProfile,
    broker: &WorkerBroker,
    launch_context: WorkerLaunchContext<'_>,
) -> Result<Value, String> {
    let worker_target_count = launch_context.worker_target_count;
    recover_incomplete_launch_transactions(
        workspace_id,
        broker,
        launch_context.managed_chat_broker,
    )
    .await?;
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

            let opening_message = prompt::bootstrap_message(workspace_name, &assignment, &receipt);
            let (target_client_id, anchor_context) = launch_context
                .anchor_route
                .map(|route| {
                    (
                        Some(route.client_id.clone()),
                        Some(ManagedChatAnchorContext {
                            conversation_id: route.tab.conversation_id.clone(),
                            conversation_url: route.tab.conversation_url.clone(),
                            project_id: route.tab.project_id.clone(),
                            project_url: route.tab.project_url.clone(),
                        }),
                    )
                })
                .unwrap_or((None, None));
            let dedupe_key = format!("worker:{}:task:{}", receipt.worker_id, receipt.task_id);
            let launch = match launch_context
                .managed_chat_broker
                .enqueue_held_with_route(
                    EnqueueManagedChatRequest {
                        dedupe_key: dedupe_key.clone(),
                        launch: ManagedChatLaunch {
                            workspace_id: workspace_id.clone(),
                            purpose: ManagedChatPurpose::Worker,
                            execution_profile: execution_profile.clone(),
                            opening_message,
                            task_marker: format!("moondesk-worker-task:{}", receipt.task_id),
                            thread_key: Some(format!("worker:{}", receipt.worker_id)),
                            open_mode: ManagedChatOpenMode::NewThread,
                            anchor_session_digest: Some(caller_identity.session_digest.clone()),
                        },
                    },
                    target_client_id,
                    anchor_context,
                )
                .await
            {
                Ok(launch) => launch,
                Err(error) => {
                    let rollback = broker
                        .rollback_unlinked_spawn(
                            workspace_id,
                            caller_identity,
                            &receipt.worker_id,
                            &receipt.task_id,
                        )
                        .await;
                    return Err(match rollback {
                        Ok(()) => format!(
                            "browser launch was not accepted; pending worker was rolled back: {error}"
                        ),
                        Err(rollback_error) => format!(
                            "browser launch was not accepted ({error}); pending worker rollback also failed: {rollback_error}"
                        ),
                    });
                }
            };
            if let Err(link_error) = broker
                .link_launch_command(
                    workspace_id,
                    caller_identity,
                    &receipt.worker_id,
                    &receipt.task_id,
                    &launch.id.to_string(),
                )
                .await
            {
                let cancel = launch_context
                    .managed_chat_broker
                    .cancel_pre_send_by_dedupe(&dedupe_key)
                    .await;
                let rollback = broker
                    .rollback_unlinked_spawn(
                        workspace_id,
                        caller_identity,
                        &receipt.worker_id,
                        &receipt.task_id,
                    )
                    .await;
                return Err(format!(
                    "worker launch command could not be linked ({link_error}); command cleanup: {}; worker rollback: {}",
                    cancel
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string()),
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string())
                ));
            }
            if let Err(activate_error) = launch_context
                .managed_chat_broker
                .activate_dispatch(&launch.id)
                .await
            {
                let cancel = launch_context
                    .managed_chat_broker
                    .cancel_pre_send_by_dedupe(&dedupe_key)
                    .await;
                let rollback = broker
                    .rollback_linked_spawn(
                        workspace_id,
                        caller_identity,
                        &receipt.worker_id,
                        &receipt.task_id,
                        &launch.id.to_string(),
                    )
                    .await;
                return Err(format!(
                    "worker launch command could not be activated ({activate_error}); command cleanup: {}; linked worker rollback: {}",
                    cancel
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string()),
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string())
                ));
            }

            Ok(json!({
                "action": "spawn",
                "familyId": receipt.family_id,
                "workerId": receipt.worker_id,
                "taskId": receipt.task_id,
                "displayId": receipt.display_id,
                "claimToken": receipt.claim_token,
                "executionProfile": execution_profile,
                "launchCommandId": launch.id,
                "state": "provisioning",
                "targetWorkerCount": worker_target_count,
                "recommendedWorkerCount": super::RECOMMENDED_WORKERS_PER_FAMILY,
                "maxWorkerCount": super::MAX_WORKERS_PER_FAMILY
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
            );
            let dedupe_key = format!("worker:{}:task:{}", receipt.worker_id, receipt.task_id);
            let launch = match launch_context
                .managed_chat_broker
                .enqueue_held_with_route(
                    EnqueueManagedChatRequest {
                        dedupe_key: dedupe_key.clone(),
                        launch: ManagedChatLaunch {
                            workspace_id: workspace_id.clone(),
                            purpose: ManagedChatPurpose::Worker,
                            execution_profile: receipt.execution_profile.clone(),
                            opening_message,
                            task_marker: format!("moondesk-worker-task:{}", receipt.task_id),
                            thread_key: Some(format!("worker:{}", receipt.worker_id)),
                            open_mode: ManagedChatOpenMode::ExistingThread,
                            anchor_session_digest: Some(caller_identity.session_digest.clone()),
                        },
                    },
                    None,
                    None,
                )
                .await
            {
                Ok(launch) => launch,
                Err(error) => {
                    let rollback = broker
                        .rollback_unlinked_reuse(
                            workspace_id,
                            caller_identity,
                            &receipt.worker_id,
                            &receipt.task_id,
                        )
                        .await;
                    return Err(match rollback {
                        Ok(()) => format!(
                            "browser wake was not accepted; pending reuse was rolled back: {error}"
                        ),
                        Err(rollback_error) => format!(
                            "browser wake was not accepted ({error}); pending reuse rollback also failed: {rollback_error}"
                        ),
                    });
                }
            };
            if let Err(link_error) = broker
                .link_launch_command(
                    workspace_id,
                    caller_identity,
                    &receipt.worker_id,
                    &receipt.task_id,
                    &launch.id.to_string(),
                )
                .await
            {
                let cancel = launch_context
                    .managed_chat_broker
                    .cancel_pre_send_by_dedupe(&dedupe_key)
                    .await;
                let rollback = broker
                    .rollback_unlinked_reuse(
                        workspace_id,
                        caller_identity,
                        &receipt.worker_id,
                        &receipt.task_id,
                    )
                    .await;
                return Err(format!(
                    "worker wake command could not be linked ({link_error}); command cleanup: {}; reuse rollback: {}",
                    cancel
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string()),
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string())
                ));
            }
            if let Err(activate_error) = launch_context
                .managed_chat_broker
                .activate_dispatch(&launch.id)
                .await
            {
                let cancel = launch_context
                    .managed_chat_broker
                    .cancel_pre_send_by_dedupe(&dedupe_key)
                    .await;
                let rollback = broker
                    .rollback_linked_reuse(
                        workspace_id,
                        caller_identity,
                        &receipt.worker_id,
                        &receipt.task_id,
                        &launch.id.to_string(),
                    )
                    .await;
                return Err(format!(
                    "worker wake command could not be activated ({activate_error}); command cleanup: {}; linked reuse rollback: {}",
                    cancel
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string()),
                    rollback
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|error| error.to_string())
                ));
            }
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
                return Ok(json!({
                    "action": "status",
                    "family": null,
                    "workers": [],
                    "targetWorkerCount": worker_target_count,
                    "recommendedWorkerCount": super::RECOMMENDED_WORKERS_PER_FAMILY,
                    "maxWorkerCount": super::MAX_WORKERS_PER_FAMILY
                }));
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
                        "launchState": worker.launch_state,
                        "launchCommandId": worker.launch_command_id,
                        "launchError": worker.launch_error,
                        "conversationUrl": worker.conversation_url,
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
                "workers": workers,
                "targetWorkerCount": worker_target_count,
                "recommendedWorkerCount": super::RECOMMENDED_WORKERS_PER_FAMILY,
                "maxWorkerCount": super::MAX_WORKERS_PER_FAMILY
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
                    launch_context
                        .managed_chat_broker
                        .settle_for_worker_retire_by_dedupe(&dedupe_key)
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
            let operation_id = parse_operation_id(arguments)?;
            let updates = broker
                .collect_updates_wait(
                    workspace_id,
                    caller_identity,
                    &operation_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_chat::types::ChatExecutionProfile;
    use uuid::Uuid;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create protocol test root");
        root
    }

    fn identity(name: &str) -> ChatIdentity {
        ChatIdentity::from_openai_meta(Some("protocol-test-subject"), name)
    }

    fn spawn_request(
        operation_id: OperationId,
        workspace_id: WorkspaceId,
        anchor_identity: ChatIdentity,
    ) -> SpawnWorkerRequest {
        SpawnWorkerRequest {
            operation_id,
            workspace_id,
            anchor_identity,
            label: "recovery".into(),
            assignment: "recover the same durable launch".into(),
            execution_profile: ChatExecutionProfile::default(),
        }
    }

    fn launch_for(
        workspace_id: WorkspaceId,
        worker_id: &WorkerId,
        task_id: &TaskId,
    ) -> EnqueueManagedChatRequest {
        EnqueueManagedChatRequest {
            dedupe_key: format!("worker:{worker_id}:task:{task_id}"),
            launch: ManagedChatLaunch {
                workspace_id,
                purpose: ManagedChatPurpose::Worker,
                execution_profile: ChatExecutionProfile::default(),
                opening_message: "recover this worker".into(),
                task_marker: task_id.to_string(),
                thread_key: None,
                open_mode: ManagedChatOpenMode::NewThread,
                anchor_session_digest: None,
            },
        }
    }

    #[tokio::test]
    async fn recovery_rolls_back_worker_created_before_command_persisted() {
        let root = temp_root("moondesk-worker-recovery-worker-only");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-worker-only");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("persist worker before simulated crash");
        drop(worker);

        let worker = WorkerBroker::open(&worker_path).expect("reopen worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        recover_incomplete_launch_transactions(&workspace, &worker, &managed)
            .await
            .expect("recover worker-only crash boundary");
        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read worker family")
            .expect("family remains");
        assert!(!family.workers.contains_key(&receipt.worker_id));
        assert!(managed.snapshot().await.commands.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_links_and_activates_the_same_held_command_after_restart() {
        let root = temp_root("moondesk-worker-recovery-held-command");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-held-command");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("persist worker");
        let command = managed
            .enqueue_held_with_route(
                launch_for(workspace.clone(), &receipt.worker_id, &receipt.task_id),
                None,
                None,
            )
            .await
            .expect("persist held command before link");
        assert!(!command.dispatch_ready);
        drop(worker);
        drop(managed);

        let worker = WorkerBroker::open(&worker_path).expect("reopen worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("reopen managed broker");
        recover_incomplete_launch_transactions(&workspace, &worker, &managed)
            .await
            .expect("recover missing link and activation");
        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read family")
            .expect("family exists");
        let recovered_worker = family
            .workers
            .get(&receipt.worker_id)
            .expect("worker survives recovery");
        let command_id = command.id.to_string();
        assert_eq!(
            recovered_worker.launch_command_id.as_deref(),
            Some(command_id.as_str())
        );
        let snapshot = managed.snapshot().await;
        assert_eq!(
            snapshot.commands.len(),
            1,
            "recovery must not enqueue a duplicate command"
        );
        let recovered_command = snapshot
            .commands
            .get(&command.id)
            .expect("same command remains");
        assert!(recovered_command.dispatch_ready);
        assert_eq!(recovered_command.state, ManagedChatCommandState::Queued);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_activates_linked_held_command_without_creating_a_second_command() {
        let root = temp_root("moondesk-worker-recovery-linked-held");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-linked-held");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("persist worker");
        let command = managed
            .enqueue_held_with_route(
                launch_for(workspace.clone(), &receipt.worker_id, &receipt.task_id),
                None,
                None,
            )
            .await
            .expect("persist held command");
        worker
            .link_launch_command(
                &workspace,
                &anchor,
                &receipt.worker_id,
                &receipt.task_id,
                &command.id.to_string(),
            )
            .await
            .expect("persist worker-command link");
        drop(worker);
        drop(managed);

        let worker = WorkerBroker::open(&worker_path).expect("reopen worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("reopen managed broker");
        recover_incomplete_launch_transactions(&workspace, &worker, &managed)
            .await
            .expect("recover linked held command");
        let snapshot = managed.snapshot().await;
        assert_eq!(snapshot.commands.len(), 1);
        assert!(
            snapshot
                .commands
                .get(&command.id)
                .expect("same command")
                .dispatch_ready
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_removes_orphan_held_command_with_no_worker_record() {
        let root = temp_root("moondesk-worker-recovery-command-only");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-command-only");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("create ids");
        worker
            .rollback_unlinked_spawn(&workspace, &anchor, &receipt.worker_id, &receipt.task_id)
            .await
            .expect("remove worker before simulated command-only crash");
        let command = managed
            .enqueue_held_with_route(
                launch_for(workspace.clone(), &receipt.worker_id, &receipt.task_id),
                None,
                None,
            )
            .await
            .expect("persist orphan held command");
        drop(worker);
        drop(managed);

        let worker = WorkerBroker::open(&worker_path).expect("reopen worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("reopen managed broker");
        recover_incomplete_launch_transactions(&workspace, &worker, &managed)
            .await
            .expect("remove command-only orphan");
        assert!(!managed.snapshot().await.commands.contains_key(&command.id));
        let _ = std::fs::remove_dir_all(root);
    }
}
