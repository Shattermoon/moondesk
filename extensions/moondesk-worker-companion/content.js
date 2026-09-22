(() => {
  if (window.__MOONDESK_WORKER_COMPANION_CONTENT__) return;
  window.__MOONDESK_WORKER_COMPANION_CONTENT__ = true;
  const DOM = window.MOONDESK_CHATGPT_DOM;
  if (!DOM) return;

  const LAUNCH_SESSION_KEY = 'moondesk-worker-launch-v1';

  function rememberLaunch(record) {
    try {
      sessionStorage.setItem(LAUNCH_SESSION_KEY, JSON.stringify({
        commandId: record.commandId,
        launchToken: record.launchToken,
        taskMarker: record.taskMarker,
        workspaceId: record.workspaceId,
        threadKey: record.threadKey || null
      }));
    } catch {}
    if (location.hash.startsWith('#moondesk-launch=')) {
      try {
        history.replaceState(history.state, '', `${location.pathname}${location.search}`);
      } catch {}
    }
  }

  function rememberedLaunch() {
    try {
      const raw = sessionStorage.getItem(LAUNCH_SESSION_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch {
      return null;
    }
  }

  async function prepareWorker(message) {
    const { commandId, launchToken, placement, launch } = message;
    if (!commandId || !launchToken || !placement || !launch?.taskMarker || !launch?.openingMessage || !launch?.executionProfile) {
      return { state: 'failed', reason: 'invalid_launch_payload' };
    }
    rememberLaunch({
      commandId,
      launchToken,
      taskMarker: launch.taskMarker,
      workspaceId: launch.workspaceId,
      threadKey: launch.threadKey || null
    });

    const expectedProjectId = placement.projectId || null;
    const existingThread = launch.openMode === 'existing_thread';
    if (existingThread) {
      if (!DOM.conversationIdFromPath()) {
        return { state: 'failed', reason: 'existing_worker_conversation_unconfirmed' };
      }
      if ((DOM.projectIdFromPath() || null) !== expectedProjectId) {
        return { state: 'failed', reason: 'worker_placement_mismatch' };
      }
      if (!DOM.composerReady()) {
        return { state: 'failed', reason: 'worker_conversation_not_ready' };
      }
    } else if (expectedProjectId) {
      if (DOM.projectIdFromPath() !== expectedProjectId) {
        return { state: 'failed', reason: 'wrong_chatgpt_project' };
      }
      if (!(await DOM.enterProject(expectedProjectId))) {
        return { state: 'failed', reason: 'project_entry_unconfirmed' };
      }
      if (!DOM.composerReady()) {
        return { state: 'failed', reason: 'project_composer_not_ready' };
      }
    } else {
      if (DOM.projectIdFromPath() !== null || DOM.conversationIdFromPath() !== null) {
        return { state: 'failed', reason: 'normal_chat_entry_unconfirmed' };
      }
      if (!DOM.composerReady()) {
        return { state: 'failed', reason: 'normal_chat_composer_not_ready' };
      }
    }

    if (!(await DOM.selectModelSettings(launch.executionProfile))) {
      return { state: 'failed', reason: 'model_or_effort_unconfirmed' };
    }
    const selection = await DOM.selectedModelAndEffort(launch.executionProfile);
    if (!selection) {
      return { state: 'failed', reason: 'model_or_effort_readback_unconfirmed' };
    }
    if (DOM.taskMarkerPresent(launch.taskMarker)) {
      const evidence = DOM.workerEvidence(launch.taskMarker);
      return {
        state: 'already_sent',
        reason: 'task_marker_already_present',
        conversationUrl: location.href,
        selection,
        evidence
      };
    }
    if (!DOM.insertPrompt(launch.openingMessage)) {
      return { state: 'failed', reason: 'prompt_insert_unconfirmed' };
    }
    return {
      state: 'ready',
      reason: 'worker_prompt_prepared',
      selection,
      evidence: DOM.workerEvidence(launch.taskMarker)
    };
  }

  async function commitWorkerSend(message) {
    const { commandId, launchToken, launch } = message;
    if (!commandId || !launchToken || !launch?.taskMarker || !launch?.openingMessage) {
      return { state: 'failed', reason: 'invalid_commit_payload' };
    }
    const remembered = rememberedLaunch();
    if (
      !remembered ||
      remembered.commandId !== commandId ||
      remembered.launchToken !== launchToken ||
      remembered.taskMarker !== launch.taskMarker
    ) {
      return { state: 'failed', reason: 'prepared_launch_identity_mismatch' };
    }
    return DOM.commitSendOnce(launch.openingMessage, launch.taskMarker);
  }

  async function reconcileWorker(message) {
    const { launch } = message;
    if (!launch?.taskMarker) return { state: 'failed', reason: 'invalid_reconcile_payload' };
    if (DOM.taskMarkerPresent(launch.taskMarker)) {
      return {
        state: 'observed',
        reason: 'task_marker_present_execution_unconfirmed',
        conversationUrl: location.href,
        selection: await DOM.selectedModelAndEffort(launch.executionProfile),
        evidence: DOM.workerEvidence(launch.taskMarker)
      };
    }
    // Never click Send from reconciliation. If the first click may have crossed the
    // browser/provider boundary, absence of immediate evidence is ambiguity, not authority
    // to submit a second time.
    return { state: 'needs_reconcile', reason: 'task_marker_still_unconfirmed', conversationUrl: location.href };
  }

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (!message || typeof message.type !== 'string') return false;
    if (message.type === 'MOONDESK_CONTEXT') {
      sendResponse({ ok: true, context: DOM.context(), rememberedLaunch: rememberedLaunch() });
      return false;
    }
    if (message.type === 'MOONDESK_CORRELATION_EVIDENCE') {
      void DOM.correlationEvidence()
        .then((evidence) => sendResponse({ ok: Boolean(evidence), evidence, context: DOM.context() }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_MODEL_CATALOG') {
      void DOM.inspectModelSettings()
        .then((catalog) => sendResponse({ ok: Boolean(catalog), catalog }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_PREPARE_WORKER') {
      void prepareWorker(message)
        .then((result) => sendResponse({ ok: true, result, rememberedLaunch: rememberedLaunch() }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
      void commitWorkerSend(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_WORKER_EVIDENCE') {
      const marker = message?.taskMarker;
      if (!marker) {
        sendResponse({ ok: false, error: 'task marker is required' });
      } else {
        sendResponse({ ok: true, evidence: DOM.workerEvidence(marker), rememberedLaunch: rememberedLaunch() });
      }
      return false;
    }
    if (message.type === 'MOONDESK_RECONCILE_WORKER') {
      void reconcileWorker(message)
        .then((result) => sendResponse({ ok: true, result, rememberedLaunch: rememberedLaunch() }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    return false;
  });
})();
