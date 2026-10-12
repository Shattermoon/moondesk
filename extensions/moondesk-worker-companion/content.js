(() => {
  if (window.__MOONDESK_WORKER_COMPANION_CONTENT__) return;
  window.__MOONDESK_WORKER_COMPANION_CONTENT__ = true;
  const DOM = window.MOONDESK_CHATGPT_DOM;
  if (!DOM) return;

  const LAUNCH_SESSION_KEY = 'moondesk-worker-launch-v1';
  const PROVIDER_CORRELATION_OBSERVED = 'moondesk-provider-correlation-observed';
  const PROVIDER_CORRELATION_ASK = 'moondesk-provider-correlation-ask';
  const PROVIDER_CORRELATION_REPLY = 'moondesk-provider-correlation-reply';
  const PROVIDER_CORRELATION_TTL_MS = 15000;
  const pendingProviderCorrelations = new Map();
  let providerCorrelationTimer = null;
  let workerLaunchResetGeneration = 0;

  function validProviderCorrelation(value) {
    if (!value || typeof value !== 'object') return null;
    const conversationId = typeof value.conversationId === 'string'
      ? value.conversationId.trim().toLowerCase()
      : '';
    if (!/^[0-9a-f-]{16,64}$/i.test(conversationId)) return null;
    const requestIds = [...new Set((Array.isArray(value.requestIds) ? value.requestIds : []).filter(
      (requestId) => typeof requestId === 'string' && /^[a-z0-9_-]{1,100}$/i.test(requestId)
    ))].slice(0, 16);
    const operationIds = [...new Set((Array.isArray(value.operationIds) ? value.operationIds : []).filter(
      (operationId) => typeof operationId === 'string' &&
        /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(operationId)
    ).map((operationId) => operationId.toLowerCase()))].slice(0, 8);
    return requestIds.length || operationIds.length
      ? {
          conversationId,
          ...(requestIds.length ? { requestIds } : {}),
          ...(operationIds.length ? { operationIds } : {})
        }
      : null;
  }

  async function publishProviderCorrelation(correlation) {
    const currentConversation = DOM.conversationIdFromPath()?.toLowerCase() || null;
    if (currentConversation !== correlation.conversationId) return false;
    for (const delayMs of [0, 150, 600]) {
      if (delayMs) await new Promise((resolve) => setTimeout(resolve, delayMs));
      try {
        const response = await chrome.runtime.sendMessage({
          type: 'MOONDESK_PROVIDER_CORRELATION',
          correlation
        });
        if (response?.ok) return true;
      } catch {}
    }
    return false;
  }

  function scheduleProviderCorrelationFlush() {
    if (providerCorrelationTimer || !pendingProviderCorrelations.size) return;
    providerCorrelationTimer = setInterval(() => {
      const now = Date.now();
      const currentConversation = DOM.conversationIdFromPath()?.toLowerCase() || null;
      for (const [conversationId, pending] of pendingProviderCorrelations) {
        if (now >= pending.expiresAt) {
          pendingProviderCorrelations.delete(conversationId);
          continue;
        }
        if (currentConversation === conversationId) {
          pendingProviderCorrelations.delete(conversationId);
          void publishProviderCorrelation(pending.correlation);
        } else if (currentConversation) {
          pendingProviderCorrelations.delete(conversationId);
        }
      }
      if (!pendingProviderCorrelations.size) {
        clearInterval(providerCorrelationTimer);
        providerCorrelationTimer = null;
      }
    }, 100);
  }

  function acceptProviderCorrelation(value) {
    const correlation = validProviderCorrelation(value);
    if (!correlation) return;
    const currentConversation = DOM.conversationIdFromPath()?.toLowerCase() || null;
    if (currentConversation === correlation.conversationId) {
      void publishProviderCorrelation(correlation);
      return;
    }
    if (currentConversation) return;
    pendingProviderCorrelations.set(correlation.conversationId, {
      correlation,
      expiresAt: Date.now() + PROVIDER_CORRELATION_TTL_MS
    });
    scheduleProviderCorrelationFlush();
  }

  if (typeof window.addEventListener === 'function') {
    window.addEventListener('message', (event) => {
      if (event.source !== window || event.origin !== location.origin) return;
      const data = event.data;
      if (!data || data.v !== 1) return;
      if (data.source === PROVIDER_CORRELATION_OBSERVED) {
        acceptProviderCorrelation(data.correlation);
      } else if (data.source === PROVIDER_CORRELATION_REPLY && Array.isArray(data.correlations)) {
        for (const correlation of data.correlations) acceptProviderCorrelation(correlation);
      }
    });
  }

  try {
    window.postMessage?.({
      source: PROVIDER_CORRELATION_ASK,
      nonce: crypto.randomUUID(),
      v: 1
    }, location.origin);
  } catch {}

  function rememberLaunch(record) {
    try {
      sessionStorage.setItem(LAUNCH_SESSION_KEY, JSON.stringify({
        commandId: record.commandId,
        launchToken: record.launchToken,
        taskMarker: record.taskMarker,
        workspaceId: record.workspaceId,
        threadKey: record.threadKey || null,
        openMode: record.openMode || null,
        targetConversationId: record.targetConversationId || null,
        projectId: record.projectId || null
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

  function exactPendingProviderConversationId() {
    const now = Date.now();
    for (const [conversationId, pending] of pendingProviderCorrelations) {
      if (now >= pending.expiresAt) pendingProviderCorrelations.delete(conversationId);
    }
    const ids = [...pendingProviderCorrelations.keys()];
    return ids.length === 1 ? ids[0] : null;
  }

  function workerEvidence(marker) {
    const evidence = DOM.workerEvidence(marker);
    if (evidence?.conversationId || evidence?.markerPresent !== true) return evidence;
    const remembered = rememberedLaunch();
    if (remembered?.openMode !== 'new_thread') return evidence;
    const providerConversationId = exactPendingProviderConversationId();
    if (!providerConversationId) return evidence;
    rememberLaunch({ ...remembered, targetConversationId: providerConversationId });
    return {
      ...evidence,
      conversationId: providerConversationId,
      conversationUrl: `https://chatgpt.com/c/${providerConversationId}`
    };
  }

  async function prepareWorker(message) {
    const { commandId, launchToken, placement, launch } = message;
    if (!commandId || !launchToken || !placement || !launch?.taskMarker || !launch?.openingMessage || !launch?.executionProfile) {
      return { state: 'failed', reason: 'invalid_launch_payload' };
    }
    const existingThread = launch.openMode === 'existing_thread';
    const workerConversation = existingThread ? DOM.conversationIdFromPath() : null;
    const targetProjectId = existingThread ? (DOM.projectIdFromPath() || null) : null;
    rememberLaunch({
      commandId,
      launchToken,
      taskMarker: launch.taskMarker,
      workspaceId: launch.workspaceId,
      threadKey: launch.threadKey || null,
      openMode: launch.openMode || 'new_thread',
      projectId: targetProjectId
    });
    const launchAlive = () => {
      const current = rememberedLaunch();
      return Boolean(
        current &&
        current.commandId === commandId &&
        current.launchToken === launchToken &&
        current.taskMarker === launch.taskMarker
      );
    };
    let stillOnTarget;
    if (existingThread) {
      if (!workerConversation) {
        return { state: 'failed', reason: 'existing_worker_conversation_unconfirmed' };
      }
      stillOnTarget = () =>
        launchAlive() &&
        DOM.conversationIdFromPath() === workerConversation &&
        (DOM.projectIdFromPath() || null) === targetProjectId;
      if (!(await DOM.waitForComposerReady(15000, stillOnTarget))) {
        return { state: 'failed', reason: stillOnTarget() ? 'worker_conversation_not_ready' : 'worker_launch_target_changed' };
      }
    } else {
      // Fresh Workers V1 chats are always ordinary ChatGPT chats. Core Project metadata is only
      // routing context; a Project route or any existing conversation here is the wrong target.
      stillOnTarget = () =>
        launchAlive() &&
        DOM.projectIdFromPath() === null &&
        DOM.conversationIdFromPath() === null;
      if (!stillOnTarget()) {
        return { state: 'failed', reason: 'normal_chat_entry_unconfirmed' };
      }
      if (!(await DOM.waitForComposerReady(15000, stillOnTarget))) {
        return { state: 'failed', reason: stillOnTarget() ? 'normal_chat_composer_not_ready' : 'worker_launch_target_changed' };
      }
    }

    if (!stillOnTarget()) {
      return { state: 'failed', reason: 'worker_launch_target_changed' };
    }
    rememberLaunch({
      commandId,
      launchToken,
      taskMarker: launch.taskMarker,
      workspaceId: launch.workspaceId,
      threadKey: launch.threadKey || null,
      openMode: launch.openMode || 'new_thread',
      targetConversationId: DOM.conversationIdFromPath() || null,
      projectId: targetProjectId
    });

    let modelFailure = 'selection_unconfirmed';
    const modelSelected = await DOM.selectModelSettings(launch.executionProfile, (code) => {
      if (typeof code === 'string' && /^[a-z_]{1,64}$/.test(code)) modelFailure = code;
    }, stillOnTarget);
    if (!modelSelected) {
      return { state: 'failed', reason: `model_or_effort_unconfirmed:${modelFailure}` };
    }
    // Chat/Work/model transitions can replace or temporarily disable the composer after the
    // picker has already confirmed the selection. Reacquire a writable host under the same exact
    // launch/route fence instead of trusting the pre-picker editor instance.
    if (!(await DOM.waitForComposerReady(15000, stillOnTarget))) {
      return { state: 'failed', reason: stillOnTarget() ? 'composer_after_model_not_ready' : 'worker_launch_target_changed' };
    }
    if (!stillOnTarget()) {
      return { state: 'failed', reason: 'worker_launch_target_changed' };
    }
    // selectModelSettings already requires an account-evaluated exact choice and confirmed picker
    // closure. Do not make Send depend on a second independent picker traversal. When the closed
    // native trigger exposes the route-stamped selection, retain it as diagnostic evidence only.
    const selection = DOM.visibleModelSelection?.(launch.executionProfile) || null;
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
    if (!DOM.preparedPromptMatches?.(launch.openingMessage) && !DOM.insertPrompt(launch.openingMessage)) {
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
    if (!commandId || !launchToken || !launch?.taskMarker || !launch?.openingMessage || !launch?.executionProfile) {
      return { state: 'failed', reason: 'invalid_commit_payload' };
    }
    const resetGeneration = workerLaunchResetGeneration;
    const remembered = rememberedLaunch();
    if (
      !remembered ||
      remembered.commandId !== commandId ||
      remembered.launchToken !== launchToken ||
      remembered.taskMarker !== launch.taskMarker
    ) {
      return { state: 'failed', reason: 'prepared_launch_identity_mismatch' };
    }
    const stillOnPreparedTarget = () =>
      workerLaunchResetGeneration === resetGeneration &&
      DOM.conversationIdFromPath() === (remembered.targetConversationId || null) &&
      (DOM.projectIdFromPath() || null) === (remembered.projectId || null);
    if (!stillOnPreparedTarget()) {
      return { state: 'failed', reason: 'prepared_launch_target_changed' };
    }
    const selection = await DOM.selectedModelAndEffort(launch.executionProfile);
    if (!selection || !stillOnPreparedTarget()) {
      return {
        state: 'failed',
        reason: stillOnPreparedTarget() ? 'model_or_effort_unconfirmed_before_send' : 'prepared_launch_target_changed'
      };
    }
    return DOM.commitSendOnce(launch.openingMessage, launch.taskMarker, stillOnPreparedTarget);
  }

  async function reconcileWorker(message) {
    const { launch } = message;
    if (!launch?.taskMarker) return { state: 'failed', reason: 'invalid_reconcile_payload' };
    if (DOM.taskMarkerPresent(launch.taskMarker)) {
      const evidence = workerEvidence(launch.taskMarker);
      return {
        state: 'observed',
        reason: 'task_marker_present_execution_unconfirmed',
        conversationUrl: evidence?.conversationUrl || location.href,
        selection: await DOM.selectedModelAndEffort(launch.executionProfile),
        evidence
      };
    }
    // Never click Send from reconciliation. If the first click may have crossed the
    // browser/provider boundary, absence of immediate evidence is ambiguity, not authority
    // to submit a second time.
    return { state: 'needs_reconcile', reason: 'task_marker_still_unconfirmed', conversationUrl: location.href };
  }

  async function inspectModelCatalogRequest(message) {
    const nonce = typeof message?.nonce === 'string' ? message.nonce : '';
    const expiresAt = Number(message?.expiresAt);
    let helperNonce = '';
    try {
      helperNonce = new URL(location.href).searchParams.get('moondesk-model-catalog') || '';
    } catch {}
    if (!/^[a-f0-9-]{36}$/i.test(nonce) || helperNonce !== nonce || !Number.isFinite(expiresAt)) {
      return { ok: false, error: 'catalog_helper_identity_unconfirmed' };
    }
    const stillCurrent = () =>
      Date.now() < expiresAt &&
      location.origin === 'https://chatgpt.com' &&
      location.pathname === '/' &&
      !DOM.conversationIdFromPath();
    if (!stillCurrent()) return { ok: false, error: 'catalog_helper_changed' };
    const readyMs = Math.max(0, Math.min(15000, expiresAt - Date.now()));
    if (!readyMs || !(await DOM.waitForComposerReady(readyMs, stillCurrent))) {
      return { ok: false, error: 'catalog_composer_not_ready' };
    }
    let failure = null;
    const catalog = await DOM.inspectModelSettings(stillCurrent, (reason) => { failure ??= reason; });
    if (!catalog || !stillCurrent()) {
      return { ok: false, error: failure || (Date.now() >= expiresAt ? 'catalog_deadline' : 'catalog_unconfirmed') };
    }
    return { ok: true, catalog };
  }

  function recoveryEvidence(marker) {
    const evidence = workerEvidence(marker);
    return {
      ...evidence,
      composerReady: DOM.composerReady?.() === true
    };
  }

  async function inspectWorkerRecovery(message) {
    const marker = message?.taskMarker;
    if (typeof marker !== 'string' || !marker) {
      return { state: 'failed', reason: 'worker_recovery_task_marker_invalid' };
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
    const evidence = recoveryEvidence(marker);
    if (!evidence.conversationId || evidence.markerPresent !== true) {
      return { state: 'failed', reason: 'worker_recovery_task_not_present', evidence };
    }
    return {
      state: evidence.generating ? 'busy' : 'idle',
      evidence
    };
  }

  async function continueWorkerRecovery(message) {
    const marker = message?.taskMarker;
    const prompt = message?.prompt;
    if (typeof marker !== 'string' || !marker || typeof prompt !== 'string' || !prompt.trim()) {
      return { state: 'failed', reason: 'worker_recovery_continue_invalid' };
    }
    const conversationId = DOM.conversationIdFromPath();
    const stillCurrent = () =>
      Boolean(conversationId) &&
      DOM.conversationIdFromPath() === conversationId &&
      DOM.taskMarkerPresent(marker);
    if (!stillCurrent()) {
      return { state: 'failed', reason: 'worker_recovery_target_changed' };
    }
    const before = recoveryEvidence(marker);
    const expected = message?.expected;
    if (
      expected &&
      (Number(expected.userTurnCount) !== before.userTurnCount ||
        Number(expected.assistantTurnCount) !== before.assistantTurnCount)
    ) {
      return { state: 'superseded', reason: 'worker_recovery_page_progressed', evidence: before };
    }

    let stopped = false;
    if (before.generating) {
      if (message?.stopIfBusy !== true) {
        return { state: 'busy', reason: 'worker_recovery_still_generating', evidence: before };
      }
      if (!(await DOM.stopGeneration(10000, stillCurrent))) {
        return { state: 'failed', reason: 'worker_recovery_stop_unconfirmed', evidence: recoveryEvidence(marker) };
      }
      stopped = true;
    }
    if (!stillCurrent()) {
      return { state: 'superseded', reason: 'worker_recovery_target_changed_after_stop' };
    }
    if (!(await DOM.waitForComposerReady(12000, stillCurrent))) {
      return { state: 'failed', reason: 'worker_recovery_composer_not_ready', evidence: recoveryEvidence(marker) };
    }
    const ready = recoveryEvidence(marker);
    if (!ready.composerEmpty) {
      return { state: 'superseded', reason: 'worker_recovery_existing_draft', evidence: ready };
    }
    if (!DOM.insertPrompt(prompt)) {
      return { state: 'failed', reason: 'worker_recovery_prompt_insert_unconfirmed', evidence: ready };
    }
    const committed = await DOM.commitSendOnce(prompt, marker, stillCurrent);
    if (committed?.state !== 'committed') {
      return {
        state: 'failed',
        reason: committed?.reason || 'worker_recovery_send_unconfirmed',
        evidence: recoveryEvidence(marker)
      };
    }
    // A click is only the irreversible boundary, not proof that ChatGPT accepted the continuation.
    // Require fresh page evidence before reporting success so an ignored/dropped click cannot make
    // Core believe a stale worker resumed. A timeout remains explicitly unconfirmed and must be
    // inspected before any retry.
    const acceptanceDeadline = Date.now() + 12000;
    while (Date.now() < acceptanceDeadline) {
      if (!stillCurrent()) {
        return { state: 'superseded', reason: 'worker_recovery_target_changed_after_send' };
      }
      const evidence = recoveryEvidence(marker);
      if (
        evidence.userTurnCount > ready.userTurnCount ||
        evidence.assistantTurnCount > ready.assistantTurnCount ||
        (evidence.generating && evidence.composerEmpty)
      ) {
        return { state: 'continued', stopped, evidence };
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    return {
      state: 'unconfirmed',
      stopped,
      reason: 'worker_recovery_send_acceptance_unconfirmed',
      evidence: recoveryEvidence(marker)
    };
  }

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (!message || typeof message.type !== 'string') return false;
    if (message.type === 'MOONDESK_CONTEXT') {
      sendResponse({ ok: true, context: DOM.context(), rememberedLaunch: rememberedLaunch() });
      return false;
    }
    if (message.type === 'MOONDESK_RECOVER_WORKER_INSPECT') {
      void inspectWorkerRecovery(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_RECOVER_WORKER_CONTINUE') {
      void continueWorkerRecovery(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_CORRELATION_EVIDENCE') {
      void DOM.correlationEvidence()
        .then((evidence) => sendResponse({ ok: Boolean(evidence), evidence, context: DOM.context() }))
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_MODEL_CATALOG') {
      void inspectModelCatalogRequest(message)
        .then(sendResponse)
        .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
      return true;
    }
    if (message.type === 'MOONDESK_CLEAR_WORKER_LAUNCHES') {
      workerLaunchResetGeneration += 1;
      try { sessionStorage.removeItem(LAUNCH_SESSION_KEY); } catch {}
      pendingProviderCorrelations.clear();
      if (providerCorrelationTimer) clearInterval(providerCorrelationTimer);
      providerCorrelationTimer = null;
      sendResponse({ ok: true, resetGeneration: workerLaunchResetGeneration });
      return false;
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
        sendResponse({ ok: true, evidence: workerEvidence(marker), rememberedLaunch: rememberedLaunch() });
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
