use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

pub const USER_BROWSER_REQUEST_TIMEOUT_MS: u64 = 25_000;
pub const USER_BROWSER_PRESENCE_MS: u64 = 75_000;
pub const USER_BROWSER_MAX_CLIENTS: usize = 8;
pub const USER_BROWSER_MAX_PENDING: usize = 32;
pub const USER_BROWSER_MAX_RESULT_BYTES: usize = 1_800_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBrowserCommand {
    pub id: String,
    pub epoch: String,
    pub owner: String,
    pub tool: String,
    pub args: Value,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBrowserClient {
    pub browser_id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBrowserPoll {
    pub epoch: String,
    pub requests: Vec<String>,
}

struct ClientState {
    companion_client_id: String,
    name: String,
    seen_ms: u64,
    enabled: bool,
}

struct PendingRequest {
    browser_id: String,
    command: UserBrowserCommand,
    claimed: bool,
    sender: Option<oneshot::Sender<Value>>,
}

struct BrokerState {
    epoch: String,
    clients: BTreeMap<String, ClientState>,
    pending: BTreeMap<String, PendingRequest>,
}

pub struct UserBrowserBroker {
    state: Mutex<BrokerState>,
}

impl Default for UserBrowserBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl UserBrowserBroker {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BrokerState {
                epoch: Uuid::new_v4().to_string(),
                clients: BTreeMap::new(),
                pending: BTreeMap::new(),
            }),
        }
    }

    pub async fn browsers(&self) -> Vec<UserBrowserClient> {
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        state
            .clients
            .iter()
            .map(|(browser_id, client)| UserBrowserClient {
                browser_id: browser_id.clone(),
                name: client.name.clone(),
                enabled: client.enabled,
            })
            .collect()
    }

    pub async fn poll(
        &self,
        browser_id: &str,
        companion_client_id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<UserBrowserPoll, String> {
        validate_browser_id(browser_id)?;
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        if let Some(existing) = state.clients.get(browser_id)
            && existing.companion_client_id != companion_client_id
        {
            return Err(
                "user browser identity is already bound to another paired Companion".into(),
            );
        }
        if !state.clients.contains_key(browser_id)
            && state.clients.len() >= USER_BROWSER_MAX_CLIENTS
        {
            return Err(format!(
                "MoonDesk already has {USER_BROWSER_MAX_CLIENTS} connected browser companions"
            ));
        }
        state.clients.insert(
            browser_id.to_string(),
            ClientState {
                companion_client_id: companion_client_id.to_string(),
                name: name.trim().chars().take(80).collect(),
                seen_ms: now_ms(),
                enabled,
            },
        );
        let epoch = state.epoch.clone();
        let requests = if enabled {
            state
                .pending
                .iter()
                .filter(|(_, pending)| pending.browser_id == browser_id && !pending.claimed)
                .map(|(id, _)| id.clone())
                .collect()
        } else {
            Vec::new()
        };
        Ok(UserBrowserPoll { epoch, requests })
    }

    pub async fn claim(
        &self,
        browser_id: &str,
        companion_client_id: &str,
        request_id: &str,
        epoch: &str,
    ) -> Option<UserBrowserCommand> {
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        if state.epoch != epoch || !browser_belongs_to(&state, browser_id, companion_client_id) {
            return None;
        }
        let pending = state.pending.get_mut(request_id)?;
        if pending.browser_id != browser_id
            || pending.claimed
            || pending.command.expires_at <= now_ms()
        {
            return None;
        }
        pending.claimed = true;
        Some(pending.command.clone())
    }

    pub async fn check(
        &self,
        browser_id: &str,
        companion_client_id: &str,
        request_id: &str,
        epoch: &str,
    ) -> bool {
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        state.epoch == epoch
            && browser_belongs_to(&state, browser_id, companion_client_id)
            && state.pending.get(request_id).is_some_and(|pending| {
                pending.browser_id == browser_id
                    && pending.claimed
                    && pending.command.expires_at > now_ms()
            })
    }

    pub async fn result(
        &self,
        browser_id: &str,
        companion_client_id: &str,
        request_id: &str,
        epoch: &str,
        result: Value,
    ) -> bool {
        if serde_json::to_vec(&result)
            .map(|encoded| encoded.len() > USER_BROWSER_MAX_RESULT_BYTES)
            .unwrap_or(true)
        {
            return false;
        }
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        if state.epoch != epoch || !browser_belongs_to(&state, browser_id, companion_client_id) {
            return false;
        }
        let valid = state
            .pending
            .get(request_id)
            .is_some_and(|pending| pending.browser_id == browser_id && pending.claimed);
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
        browser_id: &str,
        owner: &str,
        tool: &str,
        args: Value,
    ) -> Result<Value, String> {
        validate_browser_id(browser_id)?;
        if owner.len() != 64 || !owner.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(
                "BROWSER_IDENTITY_REQUIRED: exact ChatGPT session identity is required".into(),
            );
        }
        let (sender, receiver) = oneshot::channel();
        let request_id = Uuid::new_v4().to_string();
        let mut state = self.state.lock().await;
        prune_clients(&mut state);
        let Some(client) = state.clients.get(browser_id) else {
            return Err("USER_BROWSER_OFFLINE: list connected user browsers again".into());
        };
        if !client.enabled {
            return Err(
                "USER_BROWSER_EXTENSION_PERMISSION: reload/update the MoonDesk Companion so Chrome grants debugger permission"
                    .into(),
            );
        }
        if state.pending.len() >= USER_BROWSER_MAX_PENDING {
            return Err("USER_BROWSER_BUSY: too many pending browser requests".into());
        }
        let command = UserBrowserCommand {
            id: request_id.clone(),
            epoch: state.epoch.clone(),
            owner: owner.to_string(),
            tool: tool.to_string(),
            args,
            expires_at: now_ms().saturating_add(USER_BROWSER_REQUEST_TIMEOUT_MS),
        };
        state.pending.insert(
            request_id.clone(),
            PendingRequest {
                browser_id: browser_id.to_string(),
                command,
                claimed: false,
                sender: Some(sender),
            },
        );
        drop(state);

        match tokio::time::timeout(
            Duration::from_millis(USER_BROWSER_REQUEST_TIMEOUT_MS),
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
                    Err("USER_BROWSER_RESULT_UNCONFIRMED: the operation was dispatched but its result was not received. Inspect the same tab before repeating input.".into())
                } else {
                    Err("USER_BROWSER_NOT_DISPATCHED: the companion did not claim this request before its deadline".into())
                }
            }
        }
    }
}

fn browser_belongs_to(state: &BrokerState, browser_id: &str, companion_client_id: &str) -> bool {
    state
        .clients
        .get(browser_id)
        .is_some_and(|client| client.companion_client_id == companion_client_id)
}

fn prune_clients(state: &mut BrokerState) {
    let now = now_ms();
    state
        .clients
        .retain(|_, client| now.saturating_sub(client.seen_ms) < USER_BROWSER_PRESENCE_MS);
}

fn validate_browser_id(browser_id: &str) -> Result<(), String> {
    Uuid::parse_str(browser_id)
        .map(|_| ())
        .map_err(|_| "invalid user browser identity".to_string())
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

    #[tokio::test]
    async fn claimed_request_is_never_offered_twice() {
        let broker = std::sync::Arc::new(UserBrowserBroker::new());
        let browser_id = Uuid::new_v4().to_string();
        broker
            .poll(&browser_id, "companion-a", "Chrome", true)
            .await
            .unwrap();
        let runner = {
            let broker = broker.clone();
            let browser_id = browser_id.clone();
            tokio::spawn(async move {
                broker
                    .execute(
                        &browser_id,
                        &"a".repeat(64),
                        "user_browser_tabs",
                        serde_json::json!({"action":"list"}),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        let poll = broker
            .poll(&browser_id, "companion-a", "Chrome", true)
            .await
            .unwrap();
        assert_eq!(poll.requests.len(), 1);
        assert!(
            broker
                .claim(&browser_id, "companion-b", &poll.requests[0], &poll.epoch)
                .await
                .is_none(),
            "another paired Companion must not claim this browser incarnation"
        );
        let command = broker
            .claim(&browser_id, "companion-a", &poll.requests[0], &poll.epoch)
            .await
            .expect("claim command");
        let second = broker
            .claim(&browser_id, "companion-a", &command.id, &poll.epoch)
            .await;
        assert!(second.is_none());
        assert!(
            broker
                .result(
                    &browser_id,
                    "companion-a",
                    &command.id,
                    &poll.epoch,
                    serde_json::json!({"ok":true})
                )
                .await
        );
        assert_eq!(
            runner.await.unwrap().unwrap(),
            serde_json::json!({"ok":true})
        );
    }

    #[tokio::test]
    async fn stale_epoch_cannot_claim_or_complete_request() {
        let broker = std::sync::Arc::new(UserBrowserBroker::new());
        let browser_id = Uuid::new_v4().to_string();
        let poll = broker
            .poll(&browser_id, "companion-a", "Chrome", true)
            .await
            .unwrap();
        assert!(
            broker
                .claim(&browser_id, "companion-a", "missing", "stale")
                .await
                .is_none()
        );
        assert!(
            !broker
                .result(
                    &browser_id,
                    "companion-a",
                    "missing",
                    &poll.epoch,
                    serde_json::json!({}),
                )
                .await
        );
    }
}
