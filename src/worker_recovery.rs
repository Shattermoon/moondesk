use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

pub const WORKER_RECOVERY_REQUEST_TIMEOUT_MS: u64 = 45_000;
const MAX_PENDING_RECOVERIES: usize = 16;
const MAX_RECOVERY_RESULT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRecoveryCommand {
    pub id: String,
    pub epoch: String,
    pub conversation_url: String,
    pub action: String,
    pub prompt: Option<String>,
    pub task_marker: String,
    pub expected: Option<Value>,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRecoveryPoll {
    pub epoch: String,
    pub requests: Vec<String>,
}

struct PendingRecovery {
    client_id: String,
    command: WorkerRecoveryCommand,
    claimed: bool,
    sender: Option<oneshot::Sender<Value>>,
}

struct RecoveryState {
    epoch: String,
    pending: BTreeMap<String, PendingRecovery>,
}

pub struct WorkerRecoveryBroker {
    state: Mutex<RecoveryState>,
}

impl Default for WorkerRecoveryBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerRecoveryBroker {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RecoveryState {
                epoch: Uuid::new_v4().to_string(),
                pending: BTreeMap::new(),
            }),
        }
    }

    pub async fn poll(&self, client_id: &str) -> Result<WorkerRecoveryPoll, String> {
        validate_client_id(client_id)?;
        let mut state = self.state.lock().await;
        prune_expired(&mut state);
        let requests = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.client_id == client_id && !pending.claimed)
            .map(|(id, _)| id.clone())
            .collect();
        Ok(WorkerRecoveryPoll {
            epoch: state.epoch.clone(),
            requests,
        })
    }

    pub async fn claim(
        &self,
        client_id: &str,
        request_id: &str,
        epoch: &str,
    ) -> Option<WorkerRecoveryCommand> {
        let mut state = self.state.lock().await;
        prune_expired(&mut state);
        if state.epoch != epoch {
            return None;
        }
        let pending = state.pending.get_mut(request_id)?;
        if pending.client_id != client_id
            || pending.claimed
            || pending.command.expires_at_ms <= now_ms()
        {
            return None;
        }
        pending.claimed = true;
        Some(pending.command.clone())
    }

    pub async fn result(
        &self,
        client_id: &str,
        request_id: &str,
        epoch: &str,
        result: Value,
    ) -> bool {
        if serde_json::to_vec(&result)
            .map(|bytes| bytes.len() > MAX_RECOVERY_RESULT_BYTES)
            .unwrap_or(true)
        {
            return false;
        }
        let mut state = self.state.lock().await;
        prune_expired(&mut state);
        if state.epoch != epoch {
            return false;
        }
        let now = now_ms();
        let valid = state.pending.get(request_id).is_some_and(|pending| {
            pending.client_id == client_id && pending.claimed && pending.command.expires_at_ms > now
        });
        if !valid {
            return false;
        }
        let Some(mut pending) = state.pending.remove(request_id) else {
            return false;
        };
        if let Some(sender) = pending.sender.take() {
            let _ = sender.send(result);
        }
        true
    }

    pub async fn execute(
        &self,
        client_id: &str,
        conversation_url: &str,
        action: &str,
        prompt: Option<String>,
        task_marker: &str,
        expected: Option<Value>,
    ) -> Result<Value, String> {
        validate_client_id(client_id)?;
        if !matches!(action, "inspect" | "continue" | "stop_continue") {
            return Err("worker recovery action is invalid".into());
        }
        if !conversation_url.starts_with("https://chatgpt.com/")
            || !conversation_url.contains("/c/")
            || conversation_url.len() > 2048
        {
            return Err("worker recovery requires a canonical ChatGPT conversation URL".into());
        }
        if task_marker.trim().is_empty() || task_marker.len() > 256 {
            return Err("worker recovery task marker is invalid".into());
        }
        if matches!(action, "continue" | "stop_continue")
            && prompt
                .as_deref()
                .is_none_or(|value| value.trim().is_empty() || value.len() > 16 * 1024)
        {
            return Err("worker recovery continuation prompt is invalid".into());
        }

        let (sender, receiver) = oneshot::channel();
        let request_id = Uuid::new_v4().to_string();
        let mut state = self.state.lock().await;
        prune_expired(&mut state);
        if state.pending.len() >= MAX_PENDING_RECOVERIES {
            return Err("worker recovery is busy".into());
        }
        let command = WorkerRecoveryCommand {
            id: request_id.clone(),
            epoch: state.epoch.clone(),
            conversation_url: conversation_url.to_string(),
            action: action.to_string(),
            prompt,
            task_marker: task_marker.to_string(),
            expected,
            expires_at_ms: now_ms().saturating_add(WORKER_RECOVERY_REQUEST_TIMEOUT_MS),
        };
        state.pending.insert(
            request_id.clone(),
            PendingRecovery {
                client_id: client_id.to_string(),
                command,
                claimed: false,
                sender: Some(sender),
            },
        );
        drop(state);

        match tokio::time::timeout(
            Duration::from_millis(WORKER_RECOVERY_REQUEST_TIMEOUT_MS),
            receiver,
        )
        .await
        {
            Ok(Ok(result)) => Ok(result),
            _ => {
                let mut state = self.state.lock().await;
                let claimed = state
                    .pending
                    .remove(&request_id)
                    .is_some_and(|pending| pending.claimed);
                if claimed {
                    Err("WORKER_RECOVERY_RESULT_UNCONFIRMED: recovery reached the Companion but its result was not received; inspect worker status before retrying".into())
                } else {
                    Err("WORKER_RECOVERY_NOT_DISPATCHED: the selected Companion did not claim recovery before its deadline".into())
                }
            }
        }
    }

    pub async fn remove_companion(&self, client_id: &str) {
        let mut state = self.state.lock().await;
        let ids = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let Some(mut pending) = state.pending.remove(&id) else {
                continue;
            };
            if let Some(sender) = pending.sender.take() {
                let message = if pending.claimed {
                    "WORKER_RECOVERY_RESULT_UNCONFIRMED: Companion disconnected after claiming recovery"
                } else {
                    "WORKER_RECOVERY_NOT_DISPATCHED: Companion disconnected before recovery"
                };
                let _ = sender.send(serde_json::json!({ "error": message }));
            }
        }
    }
}

fn prune_expired(state: &mut RecoveryState) {
    let now = now_ms();
    // Keep claimed commands until their owning execute() observes the timeout. Dropping a claimed
    // command here would make a lost destructive result look like "not dispatched", which could
    // invite an unsafe retry. Unclaimed expired work can be discarded immediately.
    state
        .pending
        .retain(|_, pending| pending.claimed || pending.command.expires_at_ms > now);
}

fn validate_client_id(client_id: &str) -> Result<(), String> {
    if client_id.trim().is_empty() || client_id.len() > 128 {
        return Err("worker recovery Companion client id is invalid".into());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn claimed_recovery_is_not_reoffered() {
        let broker = Arc::new(WorkerRecoveryBroker::new());
        let runner = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .execute(
                        "companion-a",
                        "https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                        "inspect",
                        None,
                        "moondesk-worker-task:test",
                        None,
                    )
                    .await
                    .expect("recovery result")
            })
        };
        tokio::task::yield_now().await;
        let poll = broker.poll("companion-a").await.expect("poll");
        let request_id = poll.requests.first().expect("pending recovery").clone();
        let command = broker
            .claim("companion-a", &request_id, &poll.epoch)
            .await
            .expect("claim recovery");
        assert_eq!(command.action, "inspect");
        assert!(
            broker
                .poll("companion-a")
                .await
                .expect("second poll")
                .requests
                .is_empty()
        );
        assert!(
            broker
                .result(
                    "companion-a",
                    &request_id,
                    &poll.epoch,
                    serde_json::json!({ "state": "idle" }),
                )
                .await
        );
        assert_eq!(runner.await.unwrap()["state"], "idle");
    }

    #[test]
    fn prune_expired_keeps_claimed_command_for_uncertain_timeout_accounting() {
        let expired_at = now_ms().saturating_sub(1);
        let command = |id: &str| WorkerRecoveryCommand {
            id: id.to_string(),
            epoch: "epoch".into(),
            conversation_url: "https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            action: "stop_continue".into(),
            prompt: Some("continue".into()),
            task_marker: "moondesk-worker-task:test".into(),
            expected: None,
            expires_at_ms: expired_at,
        };
        let (claimed_sender, _claimed_receiver) = oneshot::channel();
        let (queued_sender, _queued_receiver) = oneshot::channel();
        let mut state = RecoveryState {
            epoch: "epoch".into(),
            pending: [
                (
                    "claimed".into(),
                    PendingRecovery {
                        client_id: "companion-a".into(),
                        command: command("claimed"),
                        claimed: true,
                        sender: Some(claimed_sender),
                    },
                ),
                (
                    "queued".into(),
                    PendingRecovery {
                        client_id: "companion-a".into(),
                        command: command("queued"),
                        claimed: false,
                        sender: Some(queued_sender),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };

        prune_expired(&mut state);

        assert!(state.pending.contains_key("claimed"));
        assert!(!state.pending.contains_key("queued"));
    }
}
