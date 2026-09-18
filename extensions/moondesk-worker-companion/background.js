const STORAGE_KEY = 'moondeskWorkerCompanionV1';
const POLL_ALARM = 'moondesk-worker-companion-poll';
const BRIDGE_PORTS = [47650, 47651, 47652, 47653, 47654];
const HELLO_PATH = '/__moondesk/companion/v1/hello';
const PAIR_PATH = '/__moondesk/companion/v1/pair';
const REPAIR_PATH = '/__moondesk/companion/v1/repair';
const STATUS_PATH = '/__moondesk/companion/v1/status';
const HELLO_TIMEOUT_MS = 1200;
const FAST_POLL_MS = 1500;
const MAX_RECONCILE_ATTEMPTS = 3;

let pumpTimer = null;
let pumpActive = false;
let connecting = null;

function freshState() {
  return {
    baseUrl: null,
    clientId: null,
    credential: null,
    bindings: {},
    launchRecords: {},
    threadRecords: {},
    blockedCommand: null
  };
}

async function readState() {
  const stored = await chrome.storage.local.get(STORAGE_KEY);
  const state = { ...freshState(), ...(stored[STORAGE_KEY] || {}) };
  if (!state.clientId) {
    state.clientId = crypto.randomUUID();
    await writeState(state);
  }
  return state;
}

async function writeState(state) {
  await chrome.storage.local.set({ [STORAGE_KEY]: state });
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
    return body?.app === 'moondesk-worker-companion' && body?.protocolVersion === 1 ? body : null;
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
  for (const baseUrl of candidates) {
    const discovered = await hello(baseUrl);
    if (!discovered) continue;
    if (state.baseUrl !== baseUrl) {
      state.baseUrl = baseUrl;
      await writeState(state);
    }
    return discovered;
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

async function tabMatchesRecord(tab, record) {
  if (!tab?.id || !tab.url?.startsWith('https://chatgpt.com/')) return false;
  if (tab.url.includes(`moondesk-launch=${encodeURIComponent(record.launchToken)}`)) return true;
  const tabUrl = canonicalChatUrl(tab.url);
  if (record.conversationUrl && tabUrl === canonicalChatUrl(record.conversationUrl)) return true;
  try {
    const response = await ensureContent(tab.id);
    return response?.rememberedLaunch?.commandId === record.commandId ||
      (record.threadKey && response?.rememberedLaunch?.threadKey === record.threadKey);
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

async function recordForCommand(state, command, binding) {
  const threadKey = command.launch.threadKey || `command:${command.id}`;
  const openMode = command.launch.openMode || 'new_thread';
  const thread = state.threadRecords?.[threadKey] || null;
  let record = state.launchRecords[command.id];
  if (!record) {
    const existingConversation = openMode === 'existing_thread' ? canonicalChatUrl(thread?.conversationUrl) : null;
    if (openMode === 'existing_thread' && !existingConversation) {
      throw new Error('Existing worker thread has no confirmed ChatGPT conversation binding');
    }
    record = {
      commandId: command.id,
      workspaceId: command.launch.workspaceId,
      taskMarker: command.launch.taskMarker,
      threadKey,
      openMode,
      launchToken: crypto.randomUUID(),
      sourceUrl: openMode === 'existing_thread' ? existingConversation : binding.sourceUrl,
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

async function ack(state, command, outcome, details = null) {
  const leaseId = command.lease?.leaseId;
  if (!leaseId) throw new Error('Managed chat command is missing lease');
  const payload = { commandId: command.id, leaseId, outcome };
  if (details !== null && outcome !== 'needs_reconcile') payload.details = String(details).slice(0, 1000);
  return api(state, '/__moondesk/companion/v1/ack', { method: 'POST', body: payload });
}

async function processCommand(state, offer) {
  const command = offer.command;
  const workspaceId = command?.launch?.workspaceId;
  const binding = state.bindings[workspaceId];
  if (!binding?.projectId || !binding?.sourceUrl) {
    await ack(state, command, 'failed', 'workspace_not_bound');
    state.blockedCommand = {
      commandId: command.id,
      workspaceId,
      reason: 'workspace_not_bound',
      retryMode: offer.reconcileRequired ? 'none' : 'fresh'
    };
    await writeState(state);
    return;
  }

  let recordAndTab;
  try {
    recordAndTab = await recordForCommand(state, command, binding);
  } catch (error) {
    if (String(error?.message || error).includes('no confirmed ChatGPT conversation binding')) {
      await ack(state, command, 'failed', 'worker_thread_binding_missing');
      state.blockedCommand = {
        commandId: command.id,
        workspaceId,
        reason: 'worker_thread_binding_missing',
        retryMode: offer.reconcileRequired ? 'none' : 'fresh'
      };
      await writeState(state);
      return;
    }
    throw error;
  }
  const { record, tab } = recordAndTab;
  if (!tab.id) throw new Error('Worker tab has no tab id');
  await waitForContent(tab.id);

  const type = offer.reconcileRequired ? 'MOONDESK_RECONCILE_WORKER' : 'MOONDESK_PREPARE_WORKER';
  const response = await sendToTab(tab.id, {
    type,
    commandId: command.id,
    launchToken: record.launchToken,
    binding,
    launch: command.launch
  }, 40000);
  if (!response?.ok) {
    await ack(state, command, 'needs_reconcile');
    record.phase = 'uncertain';
    record.reconcileAttempts = (record.reconcileAttempts || 0) + 1;
    await writeState(state);
    return;
  }

  const result = response.result || {};
  const confirmedConversation = result.conversationUrl ? canonicalConversationUrl(result.conversationUrl) : null;
  if (confirmedConversation) record.conversationUrl = confirmedConversation;
  if (result.state === 'succeeded') {
    if (record.threadKey && confirmedConversation) {
      state.threadRecords[record.threadKey] = {
        workspaceId,
        projectId: binding.projectId,
        conversationUrl: confirmedConversation,
        tabId: tab.id ?? null,
        updatedAt: Date.now()
      };
    }
    record.phase = 'succeeded';
    await writeState(state);
    await ack(state, command, 'succeeded', result.reason || 'task marker confirmed');
    state.blockedCommand = null;
    await writeState(state);
    return;
  }
  if (result.state === 'failed') {
    const reason = result.reason || 'worker launch failed';
    record.phase = 'failed';
    await writeState(state);
    await ack(state, command, 'failed', reason);
    state.blockedCommand = {
      commandId: command.id,
      workspaceId,
      reason,
      retryMode: offer.reconcileRequired ? 'none' : 'fresh'
    };
    await writeState(state);
    return;
  }

  record.phase = 'uncertain';
  record.reconcileAttempts = (record.reconcileAttempts || 0) + 1;
  await writeState(state);
  await ack(state, command, 'needs_reconcile');
  if (record.reconcileAttempts >= MAX_RECONCILE_ATTEMPTS) {
    state.blockedCommand = {
      commandId: command.id,
      workspaceId,
      reason: result.reason || 'reconciliation_unconfirmed',
      retryMode: 'reconcile'
    };
    await writeState(state);
  }
}

function schedulePump(delayMs = FAST_POLL_MS) {
  if (pumpTimer) clearTimeout(pumpTimer);
  pumpTimer = setTimeout(() => { void pump(); }, delayMs);
}

async function pump() {
  if (pumpActive) return;
  pumpActive = true;
  try {
    const state = await ensureConnected();
    if (state.blockedCommand) {
      const blocked = state.blockedCommand;
      const binding = state.bindings[blocked.workspaceId];
      if (
        blocked.reason === 'workspace_not_bound' &&
        blocked.retryMode === 'fresh' &&
        binding?.projectId &&
        binding?.sourceUrl
      ) {
        await api(state, '/__moondesk/companion/v1/commands/retry', {
          method: 'POST',
          body: { commandId: blocked.commandId }
        });
        state.blockedCommand = null;
        await writeState(state);
      } else {
        return;
      }
    }
    const offer = await api(state, '/__moondesk/companion/v1/commands/redeem', { method: 'POST' });
    if (offer?.command) await processCommand(state, offer);
  } catch (error) {
    // Keep the service worker quiet; popup/status surfaces the actionable connection state.
    console.warn('MoonDesk worker companion pump failed:', String(error?.message || error));
  } finally {
    pumpActive = false;
    schedulePump();
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
  state.blockedCommand = null;
  await writeState(state);
  schedulePump(50);
  return { paired: true, connected: true, clientId: state.clientId, baseUrl: state.baseUrl };
}

async function status() {
  try {
    const state = await ensureConnected();
    const remote = await api(state, STATUS_PATH);
    return {
      ...remote,
      paired: true,
      connected: true,
      baseUrl: state.baseUrl,
      blockedCommand: state.blockedCommand
    };
  } catch (error) {
    const state = await readState();
    return {
      paired: Boolean(state.credential),
      connected: false,
      baseUrl: state.baseUrl,
      error: String(error?.message || error),
      errorCode: error?.code || null,
      repairRequired: error?.status === 409,
      blockedCommand: state.blockedCommand
    };
  }
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

async function bindProject({ workspaceId, context }) {
  if (!workspaceId || !context?.projectId || !context?.sourceUrl || !context?.conversationId) {
    throw new Error('Open an existing conversation inside the ChatGPT Project before binding it');
  }
  const source = new URL(context.sourceUrl);
  if (source.origin !== 'https://chatgpt.com') throw new Error('Binding source must be ChatGPT');
  const state = await ensureConnected();
  state.bindings[workspaceId] = {
    projectId: context.projectId,
    sourceUrl: source.toString().split('#')[0],
    projectUrl: context.projectUrl || null,
    boundAt: Date.now()
  };
  const blocked = state.blockedCommand;
  if (
    blocked?.workspaceId === workspaceId &&
    blocked.reason === 'workspace_not_bound' &&
    blocked.retryMode === 'fresh'
  ) {
    await api(state, '/__moondesk/companion/v1/commands/retry', {
      method: 'POST',
      body: { commandId: blocked.commandId }
    });
    state.blockedCommand = null;
  }
  await writeState(state);
  schedulePump(50);
  return state.bindings[workspaceId];
}

async function currentBindings() {
  return (await readState()).bindings;
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || typeof message.type !== 'string') return false;
  const task = (() => {
    switch (message.type) {
      case 'MOONDESK_PAIR': return pair(message);
      case 'MOONDESK_STATUS': return status();
      case 'MOONDESK_WORKSPACES': return workspaces();
      case 'MOONDESK_PROFILE': return profile();
      case 'MOONDESK_SET_PROFILE': return setProfile(message);
      case 'MOONDESK_BIND_PROJECT': return bindProject(message);
      case 'MOONDESK_BINDINGS': return currentBindings();
      case 'MOONDESK_RETRY_BLOCKED': return (async () => {
        const state = await ensureConnected();
        const blocked = state.blockedCommand;
        if (!blocked) return { ok: true };
        if (blocked.retryMode === 'none') {
          throw new Error('This worker launch crossed a possible Send boundary and cannot be fresh-retried. Reconciliation or manual inspection is required.');
        }
        if (blocked.retryMode === 'fresh') {
          await api(state, '/__moondesk/companion/v1/commands/retry', {
            method: 'POST',
            body: { commandId: blocked.commandId }
          });
        }
        state.blockedCommand = null;
        await writeState(state);
        schedulePump(50);
        return { ok: true };
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

void chrome.alarms.create(POLL_ALARM, { periodInMinutes: 0.5 });
schedulePump(500);
