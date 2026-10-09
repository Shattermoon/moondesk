use super::broker::{
    FinishTaskRequest, InactiveWorkerCleanupSummary, MessageWorkerRequest, ReportWorkerRequest,
    ReuseWorkerRequest, SpawnWorkerRequest, WorkerBroker, WorkerBrokerError,
};
use super::prompt;
use super::types::{
    ChatIdentity, OperationId, TaskId, WorkerExecutionProfile, WorkerId, WorkerLaunchState,
    WorkerMessageId, WorkerResult, WorkerState,
};
use crate::companion::CompanionAnchorRoute;
#[cfg(test)]
use crate::managed_chat::broker::ManagedChatAckOutcome;
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

pub(crate) async fn cleanup_inactive_capacity_for_workspace(
    workspace_id: &WorkspaceId,
    worker_broker: &WorkerBroker,
    managed_chat_broker: &ManagedChatBroker,
) -> Result<InactiveWorkerCleanupSummary, String> {
    let _lifecycle_guard = super::WORKER_LIFECYCLE_LOCK.lock().await;
    let session_digests = worker_broker
        .inactive_cleanup_session_digests_for_workspace(workspace_id)
        .await
        .map_err(broker_error)?;
    if session_digests.is_empty() {
        return Ok(InactiveWorkerCleanupSummary::default());
    }

    for session_digest in &session_digests {
        managed_chat_broker
            .ensure_clearable_anchor_session(session_digest)
            .await
            .map_err(|error| {
                format!(
                    "inactive Worker cleanup is blocked by active or ambiguous browser launch state: {error}"
                )
            })?;
    }

    // Purge only terminal managed-chat history first. If Worker persistence then fails, the idle
    // family remains durable and the operation can be retried safely; reuse can create a fresh
    // command from the worker's exact durable conversation binding.
    for session_digest in &session_digests {
        managed_chat_broker
            .purge_terminal_for_anchor_session(session_digest)
            .await
            .map_err(|error| {
                format!(
                    "terminal browser launch cleanup did not complete; Worker families were left intact and retry is safe: {error}"
                )
            })?;
    }

    worker_broker
        .cleanup_inactive_families_for_workspace(workspace_id)
        .await
        .map_err(|error| {
            format!(
                "terminal browser launch history was cleared but inactive Worker families were not removed; retry is safe: {error}"
            )
        })
}

pub struct WorkerLaunchContext<'a> {
    pub managed_chat_broker: &'a ManagedChatBroker,
    pub anchor_route: Option<&'a CompanionAnchorRoute>,
    pub worker_target_count: usize,
}

#[cfg(test)]
struct TransactionPause {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
fn transaction_pause_registry()
-> &'static std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<TransactionPause>>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<TransactionPause>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

#[cfg(test)]
fn register_transaction_pause(operation_id: &OperationId) -> std::sync::Arc<TransactionPause> {
    let pause = std::sync::Arc::new(TransactionPause {
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    transaction_pause_registry()
        .lock()
        .expect("transaction pause registry")
        .insert(operation_id.to_string(), pause.clone());
    pause
}

#[cfg(test)]
async fn maybe_pause_after_worker_persist(operation_id: &OperationId) {
    let pause = transaction_pause_registry()
        .lock()
        .expect("transaction pause registry")
        .get(&operation_id.to_string())
        .cloned();
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.resume.notified().await;
        transaction_pause_registry()
            .lock()
            .expect("transaction pause registry")
            .remove(&operation_id.to_string());
    }
}

#[cfg(not(test))]
async fn maybe_pause_after_worker_persist(_operation_id: &OperationId) {}

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
                    worker.launch_state,
                    worker.conversation_url.clone(),
                    worker.launch_command_id.clone(),
                ))
            })
        })
        .collect::<Vec<_>>();

    for (
        anchor_identity,
        worker_id,
        task_id,
        worker_state,
        claimed,
        launch_state,
        conversation_url,
        linked_command_id,
    ) in pending
    {
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
                        .map_err(|error| {
                            format!("worker launch recovery could not restore link: {error}")
                        })?;
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
                        .map_err(|error| {
                            format!("worker launch recovery could not activate command: {error}")
                        })?;
                }
            }
            None => match (worker_state, claimed, linked_command_id.as_deref()) {
                (WorkerState::Provisioning, false, None) => worker_broker
                    .rollback_unlinked_spawn(workspace_id, &anchor_identity, &worker_id, &task_id)
                    .await
                    .map_err(|error| {
                        format!("worker launch recovery could not roll back orphan spawn: {error}")
                    })?,
                (WorkerState::Waking, true, None) => worker_broker
                    .rollback_unlinked_reuse(workspace_id, &anchor_identity, &worker_id, &task_id)
                    .await
                    .map_err(|error| {
                        format!("worker launch recovery could not roll back orphan reuse: {error}")
                    })?,
                (WorkerState::Provisioning | WorkerState::Waking, _, Some(command_id)) => {
                    // A missing linked command is not proof that Send never happened: terminal
                    // managed-chat history may have been compacted after a successful launch, or
                    // persistence may have advanced farther than the worker mirror before a crash.
                    // Preserve canonical/WaitingClaim workers exactly so their claim capability
                    // remains valid. Otherwise fail closed by pausing the launch instead of deleting
                    // a potentially-real ChatGPT conversation.
                    if launch_state != WorkerLaunchState::WaitingClaim && conversation_url.is_none()
                    {
                        worker_broker
                            .update_launch_by_command(
                                command_id,
                                WorkerLaunchState::Paused,
                                Some("linked launch command is missing; automatic rollback was refused because the Send boundary cannot be proven safe".into()),
                                None,
                            )
                            .await
                            .map_err(|error| format!("worker launch recovery could not pause missing linked command: {error}"))?;
                    }
                }
                _ => {
                    return Err(format!(
                        "worker launch recovery found inconsistent pending state for {worker_id}"
                    ));
                }
            },
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

pub(crate) async fn recover_registered_workspaces(
    workspace_ids: &[WorkspaceId],
    worker_broker: &WorkerBroker,
    managed_chat_broker: &ManagedChatBroker,
) -> Result<(), String> {
    let _lifecycle_guard = super::WORKER_LIFECYCLE_LOCK.lock().await;
    let mut errors = Vec::new();
    for workspace_id in workspace_ids {
        if let Err(error) =
            recover_incomplete_launch_transactions(workspace_id, worker_broker, managed_chat_broker)
                .await
        {
            errors.push(format!("{}: {error}", workspace_id.as_str()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
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
    let action = required_string(arguments, "action")?;
    match action {
        "spawn" => {
            let _lifecycle_guard = super::WORKER_LIFECYCLE_LOCK.lock().await;
            recover_incomplete_launch_transactions(
                workspace_id,
                broker,
                launch_context.managed_chat_broker,
            )
            .await?;
            let assignment = required_string(arguments, "task")?.to_string();
            let execution_profile = execution_profile.clone();
            let operation_id = parse_operation_id(arguments)?;
            let receipt = broker
                .spawn_worker(SpawnWorkerRequest {
                    operation_id: operation_id.clone(),
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    label: required_string(arguments, "label")?.to_string(),
                    assignment: assignment.clone(),
                    execution_profile: execution_profile.clone(),
                })
                .await
                .map_err(broker_error)?;
            maybe_pause_after_worker_persist(&operation_id).await;

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
            let _lifecycle_guard = super::WORKER_LIFECYCLE_LOCK.lock().await;
            recover_incomplete_launch_transactions(
                workspace_id,
                broker,
                launch_context.managed_chat_broker,
            )
            .await?;
            let route = launch_context.anchor_route.ok_or_else(|| {
                "worker reuse requires a confirmed companion Core route".to_string()
            })?;
            let assignment = required_string(arguments, "task")?.to_string();
            let operation_id = parse_operation_id(arguments)?;
            let receipt = broker
                .reuse_worker(ReuseWorkerRequest {
                    operation_id: operation_id.clone(),
                    workspace_id: workspace_id.clone(),
                    anchor_identity: caller_identity.clone(),
                    worker_id: parse_worker_id(arguments)?,
                    assignment: assignment.clone(),
                })
                .await
                .map_err(broker_error)?;
            maybe_pause_after_worker_persist(&operation_id).await;
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
                    Some(route.client_id.clone()),
                    Some(ManagedChatAnchorContext {
                        conversation_id: route.tab.conversation_id.clone(),
                        conversation_url: route.tab.conversation_url.clone(),
                        project_id: route.tab.project_id.clone(),
                        project_url: route.tab.project_url.clone(),
                    }),
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
            let _lifecycle_guard = super::WORKER_LIFECYCLE_LOCK.lock().await;
            recover_incomplete_launch_transactions(
                workspace_id,
                broker,
                launch_context.managed_chat_broker,
            )
            .await?;
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
    use crate::companion::CompanionTabPresence;
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

    fn core_route() -> CompanionAnchorRoute {
        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        CompanionAnchorRoute {
            client_id: "extension-a".into(),
            tab: CompanionTabPresence {
                conversation_id: conversation_id.into(),
                conversation_url: format!("https://chatgpt.com/c/{conversation_id}"),
                project_id: None,
                project_url: None,
                active: true,
                window_focused: true,
                generating: false,
            },
        }
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

    async fn make_idle_collected_worker(
        worker: &WorkerBroker,
        workspace: &WorkspaceId,
        anchor: &ChatIdentity,
    ) -> (WorkerId, TaskId) {
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("spawn cleanup worker");
        let command_id = Uuid::new_v4().to_string();
        worker
            .link_launch_command(
                workspace,
                anchor,
                &receipt.worker_id,
                &receipt.task_id,
                &command_id,
            )
            .await
            .expect("link cleanup worker launch");
        let conversation_id = Uuid::new_v4().to_string();
        worker
            .update_launch_by_command(
                &command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(format!("https://chatgpt.com/c/{conversation_id}")),
            )
            .await
            .expect("make cleanup worker claimable");
        let worker_identity = identity(&conversation_id);
        worker
            .claim_worker(
                workspace,
                &receipt.worker_id,
                &receipt.task_id,
                &receipt.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim cleanup worker");
        worker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: receipt.worker_id.clone(),
                task_id: receipt.task_id.clone(),
                result: WorkerResult {
                    result: "done".into(),
                    changes: "cleanup fixture".into(),
                    validation: "cleanup fixture".into(),
                    blockers: Vec::new(),
                },
            })
            .await
            .expect("finish cleanup worker");
        let updates = worker
            .collect_updates_wait(workspace, anchor, &OperationId::new(), 0)
            .await
            .expect("collect cleanup worker result");
        assert_eq!(updates.completed.len(), 1);
        (receipt.worker_id, receipt.task_id)
    }

    #[tokio::test]
    async fn host_capacity_cleanup_fails_closed_on_active_managed_launch_state() {
        let root = temp_root("moondesk-worker-host-cleanup-managed-active");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("cleanup-core");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let (worker_id, task_id) = make_idle_collected_worker(&worker, &workspace, &anchor).await;

        let mut request = launch_for(workspace.clone(), &worker_id, &task_id);
        request.dedupe_key = "cleanup-active-managed-command".into();
        request.launch.anchor_session_digest = Some(anchor.session_digest.clone());
        let command = managed
            .enqueue_held_with_route(request, None, None)
            .await
            .expect("persist active held managed launch");

        let error = cleanup_inactive_capacity_for_workspace(&workspace, &worker, &managed)
            .await
            .expect_err("active managed launch must block host cleanup");
        assert!(error.contains("active or ambiguous browser launch state"));
        assert!(
            worker
                .family_for_anchor(&workspace, &anchor)
                .await
                .expect("read cleanup family")
                .is_some(),
            "worker history must remain when managed state is not clearable"
        );
        assert!(managed.snapshot().await.commands.contains_key(&command.id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn host_capacity_cleanup_purges_terminal_managed_history_with_worker_family() {
        let root = temp_root("moondesk-worker-host-cleanup-managed-terminal");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("cleanup-terminal-core");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let (worker_id, task_id) = make_idle_collected_worker(&worker, &workspace, &anchor).await;

        let mut request = launch_for(workspace.clone(), &worker_id, &task_id);
        request.dedupe_key = "cleanup-terminal-managed-command".into();
        request.launch.anchor_session_digest = Some(anchor.session_digest.clone());
        let queued = managed
            .enqueue(request)
            .await
            .expect("enqueue managed launch");
        let offer = managed
            .redeem("cleanup-browser", 1)
            .await
            .expect("redeem managed launch")
            .expect("managed launch offer");
        assert_eq!(offer.command.id, queued.id);
        let lease_id = offer
            .command
            .lease
            .as_ref()
            .expect("managed launch lease")
            .lease_id
            .clone();
        managed
            .acknowledge(
                &queued.id,
                &lease_id,
                "cleanup-browser",
                ManagedChatAckOutcome::Failed {
                    details: Some("proven pre-Send failure".into()),
                },
            )
            .await
            .expect("make managed launch safely terminal");

        let removed = cleanup_inactive_capacity_for_workspace(&workspace, &worker, &managed)
            .await
            .expect("cleanup terminal managed history and worker family");
        assert_eq!(removed.family_count, 1);
        assert!(
            worker
                .family_for_anchor(&workspace, &anchor)
                .await
                .expect("read cleanup family")
                .is_none()
        );
        assert!(managed.snapshot().await.commands.is_empty());
        let _ = std::fs::remove_dir_all(root);
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
    async fn startup_recovery_activates_linked_held_command_without_new_workers_call() {
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
        recover_registered_workspaces(std::slice::from_ref(&workspace), &worker, &managed)
            .await
            .expect("startup recovery restores linked held command");
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
    async fn startup_recovery_continues_after_one_workspace_fails() {
        let root = temp_root("moondesk-worker-recovery-multi-workspace");
        let worker =
            WorkerBroker::open(root.join("worker-state-v1.json")).expect("open worker broker");
        let managed = ManagedChatBroker::open(root.join("managed-chat-state-v1.json"))
            .expect("open managed broker");
        let broken_workspace = WorkspaceId::new();
        let healthy_workspace = WorkspaceId::new();
        let broken_anchor = identity("anchor-recovery-broken-workspace");
        let healthy_anchor = identity("anchor-recovery-healthy-workspace");

        let broken = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                broken_workspace.clone(),
                broken_anchor.clone(),
            ))
            .await
            .expect("persist broken workspace worker");
        let broken_command = managed
            .enqueue_held_with_route(
                launch_for(broken_workspace.clone(), &broken.worker_id, &broken.task_id),
                None,
                None,
            )
            .await
            .expect("persist broken workspace command");
        worker
            .link_launch_command(
                &broken_workspace,
                &broken_anchor,
                &broken.worker_id,
                &broken.task_id,
                &Uuid::new_v4().to_string(),
            )
            .await
            .expect("seed conflicting broken workspace link");

        let healthy = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                healthy_workspace.clone(),
                healthy_anchor.clone(),
            ))
            .await
            .expect("persist healthy workspace worker");
        let healthy_command = managed
            .enqueue_held_with_route(
                launch_for(
                    healthy_workspace.clone(),
                    &healthy.worker_id,
                    &healthy.task_id,
                ),
                None,
                None,
            )
            .await
            .expect("persist healthy workspace command");
        worker
            .link_launch_command(
                &healthy_workspace,
                &healthy_anchor,
                &healthy.worker_id,
                &healthy.task_id,
                &healthy_command.id.to_string(),
            )
            .await
            .expect("persist healthy workspace link");

        let error = recover_registered_workspaces(
            &[broken_workspace.clone(), healthy_workspace.clone()],
            &worker,
            &managed,
        )
        .await
        .expect_err("broken workspace should still be reported");
        assert!(error.contains(broken_workspace.as_str()));
        let snapshot = managed.snapshot().await;
        assert!(
            !snapshot
                .commands
                .get(&broken_command.id)
                .expect("broken command remains")
                .dispatch_ready
        );
        assert!(
            snapshot
                .commands
                .get(&healthy_command.id)
                .expect("healthy command remains")
                .dispatch_ready,
            "later workspace recovery must proceed even after an earlier workspace fails"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_spawn_status_and_spawn_cannot_rollback_live_transaction() {
        let root = temp_root("moondesk-worker-spawn-concurrency");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-spawn-concurrency");
        let worker = std::sync::Arc::new(
            WorkerBroker::open(root.join("worker-state-v1.json")).expect("open worker broker"),
        );
        let managed = std::sync::Arc::new(
            ManagedChatBroker::open(root.join("managed-chat-state-v1.json"))
                .expect("open managed broker"),
        );
        let first_operation = OperationId::new();
        let pause = register_transaction_pause(&first_operation);
        let first = {
            let worker = worker.clone();
            let managed = managed.clone();
            let workspace = workspace.clone();
            let anchor = anchor.clone();
            let operation = first_operation.to_string();
            tokio::spawn(async move {
                handle(
                    &json!({
                        "action": "spawn",
                        "operation_id": operation,
                        "label": "first",
                        "task": "first concurrent task"
                    }),
                    &workspace,
                    "spawn-concurrency",
                    &anchor,
                    &ChatExecutionProfile::default(),
                    worker.as_ref(),
                    WorkerLaunchContext {
                        managed_chat_broker: managed.as_ref(),
                        anchor_route: None,
                        worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
                    },
                )
                .await
            })
        };
        pause.reached.notified().await;

        let status = handle(
            &json!({ "action": "status" }),
            &workspace,
            "spawn-concurrency",
            &anchor,
            &ChatExecutionProfile::default(),
            worker.as_ref(),
            WorkerLaunchContext {
                managed_chat_broker: managed.as_ref(),
                anchor_route: None,
                worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
            },
        )
        .await
        .expect("status remains read-only during paused transaction");
        assert_eq!(status["workers"].as_array().map(Vec::len), Some(1));

        let second = {
            let worker = worker.clone();
            let managed = managed.clone();
            let workspace = workspace.clone();
            let anchor = anchor.clone();
            tokio::spawn(async move {
                handle(
                    &json!({
                        "action": "spawn",
                        "operation_id": OperationId::new().to_string(),
                        "label": "second",
                        "task": "second concurrent task"
                    }),
                    &workspace,
                    "spawn-concurrency",
                    &anchor,
                    &ChatExecutionProfile::default(),
                    worker.as_ref(),
                    WorkerLaunchContext {
                        managed_chat_broker: managed.as_ref(),
                        anchor_route: None,
                        worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
                    },
                )
                .await
            })
        };
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "another spawn must wait for the full cross-store lifecycle boundary"
        );
        pause.resume.notify_one();
        first.await.expect("first task join").expect("first spawn");
        second
            .await
            .expect("second task join")
            .expect("second spawn after serialization");

        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read family")
            .expect("family remains");
        assert_eq!(family.workers.len(), 2);
        let managed_snapshot = managed.snapshot().await;
        assert_eq!(managed_snapshot.commands.len(), 2);
        assert!(
            managed_snapshot
                .commands
                .values()
                .all(|command| command.dispatch_ready)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_reuse_report_and_status_do_not_trigger_crash_recovery() {
        let root = temp_root("moondesk-worker-reuse-concurrency");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-reuse-concurrency");
        let worker_identity = identity("worker-reuse-concurrency");
        let worker = std::sync::Arc::new(
            WorkerBroker::open(root.join("worker-state-v1.json")).expect("open worker broker"),
        );
        let managed = std::sync::Arc::new(
            ManagedChatBroker::open(root.join("managed-chat-state-v1.json"))
                .expect("open managed broker"),
        );
        let spawned = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("seed reusable worker");
        let seed_command = Uuid::new_v4().to_string();
        worker
            .link_launch_command(
                &workspace,
                &anchor,
                &spawned.worker_id,
                &spawned.task_id,
                &seed_command,
            )
            .await
            .expect("seed launch link");
        worker
            .update_launch_by_command(
                &seed_command,
                WorkerLaunchState::WaitingClaim,
                None,
                Some("https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
            )
            .await
            .expect("seed canonical conversation");
        worker
            .claim_worker(
                &workspace,
                &spawned.worker_id,
                &spawned.task_id,
                &spawned.claim_token,
                worker_identity.clone(),
            )
            .await
            .expect("claim reusable worker");
        worker
            .finish_task(FinishTaskRequest {
                workspace_id: workspace.clone(),
                worker_identity: worker_identity.clone(),
                worker_id: spawned.worker_id.clone(),
                task_id: spawned.task_id.clone(),
                result: WorkerResult {
                    result: "seed complete".into(),
                    changes: "none".into(),
                    validation: "seed reusable worker".into(),
                    blockers: Vec::new(),
                },
            })
            .await
            .expect("make worker idle before reuse");

        let reuse_operation = OperationId::new();
        let pause = register_transaction_pause(&reuse_operation);
        let route = core_route();
        let reuse = {
            let worker = worker.clone();
            let managed = managed.clone();
            let workspace = workspace.clone();
            let anchor = anchor.clone();
            let route = route.clone();
            let worker_id = spawned.worker_id.to_string();
            let operation = reuse_operation.to_string();
            tokio::spawn(async move {
                handle(
                    &json!({
                        "action": "reuse",
                        "operation_id": operation,
                        "worker_id": worker_id,
                        "task": "paused reuse assignment"
                    }),
                    &workspace,
                    "reuse-concurrency",
                    &anchor,
                    &ChatExecutionProfile::default(),
                    worker.as_ref(),
                    WorkerLaunchContext {
                        managed_chat_broker: managed.as_ref(),
                        anchor_route: Some(&route),
                        worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
                    },
                )
                .await
            })
        };
        pause.reached.notified().await;
        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read paused reuse family")
            .expect("family remains");
        let waking = family
            .workers
            .get(&spawned.worker_id)
            .expect("worker remains");
        assert_eq!(waking.state, WorkerState::Waking);
        let reuse_task_id = waking.current_task_id.clone().expect("reuse task id");
        worker
            .report_worker(ReportWorkerRequest {
                operation_id: OperationId::new(),
                workspace_id: workspace.clone(),
                worker_identity,
                worker_id: spawned.worker_id.clone(),
                task_id: reuse_task_id,
                body: "concurrent worker report".into(),
            })
            .await
            .expect("report cannot invoke crash recovery");
        let status = handle(
            &json!({ "action": "status" }),
            &workspace,
            "reuse-concurrency",
            &anchor,
            &ChatExecutionProfile::default(),
            worker.as_ref(),
            WorkerLaunchContext {
                managed_chat_broker: managed.as_ref(),
                anchor_route: None,
                worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
            },
        )
        .await
        .expect("status cannot invoke crash recovery");
        assert_eq!(status["workers"].as_array().map(Vec::len), Some(1));
        pause.resume.notify_one();
        reuse.await.expect("reuse join").expect("reuse completes");
        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read completed reuse family")
            .expect("family remains");
        assert_eq!(
            family
                .workers
                .get(&spawned.worker_id)
                .expect("worker remains")
                .state,
            WorkerState::Waking
        );
        let managed_snapshot = managed.snapshot().await;
        assert_eq!(managed_snapshot.commands.len(), 1);
        let command = managed_snapshot
            .commands
            .values()
            .next()
            .expect("reuse command remains");
        assert_eq!(command.target_client_id.as_deref(), Some("extension-a"));
        assert_eq!(
            command
                .anchor_context
                .as_ref()
                .map(|context| context.conversation_id.as_str()),
            Some(route.tab.conversation_id.as_str())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn status_never_reconciles_an_inflight_cross_store_launch() {
        let root = temp_root("moondesk-worker-status-no-recovery");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-status-no-recovery");
        let worker = WorkerBroker::open(&worker_path).expect("open worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("open managed broker");
        let receipt = worker
            .spawn_worker(spawn_request(
                OperationId::new(),
                workspace.clone(),
                anchor.clone(),
            ))
            .await
            .expect("persist worker before command commit");

        let status = handle(
            &json!({ "action": "status" }),
            &workspace,
            "status-no-recovery",
            &anchor,
            &ChatExecutionProfile::default(),
            &worker,
            WorkerLaunchContext {
                managed_chat_broker: &managed,
                anchor_route: None,
                worker_target_count: super::super::RECOMMENDED_WORKERS_PER_FAMILY,
            },
        )
        .await
        .expect("status while launch transaction is in flight");
        assert_eq!(status["workers"].as_array().map(Vec::len), Some(1));
        assert!(
            worker
                .family_for_anchor(&workspace, &anchor)
                .await
                .expect("read worker family")
                .expect("family remains")
                .workers
                .contains_key(&receipt.worker_id),
            "read-only worker actions must not run crash recovery against a live transaction"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_preserves_waiting_claim_when_linked_command_history_is_gone() {
        let root = temp_root("moondesk-worker-recovery-post-send-barrier");
        let worker_path = root.join("worker-state-v1.json");
        let managed_path = root.join("managed-chat-state-v1.json");
        let workspace = WorkspaceId::new();
        let anchor = identity("anchor-post-send-barrier");
        let worker_identity = identity("worker-post-send-barrier");
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
        let mut initial_launch =
            launch_for(workspace.clone(), &receipt.worker_id, &receipt.task_id);
        initial_launch.launch.thread_key = Some(format!("worker:{}", receipt.worker_id));
        let initial_command = managed
            .enqueue(initial_launch)
            .await
            .expect("persist the real worker launch command");
        let compacted_command_id = initial_command.id.to_string();
        worker
            .link_launch_command(
                &workspace,
                &anchor,
                &receipt.worker_id,
                &receipt.task_id,
                &compacted_command_id,
            )
            .await
            .expect("link command before terminal history is compacted");
        let initial_offer = managed
            .redeem("extension-a", 1_000)
            .await
            .expect("redeem original worker launch")
            .expect("original worker launch lease");
        let initial_lease = initial_offer
            .command
            .lease
            .as_ref()
            .expect("original worker launch lease id")
            .lease_id
            .clone();
        managed
            .mark_send_started(&initial_command.id, &initial_lease, "extension-a")
            .await
            .expect("cross original worker Send boundary");
        let conversation_url = "https://chatgpt.com/c/6aaef8db-ecf0-83ee-bfcc-d6f94853c540";
        managed
            .acknowledge(
                &initial_command.id,
                &initial_lease,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("original Send accepted".into()),
                    conversation_url: Some(conversation_url.into()),
                },
            )
            .await
            .expect("persist original successful worker launch");
        worker
            .update_launch_by_command(
                &compacted_command_id,
                WorkerLaunchState::WaitingClaim,
                None,
                Some(conversation_url.into()),
            )
            .await
            .expect("record durable post-Send conversation");

        let filler_count = crate::managed_chat::MAX_MANAGED_CHAT_THREAD_AFFINITIES
            + crate::managed_chat::MAX_MANAGED_CHAT_TERMINAL_HISTORY
            + 1;
        for index in 0..filler_count {
            let filler_worker = WorkerId::new();
            let filler_task = TaskId::new();
            let mut filler = launch_for(workspace.clone(), &filler_worker, &filler_task);
            filler.dedupe_key = format!("worker:compaction:{index}");
            filler.launch.thread_key = Some(format!("worker:compaction:{index}"));
            filler.launch.task_marker = format!("compaction-{index}");
            let command = managed
                .enqueue(filler)
                .await
                .expect("enqueue compaction filler");
            let offer = managed
                .redeem("extension-a", 10_000 + index as u64)
                .await
                .expect("redeem compaction filler")
                .expect("compaction filler lease");
            let lease_id = offer
                .command
                .lease
                .as_ref()
                .expect("compaction filler lease id")
                .lease_id
                .clone();
            managed
                .mark_send_started(&command.id, &lease_id, "extension-a")
                .await
                .expect("cross filler Send boundary");
            managed
                .acknowledge(
                    &command.id,
                    &lease_id,
                    "extension-a",
                    ManagedChatAckOutcome::Succeeded {
                        details: Some("filler accepted".into()),
                        conversation_url: Some(format!("https://chatgpt.com/c/{:032x}", index + 1)),
                    },
                )
                .await
                .expect("persist filler success");
        }
        assert!(
            !managed
                .snapshot()
                .await
                .commands
                .contains_key(&initial_command.id),
            "terminal compaction must actually remove the old successful command in this regression"
        );
        drop(worker);
        drop(managed);

        let worker = WorkerBroker::open(&worker_path).expect("reopen worker broker");
        let managed = ManagedChatBroker::open(&managed_path).expect("reopen empty managed broker");
        recover_registered_workspaces(std::slice::from_ref(&workspace), &worker, &managed)
            .await
            .expect("missing terminal history must not roll back accepted Send");
        let family = worker
            .family_for_anchor(&workspace, &anchor)
            .await
            .expect("read family")
            .expect("family remains");
        let recovered = family
            .workers
            .get(&receipt.worker_id)
            .expect("WaitingClaim worker survives recovery");
        assert_eq!(recovered.launch_state, WorkerLaunchState::WaitingClaim);
        assert_eq!(
            recovered.conversation_url.as_deref(),
            Some(conversation_url)
        );
        worker
            .claim_worker(
                &workspace,
                &receipt.worker_id,
                &receipt.task_id,
                &receipt.claim_token,
                worker_identity,
            )
            .await
            .expect("preserved claim capability remains usable");
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
