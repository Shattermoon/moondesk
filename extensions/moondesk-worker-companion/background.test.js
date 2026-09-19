const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');

const backgroundPath = path.join(__dirname, 'background.js');
const serverPath = path.join(__dirname, '..', '..', 'src', 'server.rs');
const manifestPath = path.join(__dirname, 'manifest.json');
const source = fs.readFileSync(backgroundPath, 'utf8');
const serverSource = fs.readFileSync(serverPath, 'utf8');
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));

function loadBackground({ existingTabs = {}, contentByTab = {} } = {}) {
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
        if (message?.type === 'MOONDESK_CONTEXT' && contentByTab[id]) return contentByTab[id];
        throw new Error('not used');
      }
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
    fetch: async () => { throw new Error('network not used'); },
    setTimeout: () => 1,
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
      'COMPANION_WORKSPACES_ROUTE',
      'COMPANION_PROFILE_ROUTE',
      'COMPANION_REDEEM_ROUTE',
      'COMPANION_ACK_ROUTE',
      'COMPANION_RETRY_ROUTE'
    ].includes(name));

  assert.equal(routes.length, 9, 'expected the complete companion route set from src/server.rs');
  for (const { name, route } of routes) {
    assert.ok(source.includes(route), `${name} must be used verbatim by background.js`);
  }
  assert.doesNotMatch(source, /['"]\/__moondesk\/companion\/v1\/ack['"]/);
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

test('new worker threads start from the bound Project home, not the binding conversation', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const binding = {
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/old-conversation',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
  };
  assert.equal(sourceUrlForCommand(binding, 'new_thread', null), binding.projectUrl);
});

test('new worker threads fall back to the bound conversation only when Project home is unavailable', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const binding = {
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/old-conversation',
    projectUrl: null
  };
  assert.equal(sourceUrlForCommand(binding, 'new_thread', null), binding.sourceUrl);
});

test('existing worker threads always reuse the confirmed worker conversation', () => {
  const { evaluate } = loadBackground();
  const sourceUrlForCommand = evaluate('sourceUrlForCommand');
  const confirmed = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-conversation';
  const binding = {
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/binding-conversation',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
  };
  assert.equal(sourceUrlForCommand(binding, 'existing_thread', confirmed), confirmed);
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

test('record creation wires new-thread launches to Project home', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const state = { launchRecords: {}, threadRecords: {} };
  const binding = {
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/binding-conversation',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
  };
  const command = {
    id: 'command-1',
    launch: {
      workspaceId: 'workspace-1',
      taskMarker: 'marker-1',
      threadKey: 'worker:test-thread',
      openMode: 'new_thread'
    }
  };

  const { record } = await recordForCommand(state, command, binding);
  assert.equal(record.sourceUrl, binding.projectUrl);
  assert.equal(createdTabs.length, 1);
  assert.match(createdTabs[0].url, /\/project#moondesk-launch=/);
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

test('existing-thread reuse fails closed across workspace or Project boundaries', async () => {
  const { createdTabs, evaluate } = loadBackground();
  const recordForCommand = evaluate('recordForCommand');
  const binding = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/binding',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
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
          projectId: binding.projectId,
          conversationUrl
        }
      }
    }, command, binding),
    /no confirmed ChatGPT conversation binding for this workspace and Project/
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
    }, command, binding),
    /no confirmed ChatGPT conversation binding for this workspace and Project/
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
  const binding = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/binding',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
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
        sourceUrl: binding.projectUrl,
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

  const { record, tab } = await recordForCommand(state, command, binding);
  assert.equal(state.threadRecords['worker:recover'].conversationUrl, workerUrl);
  assert.equal(record.sourceUrl, workerUrl);
  assert.equal(record.conversationUrl, workerUrl);
  assert.equal(tab.id, 77);
  assert.equal(createdTabs.length, 0);
});

test('existing-thread reuse accepts a confirmed thread owned by the same workspace and Project', async () => {
  const conversationUrl = 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/worker-thread';
  const { createdTabs, evaluate } = loadBackground({
    existingTabs: {
      77: { id: 77, url: conversationUrl }
    }
  });
  const recordForCommand = evaluate('recordForCommand');
  const binding = {
    projectId: 'g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    sourceUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/c/binding',
    projectUrl: 'https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-moondesk/project'
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
        projectId: binding.projectId,
        conversationUrl,
        tabId: 77
      }
    }
  };

  const { record, tab } = await recordForCommand(state, command, binding);
  assert.equal(record.conversationUrl, conversationUrl);
  assert.equal(record.sourceUrl, conversationUrl);
  assert.equal(tab.id, 77);
  assert.equal(createdTabs.length, 0);
});
