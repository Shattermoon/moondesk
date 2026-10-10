const STORAGE_KEY = 'moondeskWorkerCompanionV1';
const POLL_ALARM = 'moondesk-worker-companion-poll';
const BRIDGE_PORTS = [47650, 47651, 47652, 47653, 47654];
const REQUIRED_PROTOCOL_VERSION = 2;
// Bump with any shipped companion runtime change that requires Chromium to load new bytes. Keep
// this aligned with COMPANION_RUNTIME_REVISION in src/server.rs.
const COMPANION_RUNTIME_REVISION = 9;
const SOURCE_DEV_MANIFEST_VERSION = '0.1.0';
const RUNTIME_RELOAD_STORAGE_KEY = 'moondeskWorkerCompanionReloadRevisionV1';
const INSTALLATION_BOOTSTRAP_FILE = 'moondesk-bootstrap.json';
const HELLO_PATH = '/__moondesk/companion/v1/hello';
const PAIR_PATH = '/__moondesk/companion/v1/pair';
const REPAIR_PATH = '/__moondesk/companion/v1/repair';
const STATUS_PATH = '/__moondesk/companion/v1/status';
const CLIENTS_PATH = '/__moondesk/companion/v1/clients';
const PRESENCE_PATH = '/__moondesk/companion/v1/presence';
const CORRELATIONS_PATH = '/__moondesk/companion/v1/correlations';
const SEND_STARTED_PATH = '/__moondesk/companion/v1/commands/send-started';
const CLEAR_WORKERS_PATH = '/__moondesk/companion/v1/workers/clear';
const WORKER_RESET_ACK_PATH = '/__moondesk/companion/v1/workers/reset-ack';
const HELLO_TIMEOUT_MS = 1200;
const FAST_POLL_MS = 1500;
const MAX_RECONCILE_ATTEMPTS = 3;
const MAX_PARALLEL_COMMANDS = 4;
const CHATGPT_MODEL_EFFORTS = new Set(['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', 'pro']);
const MODEL_CATALOG_STORAGE_KEY = 'moondeskWorkerModelCatalogV1';
const MODEL_CATALOG_MAX_AGE_MS = 6 * 60 * 60 * 1000;
const MODEL_CATALOG_RETRY_MS = 5 * 60 * 1000;

let pumpTimer = null;
let pumpActive = false;
let connecting = null;
let modelCatalogFlight = null;
let stateWriteQueue = Promise.resolve();
let workerClearGeneration = 0;
let workerClearInProgress = false;
let workerSendCommitInFlight = 0;
const sessionReconcileCommandIds = new Set();

function freshState() {
  return {
    baseUrl: null,
    clientId: null,
    credential: null,
    bindings: {},
    launchRecords: {},
    threadRecords: {},
    blockedCommands: {},
    workerResetEpoch: 0
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

async function writeWorkerCommandState(state, clearGeneration) {
  if (clearGeneration !== workerClearGeneration) return false;
  await writeState(state);
  return clearGeneration === workerClearGeneration;
}

function normalizedWorkerResetEpoch(value) {
  const epoch = Number(value);
  return Number.isSafeInteger(epoch) && epoch >= 0 ? epoch : 0;
}

async function applyHostWorkerResetEpoch(state, remote) {
  const remoteEpoch = normalizedWorkerResetEpoch(remote?.workerResetEpoch);
  const localEpoch = normalizedWorkerResetEpoch(state.workerResetEpoch);
  let changed = false;
  if (remoteEpoch > localEpoch) {
    // A reset from any paired browser invalidates this extension's already-running command work
    // immediately. Do not persist the new epoch until every still-open ChatGPT tab confirms that
    // its content-side Send generation was cancelled; otherwise a background timeout could ACK
    // while an older content task is still capable of clicking Send.
    workerClearGeneration += 1;
    sessionReconcileCommandIds.clear();
    state.launchRecords = {};
    state.threadRecords = {};
    state.blockedCommands = {};
    await writeState(state);
    await clearRememberedWorkerLaunchesInTabs();
    state.workerResetEpoch = remoteEpoch;
    await writeState(state);
    changed = true;
  }
  if (
    remote?.workerResetAckRequired === true &&
    normalizedWorkerResetEpoch(state.workerResetEpoch) === remoteEpoch &&
    workerSendCommitInFlight === 0
  ) {
    await api(state, WORKER_RESET_ACK_PATH, {
      method: 'POST',
      body: { workerResetEpoch: remoteEpoch }
    });
  }
  return changed;
}

async function syncHostWorkerResetEpoch(state) {
  const remote = await api(state, STATUS_PATH);
  await applyHostWorkerResetEpoch(state, remote);
  return remote;
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

async function installationBootstrapToken() {
  let response;
  try {
    response = await fetch(chrome.runtime.getURL(INSTALLATION_BOOTSTRAP_FILE), { cache: 'no-store' });
  } catch {
    response = null;
  }
  if (!response?.ok) {
    throw requestError(
      'This Worker Companion copy has no MoonDesk installation capability. For the recommended install, load the folder shown in MoonDesk Settings -> Workers. For a source or release-ZIP recovery install, open Manual Repair in this popup and paste the current repair code from MoonDesk Settings -> Workers.',
      0,
      'companion_installation_capability_missing'
    );
  }
  const body = await response.json().catch(() => null);
  const token = typeof body?.pairingToken === 'string' ? body.pairingToken.trim() : '';
  if (!/^[0-9a-f]{64}$/i.test(token)) {
    throw requestError(
      'MoonDesk Worker Companion installation capability is invalid. Restart MoonDesk to refresh the companion folder.',
      0,
      'companion_installation_capability_invalid'
    );
  }
  return token;
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
    return body?.app === 'moondesk-worker-companion'
      && Number.isInteger(body?.protocolVersion)
      && body.protocolVersion > 0
      ? body
      : null;
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

function companionRuntimeMismatch(helloBody) {
  const remoteRevision = helloBody?.companionRuntimeRevision;
  const revisionMismatch = Number.isInteger(remoteRevision)
    && remoteRevision !== COMPANION_RUNTIME_REVISION;
  const localVersion = String(chrome.runtime.getManifest()?.version || '');
  const remoteVersion = typeof helloBody?.appVersion === 'string' ? helloBody.appVersion : '';
  // Source contributors load manifest 0.1.0 directly while Cargo keeps the project version. Release
  // candidates version the manifest and MoonDesk together, so version mismatch is authoritative there.
  const versionMismatch = localVersion
    && localVersion !== SOURCE_DEV_MANIFEST_VERSION
    && remoteVersion
    && localVersion !== remoteVersion;
  return { revisionMismatch, versionMismatch, localVersion, remoteVersion, remoteRevision };
}

function hasCompanionRuntimeMismatch(helloBody) {
  const mismatch = companionRuntimeMismatch(helloBody);
  return mismatch.revisionMismatch || mismatch.versionMismatch;
}

async function maybeReloadForRuntimeMismatch(helloBody) {
  const mismatch = companionRuntimeMismatch(helloBody);
  if (!mismatch.revisionMismatch && !mismatch.versionMismatch) return false;

  const target = `${mismatch.remoteVersion || 'unknown'}:${Number.isInteger(mismatch.remoteRevision) ? mismatch.remoteRevision : 'legacy'}`;
  const stored = await chrome.storage.local.get(RUNTIME_RELOAD_STORAGE_KEY);
  if (stored[RUNTIME_RELOAD_STORAGE_KEY] === target) {
    throw requestError(
      `MoonDesk refreshed the Worker Companion files, but Chromium is still running the previous extension generation. Reload MoonDesk Worker Companion once on the browser extensions page.`,
      0,
      'companion_runtime_reload_required'
    );
  }

  await chrome.storage.local.set({ [RUNTIME_RELOAD_STORAGE_KEY]: target });
  chrome.runtime.reload();
  return true;
}

async function discoverBridge(state) {
  const preferred = bridgeBaseUrl(state.baseUrl);
  const candidates = preferred
    ? [preferred, ...BRIDGE_PORTS.map((port) => `http://127.0.0.1:${port}`).filter((url) => url !== preferred)]
    : BRIDGE_PORTS.map((port) => `http://127.0.0.1:${port}`);
  const probes = await Promise.all(candidates.map(async (baseUrl) => ({ baseUrl, hello: await hello(baseUrl) })));
  const runtimeCompatible = probes.filter((probe) => {
    if (probe.hello?.protocolVersion !== REQUIRED_PROTOCOL_VERSION) return false;
    const mismatch = companionRuntimeMismatch(probe.hello);
    return !mismatch.revisionMismatch && !mismatch.versionMismatch;
  });
  const exactCompatible = runtimeCompatible.filter((probe) =>
    probe.hello?.companionRuntimeRevision === COMPANION_RUNTIME_REVISION
  );
  const legacyCompatible = runtimeCompatible.filter((probe) =>
    !Number.isInteger(probe.hello?.companionRuntimeRevision)
  );
  const match = exactCompatible.find((probe) => probe.baseUrl === preferred)
    || exactCompatible[0]
    || legacyCompatible.find((probe) => probe.baseUrl === preferred)
    || legacyCompatible[0];
  if (match) {
    await chrome.storage.local.remove(RUNTIME_RELOAD_STORAGE_KEY);
    if (state.baseUrl !== match.baseUrl) {
      state.baseUrl = match.baseUrl;
      await writeState(state);
    }
    return match.hello;
  }
  const runtimeMismatch = probes.find((probe) =>
    probe.baseUrl === preferred
    && probe.hello
    && hasCompanionRuntimeMismatch(probe.hello)
  ) || probes.find((probe) => probe.hello && hasCompanionRuntimeMismatch(probe.hello));
  if (runtimeMismatch && await maybeReloadForRuntimeMismatch(runtimeMismatch.hello)) {
    throw requestError(
      'MoonDesk refreshed the Worker Companion runtime; Chromium is reloading the extension.',
      0,
      'companion_runtime_reloading'
    );
  }
  const older = probes.find((probe) => probe.hello);
  if (older) {
    throw requestError(
      `MoonDesk and Worker Companion do not match (bridge protocol ${older.hello.protocolVersion}; expected ${REQUIRED_PROTOCOL_VERSION}). Restart MoonDesk so it can synchronize the companion files, then reload MoonDesk Worker Companion once if Chromium does not reload it automatically.`,
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
      await applyHostWorkerResetEpoch(state, remote);
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

  const bootstrapToken = await installationBootstrapToken();
  await api(state, PAIR_PATH, {
    method: 'POST',
    authenticated: false,
    body: { clientId: state.clientId, credential: state.credential, bootstrapToken }
  });
  const remote = await api(state, STATUS_PATH);
  await applyHostWorkerResetEpoch(state, remote);
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
      files: ['provider-correlation-main.js', 'model-state-main.js']
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

function sourceWithLaunchToken(sourceUrl, token, executionProfile = null) {
  const url = new URL(sourceUrl);
  const model = typeof executionProfile?.modelKey === 'string' && /^[a-zA-Z0-9._-]{1,80}$/.test(executionProfile.modelKey)
    ? executionProfile.modelKey
    : null;
  const effort = typeof executionProfile?.reasoningEffort === 'string' && CHATGPT_MODEL_EFFORTS.has(executionProfile.reasoningEffort)
    ? executionProfile.reasoningEffort
    : null;
  // Keep the launch marker in both query and fragment because the shell has rewritten either one
  // during startup across builds, and seed the requested model/effort before content preparation.
  url.searchParams.set('moondesk-launch', token);
  if (model) url.searchParams.set('model', model);
  if (effort) url.searchParams.set('reasoning_effort', effort);
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
    return /\/c\/[0-9a-f-]{16,64}(?:\/|$)/i.test(url.pathname) ? canonical : null;
  } catch {
    return null;
  }
}

function conversationUrlFromEvidence(evidence) {
  const conversationId = typeof evidence?.conversationId === 'string'
    ? evidence.conversationId.trim().toLowerCase()
    : '';
  return /^[0-9a-f-]{16,64}$/i.test(conversationId)
    ? `https://chatgpt.com/c/${conversationId}`
    : null;
}

function browserLabel() {
  const ua = navigator.userAgent || '';
  if (/Edg\//.test(ua)) return 'Edge';
  if (/Chrome\//.test(ua)) return 'Chrome';
  if (/Firefox\//.test(ua)) return 'Firefox';
  return 'Chromium';
}

function normalizedProviderCorrelation(value) {
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

async function publishProviderCorrelation(state, context, correlation) {
  const exact = normalizedProviderCorrelation(correlation);
  if (!context || !exact || context.conversationId.toLowerCase() !== exact.conversationId) {
    throw new Error('provider correlation did not match the exact ChatGPT conversation route');
  }
  return api(state, CORRELATIONS_PATH, {
    method: 'POST',
    body: {
      conversationId: context.conversationId,
      conversationUrl: context.conversationUrl,
      projectId: context.projectId,
      projectUrl: context.projectUrl,
      requestIds: exact.requestIds,
      operationIds: exact.operationIds
    }
  });
}

function chatContextFromUrl(value) {
  try {
    const url = new URL(value);
    if (url.origin !== 'https://chatgpt.com') return null;
    const conversation = /\/c\/([0-9a-f-]{16,64})(?:\/|$)/i.exec(url.pathname);
    if (!conversation) return null;

    let projectId = null;
    let projectUrl = null;
    const projectPath = /^\/g\/([^/]+)\/(?:shared\/)?c\//i.exec(url.pathname);
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
  if (Number.isInteger(focusedTab?.id)) {
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
    const durableConversationUrl = canonicalConversationUrl(command.launch.existingConversationUrl);
    if (durableConversationUrl) {
      return {
        projectId: projectIdFromChatUrl(durableConversationUrl),
        projectUrl: null,
        anchorConversationUrl: null
      };
    }
    return null;
  }

  // Compatibility only for commands created before automatic Core routing existed.
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

function sourceUrlForCommand(_placement, openMode, existingConversation) {
  if (openMode === 'existing_thread') return existingConversation;
  // Workers V1 always creates fresh workers from ordinary ChatGPT home. Core Project metadata
  // still identifies/routes the owning Core, but it is never used as a worker placement target.
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

function threadRecordOwnedByWorkspace(thread, workspaceId) {
  return Boolean(thread && thread.workspaceId === workspaceId);
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
  if (!Number.isInteger(tab?.id) || !tab.url?.startsWith('https://chatgpt.com/')) return false;
  if (tab.url.includes(`moondesk-launch=${encodeURIComponent(record.launchToken)}`)) return true;
  const tabUrl = canonicalChatUrl(tab.url);
  if (record.conversationUrl && tabUrl === canonicalChatUrl(record.conversationUrl)) return true;
  // Once reuse has a confirmed durable conversation URL, never adopt a different tab merely
  // because it remembers the same thread key. ChatGPT can leave hidden tabs on a shared
  // local-chatgpt placeholder route; reuse must reopen/target the durable /c/<id> instead.
  if (record.openMode === 'existing_thread' && record.conversationUrl) return false;
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

async function recoverThreadRecord(state, threadKey, workspaceId) {
  const candidates = Object.values(state.launchRecords || {}).filter((record) =>
    record?.threadKey === threadKey &&
    record?.workspaceId === workspaceId &&
    record?.phase === 'succeeded'
  );
  let recovered = null;
  for (const candidate of candidates) {
    const tab = await recoverTab(candidate);
    if (!tab) continue;
    const conversationUrl = canonicalConversationUrl(candidate.conversationUrl) || canonicalConversationUrl(tab.url);
    if (!conversationUrl) continue;
    const next = {
      workspaceId,
      projectId: projectIdFromChatUrl(conversationUrl),
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

async function recordForCommand(state, command, placement, reconcileRequired = false, clearGeneration = workerClearGeneration) {
  const threadKey = command.launch.threadKey || `command:${command.id}`;
  const openMode = command.launch.openMode || 'new_thread';
  let thread = state.threadRecords?.[threadKey] || null;
  const durableExistingConversation = openMode === 'existing_thread'
    ? canonicalConversationUrl(command.launch.existingConversationUrl)
    : null;
  if (openMode === 'existing_thread' && !durableExistingConversation) {
    throw new Error('Existing worker thread has no valid durable ChatGPT conversation URL from MoonDesk');
  }
  if (
    openMode === 'existing_thread' &&
    threadRecordOwnedByWorkspace(thread, command.launch.workspaceId) &&
    canonicalConversationUrl(thread.conversationUrl) !== durableExistingConversation
  ) {
    throw new Error('Existing worker thread durable binding conflicts with companion-local history');
  }
  if (
    openMode === 'existing_thread' &&
    !threadRecordOwnedByWorkspace(thread, command.launch.workspaceId)
  ) {
    thread = null;
  }
  let record = state.launchRecords[command.id];
  if (
    record &&
    openMode === 'existing_thread' &&
    canonicalConversationUrl(record.conversationUrl || record.sourceUrl) !== durableExistingConversation
  ) {
    throw new Error('Existing worker launch record conflicts with MoonDesk durable conversation binding');
  }
  if (!record && reconcileRequired) {
    throw new Error('Reconciliation has no durable browser launch record and cannot create a fresh worker thread');
  }
  if (!record) {
    const threadOwnedByWorkspace = threadRecordOwnedByWorkspace(thread, command.launch.workspaceId);
    const existingConversation = openMode === 'existing_thread'
      ? durableExistingConversation
      : null;
    if (
      openMode === 'existing_thread' &&
      threadOwnedByWorkspace &&
      canonicalConversationUrl(thread.conversationUrl) !== existingConversation
    ) {
      throw new Error('Existing worker thread durable binding conflicts with companion-local history');
    }
    const sourceUrl = sourceUrlForCommand(placement, openMode, existingConversation);
    if (!sourceUrl) {
      throw new Error('Worker launch has no exact ChatGPT source URL for this placement');
    }
    record = {
      commandId: command.id,
      workspaceId: command.launch.workspaceId,
      taskMarker: command.launch.taskMarker,
      threadKey,
      openMode,
      launchToken: crypto.randomUUID(),
      sourceUrl,
      tabId: null,
      conversationUrl: existingConversation,
      phase: 'creating',
      reconcileAttempts: 0
    };
    state.launchRecords[command.id] = record;
    if (!(await writeWorkerCommandState(state, clearGeneration))) {
      throw new Error('worker_clear_superseded');
    }
  }
  let tab = await recoverTab(record);
  if (!tab) {
    if (reconcileRequired && !record.conversationUrl) {
      throw new Error('Reconciliation has no confirmed worker conversation URL and cannot create a fresh worker thread');
    }
    const targetUrl = record.conversationUrl || record.sourceUrl;
    tab = await chrome.tabs.create({
      url: sourceWithLaunchToken(targetUrl, record.launchToken, command.launch.executionProfile),
      active: false
    });
    record.tabId = tab.id ?? null;
    record.phase = 'created';
    if (!(await writeWorkerCommandState(state, clearGeneration))) {
      throw new Error('worker_clear_superseded');
    }
  } else if (record.tabId !== tab.id) {
    record.tabId = tab.id ?? null;
    if (!(await writeWorkerCommandState(state, clearGeneration))) {
      throw new Error('worker_clear_superseded');
    }
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
  const conversationContext = chatContextFromUrl(conversationUrl);
  if (!conversationContext?.conversationId) return false;
  if (
    typeof evidence.conversationId !== 'string' ||
    evidence.conversationId.toLowerCase() !== conversationContext.conversationId.toLowerCase()
  ) return false;
  const conversationProjectId = projectIdFromChatUrl(conversationUrl);
  if (command.launch.openMode !== 'existing_thread' && conversationProjectId !== null) return false;
  if (
    command.launch.openMode === 'existing_thread' &&
    baseline?.conversationId &&
    evidence.conversationId !== baseline.conversationId
  ) return false;
  const marker = evidence.markerPresent === true && baseline?.markerPresent !== true;
  const newUserTurn = (
    Number.isInteger(evidence.userTurnCount) &&
    Number.isInteger(baseline?.userTurnCount) &&
    evidence.userTurnCount > baseline.userTurnCount
  );
  // The exact unique marker appearing in a new native user turn is the browser/provider Send
  // receipt. Do not make conversation binding wait for assistant generation to begin; hidden tabs
  // and deliberate models can remain quiet after ChatGPT has already accepted the user message.
  return marker && newUserTurn;
}

async function observeWorkerAcceptance(
  state,
  command,
  record,
  placement,
  baseline,
  clearGeneration,
  timeoutMs = 45000
) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (clearGeneration !== workerClearGeneration) return null;
    let tab = null;
    if (Number.isInteger(record.tabId)) {
      try { tab = await chrome.tabs.get(record.tabId); } catch {}
    }
    if (!tab) {
      try { tab = await recoverTab(record); } catch {}
    }
    if (Number.isInteger(tab?.id) && tab.url?.startsWith('https://chatgpt.com/')) {
      try {
        await ensureContent(tab.id);
        const response = await sendToTab(tab.id, {
          type: 'MOONDESK_WORKER_EVIDENCE',
          taskMarker: command.launch.taskMarker
        }, 3000);
        const conversationUrl = canonicalConversationUrl(tab.url) || conversationUrlFromEvidence(response?.evidence);
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
          if (!(await writeWorkerCommandState(state, clearGeneration))) return null;
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

async function reconcileOrBlock(state, command, record, workspaceId, reason, clearGeneration) {
  record.phase = 'uncertain';
  record.reconcileAttempts = (record.reconcileAttempts || 0) + 1;
  if (!(await writeWorkerCommandState(state, clearGeneration))) return 'cleared';
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
    if (!(await writeWorkerCommandState(state, clearGeneration))) return 'cleared';
    return 'blocked';
  }
  sessionReconcileCommandIds.add(command.id);
  await ack(state, command, 'needs_reconcile');
  return 'needs_reconcile';
}

const TRANSIENT_PREPARE_FAILURES = new Set([
  'worker_conversation_not_ready',
  'normal_chat_composer_not_ready',
  'composer_after_model_not_ready'
]);
const MAX_PREPARE_ATTEMPTS = 3;
const PREPARE_RETRY_DELAY_MS = 1500;

async function prepareWorkerWithRetry(tabId, payload, deadlineMs = Date.now() + 60000) {
  let response = null;
  let lastTransportError = null;
  for (let attempt = 1; attempt <= MAX_PREPARE_ATTEMPTS; attempt += 1) {
    const remainingMs = deadlineMs - Date.now();
    if (remainingMs <= 0) throw new Error('worker_prepare_timeout');
    try {
      // One overall preparation deadline owns the whole bootstrap. Do not nest a shorter
      // transport cutoff inside it; slow ChatGPT shell/model readiness must remain independently
      // bounded per worker rather than escaping as an ambiguous preparing state.
      response = await sendToTab(tabId, payload, remainingMs);
      lastTransportError = null;
    } catch (error) {
      lastTransportError = error;
      if (attempt === MAX_PREPARE_ATTEMPTS) throw error;
      const afterFailureMs = deadlineMs - Date.now();
      if (afterFailureMs <= 0) throw new Error('worker_prepare_timeout');
      // ChatGPT can replace the document/message port during shell/model transitions. Preparation
      // has not crossed Send, so it is safe to reacquire the content script and re-run the
      // idempotent prepare step on the same tab under the same durable launch identity.
      try {
        await waitForContent(tabId, Math.min(10000, afterFailureMs));
      } catch {}
    }
    if (!lastTransportError) {
      if (!response?.ok) return response;
      const result = response.result || {};
      if (
        result.state !== 'failed' ||
        !TRANSIENT_PREPARE_FAILURES.has(result.reason) ||
        attempt === MAX_PREPARE_ATTEMPTS
      ) {
        return response;
      }
    }
    const retryDelayMs = Math.min(PREPARE_RETRY_DELAY_MS, Math.max(0, deadlineMs - Date.now()));
    if (retryDelayMs === 0) throw new Error('worker_prepare_timeout');
    await new Promise((resolve) => setTimeout(resolve, retryDelayMs));
  }
  if (lastTransportError) throw lastTransportError;
  return response;
}

async function processCommand(state, offer, clearGeneration = workerClearGeneration) {
  const clearStillCurrent = () => clearGeneration === workerClearGeneration;
  if (!clearStillCurrent()) return;
  let command = offer.command;
  const workspaceId = command?.launch?.workspaceId;
  const placement = placementForOffer(state, offer);
  const failBeforeSend = async (reason) => {
    if (!clearStillCurrent()) return;
    await ack(state, command, 'failed', reason);
    if (!clearStillCurrent()) return;
    blockCommand(state, {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: 'fresh'
    });
    await writeWorkerCommandState(state, clearGeneration);
  };
  const pauseAfterSend = async (reason) => {
    if (!clearStillCurrent()) return;
    await ack(state, command, 'paused', reason);
    if (!clearStillCurrent()) return;
    sessionReconcileCommandIds.delete(command.id);
    blockCommand(state, {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: 'reconcile'
    });
    await writeWorkerCommandState(state, clearGeneration);
  };

  if (!placement) {
    if (offer.reconcileRequired) await pauseAfterSend('anchor_context_unconfirmed');
    else await failBeforeSend('anchor_context_unconfirmed');
    return;
  }
  let recordAndTab;
  try {
    recordAndTab = await recordForCommand(
      state,
      command,
      placement,
      offer.reconcileRequired === true,
      clearGeneration
    );
  } catch (error) {
    if (!clearStillCurrent()) return;
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
    if (message.includes('no exact ChatGPT source URL')) {
      await failBeforeSend('worker_source_url_unconfirmed');
      return;
    }
    throw error;
  }
  if (!clearStillCurrent()) return;

  const { record, tab } = recordAndTab;
  if (!Number.isInteger(tab?.id)) {
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
    if (!clearStillCurrent()) return;
    if (!response?.ok) {
      await reconcileOrBlock(state, command, record, workspaceId, 'reconciliation_tab_response_unconfirmed', clearGeneration);
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
            projectId: projectIdFromChatUrl(confirmedConversation),
            conversationUrl: confirmedConversation,
            tabId: tab.id,
            updatedAt: Date.now()
          };
        }
        if (!(await writeWorkerCommandState(state, clearGeneration))) return;
        await ack(state, command, 'succeeded', 'worker execution acceptance confirmed', confirmedConversation);
        if (!clearStillCurrent()) return;
        sessionReconcileCommandIds.delete(command.id);
        clearBlockedCommand(state, command.id);
        if (!(await writeWorkerCommandState(state, clearGeneration))) return;
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
      result.reason || 'reconciliation_unconfirmed',
      clearGeneration
    );
    return;
  }

  let prepared;
  try {
    const prepareDeadlineMs = Date.now() + 60000;
    await waitForContent(tab.id, Math.max(1, prepareDeadlineMs - Date.now()));
    prepared = await prepareWorkerWithRetry(tab.id, {
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: command.id,
      launchToken: record.launchToken,
      placement,
      launch: command.launch
    }, prepareDeadlineMs);
  } catch (error) {
    if (!clearStillCurrent()) return;
    record.phase = 'failed';
    if (!(await writeWorkerCommandState(state, clearGeneration))) return;
    const message = String(error?.message || error);
    const timedOut = message === 'worker_prepare_timeout' || /timed out/i.test(message);
    await failBeforeSend(timedOut ? 'worker_prepare_timeout' : 'worker_prepare_exception');
    return;
  }
  if (!clearStillCurrent()) return;
  if (!prepared?.ok) {
    await failBeforeSend('worker_prepare_response_unconfirmed');
    return;
  }
  const prepareResult = prepared.result || {};
  if (prepareResult.state === 'failed') {
    record.phase = 'failed';
    if (!(await writeWorkerCommandState(state, clearGeneration))) return;
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
  if (!(await writeWorkerCommandState(state, clearGeneration))) return;
  if (!clearStillCurrent()) return;
  command = await markSendStarted(state, command);
  if (!clearStillCurrent()) return;
  record.phase = 'send_started';
  if (!(await writeWorkerCommandState(state, clearGeneration))) return;

  if (prepareResult.state === 'already_sent') {
    const existingConversation = canonicalConversationUrl(prepareResult.conversationUrl || tab.url);
    if (!existingConversation) {
      await reconcileOrBlock(state, command, record, workspaceId, 'preexisting_marker_conversation_unconfirmed', clearGeneration);
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
      await reconcileOrBlock(state, command, record, workspaceId, 'preexisting_marker_execution_unconfirmed', clearGeneration);
      return;
    }
    record.conversationUrl = existingConversation;
    record.phase = 'succeeded';
    if (!(await writeWorkerCommandState(state, clearGeneration))) return;
    await ack(state, command, 'succeeded', 'worker execution acceptance confirmed', existingConversation);
    if (!clearStillCurrent()) return;
    sessionReconcileCommandIds.delete(command.id);
    clearBlockedCommand(state, command.id);
    if (!(await writeWorkerCommandState(state, clearGeneration))) return;
    return;
  }

  // markSendStarted proves only that MoonDesk authorized the browser-side Send boundary. Another
  // paired companion may clear all Workers after that response but before this tab is clicked, so
  // re-read the durable host reset epoch at the last asynchronous boundary before DOM Send.
  await syncHostWorkerResetEpoch(state);
  if (!clearStillCurrent()) return;

  let committed = null;
  workerSendCommitInFlight += 1;
  try {
    committed = await sendToTab(tab.id, {
      type: 'MOONDESK_COMMIT_WORKER_SEND',
      commandId: command.id,
      launchToken: record.launchToken,
      placement,
      launch: command.launch
    }, 12000);
  } finally {
    workerSendCommitInFlight -= 1;
    if (workerSendCommitInFlight === 0) {
      try { await syncHostWorkerResetEpoch(state); } catch {}
    }
  }
  if (!clearStillCurrent()) return;
  if (!committed?.ok || committed.result?.state !== 'committed') {
    await pauseAfterSend(committed?.result?.reason || 'worker_send_commit_unconfirmed');
    return;
  }

  const baseline = committed.result.baseline || record.baseline || {};
  record.baseline = baseline;
  if (!(await writeWorkerCommandState(state, clearGeneration))) return;
  const accepted = await observeWorkerAcceptance(
    state,
    command,
    record,
    placement,
    baseline,
    clearGeneration
  );
  if (!clearStillCurrent()) return;
  if (!accepted) {
    await reconcileOrBlock(state, command, record, workspaceId, 'worker_send_acceptance_unconfirmed', clearGeneration);
    return;
  }

  if (record.threadKey) {
    state.threadRecords[record.threadKey] = {
      workspaceId,
      projectId: projectIdFromChatUrl(accepted.conversationUrl),
      conversationUrl: accepted.conversationUrl,
      tabId: record.tabId,
      updatedAt: Date.now()
    };
  }
  record.phase = 'succeeded';
  if (!(await writeWorkerCommandState(state, clearGeneration))) return;
  await ack(state, command, 'succeeded', 'worker send acceptance confirmed', accepted.conversationUrl);
  if (!clearStillCurrent()) return;
  sessionReconcileCommandIds.delete(command.id);
  clearBlockedCommand(state, command.id);
  await writeWorkerCommandState(state, clearGeneration);
}

function schedulePump(delayMs = FAST_POLL_MS) {
  if (pumpTimer) clearTimeout(pumpTimer);
  pumpTimer = setTimeout(() => { void pump(); }, delayMs);
}

async function pauseInheritedReconciliation(state, offer, clearGeneration) {
  if (clearGeneration !== workerClearGeneration) return true;
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
  if (clearGeneration !== workerClearGeneration) return true;
  await ack(state, command, 'paused', `reconciliation_paused:${reason}`);
  if (clearGeneration !== workerClearGeneration) return true;
  blockCommand(state, {
    commandId: command.id,
    workspaceId: command?.launch?.workspaceId || previous?.workspaceId || null,
    reason,
    retryMode: 'reconcile'
  });
  await writeWorkerCommandState(state, clearGeneration);
  return true;
}

async function redeemCommandBatch(state, limit = MAX_PARALLEL_COMMANDS, clearGeneration = workerClearGeneration) {
  const offers = [];
  for (let index = 0; index < limit; index += 1) {
    if (workerClearInProgress || clearGeneration !== workerClearGeneration) break;
    const offer = await api(state, '/__moondesk/companion/v1/commands/redeem', { method: 'POST' });
    if (workerClearInProgress || clearGeneration !== workerClearGeneration) break;
    if (!offer?.command) break;
    if (await pauseInheritedReconciliation(state, offer, clearGeneration)) continue;
    offers.push(offer);
  }
  return offers;
}

async function processCommandBatch(
  state,
  offers,
  processor = processCommand,
  clearGeneration = workerClearGeneration
) {
  return Promise.allSettled(offers.map((offer) => processor(state, offer, clearGeneration)));
}

async function pump() {
  if (pumpActive || workerClearInProgress) return;
  const clearGeneration = workerClearGeneration;
  pumpActive = true;
  let redeemed = 0;
  try {
    const state = await ensureConnected();
    if (workerClearInProgress || clearGeneration !== workerClearGeneration) return;
    await collectPresence(state);
    if (workerClearInProgress || clearGeneration !== workerClearGeneration) return;
    const offers = await redeemCommandBatch(state, MAX_PARALLEL_COMMANDS, clearGeneration);
    redeemed = offers.length;
    if (offers.length && clearGeneration === workerClearGeneration) {
      const outcomes = await processCommandBatch(state, offers, processCommand, clearGeneration);
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
    await applyHostWorkerResetEpoch(state, remote);
    const hasLocalWorkerState =
      Object.keys(state.launchRecords || {}).length > 0 ||
      Object.keys(state.threadRecords || {}).length > 0 ||
      Object.keys(state.blockedCommands || {}).length > 0;
    return {
      ...remote,
      hasWorkerState: remote?.hasWorkerState === true || hasLocalWorkerState,
      paired: true,
      connected: true,
      baseUrl: state.baseUrl,
      companionVersion: chrome.runtime.getManifest().version,
      requiredProtocolVersion: REQUIRED_PROTOCOL_VERSION
    };
  } catch (error) {
    const state = await readState();
    return {
      paired: Boolean(state.credential),
      connected: false,
      baseUrl: state.baseUrl,
      companionVersion: chrome.runtime.getManifest().version,
      requiredProtocolVersion: REQUIRED_PROTOCOL_VERSION,
      error: String(error?.message || error),
      errorCode: error?.code || null,
      repairRequired: error?.status === 409
        || error?.code === 'companion_installation_capability_missing'
        || error?.code === 'companion_installation_capability_invalid'
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

async function clearRememberedWorkerLaunchesInTabs() {
  const tabs = await chrome.tabs.query({ url: 'https://chatgpt.com/*' });
  await Promise.all(tabs.map(async (tab) => {
    if (!Number.isInteger(tab?.id)) return;
    try {
      await ensureContent(tab.id);
      const response = await sendToTab(tab.id, { type: 'MOONDESK_CLEAR_WORKER_LAUNCHES' }, 1500);
      if (!response?.ok) throw new Error('ChatGPT tab did not confirm Worker Send cancellation');
    } catch (error) {
      // A tab that closed during reset cannot click Send anymore and is therefore safe. Any still-
      // open tab must explicitly confirm its content-side cancellation generation before reset ACK.
      try { await chrome.tabs.get(tab.id); } catch { return; }
      throw error;
    }
  }));
}

async function clearWorkers() {
  if (workerClearInProgress) throw new Error('Clear Workers is already in progress');

  // Fence this browser synchronously with the user's click. The host publishes its own durable
  // reset epoch so every other paired companion can independently discard stale worker authority.
  // A Send that already reached ChatGPT cannot be unsent and is reported separately by the host.
  workerClearInProgress = true;
  workerClearGeneration += 1;
  sessionReconcileCommandIds.clear();
  try {
    const state = await ensureConnected();
    await clearRememberedWorkerLaunchesInTabs();
    const response = await api(state, CLEAR_WORKERS_PATH, {
      method: 'POST',
      body: {}
    });

    // The host has now durably destroyed worker ownership. Only now erase the persisted companion
    // records; if the host reset fails, those records remain available for diagnosis/retry.
    state.launchRecords = {};
    state.threadRecords = {};
    state.blockedCommands = {};
    state.workerResetEpoch = Math.max(
      normalizedWorkerResetEpoch(state.workerResetEpoch),
      normalizedWorkerResetEpoch(response.workerResetEpoch)
    );
    await writeState(state);
    return response;
  } finally {
    workerClearInProgress = false;
    schedulePump(50);
  }
}

function modelCatalogHelperUrl(nonce) {
  return `https://chatgpt.com/?moondesk-model-catalog=${encodeURIComponent(nonce)}`;
}

async function closeModelCatalogHelper(tabId) {
  if (!Number.isInteger(tabId)) return;
  try {
    // This tab id is created exclusively for the current discovery flight. ChatGPT is allowed to
    // rewrite its URL while the picker opens, so URL-based ownership checks can leak the helper tab.
    // Closing by the exact created tab id keeps discovery disposable without touching user tabs.
    await chrome.tabs.remove(tabId);
  } catch {}
}

function modelCatalogFamilyKey(label) {
  const normalized = String(label || '')
    .trim()
    .toLowerCase()
    .replace(/^gpt[\s_-]*/i, '')
    .replace(/[^a-z0-9.]+/g, '');
  return normalized || null;
}

function normalizeModelCatalog(catalog) {
  const families = new Map();
  for (const model of Array.isArray(catalog) ? catalog : []) {
    const key = modelCatalogFamilyKey(model?.label);
    if (!key) continue;
    const choices = Array.isArray(model?.choices) ? model.choices : [];
    const aliases = Array.isArray(model?.aliases) ? model.aliases : [];
    let family = families.get(key);
    if (!family) {
      family = {
        id: typeof model?.id === 'string' && model.id ? model.id : key,
        label: String(model?.label || '').trim(),
        efforts: [],
        aliases: [],
        choices: []
      };
      families.set(key, family);
    }
    for (const alias of aliases) {
      if (typeof alias === 'string' && alias && !family.aliases.includes(alias)) family.aliases.push(alias);
    }
    if (typeof model?.id === 'string' && model.id && !family.aliases.includes(model.id)) {
      family.aliases.push(model.id);
    }
    for (const choice of choices) {
      if (!choice || typeof choice.id !== 'string' || !choice.id || typeof choice.effort !== 'string' || !choice.effort) continue;
      if (!family.aliases.includes(choice.id)) family.aliases.push(choice.id);
      if (!family.efforts.includes(choice.effort)) family.efforts.push(choice.effort);
      if (!family.choices.some((entry) => entry.id === choice.id && entry.effort === choice.effort)) {
        family.choices.push({ id: choice.id, effort: choice.effort });
      }
    }
    for (const effort of Array.isArray(model?.efforts) ? model.efforts : []) {
      if (typeof effort === 'string' && effort && !family.efforts.includes(effort)) family.efforts.push(effort);
    }
  }
  return [...families.values()].filter((family) => family.choices.length > 0);
}

async function readModelCatalogCache() {
  try {
    const stored = await chrome.storage.local.get(MODEL_CATALOG_STORAGE_KEY);
    const record = stored[MODEL_CATALOG_STORAGE_KEY];
    return record && typeof record === 'object' && !Array.isArray(record) ? record : {};
  } catch {
    return {};
  }
}

async function writeModelCatalogCache(record) {
  await chrome.storage.local.set({ [MODEL_CATALOG_STORAGE_KEY]: record });
}

async function signedInChatGptPageAvailable() {
  let tabs = [];
  try {
    tabs = await chrome.tabs.query({ url: 'https://chatgpt.com/*' });
  } catch {
    return false;
  }
  for (const tab of tabs) {
    if (!Number.isInteger(tab?.id) || tab.incognito || tab.discarded || tab.status !== 'complete') continue;
    try {
      const [frame] = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: async () => {
          try {
            const response = await fetch('/api/auth/session', { credentials: 'include', cache: 'no-store' });
            if (!response.ok) return false;
            const session = await response.json();
            return Boolean(session && typeof session === 'object' && (session.accessToken || session.user));
          } catch {
            return false;
          }
        }
      });
      if (frame?.result === true) return true;
    } catch {}
  }
  return false;
}

async function discoverModelCatalog({ force = false, requireExistingPage = false } = {}) {
  if (modelCatalogFlight) return modelCatalogFlight;
  modelCatalogFlight = (async () => {
    const nonce = crypto.randomUUID();
    const now = Date.now();
    const cached = await readModelCatalogCache();
    const cachedCatalog = Array.isArray(cached.catalog) ? cached.catalog : [];
    const updatedAt = Number(cached.updatedAt) || 0;
    const lastAttemptAt = Number(cached.lastAttemptAt) || 0;
    if (!force && cachedCatalog.length && now - updatedAt < MODEL_CATALOG_MAX_AGE_MS) return cachedCatalog;
    if (!force && lastAttemptAt && now - lastAttemptAt < MODEL_CATALOG_RETRY_MS) return cachedCatalog;
    if (requireExistingPage && !(await signedInChatGptPageAvailable())) return cachedCatalog;
    await writeModelCatalogCache({ ...cached, lastAttemptAt: now });
    const expiresAt = now + 60000;
    let tab = null;
    try {
      tab = await chrome.tabs.create({ url: modelCatalogHelperUrl(nonce), active: false });
      if (!Number.isInteger(tab?.id)) throw new Error('Could not create ChatGPT model-discovery helper');
      try { await chrome.tabs.update(tab.id, { autoDiscardable: false }); } catch {}
      await waitForContent(tab.id, Math.min(20000, Math.max(1, expiresAt - Date.now())));
      const remaining = expiresAt - Date.now();
      if (remaining <= 0) throw new Error('ChatGPT model discovery timed out before inspection');
      const response = await sendToTab(tab.id, {
        type: 'MOONDESK_MODEL_CATALOG',
        nonce,
        expiresAt
      }, remaining);
      if (!response?.ok || !Array.isArray(response.catalog) || !response.catalog.length) {
        const reason = response?.error || 'catalog_unconfirmed';
        throw new Error(`Could not confirm ChatGPT model catalog (${reason})`);
      }
      const catalog = normalizeModelCatalog(response.catalog);
      if (!catalog.length) throw new Error('Could not confirm ChatGPT model catalog (catalog_empty_after_normalization)');
      await writeModelCatalogCache({ catalog, updatedAt: Date.now(), lastAttemptAt: now });
      return catalog;
    } finally {
      if (Number.isInteger(tab?.id)) await closeModelCatalogHelper(tab.id);
    }
  })();
  try {
    return await modelCatalogFlight;
  } finally {
    modelCatalogFlight = null;
  }
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || typeof message.type !== 'string') return false;
  const task = (() => {
    switch (message.type) {
      case 'MOONDESK_PAIR': return pair(message);
      case 'MOONDESK_STATUS': return status();
      case 'MOONDESK_CLIENTS': return clients();
      case 'MOONDESK_REVOKE_CLIENT': return revokeClient(message);
      case 'MOONDESK_PROFILE': return profile();
      case 'MOONDESK_SET_PROFILE': return setProfile(message);
      case 'MOONDESK_DISCOVER_MODELS': return discoverModelCatalog({ force: true });
      case 'MOONDESK_CLEAR_WORKERS': return clearWorkers(message);
      case 'MOONDESK_PROVIDER_CORRELATION': return (async () => {
        const context = chatContextFromUrl(sender?.tab?.url || sender?.url || '');
        if (!context || !Number.isInteger(sender?.tab?.id)) {
          throw new Error('provider correlation sender was not an exact ChatGPT conversation tab');
        }
        const state = await ensureConnected();
        return publishProviderCorrelation(state, context, message.correlation);
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
  void discoverModelCatalog({ requireExistingPage: true }).catch(() => {});
});
chrome.runtime.onStartup.addListener(() => {
  void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
  schedulePump(200);
  void discoverModelCatalog({ requireExistingPage: true }).catch(() => {});
});
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === POLL_ALARM) {
    void pump();
    void discoverModelCatalog({ requireExistingPage: true }).catch(() => {});
  }
});
chrome.tabs.onActivated.addListener(() => {
  schedulePump(25);
});
chrome.tabs.onUpdated.addListener((_tabId, changeInfo, tab) => {
  if ((changeInfo.url || changeInfo.status === 'complete') && tab.url?.startsWith('https://chatgpt.com/')) {
    void discoverModelCatalog({ requireExistingPage: true }).catch(() => {});
    schedulePump(25);
  }
});
chrome.tabs.onRemoved.addListener(() => {
  // A closed worker tab is not a finished worker. Publish fresh presence quickly so MoonDesk can
  // mark the durable worker as detached/no-tab without guessing that its server-side turn stopped.
  schedulePump(25);
});
chrome.windows.onFocusChanged.addListener(() => {
  schedulePump(25);
});

void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
schedulePump(500);
void discoverModelCatalog({ requireExistingPage: true }).catch(() => {});
