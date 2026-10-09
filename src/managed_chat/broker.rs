use super::store;
use super::types::{
    ManagedChatAnchorContext, ManagedChatCommand, ManagedChatCommandId, ManagedChatCommandState,
    ManagedChatLaunch, ManagedChatLease, ManagedChatLeaseId, ManagedChatStoreData,
    ManagedChatTerminalResult,
};
use super::{
    DEFAULT_COMMAND_LEASE_MS, MAX_MANAGED_CHAT_COMMANDS, MAX_MANAGED_CHAT_DETAIL_BYTES,
    MAX_MANAGED_CHAT_TERMINAL_HISTORY, MAX_MANAGED_CHAT_THREAD_AFFINITIES,
};
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
    Succeeded {
        details: Option<String>,
        conversation_url: Option<String>,
    },
    Failed {
        details: Option<String>,
    },
    NeedsReconcile,
    Paused {
        details: Option<String>,
    },
}

pub struct ManagedChatBroker {
    path: PathBuf,
    data: Mutex<ManagedChatStoreData>,
    #[cfg(test)]
    fail_next_commit: std::sync::atomic::AtomicBool,
}

impl ManagedChatBroker {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ManagedChatError> {
        let path = path.into();
        let data = store::load(&path).map_err(storage_error)?;
        Ok(Self {
            path,
            data: Mutex::new(data),
            #[cfg(test)]
            fail_next_commit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) async fn snapshot(&self) -> ManagedChatStoreData {
        self.data.lock().await.clone()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_commit_for_test(&self) {
        self.fail_next_commit
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

    pub async fn anchor_session_digest_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, ManagedChatError> {
        if conversation_id.trim().is_empty() || conversation_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat Core conversation id is invalid".into(),
            ));
        }
        let guard = self.data.lock().await;
        let mut digest: Option<String> = None;
        for command in guard.commands.values() {
            if command
                .anchor_context
                .as_ref()
                .is_none_or(|context| context.conversation_id != conversation_id)
            {
                continue;
            }
            let Some(candidate) = command.launch.anchor_session_digest.as_ref() else {
                continue;
            };
            if digest
                .as_ref()
                .is_some_and(|existing| existing != candidate)
            {
                return Err(ManagedChatError::Conflict(
                    "Core conversation maps to multiple worker session identities".into(),
                ));
            }
            digest = Some(candidate.clone());
        }
        Ok(digest)
    }

    pub async fn ensure_clearable_anchor_session(
        &self,
        session_digest: &str,
    ) -> Result<Vec<ManagedChatCommand>, ManagedChatError> {
        validate_core_session_digest(session_digest)?;
        let guard = self.data.lock().await;
        let matching = guard
            .commands
            .values()
            .filter(|command| {
                command.launch.anchor_session_digest.as_deref() == Some(session_digest)
            })
            .cloned()
            .collect::<Vec<_>>();
        ensure_commands_clearable(&matching)?;
        Ok(matching)
    }

    pub async fn purge_terminal_for_anchor_session(
        &self,
        session_digest: &str,
    ) -> Result<Vec<ManagedChatCommand>, ManagedChatError> {
        validate_core_session_digest(session_digest)?;
        let mut guard = self.data.lock().await;
        let matching = guard
            .commands
            .values()
            .filter(|command| {
                command.launch.anchor_session_digest.as_deref() == Some(session_digest)
            })
            .cloned()
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return Ok(Vec::new());
        }
        ensure_commands_clearable(&matching)?;

        let mut candidate = guard.clone();
        for command in &matching {
            candidate.commands.remove(&command.id);
            candidate.dedupe.remove(&command.dedupe_key);
        }
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(matching)
    }

    pub async fn settle_revoked_client(
        &self,
        client_id: &str,
    ) -> Result<(usize, usize, Vec<ManagedChatCommand>), ManagedChatError> {
        if client_id.trim().is_empty() || client_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat client id must contain 1..=128 bytes".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();
        let mut retargeted = 0usize;
        let mut paused = 0usize;
        let mut changed = Vec::new();
        for command in candidate.commands.values_mut() {
            if command.target_client_id.as_deref() != Some(client_id) {
                continue;
            }
            match command.state {
                ManagedChatCommandState::Queued => {
                    command.target_client_id = None;
                    retargeted += 1;
                }
                ManagedChatCommandState::Leased if !command.reconcile_history => {
                    command.state = ManagedChatCommandState::Queued;
                    command.lease = None;
                    command.target_client_id = None;
                    retargeted += 1;
                }
                ManagedChatCommandState::Leased
                | ManagedChatCommandState::SendStarted
                | ManagedChatCommandState::NeedsReconcile => {
                    command.state = ManagedChatCommandState::Paused;
                    command.reconcile_history = true;
                    command.lease = None;
                    command.target_client_id = None;
                    command.terminal = Some(ManagedChatTerminalResult {
                        succeeded: false,
                        details: Some("browser_revoked_before_reconciliation".into()),
                        conversation_url: None,
                    });
                    paused += 1;
                }
                ManagedChatCommandState::Paused
                | ManagedChatCommandState::Succeeded
                | ManagedChatCommandState::Failed => {
                    command.target_client_id = None;
                }
            }
            if command.target_client_id.is_none() {
                command.target_client_pinned_by_request = false;
            }
            changed.push(command.clone());
        }
        if changed.is_empty() {
            return Ok((0, 0, changed));
        }
        self.commit_candidate(&mut guard, candidate).await?;
        Ok((retargeted, paused, changed))
    }

    pub async fn settle_for_worker_retire_by_dedupe(
        &self,
        dedupe_key: &str,
    ) -> Result<bool, ManagedChatError> {
        if dedupe_key.trim().is_empty() || dedupe_key.len() > 256 {
            return Err(ManagedChatError::Invalid(
                "managed chat dedupe key must contain 1..=256 bytes".into(),
            ));
        }
        let guard = self.data.lock().await;
        let Some(command_id) = guard.dedupe.get(dedupe_key).cloned() else {
            return Ok(false);
        };
        let command = guard.commands.get(&command_id).ok_or_else(|| {
            ManagedChatError::Storage("managed chat dedupe index is corrupt".into())
        })?;
        if command.state == ManagedChatCommandState::Paused
            || (command.state == ManagedChatCommandState::Failed && command.reconcile_history)
        {
            return Ok(true);
        }
        drop(guard);
        self.cancel_pre_send_by_dedupe(dedupe_key).await
    }

    pub async fn cancel_pre_send_by_dedupe(
        &self,
        dedupe_key: &str,
    ) -> Result<bool, ManagedChatError> {
        if dedupe_key.trim().is_empty() || dedupe_key.len() > 256 {
            return Err(ManagedChatError::Invalid(
                "managed chat dedupe key must contain 1..=256 bytes".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let Some(command_id) = guard.dedupe.get(dedupe_key).cloned() else {
            return Ok(false);
        };
        let command = guard.commands.get(&command_id).ok_or_else(|| {
            ManagedChatError::Storage("managed chat dedupe index is corrupt".into())
        })?;
        let safe = matches!(command.state, ManagedChatCommandState::Queued)
            || (command.state == ManagedChatCommandState::Leased && !command.reconcile_history)
            || (command.state == ManagedChatCommandState::Failed && !command.reconcile_history);
        if !safe {
            return Err(ManagedChatError::Conflict(
                "managed chat launch cannot be cancelled because it may have crossed the Send boundary"
                    .into(),
            ));
        }

        let mut candidate = guard.clone();
        candidate.commands.remove(&command_id);
        candidate.dedupe.remove(dedupe_key);
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(true)
    }

    #[cfg(test)]
    pub async fn enqueue(
        &self,
        request: EnqueueManagedChatRequest,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        self.enqueue_with_route_dispatch(request, None, None, true)
            .await
    }

    #[cfg(test)]
    pub async fn enqueue_with_route(
        &self,
        request: EnqueueManagedChatRequest,
        requested_client_id: Option<String>,
        requested_anchor_context: Option<ManagedChatAnchorContext>,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        self.enqueue_with_route_dispatch(
            request,
            requested_client_id,
            requested_anchor_context,
            true,
        )
        .await
    }

    pub async fn enqueue_held_with_route(
        &self,
        request: EnqueueManagedChatRequest,
        requested_client_id: Option<String>,
        requested_anchor_context: Option<ManagedChatAnchorContext>,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        self.enqueue_with_route_dispatch(
            request,
            requested_client_id,
            requested_anchor_context,
            false,
        )
        .await
    }

    async fn enqueue_with_route_dispatch(
        &self,
        request: EnqueueManagedChatRequest,
        requested_client_id: Option<String>,
        requested_anchor_context: Option<ManagedChatAnchorContext>,
        dispatch_ready: bool,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        validate_enqueue_request(&request)?;
        if requested_client_id
            .as_deref()
            .is_some_and(|client_id| client_id.trim().is_empty() || client_id.len() > 128)
        {
            return Err(ManagedChatError::Invalid(
                "managed chat requested client id is invalid".into(),
            ));
        }
        if let Some(anchor_context) = requested_anchor_context.as_ref() {
            anchor_context
                .validate()
                .map_err(ManagedChatError::Invalid)?;
        }
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
        let mut candidate = guard.clone();
        compact_terminal_history(&mut candidate);
        if candidate.commands.len() >= MAX_MANAGED_CHAT_COMMANDS {
            return Err(ManagedChatError::Limit(format!(
                "managed chat command store is limited to {MAX_MANAGED_CHAT_COMMANDS} active, paused, and retained commands"
            )));
        }
        let sequence = candidate.next_sequence;
        candidate.next_sequence = candidate.next_sequence.checked_add(1).ok_or_else(|| {
            ManagedChatError::Limit("managed chat command sequence is exhausted".into())
        })?;
        let command_id = ManagedChatCommandId::new();
        let prior_thread_command =
            if request.launch.open_mode == super::types::ManagedChatOpenMode::ExistingThread {
                request.launch.thread_key.as_deref().and_then(|thread_key| {
                    candidate
                        .commands
                        .values()
                        .filter(|command| {
                            command.state == ManagedChatCommandState::Succeeded
                                && command.launch.thread_key.as_deref() == Some(thread_key)
                        })
                        .max_by_key(|command| command.sequence)
                })
            } else {
                None
            };
        let prior_target_client_id =
            prior_thread_command.and_then(|command| command.target_client_id.clone());
        let target_client_pinned_by_request =
            prior_target_client_id.is_some() || requested_client_id.is_some();
        let target_client_id = prior_target_client_id.or(requested_client_id);
        let anchor_context = prior_thread_command
            .and_then(|command| command.anchor_context.clone())
            .or(requested_anchor_context);
        let command = ManagedChatCommand {
            id: command_id.clone(),
            sequence,
            dedupe_key: request.dedupe_key.clone(),
            request_fingerprint: fingerprint,
            launch: request.launch,
            state: ManagedChatCommandState::Queued,
            dispatch_ready,
            reconcile_history: false,
            lease: None,
            terminal: None,
            target_client_id,
            target_client_pinned_by_request,
            anchor_context,
        };
        candidate
            .commands
            .insert(command_id.clone(), command.clone());
        candidate.dedupe.insert(request.dedupe_key, command_id);
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(command)
    }

    pub async fn activate_dispatch(
        &self,
        command_id: &ManagedChatCommandId,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        let mut guard = self.data.lock().await;
        let command = guard
            .commands
            .get(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        if command.dispatch_ready {
            return Ok(command.clone());
        }
        if command.state != ManagedChatCommandState::Queued
            || command.lease.is_some()
            || command.terminal.is_some()
            || command.reconcile_history
        {
            return Err(ManagedChatError::Conflict(
                "held managed chat command can only be activated before dispatch".into(),
            ));
        }

        let mut candidate = guard.clone();
        let command = candidate
            .commands
            .get_mut(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        command.dispatch_ready = true;
        let activated = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(activated)
    }

    #[cfg(test)]
    pub async fn redeem(
        &self,
        client_id: &str,
        now_ms: u64,
    ) -> Result<Option<ManagedChatLeaseOffer>, ManagedChatError> {
        self.redeem_for_anchors(client_id, now_ms, None).await
    }

    pub async fn redeem_for_anchors(
        &self,
        client_id: &str,
        now_ms: u64,
        eligible_anchor_digests: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<Option<ManagedChatLeaseOffer>, ManagedChatError> {
        if client_id.trim().is_empty() || client_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat client id must contain 1..=128 bytes".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let mut candidate = guard.clone();

        for command in candidate.commands.values_mut() {
            let expired = command
                .lease
                .as_ref()
                .is_some_and(|lease| lease.expires_at_ms <= now_ms);
            if !expired {
                continue;
            }
            match command.state {
                ManagedChatCommandState::Leased => {
                    if command.reconcile_history {
                        command.state = ManagedChatCommandState::NeedsReconcile;
                    } else {
                        command.state = ManagedChatCommandState::Queued;
                        if !command.target_client_pinned_by_request {
                            command.target_client_id = None;
                        }
                    }
                    command.lease = None;
                }
                ManagedChatCommandState::SendStarted => {
                    command.state = ManagedChatCommandState::NeedsReconcile;
                    command.reconcile_history = true;
                    command.lease = None;
                }
                _ => {}
            }
        }

        let selected_id = candidate
            .commands
            .values()
            .filter(|command| {
                command.dispatch_ready
                    && command.state == ManagedChatCommandState::NeedsReconcile
                    && command
                        .target_client_id
                        .as_deref()
                        .is_none_or(|target| target == client_id)
                    && anchor_is_eligible(command, eligible_anchor_digests)
            })
            .min_by_key(|command| command.sequence)
            .or_else(|| {
                candidate
                    .commands
                    .values()
                    .filter(|command| {
                        command.dispatch_ready
                            && command.state == ManagedChatCommandState::Queued
                            && command
                                .target_client_id
                                .as_deref()
                                .is_none_or(|target| target == client_id)
                            && anchor_is_eligible(command, eligible_anchor_digests)
                    })
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
        let reconcile_required =
            command.reconcile_history || command.state == ManagedChatCommandState::NeedsReconcile;
        let lease = ManagedChatLease {
            lease_id: ManagedChatLeaseId::new(),
            client_id: client_id.to_string(),
            expires_at_ms: now_ms.saturating_add(DEFAULT_COMMAND_LEASE_MS),
        };
        if command.target_client_id.is_none() {
            command.target_client_id = Some(client_id.to_string());
        }
        command.state = ManagedChatCommandState::Leased;
        command.lease = Some(lease);
        let offered = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(Some(ManagedChatLeaseOffer {
            command: offered,
            reconcile_required,
        }))
    }

    pub async fn mark_send_started(
        &self,
        command_id: &ManagedChatCommandId,
        lease_id: &ManagedChatLeaseId,
        client_id: &str,
    ) -> Result<ManagedChatCommand, ManagedChatError> {
        if client_id.trim().is_empty() || client_id.len() > 128 {
            return Err(ManagedChatError::Invalid(
                "managed chat client id must contain 1..=128 bytes".into(),
            ));
        }
        let mut guard = self.data.lock().await;
        let command = guard
            .commands
            .get(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        if command.state == ManagedChatCommandState::SendStarted {
            let lease = command.lease.as_ref().ok_or_else(|| {
                ManagedChatError::Conflict("send-started command has no active lease".into())
            })?;
            if &lease.lease_id == lease_id && lease.client_id == client_id {
                return Ok(command.clone());
            }
        }
        if command.state != ManagedChatCommandState::Leased || command.reconcile_history {
            return Err(ManagedChatError::Conflict(
                "managed chat command can cross the Send boundary only from a fresh pre-Send lease"
                    .into(),
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
        command.state = ManagedChatCommandState::SendStarted;
        command.reconcile_history = true;
        let updated = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(updated)
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
            ManagedChatAckOutcome::Succeeded { details, .. }
            | ManagedChatAckOutcome::Failed { details }
            | ManagedChatAckOutcome::Paused { details } => details.as_deref(),
            ManagedChatAckOutcome::NeedsReconcile => None,
        };
        if details.is_some_and(|value| value.len() > MAX_MANAGED_CHAT_DETAIL_BYTES) {
            return Err(ManagedChatError::Invalid(format!(
                "managed chat ack details exceed {MAX_MANAGED_CHAT_DETAIL_BYTES} bytes"
            )));
        }
        if let ManagedChatAckOutcome::Succeeded {
            conversation_url, ..
        } = &outcome
        {
            let Some(conversation_url) = conversation_url.as_deref() else {
                return Err(ManagedChatError::Invalid(
                    "managed chat worker success requires a confirmed ChatGPT conversation URL"
                        .into(),
                ));
            };
            if conversation_url.len() > 2048
                || canonical_chatgpt_conversation_id(conversation_url).is_none()
            {
                return Err(ManagedChatError::Invalid(
                    "managed chat worker success requires a canonical ChatGPT /c/<id> conversation URL"
                        .into(),
                ));
            }
        }

        let mut guard = self.data.lock().await;
        let command = guard
            .commands
            .get(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        if matches!(
            command.state,
            ManagedChatCommandState::Succeeded
                | ManagedChatCommandState::Failed
                | ManagedChatCommandState::Paused
        ) {
            if terminal_matches(command, &outcome) {
                return Ok(command.clone());
            }
            return Err(ManagedChatError::Conflict(
                "managed chat command is already terminal or paused with a different outcome"
                    .into(),
            ));
        }
        if command.state == ManagedChatCommandState::NeedsReconcile
            && matches!(outcome, ManagedChatAckOutcome::NeedsReconcile)
        {
            return Ok(command.clone());
        }
        let active_lease = command.lease.as_ref().ok_or_else(|| {
            ManagedChatError::Conflict("managed chat command has no active lease".into())
        })?;
        if &active_lease.lease_id != lease_id || active_lease.client_id != client_id {
            return Err(ManagedChatError::Conflict(
                "managed chat lease is stale or belongs to another redemption".into(),
            ));
        }

        match &outcome {
            ManagedChatAckOutcome::Succeeded { .. } => {
                if command.state != ManagedChatCommandState::SendStarted
                    && !command.reconcile_history
                {
                    return Err(ManagedChatError::Conflict(
                        "fresh managed chat success requires a durable send_started boundary"
                            .into(),
                    ));
                }
            }
            ManagedChatAckOutcome::Failed { .. } => {
                if command.state != ManagedChatCommandState::Leased || command.reconcile_history {
                    return Err(ManagedChatError::Conflict(
                        "post-Send or reconciliation failures must pause or reconcile instead of becoming fresh-retry failures"
                            .into(),
                    ));
                }
            }
            ManagedChatAckOutcome::NeedsReconcile | ManagedChatAckOutcome::Paused { .. } => {
                if command.state != ManagedChatCommandState::SendStarted
                    && !command.reconcile_history
                {
                    return Err(ManagedChatError::Conflict(
                        "managed chat reconciliation is only valid after the Send boundary".into(),
                    ));
                }
            }
        }

        let mut candidate = guard.clone();
        let command = candidate
            .commands
            .get_mut(command_id)
            .ok_or(ManagedChatError::NotFound)?;
        command.lease = None;
        match outcome {
            ManagedChatAckOutcome::Succeeded {
                details,
                conversation_url,
            } => {
                command.state = ManagedChatCommandState::Succeeded;
                command.terminal = Some(ManagedChatTerminalResult {
                    succeeded: true,
                    details,
                    conversation_url,
                });
            }
            ManagedChatAckOutcome::Failed { details } => {
                command.state = ManagedChatCommandState::Failed;
                command.terminal = Some(ManagedChatTerminalResult {
                    succeeded: false,
                    details,
                    conversation_url: None,
                });
            }
            ManagedChatAckOutcome::NeedsReconcile => {
                command.state = ManagedChatCommandState::NeedsReconcile;
                command.reconcile_history = true;
                command.terminal = None;
            }
            ManagedChatAckOutcome::Paused { details } => {
                command.state = ManagedChatCommandState::Paused;
                command.reconcile_history = true;
                command.terminal = Some(ManagedChatTerminalResult {
                    succeeded: false,
                    details,
                    conversation_url: None,
                });
            }
        }
        let acknowledged = command.clone();
        self.commit_candidate(&mut guard, candidate).await?;
        Ok(acknowledged)
    }

    async fn commit_candidate(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, ManagedChatStoreData>,
        candidate: ManagedChatStoreData,
    ) -> Result<(), ManagedChatError> {
        #[cfg(test)]
        if self
            .fail_next_commit
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ManagedChatError::Storage(
                "injected managed-chat persistence failure".into(),
            ));
        }
        let path = self.path.clone();
        let persisted = candidate.clone();
        let prepared = tokio::task::spawn_blocking(move || store::prepare_save(&path, &persisted))
            .await
            .map_err(|error| {
                ManagedChatError::Storage(format!("managed chat persistence task failed: {error}"))
            })?
            .map_err(storage_error)?;
        // Keep the cancellation point before the canonical store mutation. The prepared temp file
        // can be abandoned safely; once this resumes, disk commit and in-memory publication happen
        // back-to-back without another await.
        store::commit_prepared(prepared).map_err(storage_error)?;
        **guard = candidate;
        Ok(())
    }
}

fn validate_core_session_digest(session_digest: &str) -> Result<(), ManagedChatError> {
    if session_digest.len() != 64 || !session_digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ManagedChatError::Invalid(
            "managed chat Core session digest is invalid".into(),
        ));
    }
    Ok(())
}

fn ensure_commands_clearable(commands: &[ManagedChatCommand]) -> Result<(), ManagedChatError> {
    if commands.iter().any(|command| match command.state {
        ManagedChatCommandState::Succeeded => false,
        ManagedChatCommandState::Failed => command.reconcile_history,
        _ => true,
    }) {
        return Err(ManagedChatError::Conflict(
            "Core workers cannot be cleared while a launch is active or its Send outcome is ambiguous"
                .into(),
        ));
    }
    Ok(())
}

fn anchor_is_eligible(
    command: &ManagedChatCommand,
    eligible_anchor_digests: Option<&std::collections::BTreeSet<String>>,
) -> bool {
    if command.target_client_id.is_some() {
        return true;
    }
    let Some(eligible) = eligible_anchor_digests else {
        return true;
    };
    command
        .anchor_context
        .as_ref()
        .is_none_or(|context| eligible.contains(&context.conversation_id))
}

fn compact_terminal_history(data: &mut ManagedChatStoreData) {
    let mut newest_thread_success =
        std::collections::BTreeMap::<String, (u64, ManagedChatCommandId)>::new();
    for command in data.commands.values() {
        if command.state != ManagedChatCommandState::Succeeded {
            continue;
        }
        let Some(thread_key) = command.launch.thread_key.as_ref() else {
            continue;
        };
        let replace = newest_thread_success
            .get(thread_key)
            .is_none_or(|(sequence, _)| command.sequence > *sequence);
        if replace {
            newest_thread_success
                .insert(thread_key.clone(), (command.sequence, command.id.clone()));
        }
    }
    let mut thread_affinities = newest_thread_success.into_values().collect::<Vec<_>>();
    thread_affinities.sort_by_key(|(sequence, _)| std::cmp::Reverse(*sequence));
    let protected = thread_affinities
        .into_iter()
        .take(MAX_MANAGED_CHAT_THREAD_AFFINITIES)
        .map(|(_, command_id)| command_id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut terminal = data
        .commands
        .values()
        .filter(|command| {
            matches!(
                command.state,
                ManagedChatCommandState::Succeeded | ManagedChatCommandState::Failed
            ) && !protected.contains(&command.id)
        })
        .map(|command| (command.sequence, command.id.clone()))
        .collect::<Vec<_>>();
    terminal.sort_by_key(|(sequence, _)| std::cmp::Reverse(*sequence));
    let remove = terminal
        .into_iter()
        .skip(MAX_MANAGED_CHAT_TERMINAL_HISTORY)
        .map(|(_, command_id)| command_id)
        .collect::<std::collections::BTreeSet<_>>();
    if remove.is_empty() {
        return;
    }
    data.commands
        .retain(|command_id, _| !remove.contains(command_id));
    data.dedupe
        .retain(|_, command_id| data.commands.contains_key(command_id));
}

fn canonical_chatgpt_conversation_id(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("chatgpt.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let valid_project_segment = |segment: &str| {
        segment
            .get(..36)
            .and_then(|prefix| prefix.strip_prefix("g-p-"))
            .is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    };
    let conversation_id = match segments.as_slice() {
        ["c", conversation_id] => *conversation_id,
        ["g", project, "c", conversation_id] if valid_project_segment(project) => *conversation_id,
        ["g", project, "shared", "c", conversation_id] if valid_project_segment(project) => {
            *conversation_id
        }
        _ => return None,
    };
    if !(16..=64).contains(&conversation_id.len())
        || !conversation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return None;
    }
    Some(conversation_id.to_ascii_lowercase())
}

fn terminal_matches(command: &ManagedChatCommand, outcome: &ManagedChatAckOutcome) -> bool {
    let Some(terminal) = &command.terminal else {
        return false;
    };
    match outcome {
        ManagedChatAckOutcome::Succeeded {
            details,
            conversation_url,
        } => {
            terminal.succeeded
                && &terminal.details == details
                && &terminal.conversation_url == conversation_url
        }
        ManagedChatAckOutcome::Failed { details } => {
            command.state == ManagedChatCommandState::Failed
                && !terminal.succeeded
                && &terminal.details == details
        }
        ManagedChatAckOutcome::Paused { details } => {
            command.state == ManagedChatCommandState::Paused
                && !terminal.succeeded
                && &terminal.details == details
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

    fn anchor_context(conversation_id: &str) -> ManagedChatAnchorContext {
        ManagedChatAnchorContext {
            conversation_id: conversation_id.to_string(),
            conversation_url: format!("https://chatgpt.com/c/{conversation_id}"),
            project_id: None,
            project_url: None,
        }
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
            anchor_session_digest: None,
        }
    }

    #[tokio::test]
    async fn held_command_cannot_be_redeemed_until_explicit_activation() {
        let root = temp_root("moondesk-managed-chat-held-dispatch");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let command = broker
            .enqueue_held_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "worker:held:task:first".into(),
                    launch: launch("held-dispatch"),
                },
                Some("extension-a".into()),
                None,
            )
            .await
            .expect("enqueue held command");
        assert!(!command.dispatch_ready);
        assert!(
            broker
                .redeem("extension-a", 100)
                .await
                .expect("redeem held command")
                .is_none()
        );

        let activated = broker
            .activate_dispatch(&command.id)
            .await
            .expect("activate held command");
        assert!(activated.dispatch_ready);
        let repeated = broker
            .activate_dispatch(&command.id)
            .await
            .expect("activation is idempotent");
        assert_eq!(repeated, activated);

        let offered = broker
            .redeem("extension-a", 200)
            .await
            .expect("redeem activated command")
            .expect("activated command is dispatchable");
        assert_eq!(offered.command.id, command.id);
        assert!(!offered.reconcile_required);

        let _ = std::fs::remove_dir_all(root);
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
    async fn core_clear_refuses_active_work_and_removes_proven_pre_send_failure() {
        let root = temp_root("moondesk-managed-chat-Core-clear");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let conversation_id = "6ac62769-0c94-83e8-a8ef-95a21c7f3e7f";
        let session_digest = "a".repeat(64);
        let mut safe_launch = launch("Core-clear-safe");
        safe_launch.anchor_session_digest = Some(session_digest.clone());
        let command = broker
            .enqueue_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "Core-clear-safe".into(),
                    launch: safe_launch,
                },
                Some("extension-a".into()),
                Some(anchor_context(conversation_id)),
            )
            .await
            .expect("enqueue Core command");

        assert_eq!(
            broker
                .anchor_session_digest_for_conversation(conversation_id)
                .await
                .expect("resolve Core session"),
            Some(session_digest.clone())
        );
        let active_error = broker
            .purge_terminal_for_anchor_session(&session_digest)
            .await
            .expect_err("queued command must block clear");
        assert!(matches!(active_error, ManagedChatError::Conflict(_)));

        let offer = broker
            .redeem("extension-a", 1_000)
            .await
            .expect("redeem Core command")
            .expect("Core command lease");
        let lease_id = offer
            .command
            .lease
            .as_ref()
            .expect("lease")
            .lease_id
            .clone();
        broker
            .acknowledge(
                &command.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("failed before Send".into()),
                },
            )
            .await
            .expect("ack safe failure");
        let removed = broker
            .purge_terminal_for_anchor_session(&session_digest)
            .await
            .expect("clear terminal Core commands");
        assert_eq!(removed.len(), 1);
        assert!(broker.snapshot().await.commands.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pre_send_cancel_only_removes_safe_commands() {
        let root = temp_root("moondesk-managed-chat-pre-send-cancel");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");

        broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "safe-queued".into(),
                launch: launch("safe-queued"),
            })
            .await
            .expect("enqueue safe queued command");
        assert!(
            broker
                .cancel_pre_send_by_dedupe("safe-queued")
                .await
                .expect("cancel queued command")
        );
        assert!(
            !broker
                .cancel_pre_send_by_dedupe("safe-queued")
                .await
                .expect("repeat missing cancel")
        );

        let failed = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "safe-failed".into(),
                launch: launch("safe-failed"),
            })
            .await
            .expect("enqueue failed command");
        let failed_offer = broker
            .redeem("extension-a", 1_000)
            .await
            .expect("redeem failed command")
            .expect("failed lease");
        broker
            .acknowledge(
                &failed.id,
                &failed_offer
                    .command
                    .lease
                    .as_ref()
                    .expect("failed lease")
                    .lease_id,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("model unavailable before send".into()),
                },
            )
            .await
            .expect("ack pre-send failure");
        assert!(
            broker
                .cancel_pre_send_by_dedupe("safe-failed")
                .await
                .expect("cancel failed pre-send command")
        );

        let ambiguous = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "unsafe-ambiguous".into(),
                launch: launch("unsafe-ambiguous"),
            })
            .await
            .expect("enqueue ambiguous command");
        let ambiguous_offer = broker
            .redeem("extension-a", 2_000)
            .await
            .expect("redeem ambiguous command")
            .expect("ambiguous lease");
        let ambiguous_lease = ambiguous_offer
            .command
            .lease
            .as_ref()
            .expect("ambiguous lease")
            .lease_id
            .clone();
        broker
            .mark_send_started(&ambiguous.id, &ambiguous_lease, "extension-a")
            .await
            .expect("cross send boundary for ambiguous command");
        broker
            .acknowledge(
                &ambiguous.id,
                &ambiguous_lease,
                "extension-a",
                ManagedChatAckOutcome::NeedsReconcile,
            )
            .await
            .expect("mark ambiguous command");
        let error = broker
            .cancel_pre_send_by_dedupe("unsafe-ambiguous")
            .await
            .expect_err("ambiguous command must not be cancelled");
        assert!(matches!(error, ManagedChatError::Conflict(_)));

        let reconcile_offer = broker
            .redeem("extension-a", 2_100)
            .await
            .expect("redeem ambiguous reconciliation")
            .expect("reconciliation lease");
        broker
            .acknowledge(
                &ambiguous.id,
                &reconcile_offer
                    .command
                    .lease
                    .as_ref()
                    .expect("reconciliation lease")
                    .lease_id,
                "extension-a",
                ManagedChatAckOutcome::Paused {
                    details: Some("reconciliation paused".into()),
                },
            )
            .await
            .expect("terminalize ambiguous command");
        let error = broker
            .cancel_pre_send_by_dedupe("unsafe-ambiguous")
            .await
            .expect_err("terminal ambiguous command must not be erased as pre-send");
        assert!(matches!(error, ManagedChatError::Conflict(_)));
        assert!(
            broker
                .settle_for_worker_retire_by_dedupe("unsafe-ambiguous")
                .await
                .expect("terminal ambiguous command is safe to retire around")
        );

        let snapshot = broker.snapshot().await;
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(
            snapshot
                .commands
                .get(&ambiguous.id)
                .map(|command| command.state),
            Some(ManagedChatCommandState::Paused)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn lease_expiry_distinguishes_pre_send_from_crossed_send_boundary() {
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
        let first_lease = first
            .command
            .lease
            .as_ref()
            .expect("first lease")
            .lease_id
            .clone();
        drop(broker);

        let reopened = ManagedChatBroker::open(&path).expect("reopen broker");
        let second = reopened
            .redeem("extension-b", 1_000 + DEFAULT_COMMAND_LEASE_MS + 1)
            .await
            .expect("redeem expired pre-send command")
            .expect("fresh command after safe expiry");
        assert!(!second.reconcile_required);
        assert_eq!(second.command.id, command.id);
        let second_lease = second
            .command
            .lease
            .as_ref()
            .expect("second lease")
            .lease_id
            .clone();
        assert_ne!(second_lease, first_lease);

        reopened
            .mark_send_started(&command.id, &second_lease, "extension-b")
            .await
            .expect("cross durable send boundary");
        drop(reopened);

        let reopened = ManagedChatBroker::open(&path).expect("reopen after send started");
        assert!(
            reopened
                .redeem("extension-c", 1_000 + (DEFAULT_COMMAND_LEASE_MS * 2) + 2)
                .await
                .expect("other browser redemption remains safe")
                .is_none(),
            "post-Send reconciliation must stay with the browser that crossed the Send boundary"
        );
        let third = reopened
            .redeem("extension-b", 1_000 + (DEFAULT_COMMAND_LEASE_MS * 2) + 2)
            .await
            .expect("redeem expired post-send command")
            .expect("reconciliation command");
        assert!(third.reconcile_required);
        assert_eq!(third.command.id, command.id);
        assert_eq!(
            third.command.target_client_id.as_deref(),
            Some("extension-b")
        );
        assert!(third.command.reconcile_history);
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
                    conversation_url: Some(
                        "https://chatgpt.com/c/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    ),
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
        broker
            .mark_send_started(&command.id, &lease_id, "extension-a")
            .await
            .expect("cross send boundary for second lease");
        let succeeded = broker
            .acknowledge(
                &command.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                    conversation_url: Some(
                        "https://chatgpt.com/c/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    ),
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
                    conversation_url: Some(
                        "https://chatgpt.com/c/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    ),
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
    async fn failed_and_paused_commands_remain_terminal_without_manual_replay() {
        let root = temp_root("moondesk-managed-chat-no-manual-replay");
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
        assert!(
            broker
                .redeem("extension-a", 200)
                .await
                .expect("redeem after terminal failure")
                .is_none()
        );

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
            .mark_send_started(&ambiguous.id, &first_lease, "extension-a")
            .await
            .expect("cross send boundary for ambiguous command");
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
        let reconcile_lease = reconcile
            .command
            .lease
            .as_ref()
            .expect("reconcile lease")
            .lease_id
            .clone();
        let paused = broker
            .acknowledge(
                &ambiguous.id,
                &reconcile_lease,
                "extension-a",
                ManagedChatAckOutcome::Paused {
                    details: Some("reconcile_payload_invalid".into()),
                },
            )
            .await
            .expect("pause after reconciliation failure");
        assert_eq!(paused.state, ManagedChatCommandState::Paused);
        assert!(paused.reconcile_history);
        assert!(
            broker
                .redeem("extension-a", 500)
                .await
                .expect("redeem after paused terminal command")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(root);
    }
    #[tokio::test]
    async fn untargeted_routed_command_is_redeemable_only_by_browser_observing_exact_anchor() {
        let root = temp_root("moondesk-managed-chat-anchor-routing");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        let command = broker
            .enqueue_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "worker:routed:task:first".into(),
                    launch: launch("anchor-routed"),
                },
                None,
                Some(anchor_context(conversation_id)),
            )
            .await
            .expect("enqueue routed command");

        let wrong =
            std::collections::BTreeSet::from(["7bbd8fc2-5c21-94ff-a8ce-e09c449975ef".to_string()]);
        assert!(
            broker
                .redeem_for_anchors("edge", 10, Some(&wrong))
                .await
                .expect("wrong browser redeem")
                .is_none()
        );

        let exact = std::collections::BTreeSet::from([conversation_id.to_string()]);
        let offer = broker
            .redeem_for_anchors("chrome", 20, Some(&exact))
            .await
            .expect("exact browser redeem")
            .expect("exact browser gets command");
        assert_eq!(offer.command.id, command.id);
        assert_eq!(offer.command.target_client_id.as_deref(), Some("chrome"));
        assert_eq!(
            offer
                .command
                .anchor_context
                .as_ref()
                .map(|context| context.conversation_id.as_str()),
            Some(conversation_id)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn successful_worker_thread_pins_reuse_to_same_browser() {
        let root = temp_root("moondesk-managed-chat-browser-affinity");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let conversation_id = "6aad7eb1-4b10-83ee-97bd-d98b338864de";
        let first = broker
            .enqueue_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "worker:affinity:task:first".into(),
                    launch: launch("first"),
                },
                Some("edge".into()),
                Some(anchor_context(conversation_id)),
            )
            .await
            .expect("enqueue first");

        let empty = std::collections::BTreeSet::new();
        let first_offer = broker
            .redeem_for_anchors("edge", 100, Some(&empty))
            .await
            .expect("redeem first")
            .expect("first offer");
        let first_lease = first_offer
            .command
            .lease
            .as_ref()
            .expect("first lease")
            .lease_id
            .clone();
        broker
            .mark_send_started(&first.id, &first_lease, "edge")
            .await
            .expect("cross send boundary for first worker thread");
        broker
            .acknowledge(
                &first.id,
                &first_lease,
                "edge",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                    conversation_url: Some(
                        "https://chatgpt.com/c/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    ),
                },
            )
            .await
            .expect("ack first");

        let mut reuse_launch = launch("reuse");
        reuse_launch.open_mode = ManagedChatOpenMode::ExistingThread;
        let reuse = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:affinity:task:reuse".into(),
                launch: reuse_launch,
            })
            .await
            .expect("enqueue reuse");
        assert_eq!(reuse.target_client_id.as_deref(), Some("edge"));
        assert_eq!(
            reuse
                .anchor_context
                .as_ref()
                .map(|context| context.conversation_id.as_str()),
            Some(conversation_id)
        );

        let same_anchor_in_chrome = std::collections::BTreeSet::from([conversation_id.to_string()]);
        assert!(
            broker
                .redeem_for_anchors("chrome", 200, Some(&same_anchor_in_chrome))
                .await
                .expect("wrong client")
                .is_none()
        );

        let reuse_offer = broker
            .redeem_for_anchors("edge", 200, Some(&empty))
            .await
            .expect("pinned client redeem")
            .expect("pinned reuse");
        assert_eq!(reuse_offer.command.id, reuse.id);
        assert_eq!(
            reuse_offer.command.target_client_id.as_deref(),
            Some("edge")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn revoked_browser_retargets_only_safe_queued_work_and_pauses_leased_work() {
        let root = temp_root("moondesk-managed-chat-revoked-browser");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");

        let leased = broker
            .enqueue_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "worker:revoked:task:leased".into(),
                    launch: launch("revoked-leased"),
                },
                Some("old-browser".into()),
                None,
            )
            .await
            .expect("enqueue leased command");
        let offer = broker
            .redeem("old-browser", 100)
            .await
            .expect("redeem old browser command")
            .expect("leased command");
        assert_eq!(offer.command.id, leased.id);
        let leased_lease = offer
            .command
            .lease
            .as_ref()
            .expect("leased command lease")
            .lease_id
            .clone();
        broker
            .mark_send_started(&leased.id, &leased_lease, "old-browser")
            .await
            .expect("cross send boundary before browser revocation");

        let queued = broker
            .enqueue_with_route(
                EnqueueManagedChatRequest {
                    dedupe_key: "worker:revoked:task:queued".into(),
                    launch: launch("revoked-queued"),
                },
                Some("old-browser".into()),
                None,
            )
            .await
            .expect("enqueue queued command");

        let (retargeted, paused, changed) = broker
            .settle_revoked_client("old-browser")
            .await
            .expect("settle revoked browser");
        assert_eq!((retargeted, paused), (1, 1));
        assert_eq!(changed.len(), 2);

        let snapshot = broker.snapshot().await;
        let leased = snapshot
            .commands
            .get(&leased.id)
            .expect("leased command remains");
        assert_eq!(leased.state, ManagedChatCommandState::Paused);
        assert!(leased.reconcile_history);
        assert!(leased.lease.is_none());
        assert_eq!(
            leased
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.details.as_deref()),
            Some("browser_revoked_before_reconciliation")
        );

        let queued = snapshot
            .commands
            .get(&queued.id)
            .expect("queued command remains");
        assert_eq!(queued.state, ManagedChatCommandState::Queued);
        assert_eq!(queued.target_client_id, None);
        assert!(!queued.reconcile_history);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn worker_success_requires_a_canonical_conversation_url() {
        let root = temp_root("moondesk-managed-chat-success-url");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:success-url:task:one".into(),
                launch: launch("success-url"),
            })
            .await
            .expect("enqueue command");
        let offer = broker
            .redeem("extension-a", 10)
            .await
            .expect("redeem command")
            .expect("lease command");
        let lease_id = offer
            .command
            .lease
            .as_ref()
            .expect("lease")
            .lease_id
            .clone();
        broker
            .mark_send_started(&command.id, &lease_id, "extension-a")
            .await
            .expect("cross Send boundary");

        for invalid in [
            None,
            Some("https://chatgpt.com/".to_string()),
            Some(
                "https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee?temporary=1"
                    .to_string(),
            ),
            Some("https://chatgpt.com/foo/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string()),
        ] {
            let error = broker
                .acknowledge(
                    &command.id,
                    &lease_id,
                    "extension-a",
                    ManagedChatAckOutcome::Succeeded {
                        details: Some("sent".into()),
                        conversation_url: invalid,
                    },
                )
                .await
                .expect_err("non-canonical worker success must be rejected");
            assert!(matches!(error, ManagedChatError::Invalid(_)));
        }

        let project = "g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk";
        let conversation = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let canonical = format!("https://chatgpt.com/g/{project}/c/{conversation}");
        let succeeded = broker
            .acknowledge(
                &command.id,
                &lease_id,
                "extension-a",
                ManagedChatAckOutcome::Succeeded {
                    details: Some("sent".into()),
                    conversation_url: Some(canonical.clone()),
                },
            )
            .await
            .expect("canonical Project conversation succeeds");
        assert_eq!(succeeded.state, ManagedChatCommandState::Succeeded);
        assert_eq!(
            succeeded
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.conversation_url.as_deref()),
            Some(canonical.as_str())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_and_paused_terminal_acks_are_not_interchangeable() {
        let root = temp_root("moondesk-managed-chat-terminal-exact");
        let broker = ManagedChatBroker::open(root.join("state.json")).expect("open broker");
        let failed_command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:terminal:task:failed".into(),
                launch: launch("terminal-failed"),
            })
            .await
            .expect("enqueue failed command");
        let failed_offer = broker
            .redeem("extension-a", 10)
            .await
            .expect("redeem failed command")
            .expect("lease failed command");
        let failed_lease = failed_offer
            .command
            .lease
            .as_ref()
            .expect("failed lease")
            .lease_id
            .clone();
        broker
            .acknowledge(
                &failed_command.id,
                &failed_lease,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("same-details".into()),
                },
            )
            .await
            .expect("terminal failure");
        let paused_as_failed = broker
            .acknowledge(
                &failed_command.id,
                &failed_lease,
                "extension-a",
                ManagedChatAckOutcome::Paused {
                    details: Some("same-details".into()),
                },
            )
            .await
            .expect_err("Paused must not replay as Failed");
        assert!(matches!(paused_as_failed, ManagedChatError::Conflict(_)));

        let paused_command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "worker:terminal:task:paused".into(),
                launch: launch("terminal-paused"),
            })
            .await
            .expect("enqueue paused command");
        let paused_offer = broker
            .redeem("extension-a", 20)
            .await
            .expect("redeem paused command")
            .expect("lease paused command");
        let paused_lease = paused_offer
            .command
            .lease
            .as_ref()
            .expect("paused lease")
            .lease_id
            .clone();
        broker
            .mark_send_started(&paused_command.id, &paused_lease, "extension-a")
            .await
            .expect("cross Send boundary for pause");
        broker
            .acknowledge(
                &paused_command.id,
                &paused_lease,
                "extension-a",
                ManagedChatAckOutcome::Paused {
                    details: Some("same-details".into()),
                },
            )
            .await
            .expect("terminal pause");
        let failed_as_paused = broker
            .acknowledge(
                &paused_command.id,
                &paused_lease,
                "extension-a",
                ManagedChatAckOutcome::Failed {
                    details: Some("same-details".into()),
                },
            )
            .await
            .expect_err("Failed must not replay as Paused");
        assert!(matches!(failed_as_paused, ManagedChatError::Conflict(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn enqueue_compacts_old_terminal_history_before_global_capacity_is_exhausted() {
        let root = temp_root("moondesk-managed-chat-terminal-compaction");
        std::fs::create_dir_all(&root).expect("create managed chat compaction root");
        let path = root.join("state.json");
        let mut data = ManagedChatStoreData::default();
        for sequence in 0..MAX_MANAGED_CHAT_COMMANDS as u64 {
            let command_id = ManagedChatCommandId::new();
            let dedupe_key = format!("terminal-history-{sequence}");
            let mut historical_launch = launch(&format!("terminal-{sequence}"));
            historical_launch.thread_key = None;
            let command = ManagedChatCommand {
                id: command_id.clone(),
                sequence,
                dedupe_key: dedupe_key.clone(),
                request_fingerprint: "a".repeat(64),
                launch: historical_launch,
                state: ManagedChatCommandState::Failed,
                dispatch_ready: true,
                reconcile_history: false,
                lease: None,
                terminal: Some(ManagedChatTerminalResult {
                    succeeded: false,
                    details: Some("historical".into()),
                    conversation_url: None,
                }),
                target_client_id: None,
                target_client_pinned_by_request: false,
                anchor_context: None,
            };
            data.dedupe.insert(dedupe_key, command_id.clone());
            data.commands.insert(command_id, command);
        }
        data.next_sequence = MAX_MANAGED_CHAT_COMMANDS as u64;
        store::save(&path, &data).expect("seed full terminal store");

        let broker = ManagedChatBroker::open(&path).expect("open full terminal store");
        let command = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "fresh-after-compaction".into(),
                launch: launch("fresh-after-compaction"),
            })
            .await
            .expect("terminal history must be compacted before capacity rejection");
        let snapshot = broker.snapshot().await;
        assert!(snapshot.commands.contains_key(&command.id));
        assert!(snapshot.commands.len() <= MAX_MANAGED_CHAT_TERMINAL_HISTORY + 1);
        assert_eq!(snapshot.commands.len(), snapshot.dedupe.len());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_compaction_bounds_per_thread_affinity_history() {
        let root = temp_root("moondesk-managed-chat-thread-affinity-compaction");
        std::fs::create_dir_all(&root).expect("create thread affinity compaction root");
        let path = root.join("state.json");
        let mut data = ManagedChatStoreData::default();
        for sequence in 0..MAX_MANAGED_CHAT_COMMANDS as u64 {
            let command_id = ManagedChatCommandId::new();
            let dedupe_key = format!("thread-affinity-history-{sequence}");
            let mut historical_launch = launch(&format!("thread-affinity-{sequence}"));
            historical_launch.thread_key = Some(format!("worker:thread-{sequence}"));
            let conversation_url = format!("https://chatgpt.com/c/{command_id}");
            let command = ManagedChatCommand {
                id: command_id.clone(),
                sequence,
                dedupe_key: dedupe_key.clone(),
                request_fingerprint: "a".repeat(64),
                launch: historical_launch,
                state: ManagedChatCommandState::Succeeded,
                dispatch_ready: true,
                reconcile_history: true,
                lease: None,
                terminal: Some(ManagedChatTerminalResult {
                    succeeded: true,
                    details: Some("historical".into()),
                    conversation_url: Some(conversation_url),
                }),
                target_client_id: Some(format!("browser-{sequence}")),
                target_client_pinned_by_request: true,
                anchor_context: None,
            };
            data.dedupe.insert(dedupe_key, command_id.clone());
            data.commands.insert(command_id, command);
        }
        data.next_sequence = MAX_MANAGED_CHAT_COMMANDS as u64;
        store::save(&path, &data).expect("seed full per-thread terminal store");

        let broker = ManagedChatBroker::open(&path).expect("open per-thread terminal store");
        broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "fresh-after-thread-affinity-compaction".into(),
                launch: launch("fresh-after-thread-affinity-compaction"),
            })
            .await
            .expect("old per-thread affinity records must compact before capacity rejection");
        let snapshot = broker.snapshot().await;
        assert!(
            snapshot.commands.len()
                <= MAX_MANAGED_CHAT_THREAD_AFFINITIES + MAX_MANAGED_CHAT_TERMINAL_HISTORY + 1
        );
        assert_eq!(snapshot.commands.len(), snapshot.dedupe.len());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn maximum_large_command_count_fits_managed_chat_store_budget() {
        let root = temp_root("moondesk-managed-chat-large-capacity");
        std::fs::create_dir_all(&root).expect("create managed chat large-capacity root");
        let path = root.join("state.json");
        let large_opening = "x".repeat(crate::managed_chat::MAX_MANAGED_CHAT_OPENING_MESSAGE_BYTES);
        let mut data = ManagedChatStoreData::default();

        for sequence in 0..MAX_MANAGED_CHAT_COMMANDS as u64 {
            let command_id = ManagedChatCommandId::new();
            let dedupe_key = format!("large-capacity-{sequence}");
            let mut large_launch = launch(&format!("large-capacity-{sequence}"));
            large_launch.opening_message = large_opening.clone();
            large_launch.thread_key = Some(format!("worker:large-capacity-{sequence}"));
            let command = ManagedChatCommand {
                id: command_id.clone(),
                sequence,
                dedupe_key: dedupe_key.clone(),
                request_fingerprint: "a".repeat(64),
                launch: large_launch,
                state: ManagedChatCommandState::Queued,
                dispatch_ready: true,
                reconcile_history: false,
                lease: None,
                terminal: None,
                target_client_id: None,
                target_client_pinned_by_request: false,
                anchor_context: None,
            };
            data.dedupe.insert(dedupe_key, command_id.clone());
            data.commands.insert(command_id, command);
        }
        data.next_sequence = MAX_MANAGED_CHAT_COMMANDS as u64;
        data.validate()
            .expect("maximum configured command set must remain serializable");
        let serialized = serde_json::to_vec_pretty(&data).expect("serialize maximum command set");
        assert!(serialized.len() as u64 <= crate::managed_chat::MAX_MANAGED_CHAT_STORE_BYTES);
        store::save(&path, &data).expect("persist maximum configured command set");

        let broker = ManagedChatBroker::open(&path).expect("reopen maximum command set");
        let overflow = broker
            .enqueue(EnqueueManagedChatRequest {
                dedupe_key: "large-capacity-overflow".into(),
                launch: launch("large-capacity-overflow"),
            })
            .await
            .expect_err("configured command count must reject overflow before persistence");
        assert!(matches!(overflow, ManagedChatError::Limit(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistence_failure_does_not_publish_command() {
        let root = temp_root("moondesk-managed-chat-persist-failure");
        let state_parent = root.join("state-parent");
        std::fs::create_dir_all(&state_parent).expect("create valid state parent");
        let broker = ManagedChatBroker::open(state_parent.join("state.json"))
            .expect("open broker before state exists");

        broker.fail_next_commit_for_test();
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
