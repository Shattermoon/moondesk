const STORAGE_KEY = 'moondeskWorkerCompanionV1';
const POLL_ALARM = 'moondesk-worker-companion-poll';
const BRIDGE_PORTS = [47650, 47651, 47652, 47653, 47654];
const REQUIRED_PROTOCOL_VERSION = 2;
const HELLO_PATH = '/__moondesk/companion/v1/hello';
const PAIR_PATH = '/__moondesk/companion/v1/pair';
const REPAIR_PATH = '/__moondesk/companion/v1/repair';
const STATUS_PATH = '/__moondesk/companion/v1/status';
const CLIENTS_PATH = '/__moondesk/companion/v1/clients';
const PRESENCE_PATH = '/__moondesk/companion/v1/presence';
const CORRELATIONS_PATH = '/__moondesk/companion/v1/correlations';
const SEND_STARTED_PATH = '/__moondesk/companion/v1/commands/send-started';
const HELLO_TIMEOUT_MS = 1200;
const FAST_POLL_MS = 1500;
const MAX_RECONCILE_ATTEMPTS = 3;
const MAX_PARALLEL_COMMANDS = 4;
const MAX_PARALLEL_PROJECT_BOOTSTRAPS = 1;

let pumpTimer = null;
let pumpActive = false;
let connecting = null;
let stateWriteQueue = Promise.resolve();
const sessionReconcileCommandIds = new Set();
let availableProjectBootstrapSlots = MAX_PARALLEL_PROJECT_BOOTSTRAPS;
const projectBootstrapWaiters = [];

function freshState() {
  return {
    baseUrl: null,
    clientId: null,
    credential: null,
    bindings: {},
    launchRecords: {},
    threadRecords: {},
    blockedCommands: {}
  };
}

async function readState() {
  const stored = await chrome.storage.local.get(STORAGE_KEY);
  const persisted = stored[STORAGE_KEY] || {};
  const state = { ...freshState(), ...persisted };
  if (!state.blockedCommands || typeof state.blockedCommands !== 'object' || Array.isArray(state.blockedCommands)) {
    state.blockedCommands = {};
  }
  if (persisted.blockedCommand?.commandId && !state.blockedCommands[persisted.blockedCommand.commandId]) {
    state.blockedCommands[persisted.blockedCommand.commandId] = persisted.blockedCommand;
  }
  delete state.blockedCommand;
  if (!state.clientId) {
    state.clientId = crypto.randomUUID();
    await writeState(state);
  }
  return state;
}

async function writeState(state) {
  const snapshot = JSON.parse(JSON.stringify(state));
  stateWriteQueue = stateWriteQueue
    .catch(() => {})
    .then(() => chrome.storage.local.set({ [STORAGE_KEY]: snapshot }));
  await stateWriteQueue;
}

function blockCommand(state, blocked) {
  if (!blocked?.commandId) return;
  if (!state.blockedCommands || typeof state.blockedCommands !== 'object') {
    state.blockedCommands = {};
  }
  state.blockedCommands[blocked.commandId] = blocked;
}

function clearBlockedCommand(state, commandId) {
  if (!commandId) return;
  delete state.blockedCommands[commandId];
}

function blockedCommandList(state) {
  return Object.values(state.blockedCommands || {});
}

function normalizeBaseUrl(value) {
  const url = new URL(String(value || `http://127.0.0.1:${BRIDGE_PORTS[0]}`));
  const host = url.hostname.toLowerCase();
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '::1', '[::1]'].includes(host)) {
    throw new Error('MoonDesk companion URL must use local HTTP loopback');
  }
  url.pathname = '';
  url.search = '';
  url.hash = '';
  return url.toString().replace(/\/$/, '');
}

function randomCredential() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function requestError(message, status = 0, code = null) {
  const error = new Error(message);
  error.status = status;
  error.code = code;
  return error;
}

async function api(state, path, { method = 'GET', body = null, authenticated = true } = {}) {
  if (!state.baseUrl) throw requestError('MoonDesk companion bridge is not connected');
  const headers = {};
  if (authenticated) {
    if (!state.credential) throw requestError('MoonDesk companion is not paired', 401, 'not_paired');
    headers['x-moondesk-companion-token'] = state.credential;
  }
  if (body !== null) headers['content-type'] = 'application/json';
  const response = await fetch(`${normalizeBaseUrl(state.baseUrl)}${path}`, {
    method,
    cache: 'no-store',
    headers,
    body: body === null ? undefined : JSON.stringify(body)
  });
  const text = await response.text();
  let parsed = {};
  if (text) {
    try { parsed = JSON.parse(text); }
    catch { throw requestError(`MoonDesk returned non-JSON response (${response.status})`, response.status); }
  }
  if (!response.ok) {
    throw requestError(parsed.error || `MoonDesk request failed (${response.status})`, response.status, parsed.error || null);
  }
  return parsed;
}

async function hello(baseUrl) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HELLO_TIMEOUT_MS);
  try {
    const response = await fetch(`${baseUrl}${HELLO_PATH}`, {
      cache: 'no-store',
      signal: controller.signal
    });
    if (!response.ok) return null;
    const body = await response.json().catch(() => null);
    return body?.app === 'moondesk-worker-companion' && [1, 2].includes(body?.protocolVersion) ? body : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

function bridgeBaseUrl(value) {
  try {
    const url = new URL(String(value || ''));
    const port = Number(url.port);
    if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || !BRIDGE_PORTS.includes(port)) return null;
    return `http://127.0.0.1:${port}`;
  } catch {
    return null;
  }
}

async function discoverBridge(state) {
  const preferred = bridgeBaseUrl(state.baseUrl);
  const candidates = preferred
    ? [preferred, ...BRIDGE_PORTS.map((port) => `http://127.0.0.1:${port}`).filter((url) => url !== preferred)]
    : BRIDGE_PORTS.map((port) => `http://127.0.0.1:${port}`);
  const probes = await Promise.all(candidates.map(async (baseUrl) => ({ baseUrl, hello: await hello(baseUrl) })));
  const compatible = probes.filter((probe) => probe.hello?.protocolVersion === REQUIRED_PROTOCOL_VERSION);
  const match = compatible.find((probe) => probe.baseUrl === preferred) || compatible[0];
  if (match) {
    if (state.baseUrl !== match.baseUrl) {
      state.baseUrl = match.baseUrl;
      await writeState(state);
    }
    return match.hello;
  }
  const older = probes.find((probe) => probe.hello);
  if (older) {
    throw requestError(
      `MoonDesk Worker Companion requires bridge protocol ${REQUIRED_PROTOCOL_VERSION}; found protocol ${older.hello.protocolVersion}. Start the matching experimental MoonDesk build.`,
      0,
      'bridge_protocol_mismatch'
    );
  }
  throw requestError('MoonDesk companion bridge was not found on this computer', 0, 'bridge_not_found');
}

async function ensureConnectedOnce() {
  const state = await readState();
  await discoverBridge(state);

  if (state.credential) {
    try {
      const remote = await api(state, STATUS_PATH);
      if (remote?.clientId && remote.clientId !== state.clientId) {
        state.clientId = remote.clientId;
        await writeState(state);
      }
      return state;
    } catch (error) {
      if (error?.status !== 401) throw error;
    }
  }

  if (!state.credential) {
    state.credential = randomCredential();
    // Persist before the network side effect. If the response is lost, retrying with
    // the same installation credential is idempotent on the MoonDesk side.
    await writeState(state);
  }

  await api(state, PAIR_PATH, {
    method: 'POST',
    authenticated: false,
    body: { clientId: state.clientId, credential: state.credential }
  });
  await api(state, STATUS_PATH);
  return state;
}

function ensureConnected() {
  if (connecting) return connecting;
  const work = ensureConnectedOnce();
  const tracked = work.finally(() => {
    if (connecting === tracked) connecting = null;
  });
  connecting = tracked;
  return tracked;
}

async function sendToTab(tabId, message, timeoutMs = 35000) {
  let timer;
  try {
    return await Promise.race([
      chrome.tabs.sendMessage(tabId, message),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('ChatGPT tab operation timed out')), timeoutMs); })
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function ensureContent(tabId) {
  // Picker verification must run in ChatGPT's MAIN world so it can read the native
  // React-owned model state. The helper exposes only a bounded allowlist snapshot.
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      world: 'MAIN',
      files: ['model-state-main.js']
    });
  } catch {}

  try {
    const response = await sendToTab(tabId, { type: 'MOONDESK_CONTEXT' }, 3000);
    if (response?.ok) return response;
  } catch {}
  await chrome.scripting.executeScript({ target: { tabId }, files: ['chatgpt-dom.js', 'content.js'] });
  return sendToTab(tabId, { type: 'MOONDESK_CONTEXT' }, 5000);
}

function launchHash(token) {
  return `#moondesk-launch=${encodeURIComponent(token)}`;
}

function sourceWithLaunchToken(sourceUrl, token) {
  const url = new URL(sourceUrl);
  url.hash = launchHash(token);
  return url.toString();
}

function canonicalChatUrl(value) {
  try {
    const url = new URL(value);
    if (url.origin !== 'https://chatgpt.com') return null;
    url.hash = '';
    return url.toString();
  } catch {
    return null;
  }
}

function canonicalConversationUrl(value) {
  const canonical = canonicalChatUrl(value);
  if (!canonical) return null;
  try {
    const url = new URL(canonical);
    return /\/c\/[^/?#]+(?:\/|$)/.test(url.pathname) ? canonical : null;
  } catch {
    return null;
  }
}

function browserLabel() {
  const ua = navigator.userAgent || '';
  if (/Edg\//.test(ua)) return 'Edge';
  if (/Chrome\//.test(ua)) return 'Chrome';
  if (/Firefox\//.test(ua)) return 'Firefox';
  return 'Chromium';
}

function chatContextFromUrl(value) {
  try {
    const url = new URL(value);
    if (url.origin !== 'https://chatgpt.com') return null;
    const conversation = /\/c\/([0-9a-f-]{16,64})(?:\/|$)/i.exec(url.pathname);
    if (!conversation) return null;

    let projectId = null;
    let projectUrl = null;
    const projectPath = /^\/g\/([^/]+)\/c\//i.exec(url.pathname);
    if (projectPath) {
      const candidate = projectPath[1].slice(0, 36).toLowerCase();
      if (!/^g-p-[0-9a-f]{32}$/.test(candidate)) return null;
      projectId = candidate;
      projectUrl = url.origin + '/g/' + projectPath[1] + '/project';
    }

    url.hash = '';
    return {
      conversationId: conversation[1],
      conversationUrl: url.toString(),
      projectId,
      projectUrl
    };
  } catch {
    return null;
  }
}

async function collectPresence(state) {
  const tabs = (await chrome.tabs.query({ url: 'https://chatgpt.com/*' })).slice(0, 32);
  let focusedWindowId = null;
  try {
    const focused = await chrome.windows.getLastFocused();
    focusedWindowId = focused?.focused ? (focused.id ?? null) : null;
  } catch {}

  const observed = (await Promise.all(tabs.map(async (tab) => {
    const context = chatContextFromUrl(tab.url);
    if (!context) return null;
    let generating = false;
    if (Number.isInteger(tab.id)) {
      let response = null;
      try {
        response = await sendToTab(tab.id, { type: 'MOONDESK_CONTEXT' }, 600);
      } catch {
        if (tab.active) {
          try { response = await ensureContent(tab.id); } catch {}
        }
      }
      const reported = response?.ok ? response.context : null;
      generating = Boolean(
        reported?.conversationId === context.conversationId && reported?.generating === true
      );
    }
    return {
      ...context,
      active: Boolean(tab.active),
      windowFocused: focusedWindowId !== null && tab.windowId === focusedWindowId,
      generating
    };
  }))).filter(Boolean);

  const presence = await api(state, PRESENCE_PATH, {
    method: 'POST',
    body: { browserLabel: browserLabel(), tabs: observed }
  });

  const focusedTab = tabs.find(
    (tab) => tab.id && tab.active && focusedWindowId !== null && tab.windowId === focusedWindowId
  );
  if (focusedTab?.id) {
    try {
      await ensureContent(focusedTab.id);
      const response = await sendToTab(
        focusedTab.id,
        { type: 'MOONDESK_CORRELATION_EVIDENCE' },
        2500
      );
      const evidence = response?.ok ? response.evidence : null;
      const context = chatContextFromUrl(focusedTab.url);
      if (
        evidence?.conversationId &&
        context?.conversationId === evidence.conversationId &&
        Array.isArray(evidence.requestIds) &&
        evidence.requestIds.length
      ) {
        await api(state, CORRELATIONS_PATH, {
          method: 'POST',
          body: {
            conversationId: context.conversationId,
            conversationUrl: context.conversationUrl,
            projectId: context.projectId,
            projectUrl: context.projectUrl,
            requestIds: evidence.requestIds
          }
        });
      }
    } catch {}
  }

  return presence;
}

function placementForOffer(state, offer) {
  const command = offer?.command;
  const context = offer?.command?.anchorContext || offer?.anchorContext;
  if (context?.conversationId && context?.conversationUrl) {
    return {
      projectId: context.projectId || null,
      projectUrl: context.projectUrl || null,
      anchorConversationUrl: context.conversationUrl
    };
  }

  if (command?.launch?.openMode === 'existing_thread') {
    const threadKey = command.launch.threadKey;
    const thread = threadKey ? state.threadRecords?.[threadKey] : null;
    if (thread?.conversationUrl && thread.workspaceId === command.launch.workspaceId) {
      return {
        projectId: thread.projectId || projectIdFromChatUrl(thread.conversationUrl),
        projectUrl: null,
        anchorConversationUrl: null
      };
    }

    const prior = Object.values(state.launchRecords || {})
      .filter((record) =>
        record?.threadKey === threadKey &&
        record?.workspaceId === command.launch.workspaceId &&
        record?.phase === 'succeeded'
      )
      .sort((left, right) => (right.updatedAt || 0) - (left.updatedAt || 0))[0];
    if (prior) {
      const priorUrl = prior.conversationUrl || prior.sourceUrl || null;
      return {
        projectId: projectIdFromChatUrl(priorUrl),
        projectUrl: null,
        anchorConversationUrl: null
      };
    }
  }

  // Compatibility only for commands created before automatic Anchor routing existed.
  const workspaceId = command?.launch?.workspaceId;
  const legacy = workspaceId ? state.bindings?.[workspaceId] : null;
  if (legacy?.projectId && legacy?.sourceUrl) {
    return {
      projectId: legacy.projectId,
      projectUrl: legacy.projectUrl || null,
      anchorConversationUrl: legacy.sourceUrl
    };
  }
  return null;
}

function sourceUrlForCommand(placement, openMode, existingConversation) {
  if (openMode === 'existing_thread') return existingConversation;
  if (placement?.projectId) return canonicalChatUrl(placement.projectUrl);
  return 'https://chatgpt.com/';
}

function shouldAdoptConversation(offer, result) {
  return !offer?.reconcileRequired || result?.state === 'succeeded';
}

function successfulResultNeedsConversationReconcile(result, confirmedConversation) {
  return result?.state === 'succeeded' && !confirmedConversation;
}

function projectIdFromChatUrl(value) {
  try {
    const url = new URL(value);
    if (url.origin !== 'https://chatgpt.com') return null;
    const match = /^\/g\/(g-p-[0-9a-f]{32})(?:-[^/]+)?(?:\/|$)/i.exec(url.pathname);
    return match?.[1]?.toLowerCase() || null;
  } catch {
    return null;
  }
}

function threadRecordOwnedByContext(thread, workspaceId, projectId) {
  return Boolean(
    thread &&
    thread.workspaceId === workspaceId &&
    (thread.projectId || null) === (projectId || null)
  );
}

function rememberedLaunchMatchesRecord(record, rememberedLaunch) {
  if (!rememberedLaunch) return false;
  if (rememberedLaunch.commandId === record.commandId) return true;
  return Boolean(
    record.openMode === 'existing_thread' &&
    record.threadKey &&
    rememberedLaunch.threadKey === record.threadKey
  );
}

async function tabMatchesRecord(tab, record) {
  if (!tab?.id || !tab.url?.startsWith('https://chatgpt.com/')) return false;
  if (tab.url.includes(`moondesk-launch=${encodeURIComponent(record.launchToken)}`)) return true;
  const tabUrl = canonicalChatUrl(tab.url);
  if (record.conversationUrl && tabUrl === canonicalChatUrl(record.conversationUrl)) return true;
  try {
    const response = await ensureContent(tab.id);
    return rememberedLaunchMatchesRecord(record, response?.rememberedLaunch);
  } catch {
    return false;
  }
}

async function recoverTab(record) {
  if (Number.isInteger(record.tabId)) {
    try {
      const tab = await chrome.tabs.get(record.tabId);
      if (await tabMatchesRecord(tab, record)) return tab;
    } catch {}
  }
  const tabs = await chrome.tabs.query({ url: 'https://chatgpt.com/*' });
  for (const tab of tabs) {
    if (await tabMatchesRecord(tab, record)) return tab;
  }
  return null;
}

async function recoverThreadRecord(state, threadKey, workspaceId, placement) {
  const candidates = Object.values(state.launchRecords || {}).filter((record) =>
    record?.threadKey === threadKey &&
    record?.workspaceId === workspaceId &&
    record?.phase === 'succeeded'
  );
  let recovered = null;
  for (const candidate of candidates) {
    const tab = await recoverTab(candidate);
    if (!tab) continue;
    const conversationUrl = canonicalConversationUrl(tab.url);
    if (!conversationUrl || projectIdFromChatUrl(conversationUrl) !== (placement?.projectId || null)) continue;
    const next = {
      workspaceId,
      projectId: placement?.projectId || null,
      conversationUrl,
      tabId: tab.id ?? null,
      updatedAt: Date.now()
    };
    if (recovered && recovered.conversationUrl !== next.conversationUrl) return null;
    recovered = next;
  }
  if (recovered) {
    state.threadRecords[threadKey] = recovered;
    await writeState(state);
  }
  return recovered;
}

async function recordForCommand(state, command, placement, reconcileRequired = false) {
  const threadKey = command.launch.threadKey || `command:${command.id}`;
  const openMode = command.launch.openMode || 'new_thread';
  let thread = state.threadRecords?.[threadKey] || null;
  if (
    openMode === 'existing_thread' &&
    !threadRecordOwnedByContext(thread, command.launch.workspaceId, placement?.projectId || null)
  ) {
    thread = await recoverThreadRecord(
      state,
      threadKey,
      command.launch.workspaceId,
      placement
    );
  }
  let record = state.launchRecords[command.id];
  if (!record && reconcileRequired) {
    throw new Error('Reconciliation has no durable browser launch record and cannot create a fresh worker thread');
  }
  if (!record) {
    const threadOwnedByContext = threadRecordOwnedByContext(
      thread,
      command.launch.workspaceId,
      placement?.projectId || null
    );
    const existingConversation = openMode === 'existing_thread' && threadOwnedByContext
      ? canonicalChatUrl(thread.conversationUrl)
      : null;
    if (openMode === 'existing_thread' && !existingConversation) {
      throw new Error('Existing worker thread has no confirmed ChatGPT conversation binding for this workspace and placement context');
    }
    record = {
      commandId: command.id,
      workspaceId: command.launch.workspaceId,
      taskMarker: command.launch.taskMarker,
      threadKey,
      openMode,
      launchToken: crypto.randomUUID(),
      sourceUrl: sourceUrlForCommand(placement, openMode, existingConversation),
      tabId: null,
      conversationUrl: existingConversation,
      phase: 'creating',
      reconcileAttempts: 0
    };
    state.launchRecords[command.id] = record;
    await writeState(state);
  }
  let tab = await recoverTab(record);
  if (!tab) {
    if (reconcileRequired && !record.conversationUrl) {
      throw new Error('Reconciliation has no confirmed worker conversation URL and cannot create a fresh worker thread');
    }
    const targetUrl = record.conversationUrl || record.sourceUrl;
    tab = await chrome.tabs.create({ url: sourceWithLaunchToken(targetUrl, record.launchToken), active: false });
    record.tabId = tab.id ?? null;
    record.phase = 'created';
    await writeState(state);
  } else if (record.tabId !== tab.id) {
    record.tabId = tab.id ?? null;
    await writeState(state);
  }
  return { record, tab };
}

async function waitForContent(tabId, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await ensureContent(tabId);
      if (response?.ok) return response;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error('ChatGPT worker tab did not become ready');
}

async function markSendStarted(state, command) {
  const leaseId = command.lease?.leaseId;
  if (!leaseId) throw new Error('Managed chat command is missing lease');
  const response = await api(state, SEND_STARTED_PATH, {
    method: 'POST',
    body: { commandId: command.id, leaseId }
  });
  return response.command || command;
}

async function ack(state, command, outcome, details = null, conversationUrl = null) {
  const leaseId = command.lease?.leaseId;
  if (!leaseId) throw new Error('Managed chat command is missing lease');
  const payload = { commandId: command.id, leaseId, outcome };
  if (details !== null && outcome !== 'needs_reconcile') payload.details = String(details).slice(0, 1000);
  if (conversationUrl !== null && outcome === 'succeeded') payload.conversationUrl = conversationUrl;
  return api(state, '/__moondesk/companion/v1/commands/ack', { method: 'POST', body: payload });
}

function acceptanceMatches(command, placement, baseline, evidence, conversationUrl, rememberedLaunch) {
  if (!conversationUrl || !evidence) return false;
  if (!rememberedLaunchMatchesRecord({
    commandId: command.id,
    openMode: command.launch.openMode || 'new_thread',
    threadKey: command.launch.threadKey || null
  }, rememberedLaunch)) return false;
  const conversationProjectId = projectIdFromChatUrl(conversationUrl);
  if ((conversationProjectId || null) !== (placement?.projectId || null)) return false;
  if (
    command.launch.openMode === 'existing_thread' &&
    baseline?.conversationId &&
    evidence.conversationId !== baseline.conversationId
  ) return false;
  const marker = evidence.markerPresent === true;
  const generationStarted = evidence.generating === true && baseline?.generating !== true;
  const newAssistantTurn = (
    Number.isInteger(evidence.assistantTurnCount) &&
    Number.isInteger(baseline?.assistantTurnCount) &&
    evidence.assistantTurnCount > baseline.assistantTurnCount
  );
  return marker && (generationStarted || newAssistantTurn);
}

async function observeWorkerAcceptance(
  state,
  command,
  record,
  placement,
  baseline,
  timeoutMs = 45000
) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    let tab = null;
    if (Number.isInteger(record.tabId)) {
      try { tab = await chrome.tabs.get(record.tabId); } catch {}
    }
    if (!tab) {
      try { tab = await recoverTab(record); } catch {}
    }
    if (tab?.id && tab.url?.startsWith('https://chatgpt.com/')) {
      const conversationUrl = canonicalConversationUrl(tab.url);
      try {
        await ensureContent(tab.id);
        const response = await sendToTab(tab.id, {
          type: 'MOONDESK_WORKER_EVIDENCE',
          taskMarker: command.launch.taskMarker
        }, 3000);
        if (
          response?.ok &&
          acceptanceMatches(
            command,
            placement,
            baseline,
            response.evidence,
            conversationUrl,
            response.rememberedLaunch
          )
        ) {
          record.tabId = tab.id;
          record.conversationUrl = conversationUrl;
          record.phase = 'succeeded';
          await writeState(state);
          return {
            conversationUrl,
            evidence: response.evidence
          };
        }
      } catch {}
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  return null;
}

async function reconcileOrBlock(state, command, record, workspaceId, reason) {
  record.phase = 'uncertain';
  record.reconcileAttempts = (record.reconcileAttempts || 0) + 1;
  await writeState(state);
  if (record.reconcileAttempts >= MAX_RECONCILE_ATTEMPTS) {
    sessionReconcileCommandIds.delete(command.id);
    const terminalReason = `reconciliation_exhausted:${reason}`;
    await ack(state, command, 'paused', terminalReason);
    blockCommand(state, {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: 'reconcile'
    });
    await writeState(state);
    return 'blocked';
  }
  sessionReconcileCommandIds.add(command.id);
  await ack(state, command, 'needs_reconcile');
  return 'needs_reconcile';
}

const TRANSIENT_PREPARE_FAILURES = new Set([
  'project_entry_unconfirmed',
  'project_composer_not_ready',
  'worker_conversation_not_ready',
  'normal_chat_composer_not_ready'
]);
const MAX_PREPARE_ATTEMPTS = 3;
const PREPARE_RETRY_DELAY_MS = 1500;

async function prepareWorkerWithRetry(tabId, payload) {
  let response = null;
  for (let attempt = 1; attempt <= MAX_PREPARE_ATTEMPTS; attempt += 1) {
    response = await sendToTab(tabId, payload, 40000);
    if (!response?.ok) return response;
    const result = response.result || {};
    if (
      result.state !== 'failed' ||
      !TRANSIENT_PREPARE_FAILURES.has(result.reason) ||
      attempt === MAX_PREPARE_ATTEMPTS
    ) {
      return response;
    }
    await new Promise((resolve) => setTimeout(resolve, PREPARE_RETRY_DELAY_MS));
  }
  return response;
}

function shouldSerializeProjectBootstrap(offer, placement) {
  return Boolean(
    offer?.reconcileRequired !== true &&
    placement?.projectId &&
    (offer?.command?.launch?.openMode || 'new_thread') !== 'existing_thread'
  );
}

async function acquireProjectBootstrapSlot() {
  if (availableProjectBootstrapSlots > 0) {
    availableProjectBootstrapSlots -= 1;
  } else {
    await new Promise((resolve) => projectBootstrapWaiters.push(resolve));
  }

  let released = false;
  return () => {
    if (released) return;
    released = true;
    const next = projectBootstrapWaiters.shift();
    if (next) {
      next();
      return;
    }
    availableProjectBootstrapSlots = Math.min(
      MAX_PARALLEL_PROJECT_BOOTSTRAPS,
      availableProjectBootstrapSlots + 1
    );
  };
}


async function processCommand(state, offer) {
  let command = offer.command;
  const workspaceId = command?.launch?.workspaceId;
  const placement = placementForOffer(state, offer);
  const failBeforeSend = async (reason) => {
    await ack(state, command, 'failed', reason);
    blockCommand(state, {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: 'fresh'
    });
    await writeState(state);
  };
  const pauseAfterSend = async (reason) => {
    await ack(state, command, 'paused', reason);
    sessionReconcileCommandIds.delete(command.id);
    blockCommand(state, {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: 'reconcile'
    });
    await writeState(state);
  };

  if (!placement) {
    if (offer.reconcileRequired) await pauseAfterSend('anchor_context_unconfirmed');
    else await failBeforeSend('anchor_context_unconfirmed');
    return;
  }
  if (placement.projectId && !placement.projectUrl && command.launch.openMode !== 'existing_thread') {
    if (offer.reconcileRequired) await pauseAfterSend('anchor_project_url_unconfirmed');
    else await failBeforeSend('anchor_project_url_unconfirmed');
    return;
  }

  let releaseProjectBootstrap = null;
  if (shouldSerializeProjectBootstrap(offer, placement)) {
    releaseProjectBootstrap = await acquireProjectBootstrapSlot();
  }

  let recordAndTab;
  try {
    recordAndTab = await recordForCommand(state, command, placement, offer.reconcileRequired === true);
  } catch (error) {
    if (releaseProjectBootstrap) {
      releaseProjectBootstrap();
      releaseProjectBootstrap = null;
    }
    const message = String(error?.message || error);
    if (offer.reconcileRequired) {
      const reason = message.includes('no durable browser launch record')
        ? 'reconciliation_launch_record_missing'
        : message.includes('no confirmed worker conversation URL')
          ? 'reconciliation_conversation_unconfirmed'
          : message.includes('no confirmed ChatGPT conversation binding')
            ? 'worker_thread_binding_missing'
            : 'reconciliation_browser_context_unavailable';
      await pauseAfterSend(reason);
      return;
    }
    if (message.includes('no confirmed ChatGPT conversation binding')) {
      await failBeforeSend('worker_thread_binding_missing');
      return;
    }
    throw error;
  }

  const { record, tab } = recordAndTab;
  if (!tab.id) {
    if (releaseProjectBootstrap) releaseProjectBootstrap();
    throw new Error('Worker tab has no tab id');
  }

  if (offer.reconcileRequired) {
    await waitForContent(tab.id);
    const response = await sendToTab(tab.id, {
      type: 'MOONDESK_RECONCILE_WORKER',
      commandId: command.id,
      launchToken: record.launchToken,
      placement,
      launch: command.launch
    }, 10000);
    if (!response?.ok) {
      await reconcileOrBlock(state, command, record, workspaceId, 'reconciliation_tab_response_unconfirmed');
      return;
    }
    const result = response.result || {};
    const confirmedConversation = result.conversationUrl
      ? canonicalConversationUrl(result.conversationUrl)
      : canonicalConversationUrl(tab.url);
    if ((result.state === 'succeeded' || result.state === 'observed') && confirmedConversation) {
      const accepted = acceptanceMatches(
        command,
        placement,
        record.baseline || {},
        result.evidence || null,
        confirmedConversation,
        response.rememberedLaunch
      );
      if (accepted) {
        record.conversationUrl = confirmedConversation;
        record.phase = 'succeeded';
        if (record.threadKey) {
          state.threadRecords[record.threadKey] = {
            workspaceId,
            projectId: placement.projectId || null,
            conversationUrl: confirmedConversation,
            tabId: tab.id,
            updatedAt: Date.now()
          };
        }
        await writeState(state);
        await ack(state, command, 'succeeded', 'worker execution acceptance confirmed', confirmedConversation);
        sessionReconcileCommandIds.delete(command.id);
        clearBlockedCommand(state, command.id);
        await writeState(state);
        return;
      }
    }
    if (result.state === 'failed') {
      await pauseAfterSend(result.reason || 'reconciliation_failed');
      return;
    }
    await reconcileOrBlock(
      state,
      command,
      record,
      workspaceId,
      result.reason || 'reconciliation_unconfirmed'
    );
    return;
  }

  let prepared;
  try {
    await waitForContent(tab.id);
    prepared = await prepareWorkerWithRetry(tab.id, {
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: command.id,
      launchToken: record.launchToken,
      placement,
      launch: command.launch
    });
  } finally {
    if (releaseProjectBootstrap) {
      releaseProjectBootstrap();
      releaseProjectBootstrap = null;
    }
  }
  if (!prepared?.ok) {
    await failBeforeSend('worker_prepare_response_unconfirmed');
    return;
  }
  const prepareResult = prepared.result || {};
  if (prepareResult.state === 'failed') {
    record.phase = 'failed';
    await writeState(state);
    await failBeforeSend(prepareResult.reason || 'worker_prepare_failed');
    return;
  }
  if (prepareResult.state !== 'ready' && prepareResult.state !== 'already_sent') {
    await failBeforeSend(prepareResult.reason || 'worker_prepare_state_invalid');
    return;
  }

  if (prepareResult.state === 'ready') {
    record.baseline = prepareResult.evidence || null;
  }
  record.phase = 'prepared';
  await writeState(state);
  command = await markSendStarted(state, command);
  record.phase = 'send_started';
  await writeState(state);

  if (prepareResult.state === 'already_sent') {
    const existingConversation = canonicalConversationUrl(prepareResult.conversationUrl || tab.url);
    if (!existingConversation) {
      await reconcileOrBlock(state, command, record, workspaceId, 'preexisting_marker_conversation_unconfirmed');
      return;
    }
    const accepted = acceptanceMatches(
      command,
      placement,
      record.baseline || {},
      prepareResult.evidence || null,
      existingConversation,
      prepared.rememberedLaunch
    );
    if (!accepted) {
      await reconcileOrBlock(state, command, record, workspaceId, 'preexisting_marker_execution_unconfirmed');
      return;
    }
    record.conversationUrl = existingConversation;
    record.phase = 'succeeded';
    await writeState(state);
    await ack(state, command, 'succeeded', 'worker execution acceptance confirmed', existingConversation);
    sessionReconcileCommandIds.delete(command.id);
    clearBlockedCommand(state, command.id);
    await writeState(state);
    return;
  }

  const committed = await sendToTab(tab.id, {
    type: 'MOONDESK_COMMIT_WORKER_SEND',
    commandId: command.id,
    launchToken: record.launchToken,
    launch: command.launch
  }, 12000);
  if (!committed?.ok || committed.result?.state !== 'committed') {
    await pauseAfterSend(committed?.result?.reason || 'worker_send_commit_unconfirmed');
    return;
  }

  const baseline = committed.result.baseline || record.baseline || {};
  record.baseline = baseline;
  await writeState(state);
  const accepted = await observeWorkerAcceptance(state, command, record, placement, baseline);
  if (!accepted) {
    await reconcileOrBlock(state, command, record, workspaceId, 'worker_send_acceptance_unconfirmed');
    return;
  }

  if (record.threadKey) {
    state.threadRecords[record.threadKey] = {
      workspaceId,
      projectId: placement.projectId || null,
      conversationUrl: accepted.conversationUrl,
      tabId: record.tabId,
      updatedAt: Date.now()
    };
  }
  record.phase = 'succeeded';
  await writeState(state);
  await ack(state, command, 'succeeded', 'worker send acceptance confirmed', accepted.conversationUrl);
  sessionReconcileCommandIds.delete(command.id);
  clearBlockedCommand(state, command.id);
  await writeState(state);
}

function schedulePump(delayMs = FAST_POLL_MS) {
  if (pumpTimer) clearTimeout(pumpTimer);
  pumpTimer = setTimeout(() => { void pump(); }, delayMs);
}

async function pauseInheritedReconciliation(state, offer) {
  const command = offer?.command;
  if (
    !offer?.reconcileRequired ||
    !command?.id ||
    sessionReconcileCommandIds.has(command.id)
  ) {
    return false;
  }
  const previous = state.blockedCommands?.[command.id] || null;
  const reason = previous?.reason || 'reconciliation_paused_after_companion_restart';
  await ack(state, command, 'paused', `reconciliation_paused:${reason}`);
  blockCommand(state, {
    commandId: command.id,
    workspaceId: command?.launch?.workspaceId || previous?.workspaceId || null,
    reason,
    retryMode: 'reconcile'
  });
  await writeState(state);
  return true;
}

async function redeemCommandBatch(state, limit = MAX_PARALLEL_COMMANDS) {
  const offers = [];
  for (let index = 0; index < limit; index += 1) {
    const offer = await api(state, '/__moondesk/companion/v1/commands/redeem', { method: 'POST' });
    if (!offer?.command) break;
    if (await pauseInheritedReconciliation(state, offer)) continue;
    offers.push(offer);
  }
  return offers;
}

async function processCommandBatch(state, offers, processor = processCommand) {
  return Promise.allSettled(offers.map((offer) => processor(state, offer)));
}

async function pump() {
  if (pumpActive) return;
  pumpActive = true;
  let redeemed = 0;
  try {
    const state = await ensureConnected();
    await collectPresence(state);
    const offers = await redeemCommandBatch(state);
    redeemed = offers.length;
    if (offers.length) {
      const outcomes = await processCommandBatch(state, offers);
      for (const outcome of outcomes) {
        if (outcome.status === 'rejected') {
          console.warn('MoonDesk worker command failed:', String(outcome.reason?.message || outcome.reason));
        }
      }
    }
  } catch (error) {
    // Keep the service worker quiet; popup/status surfaces the actionable connection state.
    console.warn('MoonDesk worker companion pump failed:', String(error?.message || error));
  } finally {
    pumpActive = false;
    schedulePump(redeemed ? 100 : FAST_POLL_MS);
  }
}

async function pair({ pairingToken }) {
  const state = await readState();
  await discoverBridge(state);
  const response = await api(state, REPAIR_PATH, {
    method: 'POST',
    authenticated: false,
    body: { pairingToken, clientId: state.clientId }
  });
  state.clientId = response.clientId;
  state.credential = response.credential;
  state.blockedCommands = {};
  await writeState(state);
  schedulePump(50);
  return { paired: true, connected: true, clientId: state.clientId, baseUrl: state.baseUrl };
}

async function status() {
  try {
    const state = await ensureConnected();
    const remote = await api(state, STATUS_PATH);
    const blockedCommands = blockedCommandList(state);
    return {
      ...remote,
      paired: true,
      connected: true,
      baseUrl: state.baseUrl,
      blockedCommand: blockedCommands[0] || null,
      blockedCommands
    };
  } catch (error) {
    const state = await readState();
    const blockedCommands = blockedCommandList(state);
    return {
      paired: Boolean(state.credential),
      connected: false,
      baseUrl: state.baseUrl,
      error: String(error?.message || error),
      errorCode: error?.code || null,
      repairRequired: error?.status === 409,
      blockedCommand: blockedCommands[0] || null,
      blockedCommands
    };
  }
}

async function clients() {
  const state = await ensureConnected();
  const response = await api(state, CLIENTS_PATH);
  return response.clients || [];
}

async function revokeClient({ clientId }) {
  if (!clientId) throw new Error('Paired browser client id is required');
  const state = await ensureConnected();
  return api(state, CLIENTS_PATH, {
    method: 'POST',
    body: { clientId }
  });
}

async function workspaces() {
  const state = await ensureConnected();
  const response = await api(state, '/__moondesk/companion/v1/workspaces');
  return response.workspaces || [];
}

async function profile() {
  const state = await ensureConnected();
  const response = await api(state, '/__moondesk/companion/v1/profile');
  return response.profile || null;
}

async function setProfile({ profile: nextProfile }) {
  if (!nextProfile?.modelKey || !nextProfile?.modelLabel || !nextProfile?.reasoningEffort) {
    throw new Error('Worker execution profile is incomplete');
  }
  const state = await ensureConnected();
  const response = await api(state, '/__moondesk/companion/v1/profile', {
    method: 'POST',
    body: nextProfile
  });
  return response.profile || null;
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || typeof message.type !== 'string') return false;
  const task = (() => {
    switch (message.type) {
      case 'MOONDESK_PAIR': return pair(message);
      case 'MOONDESK_STATUS': return status();
      case 'MOONDESK_CLIENTS': return clients();
      case 'MOONDESK_REVOKE_CLIENT': return revokeClient(message);
      case 'MOONDESK_PROFILE': return profile();
      case 'MOONDESK_SET_PROFILE': return setProfile(message);
      case 'MOONDESK_RETRY_BLOCKED': return (async () => {
        const state = await ensureConnected();
        const blocked = blockedCommandList(state);
        if (!blocked.length) return { ok: true, retried: 0, manual: 0 };
        let retried = 0;
        let manual = 0;
        for (const entry of blocked) {
          if (entry.retryMode === 'none') {
            manual += 1;
            continue;
          }
          if (entry.retryMode === 'fresh' || entry.retryMode === 'reconcile') {
            await api(state, '/__moondesk/companion/v1/commands/retry', {
              method: 'POST',
              body: { commandId: entry.commandId }
            });
            if (entry.retryMode === 'reconcile') {
              sessionReconcileCommandIds.add(entry.commandId);
            }
          }
          clearBlockedCommand(state, entry.commandId);
          retried += 1;
        }
        await writeState(state);
        if (retried) schedulePump(50);
        return { ok: true, retried, manual };
      })();
      default: return null;
    }
  })();
  if (!task) return false;
  void Promise.resolve(task)
    .then((result) => sendResponse({ ok: true, result }))
    .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
  return true;
});

chrome.runtime.onInstalled.addListener(() => {
  void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
  schedulePump(200);
});
chrome.runtime.onStartup.addListener(() => {
  void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
  schedulePump(200);
});
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === POLL_ALARM) void pump();
});
chrome.tabs.onActivated.addListener(() => {
  schedulePump(25);
});
chrome.tabs.onUpdated.addListener((_tabId, changeInfo, tab) => {
  if (changeInfo.url && tab.url?.startsWith('https://chatgpt.com/')) {
    schedulePump(25);
  }
});
chrome.windows.onFocusChanged.addListener(() => {
  schedulePump(25);
});

void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
schedulePump(500);
