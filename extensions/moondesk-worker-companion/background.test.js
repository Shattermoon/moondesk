const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');

const backgroundPath = path.join(__dirname, 'background.js');
const serverPath = path.join(__dirname, '..', '..', 'src', 'server.rs');
const manifestPath = path.join(__dirname, 'manifest.json');
const modelStateMainPath = path.join(__dirname, 'model-state-main.js');
const source = fs.readFileSync(backgroundPath, 'utf8');
const modelStateMainSource = fs.readFileSync(modelStateMainPath, 'utf8');
const serverSource = fs.readFileSync(serverPath, 'utf8');
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));

function loadBackground({ existingTabs = {}, contentByTab = {}, sendMessageImpl = null, fetchImpl = async () => { throw new Error('network not used'); }, setTimeoutImpl = null } = {}) {
  const createdTabs = [];
  let stored = {};

  const chrome = {
    storage: {
      local: {
        async get() { return stored; },
        async set(value) { stored = { ...stored, ...value }; }
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
        const tab = { id: 101 + createdTabs.length, url: options.url };
        createdTabs.push(tab);
        existingTabs[tab.id] = tab;
        return tab;
      },
      async sendMessage(id, message) {
        if (sendMessageImpl) return sendMessageImpl(id, message, { existingTabs, createdTabs });
        if (message?.type === 'MOONDESK_CONTEXT' && contentByTab[id]) return contentByTab[id];
        throw new Error('not used');
      },
      onActivated: { addListener() {} },
      onUpdated: { addListener() {} }
    },
    windows: {
      async getLastFocused() { return { id: 1, focused: true }; },
      onFocusChanged: { addListener() {} }
    },
    scripting: {
      async executeScript() { return []; }
    },
    alarms: {
      async create() {},
      onAlarm: { addListener() {} }
    },
    runtime: {
      onMessage: { addListener() {} },
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
    fetch: fetchImpl,
    setTimeout: setTimeoutImpl || (() => 1),
    clearTimeout: () => {}
  });

  vm.runInContext(source, context, { filename: backgroundPath });

  return {
    createdTabs,
    evaluate(expression) {
      return vm.runInContext(expression, context);
    }
  };
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
      'COMPANION_RETRY_ROUTE'
    ].includes(name));

  assert.equal(routes.length, 13, 'expected the complete companion route set from src/server.rs');
  for (const { name, route } of routes) {
    assert.ok(source.includes(route), `${name} must be used verbatim by background.js`);
  }
  assert.doesNotMatch(source, /['"]\/__moondesk\/companion\/v1\/ack['"]/);
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

test('model-state bridge is injected in ChatGPT MAIN world before isolated content scripts', () => {
  const main = manifest.content_scripts.find((entry) =>
    entry.world === 'MAIN' && entry.js?.includes('model-state-main.js')
  );
  assert.ok(main, 'model-state-main.js must be declared as a MAIN-world content script');
  assert.deepEqual(main.matches, ['https://chatgpt.com/*']);
  assert.equal(main.run_at, 'document_start');

  const isolated = manifest.content_scripts.find((entry) =>
    entry.js?.includes('chatgpt-dom.js') && entry.js?.includes('content.js')
  );
  assert.ok(isolated, 'isolated companion content scripts must remain declared');
  assert.equal(isolated.world, undefined);
  assert.equal(isolated.run_at, 'document_idle');
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
        return selector === 'section[data-testid^="conversation-turn"]'
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

test('existing worker placement survives without Anchor presence by using its durable thread record', () => {
  const { evaluate } = loadBackground();
  const placementForOffer = evaluate('placementForOffer');
  const workerUrl = 'https://chatgpt.com/g/g-p-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-any-name/c/6aad7eb1-4b10-83ee-97bd-d98b338864de';
  const state = {
    bindings: {},
    launchRecords: {},
    threadRecords: {
      'worker:pinned': {
        workspaceId: 'workspace-a',
        projectId: 'g-p-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
        conversationUrl: workerUrl
      }
    }
  };
  const offer = {
    anchorContext: null,
    command: {
      launch: {
        workspaceId: 'workspace-a',
        threadKey: 'worker:pinned',
        openMode: 'existing_thread'
      }
    }
  };
  const placement = placementForOffer(state, offer);
  assert.equal(placement.projectId, 'g-p-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
  assert.equal(placement.projectUrl, null);
});

test('new worker threads in a Project start from the exact Anchor Project home', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-any-human-name/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-any-human-name/c/anchor'
  };
  assert.equal(sourceUrlForCommand(placement, 'new_thread', null), placement.projectUrl);
});

test('new worker threads from normal Anchors start from normal ChatGPT home', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const placement = {
    projectId: null,
    projectUrl: null,
    anchorConversationUrl: 'https://chatgpt.com/c/anchor-normal'
  };
  assert.equal(sourceUrlForCommand(placement, 'new_thread', null), 'https://chatgpt.com/');
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

test('record creation wires Project launches to exact Project ID and normal launches to normal home', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const command = {
    id: 'command-1',
    launch: {
      workspaceId: 'workspace-1',
      taskMarker: 'marker-1',
      threadKey: 'worker:test-thread',
      openMode: 'new_thread'
    }
  };

  const projectPlacement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unrelated-name/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unrelated-name/c/anchor'
  };
  const state = { launchRecords: {}, threadRecords: {} };
  const { record } = await recordForCommand(state, command, projectPlacement);
  assert.equal(record.sourceUrl, projectPlacement.projectUrl);
  assert.match(createdTabs[0].url, /\/project#moondesk-launch=/);

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
  assert.match(createdTabs[1].url, /^https:\/\/chatgpt\.com\/#moondesk-launch=/);
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

test('reconciliation without a confirmed conversation URL never reopens the Anchor or Project home', async () => {
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

test('existing-thread reuse fails closed across workspace or exact placement boundaries', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-anything/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-anything/c/anchor'
  };
  const command = {
    id: 'command-reuse',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-reuse',
      threadKey: 'worker:reuse',
      openMode: 'existing_thread'
    }
  };
  const conversationUrl = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-thread';

  await assert.rejects(
    recordForCommand({
      launchRecords: {},
      threadRecords: {
        'worker:reuse': {
          workspaceId: 'workspace-other',
          projectId: placement.projectId,
          conversationUrl
        }
      }
    }, command, placement),
    /no confirmed ChatGPT conversation binding for this workspace and placement context/
  );

  await assert.rejects(
    recordForCommand({
      launchRecords: {},
      threadRecords: {
        'worker:reuse': {
          workspaceId: command.launch.workspaceId,
          projectId: 'g-p-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
          conversationUrl
        }
      }
    }, command, placement),
    /no confirmed ChatGPT conversation binding for this workspace and placement context/
  );

  assert.equal(createdTabs.length, 0);
});

test('existing-thread reuse self-heals a missing thread record from a succeeded launch tab', async () => {
  const workerUrl = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/recovered-worker';
  const { createdTabs, evaluate } = loadBackground({
    existingTabs: {
      77: { id: 77, url: workerUrl }
    },
    contentByTab: {
      77: {
        ok: true,
        rememberedLaunch: {
          commandId: 'command-first',
          threadKey: 'worker:recover'
        }
      }
    }
  });

  const recordForCommand = evaluate('recordForCommand');
  const placement = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/project',
    anchorConversationUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-random/c/anchor'
  };
  const state = {
    launchRecords: {
      'command-first': {
        commandId: 'command-first',
        workspaceId: 'workspace-current',
        taskMarker: 'marker-first',
        threadKey: 'worker:recover',
        openMode: 'new_thread',
        launchToken: 'launch-first',
        sourceUrl: placement.projectUrl,
        tabId: 77,
        conversationUrl: null,
        phase: 'succeeded',
        reconcileAttempts: 0
      }
    },
    threadRecords: {}
  };
  const command = {
    id: 'command-reuse-recovered',
    launch: {
      workspaceId: 'workspace-current',
      taskMarker: 'marker-reuse-recovered',
      threadKey: 'worker:recover',
      openMode: 'existing_thread'
    }
  };

  const { record, tab } = await recordForCommand(state, command, placement);
  assert.equal(state.threadRecords['worker:recover'].conversationUrl, workerUrl);
  assert.equal(record.sourceUrl, workerUrl);
  assert.equal(record.conversationUrl, workerUrl);
  assert.equal(tab.id, 77);
  assert.equal(createdTabs.length, 0);
});

test('existing-thread reuse accepts a confirmed thread owned by the same workspace and exact placement', async () => {
  const conversationUrl = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-thread';
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
      openMode: 'existing_thread'
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

test('worker launch transaction survives ChatGPT navigation and ACKs the confirmed conversation', async () => {
  const projectId = 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const commandId = '11111111-2222-4333-8444-555555555555';
  const leaseId = '66666666-7777-4888-8999-aaaaaaaaaaaa';
  const taskMarker = 'moondesk-worker-task:transaction-test';
  const conversationUrl = `https://chatgpt.com/g/${projectId}-moondesk/c/bbbbbbbb-cccc-4ddd-8eee-ffffffffffff`;
  const events = [];
  const ackPayloads = [];
  let launchIdentity = null;

  const command = {
    id: commandId,
    state: 'leased',
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
          projectId,
          projectUrl: command.anchorContext.projectUrl,
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
          projectId,
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

test('transient Project readiness retries preparation on the same tab before Send', async () => {
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
          result: { state: 'failed', reason: 'project_entry_unconfirmed' }
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

test('four fresh Project launches do not overlap the fragile bootstrap stage', async () => {
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
  const poisonedTabs = new Set();
  const ackPayloads = [];
  let activeProjectBootstraps = 0;
  let maxActiveProjectBootstraps = 0;

  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    const body = options.body ? JSON.parse(options.body) : {};
    if (parsed.pathname === '/__moondesk/companion/v1/commands/send-started') {
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
          projectId,
          projectUrl: `https://chatgpt.com/g/${projectId}-moondesk/project`,
          conversationId,
          sourceUrl: existingTabs[tabId].url,
          generating: Boolean(conversationId)
        },
        rememberedLaunch: rememberedByTab.get(tabId) || null
      };
    }

    if (message.type === 'MOONDESK_PREPARE_WORKER') {
      rememberedByTab.set(tabId, {
        commandId: message.commandId,
        launchToken: message.launchToken,
        taskMarker: message.launch.taskMarker,
        workspaceId: message.launch.workspaceId,
        threadKey: message.launch.threadKey
      });
      activeProjectBootstraps += 1;
      maxActiveProjectBootstraps = Math.max(maxActiveProjectBootstraps, activeProjectBootstraps);
      if (activeProjectBootstraps > 1) poisonedTabs.add(tabId);
      await new Promise((resolve) => setImmediate(resolve));
      activeProjectBootstraps -= 1;

      if (poisonedTabs.has(tabId)) {
        return {
          ok: true,
          result: { state: 'failed', reason: 'project_entry_unconfirmed' }
        };
      }
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
      existingTabs[tabId].url = `https://chatgpt.com/g/${projectId}-moondesk/c/${conversationId}`;
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
          projectId,
          markerPresent: true,
          generating: true,
          userTurnCount: 1,
          composerEmpty: true
        }
      };
    }

    throw new Error(`unexpected tab message: ${message.type}`);
  };

  const { evaluate } = loadBackground({
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

  const outcomes = await processCommandBatch(
    state,
    commands.map((command) => ({ command, reconcileRequired: false }))
  );

  assert.deepEqual(
    Array.from(outcomes, (outcome) => outcome.status),
    ['fulfilled', 'fulfilled', 'fulfilled', 'fulfilled']
  );
  assert.equal(
    maxActiveProjectBootstraps,
    1,
    'fresh Project initialization must be serialized even while worker commands remain concurrently active'
  );
  assert.equal(ackPayloads.filter((payload) => payload.outcome === 'succeeded').length, 4);
  assert.equal(ackPayloads.filter((payload) => payload.outcome === 'failed').length, 0);
});

test('Project bootstrap slot hands off to the next launch and becomes reusable after release', async () => {
  const { evaluate } = loadBackground();
  const acquireProjectBootstrapSlot = evaluate('acquireProjectBootstrapSlot');

  const releaseFirst = await acquireProjectBootstrapSlot();
  let secondEntered = false;
  const second = acquireProjectBootstrapSlot().then((release) => {
    secondEntered = true;
    return release;
  });

  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(secondEntered, false, 'a second fresh Project bootstrap must wait for the active slot');

  releaseFirst();
  const releaseSecond = await second;
  assert.equal(secondEntered, true, 'releasing a bootstrap must hand the slot to the next waiter');

  releaseSecond();
  const releaseThird = await acquireProjectBootstrapSlot();
  releaseThird();
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
