const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');
const { ReadableStream } = require('node:stream/web');

const backgroundPath = path.join(__dirname, 'background.js');
const serverPath = path.join(__dirname, '..', '..', 'src', 'server.rs');
const manifestPath = path.join(__dirname, 'manifest.json');
const providerCorrelationMainPath = path.join(__dirname, 'provider-correlation-main.js');
const modelStateMainPath = path.join(__dirname, 'model-state-main.js');
const chatgptDomPath = path.join(__dirname, 'chatgpt-dom.js');
const contentPath = path.join(__dirname, 'content.js');
const popupPath = path.join(__dirname, 'popup.js');
const source = fs.readFileSync(backgroundPath, 'utf8');
const providerCorrelationMainSource = fs.readFileSync(providerCorrelationMainPath, 'utf8');
const modelStateMainSource = fs.readFileSync(modelStateMainPath, 'utf8');
const chatgptDomSource = fs.readFileSync(chatgptDomPath, 'utf8');
const contentSource = fs.readFileSync(contentPath, 'utf8');
const popupSource = fs.readFileSync(popupPath, 'utf8');
const serverSource = fs.readFileSync(serverPath, 'utf8');
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));

function loadBackground({ existingTabs = {}, contentByTab = {}, sendMessageImpl = null, scriptingExecuteImpl = null, fetchImpl = async () => { throw new Error('network not used'); }, setTimeoutImpl = null, runtimeManifest = manifest, initialStored = {}, createdTabIdStart = 101, bootstrapAvailable = true, bootstrapToken = 'b'.repeat(64) } = {}) {
  const createdTabs = [];
  const removedTabs = [];
  const runtimeReloads = [];
  let stored = { ...initialStored };
  let runtimeMessageHandler = null;
  let tabRemovedHandler = null;
  let tabUpdatedHandler = null;
  let alarmHandler = null;

  const chrome = {
    storage: {
      local: {
        async get() { return stored; },
        async set(value) { stored = { ...stored, ...value }; },
        async remove(key) {
          for (const entry of Array.isArray(key) ? key : [key]) delete stored[entry];
        }
      }
    },
    tabs: {
      async get(id) {
        if (existingTabs[id]) return existingTabs[id];
        throw new Error('missing tab');
      },
      async query() {
        return Object.values(existingTabs);
      },
      async create(options) {
        const tab = { id: createdTabIdStart + createdTabs.length, url: options.url, active: options.active !== false };
        createdTabs.push(tab);
        existingTabs[tab.id] = tab;
        return tab;
      },
      async update(id, changes) {
        if (!existingTabs[id]) throw new Error('missing tab');
        existingTabs[id] = { ...existingTabs[id], ...changes };
        return existingTabs[id];
      },
      async remove(id) {
        if (!existingTabs[id]) throw new Error('missing tab');
        removedTabs.push(id);
        delete existingTabs[id];
      },
      async sendMessage(id, message) {
        if (sendMessageImpl) return sendMessageImpl(id, message, { existingTabs, createdTabs });
        if (message?.type === 'MOONDESK_CONTEXT' && contentByTab[id]) return contentByTab[id];
        throw new Error('not used');
      },
      onActivated: { addListener() {} },
      onUpdated: { addListener(handler) { tabUpdatedHandler = handler; } },
      onRemoved: { addListener(handler) { tabRemovedHandler = handler; } }
    },
    windows: {
      async getLastFocused() { return { id: 1, focused: true }; },
      onFocusChanged: { addListener() {} }
    },
    scripting: {
      async executeScript(options) { return scriptingExecuteImpl ? scriptingExecuteImpl(options) : []; }
    },
    alarms: {
      async create() {},
      onAlarm: { addListener(handler) { alarmHandler = handler; } }
    },
    runtime: {
      getManifest() { return runtimeManifest; },
      getURL(path) { return `chrome-extension://test/${path}`; },
      reload() { runtimeReloads.push(true); },
      onMessage: { addListener(handler) { runtimeMessageHandler = handler; } },
      onInstalled: { addListener() {} },
      onStartup: { addListener() {} }
    }
  };

  const context = vm.createContext({
    chrome,
    console: { warn() {} },
    crypto: webcrypto,
    URL,
    AbortController,
    navigator: { userAgent: 'Mozilla/5.0 Chrome/153.0.0.0 Safari/537.36' },
    fetch: async (url, options = {}) => {
      if (String(url) === 'chrome-extension://test/moondesk-bootstrap.json') {
        if (!bootstrapAvailable) {
          return {
            ok: false,
            status: 404,
            async json() { return null; },
            async text() { return ''; }
          };
        }
        const body = { pairingToken: bootstrapToken };
        return {
          ok: true,
          status: 200,
          async json() { return body; },
          async text() { return JSON.stringify(body); }
        };
      }
      return fetchImpl(url, options);
    },
    setTimeout: setTimeoutImpl || (() => 1),
    clearTimeout: () => {}
  });

  vm.runInContext(source, context, { filename: backgroundPath });

  return {
    createdTabs,
    removedTabs,
    runtimeReloads,
    evaluate(expression) {
      return vm.runInContext(expression, context);
    },
    dispatchRuntimeMessage(message, sender = {}) {
      if (!runtimeMessageHandler) throw new Error('runtime message handler was not registered');
      return new Promise((resolve) => {
        const keepAlive = runtimeMessageHandler(message, sender, resolve);
        if (!keepAlive) resolve(undefined);
      });
    },
    dispatchTabUpdated(tabId, changeInfo, tab) {
      if (!tabUpdatedHandler) throw new Error('tab updated handler was not registered');
      tabUpdatedHandler(tabId, changeInfo, tab);
    },
    dispatchAlarm(alarm = { name: 'moondesk-worker-companion-poll' }) {
      if (!alarmHandler) throw new Error('alarm handler was not registered');
      alarmHandler(alarm);
    },
    dispatchTabRemoved(tabId = 1, removeInfo = { windowId: 1, isWindowClosing: false }) {
      if (!tabRemovedHandler) throw new Error('tab removed handler was not registered');
      tabRemovedHandler(tabId, removeInfo);
    }
  };
}

function bodyStreamResponse(text, contentType) {
  const bytes = new TextEncoder().encode(text);
  return {
    headers: { get(name) { return String(name).toLowerCase() === 'content-type' ? contentType : null; } },
    clone() {
      return {
        body: new ReadableStream({
          start(controller) {
            controller.enqueue(bytes);
            controller.close();
          }
        })
      };
    }
  };
}

function eventStreamResponse(text) {
  return bodyStreamResponse(text, 'text/event-stream');
}

function jsonStreamResponse(value) {
  return bodyStreamResponse(JSON.stringify(value), 'application/json');
}

async function flushMicrotasks() {
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
}

test('companion HTTP paths used by the extension match server route constants', () => {
  const routes = [...serverSource.matchAll(/pub const (COMPANION_[A-Z_]+_ROUTE): &str = "([^"]+)";/g)]
    .map(([, name, route]) => ({ name, route }))
    .filter(({ name }) => [
      'COMPANION_HELLO_ROUTE',
      'COMPANION_PAIR_ROUTE',
      'COMPANION_REPAIR_ROUTE',
      'COMPANION_STATUS_ROUTE',
      'COMPANION_CLIENTS_ROUTE',
      'COMPANION_PRESENCE_ROUTE',
      'COMPANION_CORRELATIONS_ROUTE',
      'COMPANION_WORKSPACES_ROUTE',
      'COMPANION_PROFILE_ROUTE',
      'COMPANION_REDEEM_ROUTE',
      'COMPANION_SEND_STARTED_ROUTE',
      'COMPANION_ACK_ROUTE',
      'COMPANION_CLEAR_WORKERS_ROUTE',
      'COMPANION_WORKER_RESET_ACK_ROUTE'
    ].includes(name));

  assert.equal(routes.length, 14, 'expected the complete companion route set from src/server.rs');
  for (const { name, route } of routes) {
    assert.ok(source.includes(route), `${name} must be used verbatim by background.js`);
  }
  assert.doesNotMatch(source, /['"]\/__moondesk\/companion\/v1\/ack['"]/);
});

test('Clear Workers mirrors Clear Swarm instead of requiring the current Core tab', () => {
  assert.match(source, /MOONDESK_CLEAR_WORKERS/);
  assert.match(source, /\/__moondesk\/companion\/v1\/workers\/clear/);
  assert.match(source, /workerClearGeneration \+= 1/);
  assert.match(source, /MOONDESK_CLEAR_WORKER_LAUNCHES/);
  assert.match(contentSource, /MOONDESK_CLEAR_WORKER_LAUNCHES/);
  assert.match(contentSource, /sessionStorage\.removeItem\(LAUNCH_SESSION_KEY\)/);
  assert.doesNotMatch(source, /MOONDESK_RETRY_BLOCKED/);
  assert.doesNotMatch(source, /\/__moondesk\/companion\/v1\/commands\/retry/);
  assert.match(popupSource, /status\?\.hasWorkerState === true/);
  assert.match(popupSource, /Clear all workers\?/);
  assert.match(popupSource, /bg\(\{ type: 'MOONDESK_CLEAR_WORKERS' \}\)/);
  assert.doesNotMatch(popupSource, /Open the Core conversation first/);
  assert.doesNotMatch(popupSource, /Retry safe launch|MOONDESK_RETRY_BLOCKED/);
});

test('clearWorkers destroys all companion-side worker history with an empty host request', async () => {
  let clearBody = null;
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    if (parsed.pathname !== '/__moondesk/companion/v1/workers/clear') {
      throw new Error(`unexpected request: ${parsed.pathname}`);
    }
    clearBody = JSON.parse(options.body || '{}');
    return {
      ok: true,
      status: 200,
      async text() {
        return JSON.stringify({
          cleared: true,
          alreadyCleared: false,
          workerIds: ['worker-record'],
          commandIds: ['command-record']
        });
      }
    };
  };
  const { evaluate } = loadBackground({ fetchImpl });
  await evaluate(`(() => {
    globalThis.__clearState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-clear-test',
      credential: '${'a'.repeat(64)}',
      launchRecords: { command: { threadKey: 'worker:worker-record' } },
      threadRecords: { 'worker:worker-record': { conversationUrl: 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee' } },
      blockedCommands: { command: { commandId: 'command' } }
    };
    connecting = Promise.resolve(globalThis.__clearState);
  })()`);
  const result = await evaluate('clearWorkers()');
  assert.equal(result.cleared, true);
  assert.deepEqual(clearBody, {});
  const state = JSON.parse(evaluate('JSON.stringify(globalThis.__clearState)'));
  assert.deepEqual(state.launchRecords, {});
  assert.deepEqual(state.threadRecords, {});
  assert.deepEqual(state.blockedCommands, {});
  assert.equal(evaluate('workerClearGeneration'), 1);
});

test('popup status keeps Clear Workers available for stale companion-only history', async () => {
  const fetchImpl = async (url) => {
    const parsed = new URL(url);
    if (parsed.pathname !== '/__moondesk/companion/v1/status') {
      throw new Error(`unexpected request: ${parsed.pathname}`);
    }
    return {
      ok: true,
      status: 200,
      async text() {
        return JSON.stringify({ paired: true, clientId: 'browser-local-history', hasWorkerState: false });
      }
    };
  };
  const { evaluate } = loadBackground({ fetchImpl });
  await evaluate(`(() => {
    globalThis.__localHistoryState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-local-history',
      credential: '${'c'.repeat(64)}',
      launchRecords: { stale: { commandId: 'stale' } }
    };
    connecting = Promise.resolve(globalThis.__localHistoryState);
  })()`);
  const result = await evaluate('status()');
  assert.equal(result.hasWorkerState, true);
});

test('Clear Workers fences in-flight browser work before awaiting the host reset', async () => {
  let releaseClear = null;
  let requestCount = 0;
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    if (parsed.pathname !== '/__moondesk/companion/v1/workers/clear') {
      throw new Error(`unexpected request: ${parsed.pathname}`);
    }
    requestCount += 1;
    assert.equal(options.method, 'POST');
    return new Promise((resolve) => {
      releaseClear = () => resolve({
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({ cleared: true, alreadyCleared: false, workerIds: [], commandIds: [] });
        }
      });
    });
  };
  const { evaluate } = loadBackground({ fetchImpl });
  await evaluate(`(() => {
    globalThis.__raceClearState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-clear-race',
      credential: '${'b'.repeat(64)}'
    };
    globalThis.__staleWorkerState = {
      ...globalThis.__raceClearState,
      launchRecords: { stale: { commandId: 'stale-after-clear' } },
      blockedCommands: { stale: { commandId: 'stale-after-clear' } }
    };
    globalThis.__staleWorkerGeneration = workerClearGeneration;
    connecting = Promise.resolve(globalThis.__raceClearState);
  })()`);

  const clearPromise = evaluate('clearWorkers()');
  assert.equal(evaluate('workerClearGeneration'), 1, 'generation must advance before the first await');
  assert.equal(evaluate('workerClearInProgress'), true);
  assert.equal(
    await evaluate('writeWorkerCommandState(globalThis.__staleWorkerState, globalThis.__staleWorkerGeneration)'),
    false,
    'a stale command generation must not republish launch history after Clear Workers begins'
  );
  await flushMicrotasks();
  assert.equal(requestCount, 1);
  await evaluate('pump()');
  assert.equal(requestCount, 1, 'pump must not redeem anything while Clear Workers is in flight');
  releaseClear();
  await clearPromise;
  assert.equal(evaluate('workerClearInProgress'), false);
  const persisted = JSON.parse(await evaluate(`chrome.storage.local.get(STORAGE_KEY).then((value) => JSON.stringify(value[STORAGE_KEY] || {}))`));
  assert.deepEqual(persisted.launchRecords || {}, {});
  assert.deepEqual(persisted.blockedCommands || {}, {});
});

test('global Clear Workers waits for another browser DOM Send already in flight and clears its ghost history', async () => {
  let hostEpoch = 0;
  let sendStarted = false;
  let commitCalls = 0;
  let browserBAckRequired = false;
  let resolveResetAck;
  const resetAcked = new Promise((resolve) => { resolveResetAck = resolve; });
  let releasePreSendStatus;
  let reportPreSendStatus;
  let preSendStatusPaused = false;
  const preSendStatusReached = new Promise((resolve) => { reportPreSendStatus = resolve; });
  const preSendStatusGate = new Promise((resolve) => { releasePreSendStatus = resolve; });
  let releaseCommit;
  let reportCommitStarted;
  let commitCompleted = false;
  const commitStarted = new Promise((resolve) => { reportCommitStarted = resolve; });
  const commitGate = new Promise((resolve) => { releaseCommit = resolve; });
  const commandId = '31111111-2222-4333-8444-555555555555';
  const leaseId = '36666666-7777-4888-8999-aaaaaaaaaaaa';
  const taskMarker = 'moondesk-worker-task:global-reset-race';
  const conversationId = 'bbbbbbbb-cccc-4ddd-8eee-ffffffffffff';
  const command = {
    id: commandId,
    state: 'leased',
    lease: { leaseId, clientId: 'browser-b' },
    anchorContext: {
      conversationId: 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      conversationUrl: 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      projectId: null,
      projectUrl: null
    },
    launch: {
      workspaceId: 'workspace-reset-race',
      purpose: 'worker',
      taskMarker,
      threadKey: 'worker:global-reset-race',
      openMode: 'new_thread',
      openingMessage: `Do the task. Marker: ${taskMarker}`,
      executionProfile: {
        modelId: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };

  const hostResponse = (body) => ({
    ok: true,
    status: 200,
    async text() { return JSON.stringify(body); }
  });
  const browserBFetch = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
      assert.equal(body.commandId, commandId);
      assert.equal(body.leaseId, leaseId);
      sendStarted = true;
      return hostResponse({ command: { ...command, state: 'send_started', reconcileHistory: true } });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      if (sendStarted && !preSendStatusPaused) {
        preSendStatusPaused = true;
        reportPreSendStatus();
        await preSendStatusGate;
      }
      return hostResponse({
        paired: true,
        clientId: 'browser-b',
        hasWorkerState: hostEpoch === 0,
        workerResetEpoch: hostEpoch,
        workerResetAckRequired: browserBAckRequired
      });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/workers/reset-ack') {
      assert.equal(body.workerResetEpoch, hostEpoch);
      browserBAckRequired = false;
      resolveResetAck();
      return hostResponse({ workerResetEpoch: hostEpoch, pendingClientCount: 0 });
    }
    throw new Error(`unexpected browser B request: ${parsed.pathname}`);
  };
  const browserAFetch = async (url, options = {}) => {
    const parsed = new URL(url);
    if (parsed.pathname === '/__moondesk/companion/v1/workers/clear') {
      assert.equal(options.method, 'POST');
      hostEpoch += 1;
      browserBAckRequired = true;
      await resetAcked;
      return hostResponse({
        cleared: true,
        alreadyCleared: false,
        workerResetEpoch: hostEpoch,
        workerIds: ['worker-global-reset'],
        commandIds: [commandId],
        sendBoundaryCommandIds: [commandId]
      });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      return hostResponse({
        paired: true,
        clientId: 'browser-a',
        hasWorkerState: hostEpoch === 0,
        workerResetEpoch: hostEpoch
      });
    }
    throw new Error(`unexpected browser A request: ${parsed.pathname}`);
  };

  let rememberedLaunch = null;
  const browserB = loadBackground({
    fetchImpl: browserBFetch,
    sendMessageImpl: async (tabId, message, { existingTabs }) => {
      if (message.type === 'MOONDESK_CONTEXT') {
        return {
          ok: true,
          context: {
            projectId: null,
            projectUrl: null,
            conversationId: existingTabs[tabId].url.includes('/c/') ? conversationId : null,
            sourceUrl: existingTabs[tabId].url,
            generating: false
          },
          rememberedLaunch
        };
      }
      if (message.type === 'MOONDESK_PREPARE_WORKER') {
        rememberedLaunch = {
          commandId,
          launchToken: message.launchToken,
          taskMarker,
          workspaceId: 'workspace-reset-race',
          threadKey: 'worker:global-reset-race'
        };
        return {
          ok: true,
          result: {
            state: 'ready',
            evidence: {
              conversationId: null,
              markerPresent: false,
              generating: false,
              userTurnCount: 0,
              composerEmpty: false
            }
          }
        };
      }
      if (message.type === 'MOONDESK_CLEAR_WORKER_LAUNCHES') {
        rememberedLaunch = null;
        return { ok: true };
      }
      if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
        commitCalls += 1;
        reportCommitStarted();
        await commitGate;
        existingTabs[tabId].url = `https://chatgpt.com/c/${conversationId}`;
        commitCompleted = true;
        return { ok: true, result: { state: 'committed', baseline: {} } };
      }
      throw new Error(`unexpected browser B tab message: ${message.type}`);
    },
    setTimeoutImpl(fn, delay) {
      if (delay <= 250) queueMicrotask(fn);
      return 1;
    }
  });
  const browserA = loadBackground({ fetchImpl: browserAFetch });

  await browserB.evaluate(`(() => {
    globalThis.__resetRaceState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-b',
      credential: '${'b'.repeat(64)}',
      launchRecords: { ghost: { commandId: 'ghost-command' } },
      threadRecords: { ghost: { conversationUrl: 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee' } }
    };
    connecting = Promise.resolve(globalThis.__resetRaceState);
    globalThis.__resetRaceCommand = ${JSON.stringify(command)};
  })()`);
  await browserA.evaluate(`(() => {
    globalThis.__resetRaceState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-a',
      credential: '${'a'.repeat(64)}'
    };
    connecting = Promise.resolve(globalThis.__resetRaceState);
  })()`);

  const browserBWork = browserB.evaluate(
    'processCommand(globalThis.__resetRaceState, { command: globalThis.__resetRaceCommand, reconcileRequired: false })'
  );
  await preSendStatusReached;
  assert.equal(sendStarted, true, 'browser B must cross host send-start authorization first');
  assert.equal(commitCalls, 0, 'browser B is paused before the actual ChatGPT Send');

  releasePreSendStatus();
  await commitStarted;
  assert.equal(commitCalls, 1, 'browser B must enter the DOM Send commit before the reset is published');
  assert.equal(commitCompleted, false, 'the DOM Send commit is still in flight');

  let clearResolved = false;
  const clearPromise = browserA.evaluate('clearWorkers()').then((value) => {
    clearResolved = true;
    return value;
  });
  await flushMicrotasks();
  assert.equal(hostEpoch, 1, 'Clear must publish the reset epoch while browser B is committing Send');
  assert.equal(clearResolved, false, 'Clear must wait for the send-capable paired browser reset acknowledgement');

  await browserB.evaluate('syncHostWorkerResetEpoch(globalThis.__resetRaceState)');
  await flushMicrotasks();
  assert.equal(browserBAckRequired, true, 'browser B must defer reset acknowledgement while DOM Send is in flight');
  assert.equal(browserB.evaluate('workerSendCommitInFlight'), 1);
  assert.equal(clearResolved, false, 'Clear must not report success before the in-flight DOM Send finishes');

  releaseCommit();
  await browserBWork;
  const cleared = await clearPromise;
  assert.equal(cleared.workerResetEpoch, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(cleared.sendBoundaryCommandIds)), [commandId]);
  assert.equal(commitCompleted, true, 'the already-started DOM Send completes before Clear reports success');
  assert.equal(commitCalls, 1);
  const browserBState = JSON.parse(browserB.evaluate('JSON.stringify(globalThis.__resetRaceState)'));
  assert.equal(browserBState.workerResetEpoch, 1);
  assert.deepEqual(browserBState.launchRecords, {});
  assert.deepEqual(browserBState.threadRecords, {});
  assert.deepEqual(browserBState.blockedCommands, {});
  const browserBStatus = await browserB.evaluate('status()');
  assert.equal(browserBStatus.hasWorkerState, false, 'the other browser must not retain ghost worker history');
});

test('a timed-out background Send cannot ACK reset until content-side cancellation prevents a late click', async () => {
  let statusCalls = 0;
  let resetAckCalls = 0;
  let contentCancelled = false;
  let lateClickCalls = 0;
  let releaseContentCommit;
  const contentCommitGate = new Promise((resolve) => { releaseContentCommit = resolve; });
  const commandId = '39111111-2222-4333-8444-555555555555';
  const leaseId = '39666666-7777-4888-8999-aaaaaaaaaaaa';
  const taskMarker = 'moondesk-worker-task:late-send-timeout-race';
  const command = {
    id: commandId,
    state: 'leased',
    lease: { leaseId, clientId: 'browser-b' },
    anchorContext: {
      conversationId: 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      conversationUrl: 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      projectId: null,
      projectUrl: null
    },
    launch: {
      workspaceId: 'workspace-late-send-race',
      purpose: 'worker',
      taskMarker,
      threadKey: 'worker:late-send-timeout-race',
      openMode: 'new_thread',
      openingMessage: `Do the task. Marker: ${taskMarker}`,
      executionProfile: {
        modelId: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };
  const hostResponse = (body) => ({
    ok: true,
    status: 200,
    async text() { return JSON.stringify(body); }
  });
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
      assert.equal(body.commandId, commandId);
      assert.equal(body.leaseId, leaseId);
      return hostResponse({ command: { ...command, state: 'send_started', reconcileHistory: true } });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      statusCalls += 1;
      const resetPublished = statusCalls >= 2;
      return hostResponse({
        paired: true,
        clientId: 'browser-b',
        hasWorkerState: !resetPublished,
        workerResetEpoch: resetPublished ? 1 : 0,
        workerResetAckRequired: resetPublished
      });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/workers/reset-ack') {
      assert.equal(body.workerResetEpoch, 1);
      assert.equal(contentCancelled, true, 'reset ACK must follow confirmed content-side cancellation');
      assert.equal(lateClickCalls, 0, 'no late content click may happen before reset ACK');
      resetAckCalls += 1;
      return hostResponse({ workerResetEpoch: 1, pendingClientCount: 0 });
    }
    throw new Error(`unexpected late-send request: ${parsed.pathname}`);
  };

  let rememberedLaunch = null;
  const browserB = loadBackground({
    fetchImpl,
    sendMessageImpl: async (_tabId, message) => {
      if (message.type === 'MOONDESK_CONTEXT') {
        return {
          ok: true,
          context: {
            projectId: null,
            projectUrl: null,
            conversationId: null,
            sourceUrl: 'https://chatgpt.com/',
            generating: false
          },
          rememberedLaunch
        };
      }
      if (message.type === 'MOONDESK_PREPARE_WORKER') {
        rememberedLaunch = {
          commandId,
          launchToken: message.launchToken,
          taskMarker,
          workspaceId: 'workspace-late-send-race',
          threadKey: 'worker:late-send-timeout-race'
        };
        return {
          ok: true,
          result: {
            state: 'ready',
            evidence: {
              conversationId: null,
              markerPresent: false,
              generating: false,
              userTurnCount: 0,
              composerEmpty: false
            }
          }
        };
      }
      if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
        await contentCommitGate;
        if (contentCancelled) {
          return { ok: true, result: { state: 'failed', reason: 'prepared_launch_target_changed' } };
        }
        lateClickCalls += 1;
        return { ok: true, result: { state: 'committed', baseline: {} } };
      }
      if (message.type === 'MOONDESK_CLEAR_WORKER_LAUNCHES') {
        contentCancelled = true;
        rememberedLaunch = null;
        releaseContentCommit();
        return { ok: true, resetGeneration: 1 };
      }
      throw new Error(`unexpected late-send tab message: ${message.type}`);
    },
    setTimeoutImpl(fn, delay) {
      if (delay === 12000 || delay <= 250) queueMicrotask(fn);
      return 1;
    }
  });

  await browserB.evaluate(`(() => {
    globalThis.__lateSendState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-b',
      credential: '${'d'.repeat(64)}'
    };
    connecting = Promise.resolve(globalThis.__lateSendState);
    globalThis.__lateSendCommand = ${JSON.stringify(command)};
  })()`);

  await assert.rejects(
    browserB.evaluate(
      'processCommand(globalThis.__lateSendState, { command: globalThis.__lateSendCommand, reconcileRequired: false })'
    ),
    /timed out/
  );
  await flushMicrotasks();

  assert.equal(statusCalls >= 2, true, 'reset must publish only after the pre-Send status check');
  assert.equal(contentCancelled, true, 'the still-open ChatGPT tab must confirm cancellation');
  assert.equal(resetAckCalls, 1, 'the reset is acknowledged after content cancellation');
  assert.equal(lateClickCalls, 0, 'the raced-out content operation must never click Send afterward');
  assert.equal(browserB.evaluate('globalThis.__lateSendState.workerResetEpoch'), 1);
});

test('an open ChatGPT tab that cannot confirm content cancellation keeps reset unacknowledged', async () => {
  let resetAckCalls = 0;
  const browser = loadBackground({
    existingTabs: { 7: { id: 7, url: 'https://chatgpt.com/' } },
    fetchImpl: async (url) => {
      if (new URL(url).pathname === '/__moondesk/companion/v1/workers/reset-ack') {
        resetAckCalls += 1;
        return { ok: true, status: 200, async text() { return JSON.stringify({ workerResetEpoch: 1, pendingClientCount: 0 }); } };
      }
      throw new Error(`unexpected reset-cancel request: ${new URL(url).pathname}`);
    },
    sendMessageImpl: async (_tabId, message) => {
      if (message.type === 'MOONDESK_CONTEXT') {
        return {
          ok: true,
          context: {
            projectId: null,
            projectUrl: null,
            conversationId: null,
            sourceUrl: 'https://chatgpt.com/',
            generating: false
          },
          rememberedLaunch: null
        };
      }
      if (message.type === 'MOONDESK_CLEAR_WORKER_LAUNCHES') {
        throw new Error('content cancellation not confirmed');
      }
      throw new Error(`unexpected reset-cancel tab message: ${message.type}`);
    }
  });
  await browser.evaluate(`(() => {
    globalThis.__resetCancelState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-b',
      credential: '${'e'.repeat(64)}'
    };
  })()`);

  await assert.rejects(
    browser.evaluate(
      `applyHostWorkerResetEpoch(globalThis.__resetCancelState, {
        workerResetEpoch: 1,
        workerResetAckRequired: true
      })`
    ),
    /content cancellation not confirmed/
  );
  assert.equal(resetAckCalls, 0, 'host reset must remain pending without confirmed content cancellation');
  assert.equal(browser.evaluate('globalThis.__resetCancelState.workerResetEpoch'), 0);
});

test('a retried global reset uses a fresh epoch to fence worker authority created after an earlier timed-out reset', async () => {
  let commitCalls = 0;
  let resetAckCalls = 0;
  let rememberedLaunch = null;
  const commandId = '41111111-2222-4333-8444-555555555555';
  const leaseId = '46666666-7777-4888-8999-aaaaaaaaaaaa';
  const taskMarker = 'moondesk-worker-task:reset-retry-race';
  const command = {
    id: commandId,
    state: 'leased',
    lease: { leaseId, clientId: 'browser-c' },
    anchorContext: {
      conversationId: 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      conversationUrl: 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee',
      projectId: null,
      projectUrl: null
    },
    launch: {
      workspaceId: 'workspace-reset-retry',
      purpose: 'worker',
      taskMarker,
      threadKey: 'worker:reset-retry-race',
      openMode: 'new_thread',
      openingMessage: `Do the task. Marker: ${taskMarker}`,
      executionProfile: {
        modelId: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };
  const hostResponse = (body) => ({
    ok: true,
    status: 200,
    async text() { return JSON.stringify(body); }
  });
  const browserCFetch = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
      assert.equal(body.commandId, commandId);
      assert.equal(body.leaseId, leaseId);
      return hostResponse({ command: { ...command, state: 'send_started', reconcileHistory: true } });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      // Browser C already applied epoch 1 from the first Clear. Browser B is still pending, but
      // Clear #2 has destructively cleared C's newly created command and therefore publishes 2.
      return hostResponse({
        paired: true,
        clientId: 'browser-c',
        hasWorkerState: false,
        workerResetEpoch: 2,
        workerResetAckRequired: true
      });
    }
    if (parsed.pathname === '/__moondesk/companion/v1/workers/reset-ack') {
      assert.equal(body.workerResetEpoch, 2);
      resetAckCalls += 1;
      // Browser B remains outstanding on the fresh epoch; C's ACK must not depend on B finishing.
      return hostResponse({ workerResetEpoch: 2, pendingClientCount: 1 });
    }
    throw new Error(`unexpected browser C request: ${parsed.pathname}`);
  };
  const browserC = loadBackground({
    fetchImpl: browserCFetch,
    sendMessageImpl: async (tabId, message, { existingTabs }) => {
      if (message.type === 'MOONDESK_CONTEXT') {
        return {
          ok: true,
          context: {
            projectId: null,
            projectUrl: null,
            conversationId: null,
            sourceUrl: existingTabs[tabId].url,
            generating: false
          },
          rememberedLaunch
        };
      }
      if (message.type === 'MOONDESK_PREPARE_WORKER') {
        rememberedLaunch = {
          commandId,
          launchToken: message.launchToken,
          taskMarker,
          workspaceId: 'workspace-reset-retry',
          threadKey: 'worker:reset-retry-race'
        };
        return {
          ok: true,
          result: {
            state: 'ready',
            evidence: {
              conversationId: null,
              markerPresent: false,
              generating: false,
              userTurnCount: 0,
              composerEmpty: false
            }
          }
        };
      }
      if (message.type === 'MOONDESK_CLEAR_WORKER_LAUNCHES') {
        rememberedLaunch = null;
        return { ok: true };
      }
      if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
        commitCalls += 1;
        return { ok: true, result: { state: 'committed', baseline: {} } };
      }
      throw new Error(`unexpected browser C tab message: ${message.type}`);
    }
  });

  await browserC.evaluate(`(() => {
    globalThis.__resetRetryState = {
      ...freshState(),
      baseUrl: 'http://127.0.0.1:47650',
      clientId: 'browser-c',
      credential: '${'c'.repeat(64)}',
      workerResetEpoch: 1
    };
    connecting = Promise.resolve(globalThis.__resetRetryState);
    globalThis.__resetRetryCommand = ${JSON.stringify(command)};
  })()`);

  await browserC.evaluate(
    'processCommand(globalThis.__resetRetryState, { command: globalThis.__resetRetryCommand, reconcileRequired: false })'
  );

  const browserCState = JSON.parse(browserC.evaluate('JSON.stringify(globalThis.__resetRetryState)'));
  assert.equal(browserCState.workerResetEpoch, 2, 'the retry must invalidate authority newer than epoch 1');
  assert.deepEqual(browserCState.launchRecords, {});
  assert.deepEqual(browserCState.threadRecords, {});
  assert.deepEqual(browserCState.blockedCommands, {});
  assert.equal(resetAckCalls, 1, 'browser C acknowledges only after applying the fresh epoch');
  assert.equal(commitCalls, 0, 'the worker created after epoch 1 must never reach DOM Send after Clear #2');
});

test('closing a browser tab schedules immediate presence publication instead of ending a worker locally', () => {
  const delays = [];
  const { dispatchTabRemoved } = loadBackground({
    setTimeoutImpl(_callback, delayMs) {
      delays.push(delayMs);
      return delays.length;
    }
  });
  delays.length = 0;
  dispatchTabRemoved(42);
  assert.deepEqual(delays, [25]);
  assert.match(source, /closed worker tab is not a finished worker/);
});

test('companion runtime revision stays aligned with the MoonDesk bridge', () => {
  const javascriptRevision = Number(source.match(/const COMPANION_RUNTIME_REVISION = (\d+);/)?.[1]);
  const rustRevision = Number(serverSource.match(/pub const COMPANION_RUNTIME_REVISION: u32 = (\d+);/)?.[1]);
  assert.ok(Number.isInteger(javascriptRevision));
  assert.equal(javascriptRevision, rustRevision);
});

test('bridge discovery selects protocol V2 without unresolved runtime constants', async () => {
  const fetchImpl = async (url) => {
    const port = new URL(url).port;
    if (port === '47651') {
      return {
        ok: true,
        status: 200,
        async json() {
          return { app: 'moondesk-worker-companion', protocolVersion: 2 };
        }
      };
    }
    return {
      ok: false,
      status: 404,
      async json() { return {}; }
    };
  };

  const { evaluate } = loadBackground({ fetchImpl });
  const hello = await evaluate('discoverBridge(freshState())');
  assert.equal(hello.protocolVersion, 2);
});

test('bridge discovery prefers an exact runtime match over another MoonDesk version', async () => {
  const fetchImpl = async (url) => {
    const port = new URL(url).port;
    if (port === '47650') {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.13.0',
            protocolVersion: 2,
            companionRuntimeRevision: 9
          };
        }
      };
    }
    if (port === '47651') {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.12.0',
            protocolVersion: 2,
            companionRuntimeRevision: 4
          };
        }
      };
    }
    return {
      ok: false,
      status: 404,
      async json() { return {}; }
    };
  };

  const { evaluate, runtimeReloads } = loadBackground({ fetchImpl });
  const hello = await evaluate('discoverBridge(freshState())');
  assert.equal(hello.companionRuntimeRevision, 9);
  assert.equal(runtimeReloads.length, 0);
});

test('bridge runtime revision mismatch reloads the unpacked companion once', async () => {
  const fetchImpl = async (url) => {
    const port = new URL(url).port;
    if (port === '47651') {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.13.0',
            protocolVersion: 2,
            companionRuntimeRevision: 8
          };
        }
      };
    }
    return {
      ok: false,
      status: 404,
      async json() { return {}; }
    };
  };

  const { evaluate, runtimeReloads } = loadBackground({ fetchImpl });
  await assert.rejects(
    evaluate('discoverBridge(freshState())'),
    /reloading the extension/
  );
  assert.equal(runtimeReloads.length, 1);

  await assert.rejects(
    evaluate('discoverBridge(freshState())'),
    /Reload MoonDesk Worker Companion once/
  );
  assert.equal(runtimeReloads.length, 1, 'mismatch recovery must not enter a reload loop');
});

test('compatible bridge clears the one-shot runtime reload marker for a future mismatch', async () => {
  const reloadKey = 'moondeskWorkerCompanionReloadRevisionV1';
  const fetchImpl = async (url) => {
    const port = new URL(url).port;
    if (port === '47651') {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.12.0',
            protocolVersion: 2,
            companionRuntimeRevision: 9
          };
        }
      };
    }
    return { ok: false, status: 404, async json() { return {}; } };
  };
  const { evaluate } = loadBackground({
    fetchImpl,
    initialStored: { [reloadKey]: '0.12.0:4' }
  });

  const hello = await evaluate('discoverBridge(freshState())');
  assert.equal(hello.companionRuntimeRevision, 9);
  const stored = await evaluate(`chrome.storage.local.get('${reloadKey}')`);
  assert.equal(stored[reloadKey], undefined);
});

test('automatic pairing proves the extension was loaded from the MoonDesk-prepared folder', async () => {
  const requests = [];
  const fetchImpl = async (url, options = {}) => {
    requests.push({ url: String(url), options });
    if (String(url).endsWith('/__moondesk/companion/v1/hello')) {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.12.0',
            protocolVersion: 2,
            companionRuntimeRevision: 9
          };
        }
      };
    }
    if (String(url).endsWith('/__moondesk/companion/v1/pair')) {
      return { ok: true, status: 200, async text() { return '{}'; } };
    }
    if (String(url).endsWith('/__moondesk/companion/v1/status')) {
      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({ clientId: 'client-bootstrap', paired: true, protocolVersion: 2 });
        }
      };
    }
    return { ok: false, status: 404, async text() { return '{}'; }, async json() { return {}; } };
  };
  const { evaluate } = loadBackground({ fetchImpl });
  const state = await evaluate(`writeState({
    ...freshState(),
    baseUrl: 'http://127.0.0.1:47651',
    clientId: 'client-bootstrap',
    credential: null
  }).then(() => ensureConnectedOnce())`);
  assert.equal(state.clientId, 'client-bootstrap');
  const pair = requests.find((request) => request.url.endsWith('/__moondesk/companion/v1/pair'));
  assert.ok(pair, 'automatic pairing must call the pair route');
  const body = JSON.parse(pair.options.body);
  assert.equal(body.clientId, 'client-bootstrap');
  assert.match(body.credential, /^[0-9a-f]{64}$/);
  assert.equal(body.bootstrapToken, 'b'.repeat(64));
});

test('source and release-ZIP installs without a bootstrap capability expose Manual Repair', async () => {
  const fetchImpl = async (url) => {
    if (String(url).endsWith('/__moondesk/companion/v1/hello')) {
      const body = {
        app: 'moondesk-worker-companion',
        appVersion: '0.12.0',
        protocolVersion: 2,
        companionRuntimeRevision: 9
      };
      return {
        ok: true,
        status: 200,
        async json() { return body; },
        async text() { return JSON.stringify(body); }
      };
    }
    return { ok: false, status: 404, async json() { return {}; }, async text() { return '{}'; } };
  };
  const { evaluate } = loadBackground({ fetchImpl, bootstrapAvailable: false });
  await evaluate(`writeState({
    ...freshState(),
    baseUrl: 'http://127.0.0.1:47651',
    clientId: 'client-source',
    credential: null
  })`);
  const result = await evaluate('status()');
  assert.equal(result.connected, false);
  assert.equal(result.repairRequired, true);
  assert.equal(result.errorCode, 'companion_installation_capability_missing');
  assert.match(result.error, /Manual Repair/);
  assert.match(
    popupSource,
    /advancedSection'\)\.hidden = !\(connected \|\| status\.repairRequired\)/,
    'Manual Repair controls must remain visible while automatic pairing is unavailable'
  );
});

test('worker tab recovery accepts Chromium tab id zero', async () => {
  const launchToken = 'launch-zero';
  const existingTabs = {
    0: {
      id: 0,
      url: `https://chatgpt.com/?moondesk-launch=${launchToken}#moondesk-launch=${launchToken}`,
      active: false
    }
  };
  const { evaluate } = loadBackground({ existingTabs });
  const recovered = await evaluate(`recoverTab({
    commandId: 'command-zero',
    workspaceId: 'workspace-zero',
    taskMarker: 'marker-zero',
    threadKey: 'thread-zero',
    openMode: 'new_thread',
    launchToken: '${launchToken}',
    sourceUrl: 'https://chatgpt.com/',
    tabId: 0,
    conversationUrl: null,
    phase: 'created',
    reconcileAttempts: 0
  })`);
  assert.equal(recovered.id, 0);
  assert.match(popupSource, /Number\.isInteger\(tab\?\.id\)/, 'popup must accept active ChatGPT tab id zero too');
});

test('release version mismatch reloads the unpacked companion even when protocol and runtime revision match', async () => {
  const fetchImpl = async (url) => {
    const port = new URL(url).port;
    if (port === '47651') {
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            app: 'moondesk-worker-companion',
            appVersion: '0.13.0',
            protocolVersion: 2,
            companionRuntimeRevision: 9
          };
        }
      };
    }
    return {
      ok: false,
      status: 404,
      async json() { return {}; }
    };
  };

  const runtimeManifest = { ...manifest, version: '0.12.0' };
  const { evaluate, runtimeReloads } = loadBackground({ fetchImpl, runtimeManifest });
  await assert.rejects(
    evaluate('discoverBridge(freshState())'),
    /reloading the extension/
  );
  assert.equal(runtimeReloads.length, 1);
});

test('model-state bridge is injected in ChatGPT MAIN world before isolated content scripts', () => {
  const main = manifest.content_scripts.find((entry) =>
    entry.world === 'MAIN' && entry.js?.includes('model-state-main.js')
  );
  assert.ok(main, 'model-state-main.js must be declared as a MAIN-world content script');
  assert.deepEqual(main.matches, ['https://chatgpt.com/*']);
  assert.deepEqual(main.js, ['provider-correlation-main.js', 'model-state-main.js']);
  assert.equal(main.run_at, 'document_start');
  assert.match(
    source,
    /files: \['provider-correlation-main\.js', 'model-state-main\.js'\]/,
    'already-open ChatGPT tabs must receive the provider observer through dynamic MAIN-world injection too'
  );

  const isolated = manifest.content_scripts.find((entry) =>
    entry.js?.includes('chatgpt-dom.js') && entry.js?.includes('content.js')
  );
  assert.ok(isolated, 'isolated companion content scripts must remain declared');
  assert.equal(isolated.world, undefined);
  assert.equal(isolated.run_at, 'document_idle');
});

test('provider correlation captures exact request identity from live conversation SSE before Fiber metadata exists', async () => {
  const conversationId = '6ac52738-64a0-83ee-bff8-59ffcc9f8c91';
  const posted = [];
  let messageHandler = null;
  const response = eventStreamResponse(
    `event: message\ndata: {"conversation_id":"${conversationId}","message":{"metadata":{"request_id":"wfr_live_request/attempt-7"}}}\n\n`
  );
  const pageWindow = {
    fetch: async () => response,
    addEventListener(type, handler) {
      if (type === 'message') messageHandler = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();

  const observed = posted.find((message) => message.source === 'moondesk-provider-correlation-observed');
  assert.ok(observed, 'live SSE must emit correlation without waiting for rendered React metadata');
  assert.equal(observed.correlation.conversationId, conversationId);
  assert.deepEqual(Array.from(observed.correlation.requestIds), ['wfr_live_request']);

  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-provider-correlation-ask', nonce: 'late-content-script', v: 1 }
  });
  const replay = posted.find((message) => message.source === 'moondesk-provider-correlation-reply');
  assert.ok(replay, 'provider proof must remain briefly replayable if isolated content loads later');
  assert.equal(replay.correlations[0].conversationId, conversationId);
  assert.deepEqual(Array.from(replay.correlations[0].requestIds), ['wfr_live_request']);
});

test('provider correlation captures exact worker operation id when the MCP header is unavailable', async () => {
  const conversationId = '6fc52738-64a0-83ee-bff8-59ffcc9f8c96';
  const operationId = '11111111-2222-4333-8444-555555555555';
  const posted = [];
  const payload = {
    conversation_id: conversationId,
    message: {
      content: {
        parts: [JSON.stringify({ operation_id: operationId, task: 'not retained' })]
      }
    }
  };
  const response = eventStreamResponse(`data: ${JSON.stringify(payload)}\n\n`);
  const pageWindow = {
    fetch: async () => response,
    addEventListener() {},
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();

  const observed = posted.find((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    Array.isArray(message.correlation.operationIds)
  );
  assert.ok(observed, 'worker operation id must be projected as exact conversation evidence');
  assert.equal(observed.correlation.conversationId, conversationId);
  assert.deepEqual(Array.from(observed.correlation.operationIds), [operationId]);
  assert.equal(observed.correlation.requestIds, undefined);
});

test('provider correlation reattaches after ChatGPT replaces fetch during page startup', async () => {
  const conversationId = '6bc52738-64a0-83ee-bff8-59ffcc9f8c92';
  const posted = [];
  let domReady = null;
  let providerCalls = 0;
  const pageWindow = {
    fetch: async () => eventStreamResponse(''),
    addEventListener(type, handler) {
      if (type === 'DOMContentLoaded') domReady = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { readyState: 'loading' },
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  const providerFetch = async () => {
    providerCalls += 1;
    return eventStreamResponse(
      `data: {"conversation_id":"${conversationId}","input_message":{"metadata":{"request_id":"wfr_after_provider_wrap"}}}\n\n`
    );
  };
  pageWindow.fetch = providerFetch;
  assert.equal(typeof domReady, 'function', 'observer must schedule a page-ready refresh');
  domReady();
  assert.notEqual(pageWindow.fetch, providerFetch, 'MoonDesk must wrap the provider-owned fetch after startup');

  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();

  assert.equal(providerCalls, 1, 'reattachment must preserve the provider fetch without recursion');
  const observed = posted.find((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    message.correlation.requestIds.includes('wfr_after_provider_wrap')
  );
  assert.ok(observed, 'request identity must survive ChatGPT replacing fetch after document_start');
  assert.equal(observed.correlation.conversationId, conversationId);
});

test('provider correlation reinjection refreshes a displaced fetch hook on an already-open tab', async () => {
  const conversationId = '6cc52738-64a0-83ee-bff8-59ffcc9f8c93';
  const posted = [];
  let providerCalls = 0;
  const pageWindow = {
    fetch: async () => eventStreamResponse(''),
    addEventListener() {},
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  const providerFetch = async () => {
    providerCalls += 1;
    return eventStreamResponse(
      `data: {"conversation_id":"${conversationId}","metadata":{"request_id":"wfr_reinjected_refresh"}}\n\n`
    );
  };
  pageWindow.fetch = providerFetch;
  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  assert.notEqual(pageWindow.fetch, providerFetch, 'MAIN-world reinjection must repair the displaced observer');

  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();

  assert.equal(providerCalls, 1);
  assert.ok(posted.some((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    message.correlation.requestIds.includes('wfr_reinjected_refresh')
  ));
});

test('provider correlation reattaches after ChatGPT replaces WebSocket during page startup', async () => {
  const conversationId = '6ec52738-64a0-83ee-bff8-59ffcc9f8c95';
  const posted = [];
  let domReady = null;
  class InitialWebSocket {
    addEventListener() {}
  }
  class ProviderWebSocket {
    static last = null;
    constructor() {
      this.listeners = new Map();
      ProviderWebSocket.last = this;
    }
    addEventListener(type, handler) {
      const handlers = this.listeners.get(type) || [];
      handlers.push(handler);
      this.listeners.set(type, handlers);
    }
    emit(type, data) {
      for (const handler of this.listeners.get(type) || []) handler({ data });
    }
  }
  const pageWindow = {
    fetch: async () => eventStreamResponse(''),
    WebSocket: InitialWebSocket,
    addEventListener(type, handler) {
      if (type === 'DOMContentLoaded') domReady = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { readyState: 'loading' },
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  pageWindow.WebSocket = ProviderWebSocket;
  domReady();
  assert.notEqual(pageWindow.WebSocket, ProviderWebSocket, 'MoonDesk must wrap the provider-owned WebSocket after startup');

  const socket = new pageWindow.WebSocket('wss://chatgpt.com/ws');
  socket.emit('message', JSON.stringify({
    conversation_id: conversationId,
    input_message: { metadata: { request_id: 'wfr_after_websocket_wrap' } }
  }));
  await flushMicrotasks();

  assert.ok(posted.some((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    message.correlation.requestIds.includes('wfr_after_websocket_wrap')
  ));
});

test('provider correlation keeps reading a bounded long response beyond the old 256 KiB cutoff', async () => {
  const conversationId = '6dc52738-64a0-83ee-bff8-59ffcc9f8c94';
  const posted = [];
  const padding = 'x'.repeat(300 * 1024);
  const response = eventStreamResponse(
    `data: {"conversation_id":"${conversationId}","padding":"${padding}"}\n\n` +
    `data: {"input_message":{"metadata":{"request_id":"wfr_late_stream_identity"}}}\n\n`
  );
  const pageWindow = {
    fetch: async () => response,
    addEventListener() {},
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();

  assert.ok(posted.some((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    message.correlation.requestIds.includes('wfr_late_stream_identity')
  ), 'late request identity must not be discarded solely because the response exceeded 256 KiB');
});

test('provider correlation follows current ChatGPT stream handoff into encoded WebSocket SSE', async () => {
  const conversationId = '7ac52738-64a0-83ee-bff8-59ffcc9f8c92';
  const topicId = 'conversation-turn-turn_12345';
  const parentMessageId = 'parent_message_12345';
  const userMessageId = 'user_message_12345';
  const posted = [];
  const response = jsonStreamResponse({
    type: 'stream_handoff',
    conversation_id: conversationId,
    options: [
      { type: 'resume_sse_endpoint', topic_id: topicId },
      { type: 'subscribe_ws_topic', topic_id: topicId }
    ]
  });
  class FakeWebSocket {
    static last = null;
    constructor() {
      this.listeners = new Map();
      FakeWebSocket.last = this;
    }
    addEventListener(type, handler) {
      const handlers = this.listeners.get(type) || [];
      handlers.push(handler);
      this.listeners.set(type, handlers);
    }
    emit(type, data) {
      for (const handler of this.listeners.get(type) || []) handler({ data });
    }
  }
  const pageWindow = {
    fetch: async () => response,
    WebSocket: FakeWebSocket,
    addEventListener() {},
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${conversationId}`,
      pathname: `/c/${conversationId}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation', {
    method: 'POST',
    body: JSON.stringify({
      conversation_id: conversationId,
      parent_message_id: parentMessageId,
      messages: [{ id: userMessageId, author: { role: 'user' }, content: { parts: ['not retained'] } }]
    })
  });
  await flushMicrotasks();

  const socket = new pageWindow.WebSocket('wss://chatgpt.com/ws');
  const encodedItem = 'data: {"message":{"metadata":{"request_id":"wfr_ws_request/attempt-3"}}}\n\n';
  socket.emit('message', JSON.stringify([{
    parent_message_id: parentMessageId,
    payload: { payload: { encoded_item: encodedItem } }
  }]));
  await flushMicrotasks();

  const observed = posted.find((message) =>
    message.source === 'moondesk-provider-correlation-observed' &&
    message.correlation.requestIds.includes('wfr_ws_request')
  );
  assert.ok(observed, 'WebSocket handoff must retain exact conversation ownership');
  assert.equal(observed.correlation.conversationId, conversationId);
});

test('provider correlation fails closed when one stream event claims conflicting conversation ids', async () => {
  const firstConversation = '7bc52738-64a0-83ee-bff8-59ffcc9f8c96';
  const secondConversation = '7cc52738-64a0-83ee-bff8-59ffcc9f8c97';
  const posted = [];
  const response = eventStreamResponse(
    `data: {"conversation_id":"${firstConversation}","message":{"conversation_id":"${secondConversation}","metadata":{"request_id":"wfr_conflicted"}}}\n\n`
  );
  const pageWindow = {
    fetch: async () => response,
    addEventListener() {},
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      href: `https://chatgpt.com/c/${firstConversation}`,
      pathname: `/c/${firstConversation}`
    },
    URL,
    TextDecoder,
    Uint8Array,
    Date,
    console
  });

  vm.runInContext(providerCorrelationMainSource, context, { filename: providerCorrelationMainPath });
  await pageWindow.fetch('https://chatgpt.com/backend-api/f/conversation');
  await flushMicrotasks();
  assert.equal(
    posted.filter((message) => message.source === 'moondesk-provider-correlation-observed').length,
    0,
    'ambiguous provider evidence must never be promoted into Core authority'
  );
});

test('isolated content forwards provider correlation immediately for the exact current conversation', async () => {
  const conversationId = '8ac52738-64a0-83ee-bff8-59ffcc9f8c93';
  let windowMessageHandler = null;
  const runtimeMessages = [];
  const pageWindow = {
    MOONDESK_CHATGPT_DOM: {
      conversationIdFromPath() { return conversationId; }
    },
    addEventListener(type, handler) {
      if (type === 'message') windowMessageHandler = handler;
    },
    postMessage() {}
  };
  const context = vm.createContext({
    window: pageWindow,
    location: {
      origin: 'https://chatgpt.com',
      pathname: `/c/${conversationId}`,
      search: '',
      hash: ''
    },
    chrome: {
      runtime: {
        async sendMessage(message) {
          runtimeMessages.push(message);
          return { ok: true };
        },
        onMessage: { addListener() {} }
      }
    },
    crypto: webcrypto,
    Date,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });

  vm.runInContext(contentSource, context, { filename: contentPath });
  assert.equal(typeof windowMessageHandler, 'function');
  windowMessageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: {
      source: 'moondesk-provider-correlation-observed',
      v: 1,
      correlation: { conversationId, requestIds: ['wfr_provider_exact'] }
    }
  });
  await flushMicrotasks();

  assert.deepEqual(JSON.parse(JSON.stringify(runtimeMessages)), [{
    type: 'MOONDESK_PROVIDER_CORRELATION',
    correlation: { conversationId, requestIds: ['wfr_provider_exact'] }
  }]);
});

test('isolated content holds a new-chat provider proof until the URL converges to the exact conversation', async () => {
  const conversationId = '8bc52738-64a0-83ee-bff8-59ffcc9f8c95';
  let currentConversation = null;
  let windowMessageHandler = null;
  const runtimeMessages = [];
  const pageWindow = {
    MOONDESK_CHATGPT_DOM: {
      conversationIdFromPath() { return currentConversation; }
    },
    addEventListener(type, handler) {
      if (type === 'message') windowMessageHandler = handler;
    },
    postMessage() {}
  };
  const context = vm.createContext({
    window: pageWindow,
    location: { origin: 'https://chatgpt.com', pathname: '/', search: '', hash: '' },
    chrome: {
      runtime: {
        async sendMessage(message) {
          runtimeMessages.push(message);
          return { ok: true };
        },
        onMessage: { addListener() {} }
      }
    },
    crypto: webcrypto,
    Date,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });

  vm.runInContext(contentSource, context, { filename: contentPath });
  windowMessageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: {
      source: 'moondesk-provider-correlation-observed',
      v: 1,
      correlation: { conversationId, requestIds: ['wfr_new_chat'] }
    }
  });
  await flushMicrotasks();
  assert.equal(runtimeMessages.length, 0, 'new-chat proof must not publish before route identity exists');

  currentConversation = conversationId;
  await new Promise((resolve) => setTimeout(resolve, 140));
  assert.deepEqual(JSON.parse(JSON.stringify(runtimeMessages)), [{
    type: 'MOONDESK_PROVIDER_CORRELATION',
    correlation: { conversationId, requestIds: ['wfr_new_chat'] }
  }]);
});

test('worker evidence promotes one exact provider conversation while ChatGPT still shows a local placeholder route', async () => {
  const conversationId = '8cc52738-64a0-83ee-bff8-59ffcc9f8c96';
  const marker = 'moondesk-worker-task:provider-finalize';
  let windowMessageHandler = null;
  let runtimeMessageHandler = null;
  const storage = new Map([[
    'moondesk-worker-launch-v1',
    JSON.stringify({
      commandId: 'provider-finalize-command',
      launchToken: 'provider-finalize-token',
      taskMarker: marker,
      workspaceId: 'workspace-a',
      threadKey: 'worker:provider-finalize',
      openMode: 'new_thread',
      targetConversationId: null,
      projectId: null
    })
  ]]);
  const localUrl = 'https://chatgpt.com/c/local-chatgpt%3A422be40f-2871-4c05-9110-62ac6e7854cd';
  const pageWindow = {
    MOONDESK_CHATGPT_DOM: {
      conversationIdFromPath() { return null; },
      workerEvidence() {
        return {
          conversationId: null,
          conversationUrl: localUrl,
          projectId: null,
          markerPresent: true,
          generating: true,
          userTurnCount: 1,
          assistantTurnCount: 0,
          composerEmpty: true
        };
      }
    },
    addEventListener(type, handler) {
      if (type === 'message') windowMessageHandler = handler;
    },
    postMessage() {}
  };
  const context = vm.createContext({
    window: pageWindow,
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      origin: 'https://chatgpt.com',
      pathname: '/c/local-chatgpt%3A422be40f-2871-4c05-9110-62ac6e7854cd',
      search: '',
      hash: '',
      href: localUrl
    },
    history: { state: null, replaceState() {} },
    chrome: {
      runtime: {
        async sendMessage() { throw new Error('provider proof must stay tab-local until a durable route exists'); },
        onMessage: { addListener(handler) { runtimeMessageHandler = handler; } }
      }
    },
    crypto: webcrypto,
    Date,
    setTimeout,
    clearTimeout,
    setInterval() { return 1; },
    clearInterval() {},
    console
  });

  vm.runInContext(contentSource, context, { filename: contentPath });
  windowMessageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: {
      source: 'moondesk-provider-correlation-observed',
      v: 1,
      correlation: { conversationId, requestIds: ['wfr_worker_finalize'] }
    }
  });
  const response = await new Promise((resolve) => {
    const keepAlive = runtimeMessageHandler({ type: 'MOONDESK_WORKER_EVIDENCE', taskMarker: marker }, null, resolve);
    assert.equal(keepAlive, false);
  });

  assert.equal(response.ok, true);
  assert.equal(response.evidence.conversationId, conversationId);
  assert.equal(response.evidence.conversationUrl, `https://chatgpt.com/c/${conversationId}`);
  assert.equal(JSON.parse(storage.get('moondesk-worker-launch-v1')).targetConversationId, conversationId);
});

test('background accepts provider correlation only from the exact sending ChatGPT tab', async () => {
  const conversationId = '9ac52738-64a0-83ee-bff8-59ffcc9f8c94';
  const requests = [];
  const fetchImpl = async (url, options = {}) => {
    requests.push({ url: String(url), options });
    if (String(url).endsWith('/__moondesk/companion/v1/hello')) {
      return {
        ok: true,
        status: 200,
        async json() { return { app: 'moondesk-worker-companion', protocolVersion: 2 }; },
        async text() { return JSON.stringify({ app: 'moondesk-worker-companion', protocolVersion: 2 }); }
      };
    }
    if (String(url).endsWith('/__moondesk/companion/v1/pair')) {
      return { ok: true, status: 200, async text() { return '{}'; } };
    }
    if (String(url).endsWith('/__moondesk/companion/v1/status')) {
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ clientId: 'client-provider', paired: true, protocolVersion: 2 }); }
      };
    }
    if (String(url).endsWith('/__moondesk/companion/v1/correlations')) {
      return { ok: true, status: 200, async text() { return JSON.stringify({ stored: 1 }); } };
    }
    return { ok: false, status: 404, async text() { return '{}'; }, async json() { return {}; } };
  };
  const { dispatchRuntimeMessage, evaluate } = loadBackground({ fetchImpl });
  await evaluate(`writeState({
    ...freshState(),
    baseUrl: 'http://127.0.0.1:47650',
    clientId: 'client-provider',
    credential: '${'a'.repeat(64)}'
  })`);

  const response = await dispatchRuntimeMessage({
    type: 'MOONDESK_PROVIDER_CORRELATION',
    correlation: { conversationId, requestIds: ['wfr_provider_exact'] }
  }, {
    tab: {
      id: 42,
      url: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/${conversationId}`
    }
  });
  assert.equal(response.ok, true);
  const posted = requests.find((request) => request.url.endsWith('/__moondesk/companion/v1/correlations'));
  assert.ok(posted, 'provider correlation must be published without waiting for the presence pump');
  assert.deepEqual(JSON.parse(posted.options.body), {
    conversationId,
    conversationUrl: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/${conversationId}`,
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project',
    requestIds: ['wfr_provider_exact']
  });

  const operationId = '22222222-3333-4444-8555-666666666666';
  const operationResponse = await dispatchRuntimeMessage({
    type: 'MOONDESK_PROVIDER_CORRELATION',
    correlation: { conversationId, operationIds: [operationId] }
  }, {
    tab: {
      id: 42,
      url: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/${conversationId}`
    }
  });
  assert.equal(operationResponse.ok, true);
  const operationPost = requests
    .filter((request) => request.url.endsWith('/__moondesk/companion/v1/correlations'))
    .at(-1);
  assert.deepEqual(JSON.parse(operationPost.options.body), {
    conversationId,
    conversationUrl: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/${conversationId}`,
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project',
    operationIds: [operationId]
  });

  const rejected = await dispatchRuntimeMessage({
    type: 'MOONDESK_PROVIDER_CORRELATION',
    correlation: { conversationId: 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee', requestIds: ['wfr_forged'] }
  }, {
    tab: { id: 42, url: `https://chatgpt.com/c/${conversationId}` }
  });
  assert.equal(rejected.ok, false);
});

test('MAIN-world correlation reader joins metadata.request_id only to the exact current Fiber conversation', () => {
  const conversationId = '6aad7eb1-4b10-83ee-97bd-d98b338864de';
  const staleConversationId = '7bbd8fc2-5c21-94ff-a8ce-e09c449975ef';

  const exactSection = {};
  exactSection.__reactFiber$test = {
    memoizedProps: {
      clientThreadId: conversationId,
      turn: {
        conversationId,
        messages: [
          { id: 'm1', metadata: { request_id: 'wfr_exact_request/attempt-7' } },
          { id: 'm2', metadata: { request_id: 'wfr_exact_request/attempt-7' } },
          { id: 'm3', metadata: { request_id: 'wfr_second_request' } }
        ]
      }
    },
    return: null
  };

  const staleSection = {};
  staleSection.__reactFiber$test = {
    memoizedProps: {
      clientThreadId: staleConversationId,
      turn: {
        conversationId: staleConversationId,
        messages: [{ id: 'stale', metadata: { request_id: 'wfr_stale_request' } }]
      }
    },
    return: null
  };

  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') messageHandler = handler;
    },
    postMessage(message) {
      posted.push(message);
    }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelectorAll(selector) {
        return selector.includes('section[data-testid^="conversation-turn"]')
          ? [staleSection, exactSection]
          : [];
      },
      querySelector() { return null; }
    },
    location: {
      origin: 'https://chatgpt.com',
      pathname: '/c/' + conversationId
    }
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  assert.equal(typeof messageHandler, 'function');
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-correlation-ask', nonce: 'nonce-1', v: 1 }
  });

  const reply = posted.find((message) => message.source === 'moondesk-correlation-reply');
  assert.ok(reply);
  assert.equal(reply.correlation.conversationId, conversationId);
  assert.deepEqual(
    Array.from(reply.correlation.requestIds),
    ['wfr_exact_request', 'wfr_second_request']
  );
  assert.ok(!reply.correlation.requestIds.includes('wfr_stale_request'));
});

test('MAIN-world correlation reader scans the current shell turn shape', () => {
  const conversationId = '8ccd9eb1-4b10-83ee-97bd-d98b338864de';
  const shellTurn = {};
  shellTurn.__reactFiber$shell = {
    memoizedProps: {
      clientThreadId: conversationId,
      turn: {
        conversationId,
        messages: [{ id: 'shell-message', metadata: { request_id: 'wfr_shell_request/attempt-2' } }]
      }
    },
    return: null
  };

  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') messageHandler = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelectorAll(selector) {
        return selector.includes('[data-turn-key]') ? [shellTurn] : [];
      },
      querySelector() { return null; }
    },
    location: {
      origin: 'https://chatgpt.com',
      pathname: '/c/' + conversationId
    }
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-correlation-ask', nonce: 'shell-correlation', v: 1 }
  });

  const reply = posted.find((message) => message.source === 'moondesk-correlation-reply');
  assert.ok(reply?.correlation, 'current shell turn must produce correlation evidence');
  assert.equal(reply.correlation.conversationId, conversationId);
  assert.deepEqual(Array.from(reply.correlation.requestIds), ['wfr_shell_request']);
});

test('MAIN-world model reader understands the current shell power-selection picker', () => {
  const trigger = {
    id: '',
    isConnected: true,
    __reactFiber$test: {
      memoizedProps: {
        selectedPowerSelection: {
          model: 'gpt-5.6-sol',
          modelLabel: 'GPT-5.6 Sol',
          powerSettingIndex: 2,
          labels: { effort: 'Max' },
          reasoningEffort: 'max'
        },
        powerSelections: [
          {
            model: 'gpt-5.6-sol',
            modelLabel: 'GPT-5.6 Sol',
            powerSettingIndex: 0,
            labels: { effort: 'Medium' },
            reasoningEffort: 'medium',
            availability: { status: 'available' }
          },
          {
            model: 'gpt-5.6-sol',
            modelLabel: 'GPT-5.6 Sol',
            powerSettingIndex: 1,
            labels: { effort: 'High' },
            reasoningEffort: 'high',
            availability: { status: 'available' }
          },
          {
            model: 'gpt-5.6-sol',
            modelLabel: 'GPT-5.6 Sol',
            powerSettingIndex: 2,
            labels: { effort: 'Max' },
            reasoningEffort: 'max',
            availability: { status: 'available' }
          }
        ],
        modelListConfig: {
          options: [{ id: 'gpt-5.6', label: 'GPT-5.6', selected: true }]
        },
        modelSelectionDisabled: false,
        modelSwitcherDenialsBySlug: {}
      },
      return: null
    },
    matches(selector) {
      return selector.includes('data-codex-intelligence-trigger') || selector.includes('button');
    },
    closest() { return null; },
    getClientRects() { return [{ width: 1, height: 1 }]; },
    attributes: Object.create(null),
    getAttribute(name) {
      if (name === 'data-codex-intelligence-trigger') return '';
      return this.attributes[name] ?? null;
    },
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; },
    textContent: 'Max'
  };
  const panel = {
    attributes: Object.create(null),
    getAttribute(name) { return this.attributes[name] ?? null; },
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; }
  };

  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') messageHandler = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelector(selector) {
        if (selector.includes('data-model-picker-view')) return panel;
        if (selector.includes('data-codex-intelligence-trigger')) return trigger;
        return null;
      },
      querySelectorAll(selector) {
        if (selector.includes('data-codex-intelligence-trigger')) return [trigger];
        return [];
      }
    },
    location: {
      origin: 'https://chatgpt.com',
      pathname: '/'
    }
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-picker-ask', nonce: 'shell-picker', v: 1 }
  });

  const reply = posted.find((message) => message.source === 'moondesk-picker-reply');
  assert.ok(reply?.picker, `new shell picker state must be readable: ${JSON.stringify(posted)}`);
  assert.equal(reply.picker.currentBucket, 2);
  assert.equal(reply.picker.choices.length, 3);
  assert.equal(reply.picker.choices[2].id, 'gpt-5.6-sol');
  assert.equal(reply.picker.choices[2].effort, 'max');
  assert.equal(trigger.attributes['data-moondesk-picker-route'], '/');
  assert.equal(panel.attributes['data-moondesk-selected-model'], 'gpt-5.6-sol');
  assert.equal(panel.attributes['data-moondesk-selected-effort'], 'max');
});

test('MAIN-world model reader follows React committed branch instead of a stale DOM Fiber pointer', () => {
  const state = { current: null };
  const oldRoot = { tag: 3, stateNode: state, return: null, child: null };
  const nextRoot = { tag: 3, stateNode: state, return: null, child: null };
  oldRoot.alternate = nextRoot;
  nextRoot.alternate = oldRoot;

  const shellProps = (effort, bucket) => ({
    selectedPowerSelection: {
      model: 'gpt-5.6-sol',
      modelLabel: 'GPT-5.6 Sol',
      powerSettingIndex: bucket,
      labels: { effort },
      reasoningEffort: effort.toLowerCase()
    },
    powerSelections: [
      {
        model: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        powerSettingIndex: 0,
        labels: { effort: 'Medium' },
        reasoningEffort: 'medium',
        availability: { status: 'available' }
      },
      {
        model: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        powerSettingIndex: 1,
        labels: { effort: 'High' },
        reasoningEffort: 'high',
        availability: { status: 'available' }
      }
    ],
    modelListConfig: { options: [{ id: 'gpt-5.6', label: 'GPT-5.6', selected: true }] },
    modelSelectionDisabled: false,
    modelSwitcherDenialsBySlug: {}
  });
  const oldFiber = { tag: 0, memoizedProps: shellProps('Medium', 0), return: oldRoot };
  const nextFiber = { tag: 0, memoizedProps: shellProps('High', 1), return: nextRoot };
  oldFiber.alternate = nextFiber;
  nextFiber.alternate = oldFiber;
  oldRoot.child = oldFiber;
  nextRoot.child = nextFiber;
  state.current = nextRoot;

  const trigger = {
    id: '',
    isConnected: true,
    __reactFiber$stale: oldFiber,
    attributes: Object.create(null),
    matches(selector) {
      return selector.includes('data-codex-intelligence-trigger') || selector.includes('button');
    },
    closest() { return null; },
    getClientRects() { return [{ width: 1, height: 1 }]; },
    getAttribute(name) {
      if (name === 'data-codex-intelligence-trigger') return '';
      return this.attributes[name] ?? null;
    },
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; },
    textContent: 'High'
  };
  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) { if (type === 'message') messageHandler = handler; },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelector(selector) {
        return selector.includes('data-codex-intelligence-trigger') ? trigger : null;
      },
      querySelectorAll(selector) {
        return selector.includes('data-codex-intelligence-trigger') ? [trigger] : [];
      }
    },
    location: { origin: 'https://chatgpt.com', pathname: '/' }
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-picker-ask', nonce: 'committed-picker', v: 1 }
  });

  const reply = posted.find((message) => message.source === 'moondesk-picker-reply');
  assert.equal(reply?.picker?.currentBucket, 1, 'reader must follow React root.current, not the stale DOM Fiber');
  assert.equal(reply?.picker?.choices?.[1]?.effort, 'high');
});

test('MAIN-world model reader reaches a committed picker more than 400 Fibers below React root', () => {
  const state = { current: null };
  const oldRoot = { tag: 3, stateNode: state, return: null, child: null };
  const nextRoot = { tag: 3, stateNode: state, return: null, child: null };
  oldRoot.alternate = nextRoot;
  nextRoot.alternate = oldRoot;

  let oldParent = oldRoot;
  let nextParent = nextRoot;
  for (let depth = 0; depth < 450; depth += 1) {
    const oldWrapper = { tag: 0, memoizedProps: {}, return: oldParent, child: null };
    const nextWrapper = { tag: 0, memoizedProps: {}, return: nextParent, child: null };
    oldWrapper.alternate = nextWrapper;
    nextWrapper.alternate = oldWrapper;
    oldParent.child = oldWrapper;
    nextParent.child = nextWrapper;
    oldParent = oldWrapper;
    nextParent = nextWrapper;
  }
  const pickerProps = (effort, bucket) => ({
    selectedPowerSelection: {
      model: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', powerSettingIndex: bucket,
      labels: { effort }, reasoningEffort: effort.toLowerCase()
    },
    powerSelections: [
      {
        model: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', powerSettingIndex: 0,
        labels: { effort: 'Medium' }, reasoningEffort: 'medium', availability: { status: 'available' }
      },
      {
        model: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', powerSettingIndex: 1,
        labels: { effort: 'High' }, reasoningEffort: 'high', availability: { status: 'available' }
      }
    ],
    modelListConfig: { options: [{ id: 'gpt-5.6', label: 'GPT-5.6', selected: true }] },
    modelSelectionDisabled: false,
    modelSwitcherDenialsBySlug: {}
  });
  const oldFiber = { tag: 0, memoizedProps: pickerProps('Medium', 0), return: oldParent };
  const nextFiber = { tag: 0, memoizedProps: pickerProps('High', 1), return: nextParent };
  oldFiber.alternate = nextFiber;
  nextFiber.alternate = oldFiber;
  oldParent.child = oldFiber;
  nextParent.child = nextFiber;
  state.current = nextRoot;

  const trigger = {
    id: '', isConnected: true, __reactFiber$deep: oldFiber, attributes: Object.create(null),
    matches(selector) { return selector.includes('data-codex-intelligence-trigger') || selector.includes('button'); },
    closest() { return null; },
    getClientRects() { return [{ width: 1, height: 1 }]; },
    getAttribute(name) { return name === 'data-codex-intelligence-trigger' ? '' : this.attributes[name] ?? null; },
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; },
    textContent: 'High'
  };
  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) { if (type === 'message') messageHandler = handler; },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelector(selector) { return selector.includes('data-codex-intelligence-trigger') ? trigger : null; },
      querySelectorAll(selector) { return selector.includes('data-codex-intelligence-trigger') ? [trigger] : []; }
    },
    location: { origin: 'https://chatgpt.com', pathname: '/' },
    structuredClone
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-picker-ask', nonce: 'deep-picker', v: 1 }
  });
  const reply = posted.find((message) => message.source === 'moondesk-picker-reply');
  assert.equal(reply?.picker?.currentBucket, 1);
});

test('MAIN-world model reader uses the reported current-shell trigger as closed-picker selection proof', () => {
  const trigger = {
    id: '',
    isConnected: true,
    __reactFiber$closed: {
      memoizedProps: { currentModelId: 'gpt-5.6-sol' },
      return: null
    },
    attributes: { 'data-selected-reasoning-effort': 'max' },
    matches(selector) {
      return selector.includes('data-codex-intelligence-trigger') || selector.includes('button');
    },
    closest() { return null; },
    getClientRects() { return [{ width: 1, height: 1 }]; },
    getAttribute(name) { return this.attributes[name] ?? null; },
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; },
    textContent: 'Max'
  };
  let messageHandler = null;
  const posted = [];
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') messageHandler = handler;
    },
    postMessage(message) { posted.push(message); }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: {
      querySelector() { return null; },
      querySelectorAll(selector) {
        return selector.includes('data-codex-intelligence-trigger') ? [trigger] : [];
      }
    },
    location: { origin: 'https://chatgpt.com', pathname: '/' }
  });

  vm.runInContext(modelStateMainSource, context, { filename: modelStateMainPath });
  messageHandler({
    source: pageWindow,
    origin: 'https://chatgpt.com',
    data: { source: 'moondesk-picker-ask', nonce: 'closed-picker', v: 1 }
  });

  const reply = posted.find((message) => message.source === 'moondesk-picker-reply');
  assert.deepEqual(
    JSON.parse(JSON.stringify(reply?.picker)),
    { selected: { id: 'gpt-5.6-sol', effort: 'max' } }
  );
  assert.equal(trigger.attributes['data-moondesk-picker-route'], '/');
  assert.equal(trigger.attributes['data-moondesk-selected-model'], 'gpt-5.6-sol');
  assert.equal(trigger.attributes['data-moondesk-selected-effort'], 'max');
});

test('isolated model readback fails closed when only an adjacent reasoning effort is offered', async () => {
  const picker = {
    version: 'gpt-5.6',
    currentBucket: 1,
    versions: [{ id: 'gpt-5.6', label: 'GPT-5.6' }],
    choices: [{
      bucket: 1,
      id: 'gpt-5.6-sol',
      label: 'GPT-5.6 Sol',
      familyId: 'gpt-5.6-sol',
      familyLabel: 'GPT-5.6 Sol',
      effort: 'medium',
      available: true
    }]
  };
  const listeners = new Set();
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') listeners.add(handler);
    },
    removeEventListener(type, handler) {
      if (type === 'message') listeners.delete(handler);
    },
    postMessage(message) {
      if (message?.source !== 'moondesk-picker-ask') return;
      for (const handler of [...listeners]) {
        handler({
          source: pageWindow,
          origin: 'https://chatgpt.com',
          data: { source: 'moondesk-picker-reply', nonce: message.nonce, v: 1, picker }
        });
      }
    }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { querySelector() { return null; }, querySelectorAll() { return []; } },
    location: { origin: 'https://chatgpt.com', pathname: '/' },
    crypto: webcrypto,
    URL,
    setTimeout,
    clearTimeout
  });

  vm.runInContext(chatgptDomSource, context, { filename: chatgptDomPath });
  const selection = await pageWindow.MOONDESK_CHATGPT_DOM.selectedModelAndEffort({
    modelKey: 'gpt-5.6-sol',
    modelLabel: 'GPT-5.6 Sol',
    reasoningEffort: 'high'
  });
  assert.equal(selection, null, 'requested High must never be silently replaced by Medium');
});

test('isolated model readback accepts a legacy Extra High profile when the current shell offers Max', async () => {
  const picker = {
    version: 'gpt-5.6',
    currentBucket: 2,
    versions: [{ id: 'gpt-5.6', label: 'GPT-5.6' }],
    choices: [{
      bucket: 2,
      id: 'gpt-5.6-sol',
      label: 'GPT-5.6 Sol',
      familyId: 'gpt-5.6-sol',
      familyLabel: 'GPT-5.6 Sol',
      effort: 'max',
      available: true
    }]
  };
  const listeners = new Set();
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') listeners.add(handler);
    },
    removeEventListener(type, handler) {
      if (type === 'message') listeners.delete(handler);
    },
    postMessage(message) {
      if (message?.source !== 'moondesk-picker-ask') return;
      for (const handler of [...listeners]) {
        handler({
          source: pageWindow,
          origin: 'https://chatgpt.com',
          data: { source: 'moondesk-picker-reply', nonce: message.nonce, v: 1, picker }
        });
      }
    }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { querySelector() { return null; }, querySelectorAll() { return []; } },
    location: { origin: 'https://chatgpt.com', pathname: '/' },
    crypto: webcrypto,
    URL,
    setTimeout,
    clearTimeout
  });

  vm.runInContext(chatgptDomSource, context, { filename: chatgptDomPath });
  const selection = await pageWindow.MOONDESK_CHATGPT_DOM.selectedModelAndEffort({
    modelKey: 'gpt-5.6-sol',
    modelLabel: 'GPT-5.6 Sol',
    reasoningEffort: 'extra_high'
  });
  assert.deepEqual(
    JSON.parse(JSON.stringify(selection)),
    { model: 'gpt-5.6-sol', reasoningEffort: 'max' }
  );
});

test('legacy GPT-5.6 Sol default resolves to ChatGPT live gpt-5-6-thinking execution id', async () => {
  const picker = {
    version: '5.6',
    currentBucket: 1,
    versions: [{ id: '5.6', label: '5.6' }],
    choices: [{
      bucket: 1,
      id: 'gpt-5-6-thinking',
      label: '5.6 Thinking',
      familyId: '5.6',
      familyLabel: '5.6',
      effort: 'high',
      available: true
    }]
  };
  const listeners = new Set();
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') listeners.add(handler);
    },
    removeEventListener(type, handler) {
      if (type === 'message') listeners.delete(handler);
    },
    postMessage(message) {
      if (message?.source !== 'moondesk-picker-ask') return;
      for (const handler of [...listeners]) {
        handler({
          source: pageWindow,
          origin: 'https://chatgpt.com',
          data: { source: 'moondesk-picker-reply', nonce: message.nonce, v: 1, picker }
        });
      }
    }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { querySelector() { return null; }, querySelectorAll() { return []; } },
    location: { origin: 'https://chatgpt.com', pathname: '/' },
    crypto: webcrypto,
    URL,
    setTimeout,
    clearTimeout
  });

  vm.runInContext(chatgptDomSource, context, { filename: chatgptDomPath });
  const selection = await pageWindow.MOONDESK_CHATGPT_DOM.selectedModelAndEffort({
    modelKey: 'gpt-5.6-sol',
    modelLabel: 'GPT-5.6 Sol',
    reasoningEffort: 'high'
  });
  assert.deepEqual(
    JSON.parse(JSON.stringify(selection)),
    { model: 'gpt-5-6-thinking', reasoningEffort: 'high' }
  );
});

test('legacy GPT-5.6 Sol compatibility never admits the separate Pro execution id', async () => {
  const picker = {
    version: '5.6',
    currentBucket: 2,
    versions: [{ id: '5.6', label: '5.6' }],
    choices: [{
      bucket: 2,
      id: 'gpt-5-6-pro',
      label: '5.6 Pro',
      familyId: '5.6-pro',
      familyLabel: '5.6 Pro',
      effort: 'pro',
      available: true
    }]
  };
  const listeners = new Set();
  const pageWindow = {
    addEventListener(type, handler) {
      if (type === 'message') listeners.add(handler);
    },
    removeEventListener(type, handler) {
      if (type === 'message') listeners.delete(handler);
    },
    postMessage(message) {
      if (message?.source !== 'moondesk-picker-ask') return;
      for (const handler of [...listeners]) {
        handler({ source: pageWindow, origin: 'https://chatgpt.com', data: {
          source: 'moondesk-picker-reply', nonce: message.nonce, v: 1, picker
        } });
      }
    }
  };
  const context = vm.createContext({
    window: pageWindow,
    document: { querySelector() { return null; }, querySelectorAll() { return []; } },
    location: { origin: 'https://chatgpt.com', pathname: '/' },
    crypto: webcrypto,
    URL,
    setTimeout,
    clearTimeout
  });

  vm.runInContext(chatgptDomSource, context, { filename: chatgptDomPath });
  const selection = await pageWindow.MOONDESK_CHATGPT_DOM.selectedModelAndEffort({
    modelKey: 'gpt-5.6-sol',
    modelLabel: 'GPT-5.6 Sol',
    reasoningEffort: 'high'
  });
  assert.equal(selection, null);
});

test('model catalog merges duplicate execution slugs into one visible family with all efforts', () => {
  const { evaluate } = loadBackground();
  const normalize = evaluate('normalizeModelCatalog');
  const raw = [
    {
      id: 'gpt-5.6-instant',
      label: '5.6',
      efforts: ['none'],
      aliases: ['gpt-5.6-instant'],
      choices: [{ id: 'gpt-5.6-instant', effort: 'none' }]
    },
    {
      id: 'gpt-5.6-sol',
      label: '5.6',
      efforts: ['medium', 'high'],
      aliases: ['gpt-5.6-sol'],
      choices: [
        { id: 'gpt-5.6-sol', effort: 'medium' },
        { id: 'gpt-5.6-sol', effort: 'high' }
      ]
    },
    {
      id: 'gpt-5.5-instant',
      label: 'GPT-5.5',
      efforts: ['none'],
      aliases: ['gpt-5.5-instant'],
      choices: [{ id: 'gpt-5.5-instant', effort: 'none' }]
    },
    {
      id: 'gpt-5.5',
      label: '5.5',
      efforts: ['medium', 'high'],
      aliases: ['gpt-5.5'],
      choices: [
        { id: 'gpt-5.5', effort: 'medium' },
        { id: 'gpt-5.5', effort: 'high' }
      ]
    }
  ];

  const catalog = JSON.parse(JSON.stringify(normalize(raw)));
  assert.equal(catalog.length, 2);
  assert.deepEqual(catalog.map((entry) => entry.label), ['5.6', 'GPT-5.5']);
  assert.deepEqual(catalog[0].efforts, ['none', 'medium', 'high']);
  assert.deepEqual(catalog[0].choices, [
    { id: 'gpt-5.6-instant', effort: 'none' },
    { id: 'gpt-5.6-sol', effort: 'medium' },
    { id: 'gpt-5.6-sol', effort: 'high' }
  ]);
  assert.deepEqual(catalog[1].efforts, ['none', 'medium', 'high']);
  assert.deepEqual(catalog[1].choices, [
    { id: 'gpt-5.5-instant', effort: 'none' },
    { id: 'gpt-5.5', effort: 'medium' },
    { id: 'gpt-5.5', effort: 'high' }
  ]);
});

test('popup defaults discovered workers to GPT-5.6 Sol High instead of catalog order', () => {
  const makeSelect = () => {
    const select = {
      options: [],
      value: '',
      append(option) {
        this.options.push(option);
        if (this.options.length === 1) this.value = option.value;
      }
    };
    Object.defineProperty(select, 'textContent', {
      get() { return ''; },
      set() { select.options = []; select.value = ''; }
    });
    return select;
  };
  const elements = {
    model: makeSelect(),
    effort: makeSelect(),
    modelLabel: { hidden: true },
    effortLabel: { hidden: true },
    saveProfile: { hidden: true },
    catalogStatus: { textContent: '' }
  };
  const document = {
    getElementById(id) { return elements[id]; },
    createElement() { return { value: '', textContent: '' }; }
  };
  const prefix = popupSource.slice(0, popupSource.indexOf('async function render()'));
  const context = vm.createContext({ document, console });
  vm.runInContext(prefix, context, { filename: popupPath });
  const catalog = [
    {
      id: '5.5', label: '5.5', choices: [
        { id: 'gpt-5.5-instant', effort: 'none' },
        { id: 'gpt-5.5', effort: 'medium' },
        { id: 'gpt-5.5', effort: 'high' }
      ]
    },
    {
      id: '5.6', label: '5.6', choices: [
        { id: 'gpt-5.6-instant', effort: 'none' },
        { id: 'gpt-5.6-sol', effort: 'medium' },
        { id: 'gpt-5.6-sol', effort: 'high' }
      ]
    }
  ];

  vm.runInContext(`modelCatalog = ${JSON.stringify(catalog)}; currentProfile = null; renderCatalog();`, context);
  assert.equal(elements.model.value, '5.6');
  assert.equal(elements.effort.value, 'high');

  vm.runInContext(`currentProfile = { modelKey: 'gpt-5.5', modelLabel: '5.5', reasoningEffort: 'medium' }; renderCatalog();`, context);
  assert.equal(elements.model.value, '5.5', 'an explicitly saved profile remains authoritative');
  assert.equal(elements.effort.value, 'medium');
});

test('model discovery runs in one owned clean helper tab and closes it after a confirmed catalog', async () => {
  const existingTabs = {
    7: { id: 7, url: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/anchor', active: true }
  };
  const seen = [];
  const catalog = [{
    id: 'gpt-5.6-sol',
    label: 'GPT-5.6 Sol',
    efforts: ['high'],
    aliases: ['gpt-5.6-sol'],
    choices: [{ id: 'gpt-5.6-sol', effort: 'high' }]
  }];
  const { createdTabs, removedTabs, evaluate } = loadBackground({
    existingTabs,
    scriptingExecuteImpl: async () => [{ result: false }],
    sendMessageImpl: async (id, message, { existingTabs: tabs }) => {
      seen.push({ id, message });
      if (message.type === 'MOONDESK_CONTEXT') return { ok: true, context: {} };
      if (message.type === 'MOONDESK_MODEL_CATALOG') {
        const helper = new URL(tabs[id].url);
        assert.equal(helper.origin, 'https://chatgpt.com');
        assert.equal(helper.pathname, '/');
        assert.equal(helper.searchParams.get('moondesk-model-catalog'), message.nonce);
        assert.ok(Number.isFinite(message.expiresAt) && message.expiresAt > Date.now());
        // ChatGPT may rewrite the helper route while the picker is open. The disposable helper
        // still belongs to this discovery flight and must be closed by its exact created tab id.
        tabs[id].url = 'https://chatgpt.com/c/helper-route-rewritten-by-chatgpt';
        return { ok: true, catalog };
      }
      throw new Error('unexpected message');
    }
  });

  await flushMicrotasks();
  await flushMicrotasks();
  const result = await evaluate('discoverModelCatalog({ force: true })');
  assert.deepEqual(JSON.parse(JSON.stringify(result)), catalog);
  assert.equal(createdTabs.length, 1);
  assert.equal(createdTabs[0].active, false, 'discovery must not steal focus from the Core');
  assert.match(createdTabs[0].url, /^https:\/\/chatgpt\.com\/\?moondesk-model-catalog=/);
  assert.deepEqual(removedTabs, [createdTabs[0].id]);
  assert.equal(existingTabs[7].url.includes('/c/anchor'), true, 'active Core tab must remain untouched');
  assert.equal(seen.filter((entry) => entry.message.type === 'MOONDESK_MODEL_CATALOG').length, 1);
});

test('model discovery closes its owned helper even when Chromium assigns tab id zero', async () => {
  const catalog = [{
    id: 'gpt-5.6-sol',
    label: 'GPT-5.6 Sol',
    efforts: ['high'],
    aliases: ['gpt-5.6-sol'],
    choices: [{ id: 'gpt-5.6-sol', effort: 'high' }]
  }];
  const { removedTabs, evaluate } = loadBackground({
    createdTabIdStart: 0,
    scriptingExecuteImpl: async () => [{ result: false }],
    sendMessageImpl: async (_id, message) => {
      if (message.type === 'MOONDESK_CONTEXT') return { ok: true, context: {} };
      if (message.type === 'MOONDESK_MODEL_CATALOG') return { ok: true, catalog };
      throw new Error('unexpected message');
    }
  });

  await flushMicrotasks();
  await flushMicrotasks();
  await evaluate('discoverModelCatalog({ force: true })');
  assert.deepEqual(removedTabs, [0]);
});

test('automatic model discovery waits for ChatGPT, persists the catalog, and avoids repeated picker work', async () => {
  const existingTabs = {};
  const catalog = [{
    id: 'gpt-5.6-sol',
    label: 'GPT-5.6 Sol',
    efforts: ['high'],
    aliases: ['gpt-5.6-sol'],
    choices: [{ id: 'gpt-5.6-sol', effort: 'high' }]
  }];
  let signedIn = false;
  const { createdTabs, evaluate, dispatchTabUpdated, dispatchAlarm } = loadBackground({
    existingTabs,
    scriptingExecuteImpl: async () => [{ result: signedIn }],
    sendMessageImpl: async (id, message, { existingTabs: tabs }) => {
      if (message.type === 'MOONDESK_CONTEXT') return { ok: true, context: {} };
      if (message.type !== 'MOONDESK_MODEL_CATALOG') throw new Error('unexpected message');
      assert.ok(tabs[id].url.includes('moondesk-model-catalog='));
      return { ok: true, catalog };
    }
  });

  await flushMicrotasks();
  assert.equal(createdTabs.length, 0, 'startup without ChatGPT must wait instead of opening a browser tab');

  existingTabs[7] = { id: 7, url: 'https://chatgpt.com/c/anchor', active: true, status: 'complete' };
  dispatchTabUpdated(7, { status: 'complete' }, existingTabs[7]);
  await flushMicrotasks();
  assert.equal(createdTabs.length, 0, 'signed-out ChatGPT must not trigger picker discovery');

  signedIn = true;
  dispatchTabUpdated(7, { status: 'complete' }, existingTabs[7]);
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(createdTabs.length, 1, 'opening a signed-in ChatGPT page later must trigger automatic discovery');
  const cached = await evaluate('readModelCatalogCache()');
  assert.deepEqual(JSON.parse(JSON.stringify(cached.catalog)), catalog);
  assert.ok(Number.isFinite(cached.updatedAt));

  dispatchAlarm();
  await flushMicrotasks();
  assert.equal(createdTabs.length, 1, 'the periodic fallback must reuse a fresh cache instead of reopening the picker');
});

test('extension reload reuses a fresh model catalog instead of opening another helper tab', async () => {
  const catalog = [{
    id: 'gpt-5.6-sol',
    label: 'GPT-5.6 Sol',
    efforts: ['high'],
    aliases: ['gpt-5.6-sol'],
    choices: [{ id: 'gpt-5.6-sol', effort: 'high' }]
  }];
  const initialStored = {
    moondeskWorkerModelCatalogV1: {
      catalog,
      updatedAt: Date.now(),
      lastAttemptAt: Date.now()
    }
  };
  const { createdTabs, evaluate } = loadBackground({ initialStored });

  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(createdTabs.length, 0, 'service-worker reload must not repeat picker discovery while cache is fresh');
  const cached = await evaluate('readModelCatalogCache()');
  assert.deepEqual(JSON.parse(JSON.stringify(cached.catalog)), catalog);
});

test('manual model refresh bypasses the automatic cache while stale automatic cache revalidates', async () => {
  const existingTabs = {
    7: { id: 7, url: 'https://chatgpt.com/c/anchor', active: true, status: 'complete' }
  };
  const catalog = [{
    id: 'gpt-5.6-sol', label: 'GPT-5.6 Sol', efforts: ['high'], aliases: ['gpt-5.6-sol'],
    choices: [{ id: 'gpt-5.6-sol', effort: 'high' }]
  }];
  let signedIn = false;
  const { createdTabs, evaluate } = loadBackground({
    existingTabs,
    scriptingExecuteImpl: async () => [{ result: signedIn }],
    sendMessageImpl: async (_id, message) => {
      if (message.type === 'MOONDESK_CONTEXT') return { ok: true, context: {} };
      if (message.type !== 'MOONDESK_MODEL_CATALOG') throw new Error('unexpected message');
      return { ok: true, catalog };
    }
  });

  await flushMicrotasks();
  signedIn = true;
  await evaluate(`writeModelCatalogCache(${JSON.stringify({ catalog, updatedAt: Date.now(), lastAttemptAt: 0 })})`);
  await evaluate('discoverModelCatalog({ requireExistingPage: true })');
  assert.equal(createdTabs.length, 0, 'fresh automatic cache should be reused');

  await evaluate('discoverModelCatalog({ force: true })');
  assert.equal(createdTabs.length, 1, 'explicit repair refresh must inspect again');

  await evaluate(`writeModelCatalogCache(${JSON.stringify({ catalog, updatedAt: Date.now() - 7 * 60 * 60 * 1000, lastAttemptAt: 0 })})`);
  await evaluate('discoverModelCatalog({ requireExistingPage: true })');
  assert.equal(createdTabs.length, 2, 'stale automatic cache should be revalidated when ChatGPT is available');
});

test('ChatGPT URL routing extracts exact conversation and Project IDs without using visible names', () => {
  const { evaluate } = loadBackground();
  const parse = evaluate('chatContextFromUrl');

  const project = parse(
    'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-totally-unrelated-visible-name/c/6aad7eb1-4b10-83ee-97bd-d98b338864de'
  );
  assert.equal(project.conversationId, '6aad7eb1-4b10-83ee-97bd-d98b338864de');
  assert.equal(project.projectId, 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
  assert.equal(
    project.projectUrl,
    'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-totally-unrelated-visible-name/project'
  );

  const sharedProject = parse(
    'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-totally-unrelated-visible-name/shared/c/6aad7eb1-4b10-83ee-97bd-d98b338864de'
  );
  assert.equal(sharedProject.conversationId, '6aad7eb1-4b10-83ee-97bd-d98b338864de');
  assert.equal(sharedProject.projectId, 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
  assert.equal(
    sharedProject.projectUrl,
    'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-totally-unrelated-visible-name/project'
  );

  const normal = parse('https://chatgpt.com/c/6aad7eb1-4b10-83ee-97bd-d98b338864de');
  assert.equal(normal.projectId, null);
  assert.equal(normal.projectUrl, null);
  assert.equal(parse('https://chatgpt.com/g/not-a-project/c/6aad7eb1-4b10-83ee-97bd-d98b338864de'), null);
});

test('presence reports the generating conversation independently of browser focus', async () => {
  const generatingConversation = '7bbd8fc2-5c21-94ff-a8ce-e09c449975ef';
  const focusedConversation = '6aad7eb1-4b10-83ee-97bd-d98b338864de';
  let presenceBody = null;
  const fetchImpl = async (url, options = {}) => {
    if (new URL(url).pathname === '/__moondesk/companion/v1/presence') {
      presenceBody = JSON.parse(options.body);
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ presence: presenceBody }); }
      };
    }
    throw new Error('unexpected network request');
  };
  const existingTabs = {
    1: { id: 1, url: 'https://chatgpt.com/c/' + focusedConversation, active: true, windowId: 1 },
    2: { id: 2, url: 'https://chatgpt.com/c/' + generatingConversation, active: false, windowId: 2 }
  };
  const contentByTab = {
    1: { ok: true, context: { conversationId: focusedConversation, generating: false } },
    2: { ok: true, context: { conversationId: generatingConversation, generating: true } }
  };
  const { evaluate } = loadBackground({ existingTabs, contentByTab, fetchImpl });
  await evaluate(
    "collectPresence({ baseUrl: 'http://127.0.0.1:47650', credential: 'token' })"
  );

  assert.ok(presenceBody);
  const focused = presenceBody.tabs.find((tab) => tab.conversationId === focusedConversation);
  const generating = presenceBody.tabs.find((tab) => tab.conversationId === generatingConversation);
  assert.equal(focused.windowFocused, true);
  assert.equal(focused.generating, false);
  assert.equal(generating.windowFocused, false);
  assert.equal(generating.generating, true);
});

test('existing worker placement survives without Core presence by using the host durable conversation URL', () => {
  const { evaluate } = loadBackground();
  const placementForOffer = evaluate('placementForOffer');
  const workerUrl = 'https://chatgpt.com/c/6aad7eb1-4b10-83ee-97bd-d98b338864de';
  const state = { bindings: {}, launchRecords: {}, threadRecords: {} };
  const offer = {
    anchorContext: null,
    command: {
      launch: {
        workspaceId: 'workspace-a',
        threadKey: 'worker:pinned',
        openMode: 'existing_thread',
        existingConversationUrl: workerUrl
      }
    }
  };
  const placement = placementForOffer(state, offer);
  assert.equal(placement.projectId, null);
  assert.equal(placement.projectUrl, null);
});

test('fresh workers always start from normal ChatGPT home regardless of Core Project placement', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const anchorConversationId = '11111111-2222-4333-8444-555555555555';
  const projectPlacement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-any-human-name/project',
    anchorConversationUrl: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-any-human-name/c/${anchorConversationId}`
  };
  const normalPlacement = {
    projectId: null,
    projectUrl: null,
    anchorConversationUrl: 'https://chatgpt.com/c/anchor-normal'
  };

  assert.equal(sourceUrlForCommand(projectPlacement, 'new_thread', null), 'https://chatgpt.com/');
  assert.equal(sourceUrlForCommand(normalPlacement, 'new_thread', null), 'https://chatgpt.com/');
  assert.equal(
    sourceUrlForCommand(projectPlacement, 'new_thread', null).includes(anchorConversationId),
    false,
    'a fresh worker URL must never contain the Core conversation id'
  );
});

test('existing worker threads always reuse the confirmed worker conversation', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const confirmed = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation';
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/c/anchor'
  };
  assert.equal(sourceUrlForCommand(placement, 'existing_thread', confirmed), confirmed);
});

test('temporary local-chatgpt routes are never accepted as durable worker conversation URLs', () => {
  const { evaluate } = loadBackground();
  const canonicalConversationUrl = evaluate('canonicalConversationUrl');
  const conversationUrlFromEvidence = evaluate('conversationUrlFromEvidence');
  const conversationId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee4';

  assert.equal(
    canonicalConversationUrl('https://chatgpt.com/c/local-chatgpt%3A422be40f-2871-4c05-9110-62ac6e7854cd'),
    null
  );
  assert.equal(
    canonicalConversationUrl(`https://chatgpt.com/c/${conversationId}`),
    `https://chatgpt.com/c/${conversationId}`
  );
  assert.equal(
    conversationUrlFromEvidence({ conversationId }),
    `https://chatgpt.com/c/${conversationId}`
  );
});

test('successful send without a confirmed conversation URL must reconcile before terminal success', () => {
  const { evaluate } = loadBackground();
  const needsReconcile = evaluate('successfulResultNeedsConversationReconcile');

  assert.equal(needsReconcile({ state: 'succeeded' }, null), true);
  assert.equal(
    needsReconcile(
      { state: 'succeeded' },
      'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-thread'
    ),
    false
  );
  assert.equal(needsReconcile({ state: 'failed' }, null), false);
});

test('uncertain reconciliation never adopts the inspected source conversation as the worker thread', () => {
  const { evaluate } = loadBackground();
  const shouldAdoptConversation = evaluate('shouldAdoptConversation');

  assert.equal(
    shouldAdoptConversation({ reconcileRequired: true }, { state: 'needs_reconcile' }),
    false
  );
  assert.equal(
    shouldAdoptConversation({ reconcileRequired: true }, { state: 'succeeded' }),
    true
  );
  assert.equal(
    shouldAdoptConversation({ reconcileRequired: false }, { state: 'needs_reconcile' }),
    true
  );
});

test('record creation opens every fresh worker on normal ChatGPT home and never clones the Core', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-1',
    launch: {
      workspaceId: 'workspace-1',
      taskMarker: 'marker-1',
      threadKey: 'worker:test-thread',
      openMode: 'new_thread',
      executionProfile: {
        modelKey: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };

  const anchorConversationId = '11111111-2222-4333-8444-555555555555';
  const projectPlacement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unrelated-name/project',
    anchorConversationUrl: `https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unrelated-name/c/${anchorConversationId}`
  };
  const state = { launchRecords: {}, threadRecords: {} };
  const { record } = await recordForCommand(state, command, projectPlacement);
  assert.equal(record.sourceUrl, 'https://chatgpt.com/');
  const projectCoreWorkerUrl = new URL(createdTabs[0].url);
  assert.equal(projectCoreWorkerUrl.origin + projectCoreWorkerUrl.pathname, 'https://chatgpt.com/');
  assert.equal(projectCoreWorkerUrl.href.includes(anchorConversationId), false);
  assert.equal(projectCoreWorkerUrl.searchParams.get('moondesk-project-entry'), null);
  assert.equal(projectCoreWorkerUrl.searchParams.get('moondesk-launch'), record.launchToken);
  assert.equal(projectCoreWorkerUrl.searchParams.get('model'), 'gpt-5.6-sol');
  assert.equal(projectCoreWorkerUrl.searchParams.get('reasoning_effort'), 'high');
  assert.equal(projectCoreWorkerUrl.hash, `#moondesk-launch=${encodeURIComponent(record.launchToken)}`);

  const normalState = { launchRecords: {}, threadRecords: {} };
  const normalCommand = {
    ...command,
    id: 'command-2',
    launch: { ...command.launch, taskMarker: 'marker-2' }
  };
  const normalPlacement = {
    projectId: null,
    projectUrl: null,
    anchorConversationUrl: 'https://chatgpt.com/c/anchor-normal'
  };
  const normal = await recordForCommand(normalState, normalCommand, normalPlacement);
  assert.equal(normal.record.sourceUrl, 'https://chatgpt.com/');
  const normalUrl = new URL(createdTabs[1].url);
  assert.equal(normalUrl.origin + normalUrl.pathname, 'https://chatgpt.com/');
  assert.equal(normalUrl.searchParams.get('model'), 'gpt-5.6-sol');
  assert.equal(normalUrl.searchParams.get('reasoning_effort'), 'high');
  assert.equal(normalUrl.searchParams.get('moondesk-launch'), normal.record.launchToken);
  assert.equal(normalUrl.hash, `#moondesk-launch=${encodeURIComponent(normal.record.launchToken)}`);
});

test('reconciliation without a durable launch record never creates a fresh worker tab', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-reconcile-missing-record',
    launch: {
      workspaceId: 'workspace-1',
      taskMarker: 'marker-reconcile-missing-record',
      threadKey: 'worker:reconcile-missing-record',
      openMode: 'new_thread'
    }
  };
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/anchor'
  };

  await assert.rejects(
    recordForCommand({ launchRecords: {}, threadRecords: {} }, command, placement, true),
    /no durable browser launch record/
  );
  assert.equal(createdTabs.length, 0);
});

test('reconciliation without a confirmed conversation URL never reopens the Core or Project home', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-reconcile-unconfirmed-conversation',
    launch: {
      workspaceId: 'workspace-1',
      taskMarker: 'marker-reconcile-unconfirmed-conversation',
      threadKey: 'worker:reconcile-unconfirmed-conversation',
      openMode: 'new_thread'
    }
  };
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/anchor'
  };
  const state = {
    launchRecords: {
      [command.id]: {
        commandId: command.id,
        workspaceId: command.launch.workspaceId,
        taskMarker: command.launch.taskMarker,
        threadKey: command.launch.threadKey,
        openMode: 'new_thread',
        launchToken: '0949b810-b739-49ab-bd7b-80fb2a57f11c',
        sourceUrl: placement.projectUrl,
        tabId: null,
        conversationUrl: null,
        phase: 'uncertain',
        reconcileAttempts: 2
      }
    },
    threadRecords: {}
  };

  await assert.rejects(
    recordForCommand(state, command, placement, true),
    /no confirmed worker conversation URL/
  );
  assert.equal(createdTabs.length, 0);
});

test('new-thread recovery never matches an older launch only by durable thread key', () => {
  const { evaluate } = loadBackground();
  const matches = evaluate('rememberedLaunchMatchesRecord');
  const newThread = {
    commandId: 'command-new',
    openMode: 'new_thread',
    threadKey: 'worker:shared'
  };
  const existingThread = {
    commandId: 'command-reuse',
    openMode: 'existing_thread',
    threadKey: 'worker:shared'
  };
  const rememberedOld = {
    commandId: 'command-old',
    threadKey: 'worker:shared'
  };

  assert.equal(matches(newThread, rememberedOld), false);
  assert.equal(matches(existingThread, rememberedOld), true);
  assert.equal(matches(newThread, { commandId: 'command-new', threadKey: 'different' }), true);
});

test('existing-thread reuse treats the host durable URL as authority instead of another workspace local record', async () => {
  const durableWorkerUrl = 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee1';
  const unrelatedWorkerUrl = 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee9';
  const { createdTabs, evaluate } = loadBackground({
    existingTabs: { 77: { id: 77, url: unrelatedWorkerUrl } }
  });
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-reuse',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-reuse',
      threadKey: 'worker:reuse',
      openMode: 'existing_thread',
      existingConversationUrl: durableWorkerUrl
    }
  };
  const state = {
    launchRecords: {},
    threadRecords: {
      'worker:reuse': {
        workspaceId: 'workspace-other',
        projectId: null,
        conversationUrl: unrelatedWorkerUrl,
        tabId: 77
      }
    }
  };

  const reused = await recordForCommand(state, command, null);
  assert.equal(reused.record.conversationUrl, durableWorkerUrl);
  assert.notEqual(reused.tab.id, 77);
  assert.equal(createdTabs.length, 1);
  assert.equal(new URL(createdTabs[0].url).pathname, '/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee1');
});

test('existing-thread reuse never adopts a remembered local-chatgpt placeholder instead of its durable conversation', async () => {
  const durableUrl = 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee5';
  const localUrl = 'https://chatgpt.com/c/local-chatgpt%3A422be40f-2871-4c05-9110-62ac6e7854cd';
  const { createdTabs, evaluate } = loadBackground({
    existingTabs: { 77: { id: 77, url: localUrl } },
    contentByTab: {
      77: {
        ok: true,
        rememberedLaunch: { commandId: 'old-command', threadKey: 'worker:durable-reuse' }
      }
    }
  });
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-durable-reuse',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-durable-reuse',
      threadKey: 'worker:durable-reuse',
      openMode: 'existing_thread',
      existingConversationUrl: durableUrl,
      executionProfile: {
        modelKey: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };
  const state = {
    launchRecords: {},
    threadRecords: {
      'worker:durable-reuse': {
        workspaceId: 'workspace-current',
        projectId: null,
        conversationUrl: durableUrl,
        tabId: 77
      }
    }
  };

  const { record, tab } = await recordForCommand(state, command, {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee6'
  });
  assert.equal(record.conversationUrl, durableUrl);
  assert.notEqual(tab.id, 77, 'the placeholder tab must not be reused by remembered thread key alone');
  assert.equal(createdTabs.length, 1);
  assert.equal(new URL(createdTabs[0].url).pathname, '/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee5');
});

test('existing-thread reuse survives companion reinstall with empty local thread and launch records', async () => {
  const workerUrl = 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee2';
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const state = { launchRecords: {}, threadRecords: {} };
  const command = {
    id: 'command-reuse-recovered',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-reuse-recovered',
      threadKey: 'worker:recover',
      openMode: 'existing_thread',
      existingConversationUrl: workerUrl,
      executionProfile: {
        modelKey: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };

  const { record, tab } = await recordForCommand(state, command, null);
  assert.equal(record.sourceUrl, workerUrl);
  assert.equal(record.conversationUrl, workerUrl);
  assert.equal(createdTabs.length, 1);
  assert.equal(tab.id, createdTabs[0].id);
  const opened = new URL(createdTabs[0].url);
  assert.equal(opened.pathname, '/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee2');
  assert.equal(opened.searchParams.get('moondesk-launch'), record.launchToken);
});

test('existing-thread reuse accepts the exact durable thread owned by the same workspace', async () => {
  const conversationUrl = 'https://chatgpt.com/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee3';
  const { createdTabs, evaluate } = loadBackground({
    existingTabs: {
      77: { id: 77, url: conversationUrl }
    }
  });
  const recordForCommand = evaluate('recordForCommand');
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/c/anchor'
  };
  const command = {
    id: 'command-reuse-owned',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-reuse-owned',
      threadKey: 'worker:reuse-owned',
      openMode: 'existing_thread',
      existingConversationUrl: conversationUrl
    }
  };
  const state = {
    launchRecords: {},
    threadRecords: {
      'worker:reuse-owned': {
        workspaceId: command.launch.workspaceId,
        projectId: placement.projectId,
        conversationUrl,
        tabId: 77
      }
    }
  };

  const { record, tab } = await recordForCommand(state, command, placement);
  assert.equal(record.conversationUrl, conversationUrl);
  assert.equal(record.sourceUrl, conversationUrl);
  assert.equal(tab.id, 77);
  assert.equal(createdTabs.length, 0);
});

test('existing-thread preparation waits for the recovered conversation composer before continuing', async () => {
  let messageHandler = null;
  let waitCalls = 0;
  let selectCalls = 0;
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return 'worker-conversation'; },
    projectIdFromPath() { return 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; },
    async waitForComposerReady(timeoutMs) {
      waitCalls += 1;
      assert.equal(timeoutMs, 15000);
      return true;
    },
    composerReady() {
      throw new Error('reuse must use the bounded readiness wait instead of a one-shot check');
    },
    async selectModelSettings(_profile, _failure, stillCurrent) {
      selectCalls += 1;
      assert.equal(typeof stillCurrent, 'function');
      assert.equal(stillCurrent(), true);
      return true;
    },
    visibleModelSelection() {
      return { model: 'gpt-5.6-sol', reasoningEffort: 'high' };
    },
    async selectedModelAndEffort() {
      throw new Error('worker preparation must not reopen the picker after successful selection');
    },
    taskMarkerPresent() { return false; },
    insertPrompt() { return true; },
    workerEvidence() {
      return {
        conversationId: 'worker-conversation',
        projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        markerPresent: false,
        generating: false,
        userTurnCount: 1,
        assistantTurnCount: 1,
        composerEmpty: false
      };
    }
  };
  const pageWindow = { MOONDESK_CHATGPT_DOM: DOM };
  const context = vm.createContext({
    window: pageWindow,
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      hash: '',
      pathname: '/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation',
      search: '',
      href: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation'
    },
    history: { state: null, replaceState() {} },
    chrome: {
      runtime: {
        onMessage: {
          addListener(handler) { messageHandler = handler; }
        }
      }
    },
    console
  });

  vm.runInContext(contentSource, context, { filename: contentPath });
  assert.equal(typeof messageHandler, 'function');

  const response = await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error('prepare response timed out')), 1000);
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: 'reuse-command',
      launchToken: 'reuse-token',
      placement: { projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' },
      launch: {
        workspaceId: 'workspace-a',
        taskMarker: 'moondesk-worker-task:reuse-ready',
        openingMessage: 'reuse-ready',
        threadKey: 'worker:reuse-ready',
        openMode: 'existing_thread',
        executionProfile: { modelId: 'gpt-5.6-sol', reasoningEffort: 'high' }
      }
    }, null, (value) => {
      clearTimeout(timeout);
      resolve(value);
    });
    assert.equal(returned, true);
  });

  assert.equal(response.ok, true);
  assert.equal(response.result.state, 'ready');
  assert.equal(waitCalls, 2, 'reuse waits once before model selection and reacquires the composer after it');
  assert.equal(selectCalls, 1);
});

test('fresh worker preparation stays on normal ChatGPT home even when the Core is in a Project', async () => {
  let messageHandler = null;
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return null; },
    projectIdFromPath() { return null; },
    async waitForComposerReady(_timeoutMs, stillCurrent) { return stillCurrent(); },
    async selectModelSettings(_profile, _failure, stillCurrent) { return stillCurrent(); },
    visibleModelSelection() { return { model: 'gpt-5-6-thinking', reasoningEffort: 'high' }; },
    taskMarkerPresent() { return false; },
    preparedPromptMatches() { return false; },
    insertPrompt() { return true; },
    workerEvidence() {
      return {
        conversationId: null,
        projectId: null,
        markerPresent: false,
        generating: false,
        userTurnCount: 0,
        assistantTurnCount: 0,
        composerEmpty: false
      };
    }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      origin: 'https://chatgpt.com',
      hash: '#moondesk-launch=normal-worker-token',
      pathname: '/',
      search: '?moondesk-launch=normal-worker-token',
      href: 'https://chatgpt.com/?moondesk-launch=normal-worker-token#moondesk-launch=normal-worker-token'
    },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });

  const response = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: 'normal-worker-command',
      launchToken: 'normal-worker-token',
      placement: {
        projectId,
        anchorConversationUrl: `https://chatgpt.com/g/${projectId}-moondesk/c/11111111-2222-4333-8444-555555555555`
      },
      launch: {
        workspaceId: 'workspace-a',
        taskMarker: 'moondesk-worker-task:normal-worker',
        openingMessage: 'normal-worker',
        threadKey: 'worker:normal-worker',
        openMode: 'new_thread',
        executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
      }
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.equal(response.ok, true);
  assert.equal(response.result.state, 'ready');
  const remembered = JSON.parse(storage.get('moondesk-worker-launch-v1'));
  assert.equal(remembered.projectId, null);
  assert.equal('projectEntry' in remembered, false);
  assert.equal('sourceConversationId' in remembered, false);
});

test('fresh worker preparation refuses a duplicated Project Core instead of transforming it', async () => {
  let messageHandler = null;
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const anchorConversationId = '11111111-2222-4333-8444-555555555555';
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return anchorConversationId; },
    projectIdFromPath() { return projectId; },
    async waitForComposerReady() { throw new Error('wrong-route fresh worker must fail before waiting on the composer'); },
    async selectModelSettings() { throw new Error('wrong-route fresh worker must fail before model selection'); },
    taskMarkerPresent() { return false; },
    insertPrompt() { throw new Error('wrong-route fresh worker must never insert a prompt'); },
    workerEvidence() { return {}; }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      origin: 'https://chatgpt.com',
      hash: '#moondesk-launch=duplicate-anchor-token',
      pathname: `/g/${projectId}-moondesk/c/${anchorConversationId}`,
      search: '?moondesk-launch=duplicate-anchor-token',
      href: `https://chatgpt.com/g/${projectId}-moondesk/c/${anchorConversationId}?moondesk-launch=duplicate-anchor-token#moondesk-launch=duplicate-anchor-token`
    },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });

  const response = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: 'duplicate-anchor-command',
      launchToken: 'duplicate-anchor-token',
      placement: {
        projectId,
        anchorConversationUrl: `https://chatgpt.com/g/${projectId}-moondesk/c/${anchorConversationId}`
      },
      launch: {
        workspaceId: 'workspace-a',
        taskMarker: 'moondesk-worker-task:duplicate-anchor',
        openingMessage: 'duplicate-anchor',
        threadKey: 'worker:duplicate-anchor',
        openMode: 'new_thread',
        executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
      }
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.equal(response.ok, true);
  assert.deepEqual(JSON.parse(JSON.stringify(response.result)), {
    state: 'failed',
    reason: 'normal_chat_entry_unconfirmed'
  });
});

test('worker preparation aborts when the target route changes during model selection', async () => {
  let messageHandler = null;
  let conversationId = null;
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return conversationId; },
    projectIdFromPath() { return null; },
    composerReady() { return true; },
    async waitForComposerReady() { return true; },
    async selectModelSettings(_profile, failure, stillCurrent) {
      assert.equal(stillCurrent(), true);
      conversationId = 'foreign-conversation';
      assert.equal(stillCurrent(), false);
      failure('launch_target_changed');
      return false;
    },
    taskMarkerPresent() { return false; },
    insertPrompt() { throw new Error('route-changed launch must not insert a prompt'); },
    workerEvidence() { throw new Error('route-changed launch must not collect send evidence'); }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      origin: 'https://chatgpt.com', hash: '', pathname: '/', search: '', href: 'https://chatgpt.com/'
    },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });

  const response = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: 'route-change-command',
      launchToken: 'route-change-token',
      placement: { projectId: null },
      launch: {
        workspaceId: 'workspace-a',
        taskMarker: 'moondesk-worker-task:route-change',
        openingMessage: 'route-change',
        threadKey: 'worker:route-change',
        openMode: 'new_thread',
        executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
      }
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.equal(response.ok, true);
  assert.deepEqual(
    JSON.parse(JSON.stringify(response.result)),
    { state: 'failed', reason: 'model_or_effort_unconfirmed:launch_target_changed' }
  );
});

test('worker commit refuses a route change that occurs after preparation but before Send', async () => {
  let messageHandler = null;
  let conversationId = null;
  let commitCalls = 0;
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return conversationId; },
    projectIdFromPath() { return null; },
    composerReady() { return true; },
    async waitForComposerReady(_timeoutMs, stillCurrent) { return stillCurrent(); },
    async selectModelSettings(_profile, _failure, stillCurrent) { return stillCurrent(); },
    visibleModelSelection() { return { model: 'gpt-5.6-sol', reasoningEffort: 'high' }; },
    taskMarkerPresent() { return false; },
    insertPrompt() { return true; },
    workerEvidence() {
      return {
        conversationId,
        projectId: null,
        markerPresent: false,
        generating: false,
        userTurnCount: 0,
        assistantTurnCount: 0,
        composerEmpty: false
      };
    },
    async commitSendOnce() {
      commitCalls += 1;
      return { state: 'committed' };
    }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: { origin: 'https://chatgpt.com', hash: '', pathname: '/', search: '', href: 'https://chatgpt.com/' },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });
  const launch = {
    workspaceId: 'workspace-a',
    taskMarker: 'moondesk-worker-task:commit-route-change',
    openingMessage: 'commit-route-change',
    threadKey: 'worker:commit-route-change',
    openMode: 'new_thread',
    executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
  };
  const prepare = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER', commandId: 'commit-route-command', launchToken: 'commit-route-token',
      placement: { projectId: null }, launch
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.equal(prepare.result.state, 'ready');

  conversationId = 'foreign-conversation';
  const commit = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_COMMIT_WORKER_SEND', commandId: 'commit-route-command', launchToken: 'commit-route-token',
      placement: { projectId: null }, launch
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.deepEqual(JSON.parse(JSON.stringify(commit.result)), {
    state: 'failed', reason: 'prepared_launch_target_changed'
  });
  assert.equal(commitCalls, 0, 'the native Send path must not run after the prepared target changes');
});

test('content reset generation cancels a delayed commit before its irreversible Send callback', async () => {
  let messageHandler = null;
  let sendCalls = 0;
  let releaseSelection;
  let reportSelectionStarted;
  const selectionStarted = new Promise((resolve) => { reportSelectionStarted = resolve; });
  const selectionGate = new Promise((resolve) => { releaseSelection = resolve; });
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return null; },
    projectIdFromPath() { return null; },
    composerReady() { return true; },
    async waitForComposerReady(_timeoutMs, stillCurrent) { return stillCurrent(); },
    async selectModelSettings(_profile, _failure, stillCurrent) { return stillCurrent(); },
    visibleModelSelection() { return { model: 'gpt-5.6-sol', reasoningEffort: 'high' }; },
    async selectedModelAndEffort() {
      reportSelectionStarted();
      await selectionGate;
      return { model: 'gpt-5.6-sol', reasoningEffort: 'high' };
    },
    taskMarkerPresent() { return false; },
    preparedPromptMatches() { return false; },
    insertPrompt() { return true; },
    workerEvidence() {
      return { conversationId: null, projectId: null, markerPresent: false, generating: false, userTurnCount: 0, assistantTurnCount: 0, composerEmpty: false };
    },
    async commitSendOnce(_prompt, _marker, stillCurrent) {
      if (!stillCurrent()) return { state: 'failed', reason: 'prepared_launch_target_changed' };
      sendCalls += 1;
      return { state: 'committed' };
    }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; },
      removeItem(key) { storage.delete(key); }
    },
    location: { origin: 'https://chatgpt.com', hash: '', pathname: '/', search: '', href: 'https://chatgpt.com/' },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });
  const launch = {
    workspaceId: 'workspace-a',
    taskMarker: 'moondesk-worker-task:content-reset-cancel',
    openingMessage: 'content-reset-cancel',
    threadKey: 'worker:content-reset-cancel',
    openMode: 'new_thread',
    executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
  };
  const prepare = await new Promise((resolve) => {
    messageHandler({
      type: 'MOONDESK_PREPARE_WORKER', commandId: 'content-reset-command', launchToken: 'content-reset-token',
      placement: { projectId: null }, launch
    }, null, resolve);
  });
  assert.equal(prepare.result.state, 'ready');

  const commitPromise = new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_COMMIT_WORKER_SEND', commandId: 'content-reset-command', launchToken: 'content-reset-token',
      placement: { projectId: null }, launch
    }, null, resolve);
    assert.equal(returned, true);
  });
  await selectionStarted;

  const reset = await new Promise((resolve) => {
    const returned = messageHandler({ type: 'MOONDESK_CLEAR_WORKER_LAUNCHES' }, null, resolve);
    assert.equal(returned, false);
  });
  assert.equal(reset.ok, true);
  assert.equal(reset.resetGeneration, 1);

  releaseSelection();
  const committed = await commitPromise;
  assert.deepEqual(JSON.parse(JSON.stringify(committed.result)), {
    state: 'failed', reason: 'prepared_launch_target_changed'
  });
  assert.equal(sendCalls, 0, 'reset generation must prevent the delayed content task from reaching Send');
});

test('worker commit revalidates the exact model and effort immediately before Send', async () => {
  let messageHandler = null;
  let sendCalls = 0;
  let readbackCalls = 0;
  const storage = new Map();
  const conversationId = 'worker-conversation';
  const DOM = {
    conversationIdFromPath() { return conversationId; },
    projectIdFromPath() { return null; },
    async waitForComposerReady(_timeoutMs, stillCurrent) { return stillCurrent(); },
    async selectModelSettings(_profile, _failure, stillCurrent) { return stillCurrent(); },
    visibleModelSelection() { return { model: 'gpt-5.6-sol', reasoningEffort: 'high' }; },
    async selectedModelAndEffort() { readbackCalls += 1; return null; },
    taskMarkerPresent() { return false; },
    preparedPromptMatches() { return false; },
    insertPrompt() { return true; },
    workerEvidence() {
      return { conversationId, projectId: null, markerPresent: false, generating: false, userTurnCount: 0, assistantTurnCount: 0, composerEmpty: false };
    },
    async commitSendOnce() { sendCalls += 1; return { state: 'committed' }; }
  };
  const context = vm.createContext({
    window: { MOONDESK_CHATGPT_DOM: DOM },
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: { origin: 'https://chatgpt.com', hash: '', pathname: `/c/${conversationId}`, search: '', href: `https://chatgpt.com/c/${conversationId}` },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    crypto: webcrypto,
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });
  const launch = {
    workspaceId: 'workspace-a', taskMarker: 'moondesk-worker-task:pre-send-model', openingMessage: 'pre-send-model',
    threadKey: 'worker:pre-send-model', openMode: 'existing_thread',
    executionProfile: { modelKey: 'gpt-5.6-sol', modelLabel: 'GPT-5.6 Sol', reasoningEffort: 'high' }
  };
  const prepare = await new Promise((resolve) => {
    messageHandler({ type: 'MOONDESK_PREPARE_WORKER', commandId: 'pre-send-command', launchToken: 'pre-send-token', placement: { projectId: null }, launch }, null, resolve);
  });
  assert.equal(prepare.result.state, 'ready');

  const commit = await new Promise((resolve) => {
    messageHandler({ type: 'MOONDESK_COMMIT_WORKER_SEND', commandId: 'pre-send-command', launchToken: 'pre-send-token', placement: { projectId: null }, launch }, null, resolve);
  });
  assert.deepEqual(JSON.parse(JSON.stringify(commit.result)), { state: 'failed', reason: 'model_or_effort_unconfirmed_before_send' });
  assert.equal(readbackCalls, 1);
  assert.equal(sendCalls, 0, 'Send must not run when the final provider selection cannot be confirmed');
});

test('worker preparation reports the exact model-picker stage that failed', async () => {
  let messageHandler = null;
  const storage = new Map();
  const DOM = {
    conversationIdFromPath() { return 'worker-conversation'; },
    projectIdFromPath() { return 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; },
    async waitForComposerReady() { return true; },
    composerReady() { return true; },
    async selectModelSettings(_profile, failure) {
      failure('version_unconfirmed');
      return false;
    },
    taskMarkerPresent() { return false; },
    insertPrompt() { throw new Error('prompt insertion must not run after model failure'); },
    workerEvidence() { throw new Error('worker evidence must not run after model failure'); }
  };
  const pageWindow = { MOONDESK_CHATGPT_DOM: DOM };
  const context = vm.createContext({
    window: pageWindow,
    sessionStorage: {
      setItem(key, value) { storage.set(key, value); },
      getItem(key) { return storage.get(key) || null; }
    },
    location: {
      hash: '',
      pathname: '/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation',
      search: '',
      href: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation'
    },
    history: { state: null, replaceState() {} },
    chrome: { runtime: { onMessage: { addListener(handler) { messageHandler = handler; } } } },
    console
  });

  vm.runInContext(contentSource, context, { filename: contentPath });
  const response = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_PREPARE_WORKER',
      commandId: 'model-stage-command',
      launchToken: 'model-stage-token',
      placement: { projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' },
      launch: {
        workspaceId: 'workspace-a',
        taskMarker: 'moondesk-worker-task:model-stage',
        openingMessage: 'model-stage',
        threadKey: 'worker:model-stage',
        openMode: 'existing_thread',
        executionProfile: { modelId: 'gpt-5.6-sol', reasoningEffort: 'high' }
      }
    }, null, resolve);
    assert.equal(returned, true);
  });

  assert.equal(response.ok, true);
  assert.deepEqual(
    JSON.parse(JSON.stringify(response.result)),
    { state: 'failed', reason: 'model_or_effort_unconfirmed:version_unconfirmed' }
  );
});

test('model-catalog content request is helper-owned and returns a bounded readiness failure', async () => {
  let messageHandler = null;
  let inspectCalls = 0;
  const nonce = '11111111-2222-4333-8444-555555555555';
  const DOM = {
    conversationIdFromPath() { return null; },
    async waitForComposerReady(timeoutMs, stillCurrent) {
      assert.ok(timeoutMs > 0 && timeoutMs <= 15000);
      assert.equal(stillCurrent(), true);
      return false;
    },
    async inspectModelSettings() {
      inspectCalls += 1;
      return [];
    }
  };
  const pageWindow = { MOONDESK_CHATGPT_DOM: DOM };
  const context = vm.createContext({
    window: pageWindow,
    sessionStorage: { setItem() {}, getItem() { return null; } },
    location: {
      origin: 'https://chatgpt.com',
      hash: '',
      pathname: '/',
      search: `?moondesk-model-catalog=${nonce}`,
      href: `https://chatgpt.com/?moondesk-model-catalog=${nonce}`
    },
    history: { state: null, replaceState() {} },
    URL,
    chrome: {
      runtime: {
        onMessage: {
          addListener(handler) { messageHandler = handler; }
        }
      }
    },
    console
  });
  vm.runInContext(contentSource, context, { filename: contentPath });

  const response = await new Promise((resolve) => {
    const returned = messageHandler({
      type: 'MOONDESK_MODEL_CATALOG',
      nonce,
      expiresAt: Date.now() + 5000
    }, null, resolve);
    assert.equal(returned, true);
  });
  assert.deepEqual(JSON.parse(JSON.stringify(response)), {
    ok: false,
    error: 'catalog_composer_not_ready'
  });
  assert.equal(inspectCalls, 0, 'picker inspection must not run before the clean helper composer is ready');
});

test('worker acceptance binds on the exact new marker user turn without waiting for assistant activity', () => {
  const { evaluate } = loadBackground();
  const acceptanceMatches = evaluate('acceptanceMatches');
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const conversationUrl = 'https://chatgpt.com/c/bbbbbbbb-cccc-4ddd-8eee-ffffffffffff';
  const command = {
    id: 'acceptance-command',
    launch: {
      openMode: 'new_thread',
      threadKey: 'worker:acceptance'
    }
  };
  const placement = { projectId };
  const baseline = {
    markerPresent: false,
    generating: false,
    userTurnCount: 0,
    assistantTurnCount: 0
  };
  const rememberedLaunch = {
    commandId: command.id,
    threadKey: command.launch.threadKey
  };

  const posted = {
    conversationId: 'bbbbbbbb-cccc-4ddd-8eee-ffffffffffff',
    markerPresent: true,
    generating: false,
    userTurnCount: 1,
    assistantTurnCount: 0,
    composerEmpty: true
  };
  assert.equal(
    acceptanceMatches(command, placement, baseline, posted, conversationUrl, rememberedLaunch),
    true,
    'the newly committed exact marker user row is sufficient Send acceptance proof'
  );
  assert.equal(
    acceptanceMatches(
      command,
      placement,
      baseline,
      posted,
      `https://chatgpt.com/g/${projectId}-moondesk/c/bbbbbbbb-cccc-4ddd-8eee-ffffffffffff`,
      rememberedLaunch
    ),
    false,
    'a fresh worker must never bind to a Project conversation'
  );

  assert.equal(
    acceptanceMatches(
      command,
      placement,
      { ...baseline, userTurnCount: 1 },
      posted,
      conversationUrl,
      rememberedLaunch
    ),
    false,
    'a marker without a newly added user turn is not fresh Send acceptance'
  );

  assert.equal(
    acceptanceMatches(
      command,
      placement,
      { ...baseline, markerPresent: true },
      posted,
      conversationUrl,
      rememberedLaunch
    ),
    false,
    'a marker already present in the pre-Send baseline cannot be reused as a new receipt'
  );
});

test('worker launch transaction survives ChatGPT navigation and ACKs the confirmed conversation', async () => {
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const commandId = '11111111-2222-4333-8444-555555555555';
  const leaseId = '66666666-7777-4888-8999-aaaaaaaaaaaa';
  const taskMarker = 'moondesk-worker-task:transaction-test';
  const anchorConversationId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee';
  const conversationUrl = 'https://chatgpt.com/c/bbbbbbbb-cccc-4ddd-8eee-ffffffffffff';
  const events = [];
  const ackPayloads = [];
  let launchIdentity = null;

  const command = {
    id: commandId,
    state: 'leased',
    lease: { leaseId, clientId: 'browser-current' },
    anchorContext: {
      conversationId: anchorConversationId,
      conversationUrl: `https://chatgpt.com/g/${projectId}-moondesk/c/${anchorConversationId}`,
      projectId,
      projectUrl: `https://chatgpt.com/g/${projectId}-moondesk/project`
    },
    launch: {
      workspaceId: 'workspace-a',
      purpose: 'worker',
      taskMarker,
      threadKey: 'worker:transaction-test',
      openMode: 'new_thread',
      openingMessage: `Do the task. Marker: ${taskMarker}`,
      executionProfile: {
        modelId: 'gpt-5.6-sol',
        modelLabel: 'GPT-5.6 Sol',
        reasoningEffort: 'high'
      }
    }
  };

  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
      events.push('send_started');
      assert.equal(body.commandId, commandId);
      assert.equal(body.leaseId, leaseId);
      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({
            command: { ...command, state: 'send_started', reconcileHistory: true }
          });
        }
      };
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ workerResetEpoch: 0 }); }
      };
    }
    if (parsed.pathname === '/__moondesk/companion/v1/commands/ack') {
      events.push(`ack:${body.outcome}`);
      ackPayloads.push(body);
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ command: { ...command, state: body.outcome } }); }
      };
    }
    throw new Error(`unexpected request: ${parsed.pathname}`);
  };

  const sendMessageImpl = async (tabId, message, { existingTabs }) => {
    if (message.type === 'MOONDESK_CONTEXT') {
      return {
        ok: true,
        context: {
          projectId: null,
          projectUrl: null,
          conversationId: existingTabs[tabId].url.includes('/c/')
            ? 'bbbbbbbb-cccc-4ddd-8eee-ffffffffffff'
            : null,
          sourceUrl: existingTabs[tabId].url,
          generating: existingTabs[tabId].url.includes('/c/')
        },
        rememberedLaunch: launchIdentity
      };
    }
    if (message.type === 'MOONDESK_PREPARE_WORKER') {
      events.push('prepare');
      launchIdentity = {
        commandId,
        launchToken: message.launchToken,
        taskMarker,
        workspaceId: 'workspace-a',
        threadKey: 'worker:transaction-test'
      };
      return {
        ok: true,
        result: {
          state: 'ready',
          reason: 'worker_prompt_prepared',
          selection: { modelId: 'gpt-5.6-sol', reasoningEffort: 'high' },
          evidence: {
            conversationId: null,
            markerPresent: false,
            generating: false,
            userTurnCount: 0,
            composerEmpty: false
          }
        }
      };
    }
    if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
      events.push('commit');
      assert.equal(message.launchToken, launchIdentity.launchToken);
      existingTabs[tabId].url = conversationUrl;
      return {
        ok: true,
        result: {
          state: 'committed',
          baseline: {
            conversationId: null,
            markerPresent: false,
            generating: false,
            userTurnCount: 0,
            composerEmpty: false
          }
        }
      };
    }
    if (message.type === 'MOONDESK_WORKER_EVIDENCE') {
      events.push('evidence');
      return {
        ok: true,
        rememberedLaunch: launchIdentity,
        evidence: {
          conversationId: 'bbbbbbbb-cccc-4ddd-8eee-ffffffffffff',
          conversationUrl,
          projectId: null,
          markerPresent: true,
          generating: true,
          userTurnCount: 1,
          composerEmpty: true
        }
      };
    }
    throw new Error(`unexpected tab message: ${message.type}`);
  };

  const { createdTabs, evaluate } = loadBackground({
    fetchImpl,
    sendMessageImpl,
    setTimeoutImpl(fn, delay) {
      if (delay <= 250) queueMicrotask(fn);
      return 1;
    }
  });
  const processCommand = evaluate('processCommand');
  const state = {
    baseUrl: 'http://127.0.0.1:47650',
    credential: 'a'.repeat(64),
    launchRecords: {},
    threadRecords: {},
    blockedCommands: {},
    bindings: {}
  };

  await processCommand(state, { command, reconcileRequired: false });

  assert.equal(createdTabs.length, 1);
  assert.deepEqual(events.slice(0, 4), ['prepare', 'send_started', 'commit', 'evidence']);
  assert.equal(ackPayloads.length, 1);
  assert.equal(ackPayloads[0].outcome, 'succeeded');
  assert.equal(ackPayloads[0].conversationUrl, conversationUrl);
  assert.equal(state.launchRecords[commandId].phase, 'succeeded');
  assert.equal(state.launchRecords[commandId].conversationUrl, conversationUrl);
  assert.equal(
    state.threadRecords['worker:transaction-test'].conversationUrl,
    conversationUrl
  );
});

test('reconciliation commits a newly observed marker user turn without waiting for assistant activity', async () => {
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const commandId = '21111111-2222-4333-8444-555555555555';
  const leaseId = '26666666-7777-4888-8999-aaaaaaaaaaaa';
  const conversationUrl = 'https://chatgpt.com/c/cbbbbbbb-cccc-4ddd-8eee-ffffffffffff';
  const threadKey = 'worker:reconcile-marker-only';
  const rememberedLaunch = { commandId, threadKey };
  const ackPayloads = [];
  const command = {
    id: commandId,
    state: 'send_started',
    lease: { leaseId, clientId: 'browser-current' },
    anchorContext: {
      conversationId: 'anchor-conversation',
      conversationUrl: `https://chatgpt.com/g/${projectId}-moondesk/c/anchor-conversation`,
      projectId,
      projectUrl: `https://chatgpt.com/g/${projectId}-moondesk/project`
    },
    launch: {
      workspaceId: 'workspace-a',
      purpose: 'worker',
      taskMarker: 'moondesk-worker-task:reconcile-marker-only',
      threadKey,
      openMode: 'new_thread',
      openingMessage: 'marker-only reconciliation test'
    }
  };
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/ack') {
      ackPayloads.push(body);
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ command: { ...command, state: body.outcome } }); }
      };
    }
    throw new Error(`unexpected request: ${parsed.pathname}`);
  };
  const sendMessageImpl = async (_tabId, message) => {
    if (message.type === 'MOONDESK_CONTEXT') {
      return {
        ok: true,
        context: {
          projectId: null,
          projectUrl: null,
          conversationId: 'cbbbbbbb-cccc-4ddd-8eee-ffffffffffff',
          sourceUrl: conversationUrl,
          generating: false
        },
        rememberedLaunch
      };
    }
    if (message.type === 'MOONDESK_RECONCILE_WORKER') {
      return {
        ok: true,
        rememberedLaunch,
        result: {
          state: 'observed',
          reason: 'task_marker_present_execution_unconfirmed',
          conversationUrl,
          evidence: {
            conversationId: 'cbbbbbbb-cccc-4ddd-8eee-ffffffffffff',
            projectId: null,
            markerPresent: true,
            generating: false,
            userTurnCount: 1,
            assistantTurnCount: 0,
            composerEmpty: true
          }
        }
      };
    }
    throw new Error(`unexpected tab message: ${message.type}`);
  };
  const { evaluate } = loadBackground({
    existingTabs: { 77: { id: 77, url: conversationUrl } },
    fetchImpl,
    sendMessageImpl
  });
  const processCommand = evaluate('processCommand');
  const state = {
    baseUrl: 'http://127.0.0.1:47650',
    credential: 'a'.repeat(64),
    launchRecords: {
      [commandId]: {
        commandId,
        launchToken: 'launch-token',
        threadKey,
        openMode: 'new_thread',
        sourceUrl: conversationUrl,
        conversationUrl,
        tabId: 77,
        phase: 'send_started',
        reconcileAttempts: 0,
        baseline: {
          generating: false,
          userTurnCount: 0,
          assistantTurnCount: 0
        }
      }
    },
    threadRecords: {},
    blockedCommands: {},
    bindings: {}
  };

  await processCommand(state, { command, reconcileRequired: true });

  assert.equal(ackPayloads.length, 1);
  assert.equal(ackPayloads[0].outcome, 'succeeded');
  assert.equal(ackPayloads[0].conversationUrl, conversationUrl);
  assert.equal(state.launchRecords[commandId].phase, 'succeeded');
  assert.equal(state.threadRecords[threadKey].conversationUrl, conversationUrl);
});

test('transient normal-chat readiness retries preparation on the same tab before Send', async () => {
  let prepareCalls = 0;
  const tabIds = [];
  const { evaluate } = loadBackground({
    sendMessageImpl: async (tabId, message) => {
      assert.equal(message.type, 'MOONDESK_PREPARE_WORKER');
      tabIds.push(tabId);
      prepareCalls += 1;
      if (prepareCalls === 1) {
        return {
          ok: true,
          result: { state: 'failed', reason: 'normal_chat_composer_not_ready' }
        };
      }
      return {
        ok: true,
        result: {
          state: 'ready',
          reason: 'worker_prompt_prepared',
          evidence: { conversationId: null, userTurnCount: 0, composerEmpty: false }
        }
      };
    },
    setTimeoutImpl(fn, delay) {
      if (delay === 1500) queueMicrotask(fn);
      return 1;
    }
  });
  const prepareWorkerWithRetry = evaluate('prepareWorkerWithRetry');
  const response = await prepareWorkerWithRetry(42, { type: 'MOONDESK_PREPARE_WORKER' });
  assert.equal(response.result.state, 'ready');
  assert.equal(prepareCalls, 2);
  assert.deepEqual(tabIds, [42, 42]);
});

test('worker preparation uses the full remaining deadline instead of the old 40 second transport cap', async () => {
  const timeoutDelays = [];
  const { evaluate } = loadBackground({
    sendMessageImpl: async (_tabId, message) => {
      assert.equal(message.type, 'MOONDESK_PREPARE_WORKER');
      return {
        ok: true,
        result: {
          state: 'ready',
          reason: 'worker_prompt_prepared',
          evidence: { conversationId: null, userTurnCount: 0, composerEmpty: false }
        }
      };
    },
    setTimeoutImpl(_fn, delay) {
      timeoutDelays.push(delay);
      return 1;
    }
  });
  const prepareWorkerWithRetry = evaluate('prepareWorkerWithRetry');
  const response = await prepareWorkerWithRetry(
    42,
    { type: 'MOONDESK_PREPARE_WORKER' },
    Date.now() + 60000
  );
  assert.equal(response.result.state, 'ready');
  assert.ok(
    timeoutDelays.some((delay) => delay > 40000),
    `expected the send-message timeout to inherit the overall deadline, got ${timeoutDelays.join(', ')}`
  );
});

test('worker preparation reacquires content and retries after a ChatGPT remount closes the message port', async () => {
  let prepareCalls = 0;
  let contextCalls = 0;
  const { evaluate } = loadBackground({
    sendMessageImpl: async (_tabId, message) => {
      if (message.type === 'MOONDESK_CONTEXT') {
        contextCalls += 1;
        return { ok: true, context: { projectId: null } };
      }
      assert.equal(message.type, 'MOONDESK_PREPARE_WORKER');
      prepareCalls += 1;
      if (prepareCalls === 1) {
        throw new Error('The message port closed before a response was received.');
      }
      return {
        ok: true,
        result: {
          state: 'ready',
          reason: 'worker_prompt_prepared',
          evidence: { conversationId: null, userTurnCount: 0, composerEmpty: false }
        }
      };
    },
    setTimeoutImpl(fn, delay) {
      if (delay === 1500) queueMicrotask(fn);
      return 1;
    }
  });
  const prepareWorkerWithRetry = evaluate('prepareWorkerWithRetry');
  const response = await prepareWorkerWithRetry(42, { type: 'MOONDESK_PREPARE_WORKER' });
  assert.equal(response.result.state, 'ready');
  assert.equal(prepareCalls, 2);
  assert.ok(contextCalls >= 1, 'the retry must reacquire the remounted content script first');
});
test('non-transient preparation failure is not retried', async () => {
  let prepareCalls = 0;
  const { evaluate } = loadBackground({
    sendMessageImpl: async () => {
      prepareCalls += 1;
      return {
        ok: true,
        result: { state: 'failed', reason: 'model_or_effort_unconfirmed' }
      };
    },
    setTimeoutImpl(fn, delay) {
      if (delay === 1500) queueMicrotask(fn);
      return 1;
    }
  });
  const prepareWorkerWithRetry = evaluate('prepareWorkerWithRetry');
  const response = await prepareWorkerWithRetry(42, { type: 'MOONDESK_PREPARE_WORKER' });
  assert.equal(response.result.reason, 'model_or_effort_unconfirmed');
  assert.equal(prepareCalls, 1);
});

test('fresh companion lifecycle pauses inherited reconciliation without opening a ChatGPT tab', async () => {
  let redeemCalls = 0;
  const ackPayloads = [];
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    if (parsed.pathname === '/__moondesk/companion/v1/commands/redeem') {
      redeemCalls += 1;
      const body = redeemCalls === 1
        ? {
            command: {
              id: 'command-stale-reconcile',
              launch: { workspaceId: 'workspace-a' },
              lease: { leaseId: 'lease-stale-reconcile' }
            },
            reconcileRequired: true
          }
        : { command: null };
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify(body); }
      };
    }
    if (parsed.pathname === '/__moondesk/companion/v1/commands/ack') {
      ackPayloads.push(JSON.parse(options.body));
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ ok: true }); }
      };
    }
    throw new Error(`unexpected request: ${parsed.pathname}`);
  };

  const { createdTabs, evaluate } = loadBackground({ fetchImpl });
  const redeemCommandBatch = evaluate('redeemCommandBatch');
  const state = {
    baseUrl: 'http://127.0.0.1:47650',
    credential: 'a'.repeat(64),
    blockedCommands: {
      'command-stale-reconcile': {
        commandId: 'command-stale-reconcile',
        workspaceId: 'workspace-a',
        reason: 'worker_conversation_url_unconfirmed',
        retryMode: 'reconcile'
      }
    }
  };

  const offers = await redeemCommandBatch(state, 4);
  assert.equal(offers.length, 0);
  assert.equal(createdTabs.length, 0);
  assert.equal(ackPayloads.length, 1);
  assert.equal(ackPayloads[0].commandId, 'command-stale-reconcile');
  assert.equal(ackPayloads[0].leaseId, 'lease-stale-reconcile');
  assert.equal(ackPayloads[0].outcome, 'paused');
  assert.match(ackPayloads[0].details, /^reconciliation_paused:/);
  assert.equal(state.blockedCommands['command-stale-reconcile'].retryMode, 'reconcile');
});

test('companion redeems four worker launches per fast pump batch', async () => {
  let redeemCalls = 0;
  const fetchImpl = async (url) => {
    const parsed = new URL(url);
    if (parsed.pathname !== '/__moondesk/companion/v1/commands/redeem') {
      throw new Error(`unexpected request: ${parsed.pathname}`);
    }
    redeemCalls += 1;
    return {
      ok: true,
      status: 200,
      async text() {
        return JSON.stringify({ command: { id: `command-${redeemCalls}` } });
      }
    };
  };
  const { evaluate } = loadBackground({ fetchImpl });
  const redeemCommandBatch = evaluate('redeemCommandBatch');
  const offers = await redeemCommandBatch({
    baseUrl: 'http://127.0.0.1:47650',
    credential: 'a'.repeat(64)
  });

  assert.equal(redeemCalls, 4);
  assert.deepEqual(
    Array.from(offers, (offer) => offer.command.id),
    ['command-1', 'command-2', 'command-3', 'command-4']
  );
});

test('companion starts a four-worker launch batch concurrently and isolates failures', async () => {
  const { evaluate } = loadBackground();
  const processCommandBatch = evaluate('processCommandBatch');
  const offers = Array.from({ length: 4 }, (_, index) => ({
    command: { id: `command-${index + 1}` }
  }));
  let started = 0;
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const batch = processCommandBatch({}, offers, async (_state, offer) => {
    started += 1;
    await gate;
    if (offer.command.id === 'command-2') throw new Error('isolated launch failure');
    return offer.command.id;
  });

  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(started, 4, 'all four launches must begin before any one launch finishes');
  release();
  const outcomes = await batch;
  assert.deepEqual(
    Array.from(outcomes, (outcome) => outcome.status),
    ['fulfilled', 'rejected', 'fulfilled', 'fulfilled']
  );
});

test('a hung fresh worker does not block three sibling normal-chat launches from reaching Send', async () => {
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const commands = Array.from({ length: 4 }, (_, index) => {
    const suffix = String(index + 1).padStart(12, '0');
    return {
      id: `10000000-0000-4000-8000-${suffix}`,
      state: 'leased',
      lease: { leaseId: `20000000-0000-4000-8000-${suffix}`, clientId: 'browser-current' },
      anchorContext: {
        conversationId: 'anchor-conversation',
        conversationUrl: `https://chatgpt.com/g/${projectId}-moondesk/c/aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee`,
        projectId,
        projectUrl: `https://chatgpt.com/g/${projectId}-moondesk/project`
      },
      launch: {
        workspaceId: 'workspace-a',
        purpose: 'worker',
        taskMarker: `moondesk-worker-task:concurrent-bootstrap-${index + 1}`,
        threadKey: `worker:concurrent-bootstrap-${index + 1}`,
        openMode: 'new_thread',
        openingMessage: `Do worker ${index + 1}`,
        executionProfile: {
          modelId: 'gpt-5.6-sol',
          modelLabel: 'GPT-5.6 Sol',
          reasoningEffort: 'high'
        }
      }
    };
  });
  const commandById = new Map(commands.map((command) => [command.id, command]));
  const indexById = new Map(commands.map((command, index) => [command.id, index]));
  const rememberedByTab = new Map();
  const launchUrlByTab = new Map();
  const ackPayloads = [];
  const sendStartedCommandIds = [];
  let releaseHungWorker;
  const hungWorkerGate = new Promise((resolve) => { releaseHungWorker = resolve; });

  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
      sendStartedCommandIds.push(body.commandId);
      const command = commandById.get(body.commandId);
      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({
            command: { ...command, state: 'send_started', reconcileHistory: true }
          });
        }
      };
    }
    if (parsed.pathname === '/__moondesk/companion/v1/status') {
      return {
        ok: true,
        status: 200,
        async text() { return JSON.stringify({ workerResetEpoch: 0 }); }
      };
    }
    if (parsed.pathname === '/__moondesk/companion/v1/commands/ack') {
      ackPayloads.push(body);
      return {
        ok: true,
        status: 200,
        async text() {
          return JSON.stringify({ command: { ...commandById.get(body.commandId), state: body.outcome } });
        }
      };
    }
    throw new Error(`unexpected request: ${parsed.pathname}`);
  };

  const sendMessageImpl = async (tabId, message, { existingTabs }) => {
    if (message.type === 'MOONDESK_CONTEXT') {
      const conversationId = existingTabs[tabId].url.includes('/c/')
        ? existingTabs[tabId].url.split('/c/')[1].split(/[?#]/)[0]
        : null;
      return {
        ok: true,
        context: {
          projectId: null,
          projectUrl: null,
          conversationId,
          sourceUrl: existingTabs[tabId].url,
          generating: Boolean(conversationId)
        },
        rememberedLaunch: rememberedByTab.get(tabId) || null
      };
    }

    if (message.type === 'MOONDESK_PREPARE_WORKER') {
      if (!launchUrlByTab.has(tabId)) launchUrlByTab.set(tabId, existingTabs[tabId].url);
      rememberedByTab.set(tabId, {
        commandId: message.commandId,
        launchToken: message.launchToken,
        taskMarker: message.launch.taskMarker,
        workspaceId: message.launch.workspaceId,
        threadKey: message.launch.threadKey
      });
      if (message.commandId === commands[0].id) {
        await hungWorkerGate;
        return {
          ok: true,
          result: { state: 'failed', reason: 'model_or_effort_unconfirmed:picker_unavailable' }
        };
      }
      await new Promise((resolve) => setImmediate(resolve));
      return {
        ok: true,
        result: {
          state: 'ready',
          reason: 'worker_prompt_prepared',
          selection: { modelId: 'gpt-5.6-sol', reasoningEffort: 'high' },
          evidence: {
            conversationId: null,
            markerPresent: false,
            generating: false,
            userTurnCount: 0,
            composerEmpty: false
          }
        }
      };
    }

    if (message.type === 'MOONDESK_COMMIT_WORKER_SEND') {
      const index = indexById.get(message.commandId);
      const conversationId = `${String(index + 1).padStart(8, '0')}-0000-4000-8000-${String(index + 1).padStart(12, '0')}`;
      existingTabs[tabId].url = `https://chatgpt.com/c/${conversationId}`;
      return {
        ok: true,
        result: {
          state: 'committed',
          baseline: {
            conversationId: null,
            markerPresent: false,
            generating: false,
            userTurnCount: 0,
            composerEmpty: false
          }
        }
      };
    }

    if (message.type === 'MOONDESK_WORKER_EVIDENCE') {
      const conversationUrl = existingTabs[tabId].url;
      return {
        ok: true,
        rememberedLaunch: rememberedByTab.get(tabId) || null,
        evidence: {
          conversationId: conversationUrl.split('/c/')[1],
          conversationUrl,
          projectId: null,
          markerPresent: true,
          generating: true,
          userTurnCount: 1,
          composerEmpty: true
        }
      };
    }

    throw new Error(`unexpected tab message: ${message.type}`);
  };

  const { createdTabs, evaluate } = loadBackground({
    fetchImpl,
    sendMessageImpl,
    setTimeoutImpl(fn, delay) {
      if (delay === 1500) queueMicrotask(fn);
      return 1;
    }
  });
  const processCommandBatch = evaluate('processCommandBatch');
  const state = {
    baseUrl: 'http://127.0.0.1:47650',
    credential: 'a'.repeat(64),
    launchRecords: {},
    threadRecords: {},
    blockedCommands: {},
    bindings: {}
  };

  const batch = processCommandBatch(
    state,
    commands.map((command) => ({ command, reconcileRequired: false }))
  );

  for (let attempt = 0; attempt < 20 && sendStartedCommandIds.length < 3; attempt += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.equal(createdTabs.length, 4);
  assert.equal(launchUrlByTab.size, 4);
  for (const launchUrl of launchUrlByTab.values()) {
    const url = new URL(launchUrl);
    assert.equal(url.origin + url.pathname, 'https://chatgpt.com/');
    assert.equal(url.searchParams.get('moondesk-project-entry'), null);
    assert.equal(url.href.includes('aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'), false);
  }
  assert.deepEqual(
    Array.from(sendStartedCommandIds).sort(),
    commands.slice(1).map((command) => command.id).sort(),
    'workers B/C/D must cross the durable Send boundary while worker A is still hung in preparation'
  );
  assert.equal(state.launchRecords[commands[0].id].phase, 'created');

  releaseHungWorker();
  const outcomes = await batch;
  assert.deepEqual(
    Array.from(outcomes, (outcome) => outcome.status),
    ['fulfilled', 'fulfilled', 'fulfilled', 'fulfilled']
  );
  assert.equal(ackPayloads.filter((payload) => payload.outcome === 'succeeded').length, 3);
  assert.equal(ackPayloads.filter((payload) => payload.outcome === 'failed').length, 1);
  assert.equal(state.launchRecords[commands[0].id].phase, 'failed');
});

test('blocked worker registry preserves independent concurrent launch failures', () => {
  const { evaluate } = loadBackground();
  const freshState = evaluate('freshState');
  const blockCommand = evaluate('blockCommand');
  const clearBlockedCommand = evaluate('clearBlockedCommand');
  const blockedCommandList = evaluate('blockedCommandList');
  const state = freshState();

  blockCommand(state, {
    commandId: 'command-1',
    workspaceId: 'workspace-a',
    reason: 'pre_send_failure',
    retryMode: 'fresh'
  });
  blockCommand(state, {
    commandId: 'command-2',
    workspaceId: 'workspace-a',
    reason: 'send_boundary_uncertain',
    retryMode: 'none'
  });

  assert.deepEqual(
    Array.from(blockedCommandList(state), (entry) => entry.commandId).sort(),
    ['command-1', 'command-2']
  );
  clearBlockedCommand(state, 'command-1');
  assert.deepEqual(
    Array.from(blockedCommandList(state), (entry) => entry.commandId),
    ['command-2']
  );
});
