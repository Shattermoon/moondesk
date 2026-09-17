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
        workspaceId: record.workspaceId
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
    const { commandId, launchToken, binding, launch } = message;
    if (!commandId || !launchToken || !binding?.projectId || !launch?.taskMarker || !launch?.openingMessage || !launch?.executionProfile) {
      return { state: 'failed', reason: 'invalid_launch_payload' };
    }
    rememberLaunch({
      commandId,
      launchToken,
      taskMarker: launch.taskMarker,
      workspaceId: launch.workspaceId
    });

    if (DOM.projectIdFromPath() !== binding.projectId) {
      return { state: 'failed', reason: 'wrong_chatgpt_project' };
    }
    if (!(await DOM.enterProject(binding.projectId))) {
      return { state: 'failed', reason: 'project_entry_unconfirmed' };
    }
    if (!DOM.composerReady()) {
      return { state: 'failed', reason: 'project_composer_not_ready' };
    }

    if (!(await DOM.selectModelSettings(launch.executionProfile))) {
      return { state: 'failed', reason: 'model_or_effort_unconfirmed' };
    }
    if (DOM.taskMarkerPresent(launch.taskMarker)) {
      return {
        state: 'succeeded',
        reason: 'task_marker_already_present',
        conversationUrl: location.href,
        selection: await DOM.selectedModelAndEffort(launch.executionProfile)
      };
    }
    if (!DOM.insertPrompt(launch.openingMessage)) {
      return { state: 'failed', reason: 'prompt_insert_unconfirmed' };
    }
    const sent = await DOM.sendOnce(launch.taskMarker);
    if (sent.state === 'succeeded') {
      const selection = await DOM.selectedModelAndEffort(launch.executionProfile);
      if (!selection) {
        return { state: 'needs_reconcile', reason: 'post_send_model_readback_unconfirmed', conversationUrl: sent.conversationUrl };
      }
      return { ...sent, selection };
    }
    return sent;
  }

  async function reconcileWorker(message) {
    const { launch } = message;
    if (!launch?.taskMarker) return { state: 'failed', reason: 'invalid_reconcile_payload' };
    if (DOM.taskMarkerPresent(launch.taskMarker)) {
      return {
        state: 'succeeded',
        reason: 'task_marker_confirmed',
        conversationUrl: location.href,
        selection: await DOM.selectedModelAndEffort(launch.executionProfile)
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
    if (message.type === 'MOONDESK_MODEL_CATALOG') {
      void DOM.inspectModelSettings()
        .then((catalog) => sendResponse({ ok: Boolean(catalog), catalog }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_PREPARE_WORKER') {
      void prepareWorker(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_RECONCILE_WORKER') {
      void reconcileWorker(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    return false;
  });
})();
