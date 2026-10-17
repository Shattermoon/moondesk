const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const nodeCrypto = require('node:crypto');

const pageSource = fs.readFileSync(path.join(__dirname, 'user-browser-page.js'), 'utf8');
const controlSource = fs.readFileSync(path.join(__dirname, 'user-browser-control.js'), 'utf8');

function eventHook() {
  const listeners = new Set();
  return {
    addListener(fn) { listeners.add(fn); },
    removeListener(fn) { listeners.delete(fn); },
    dispatch(...args) { for (const fn of [...listeners]) fn(...args); }
  };
}

function loadControl({ initialTabs = {}, protectedTab = () => false, pendingCommand = null, failCapture = false, delayFrameTreeMs = 0, onDebuggerCommand = null, navigationHistory = null, initialSessionStore = {}, sharedLocalStore = null } = {}) {
  const tabs = new Map(Object.entries(initialTabs).map(([id, tab]) => [Number(id), { id: Number(id), ...tab }]));
  let nextTabId = Math.max(100, ...tabs.keys()) + 1;
  let sessionStore = { ...initialSessionStore };
  const localStore = sharedLocalStore || {};
  const attached = new Set();
  const attachCalls = [];
  const detachCalls = [];
  const debuggerCommands = [];
  const results = [];
  const transportCalls = [];
  let pending = pendingCommand ? { ...pendingCommand } : null;
  let failPollStatus = null;
  let failResultStatus = null;
  const onUpdated = eventHook();
  const onRemoved = eventHook();
  const onDebuggerEvent = eventHook();
  const onDebuggerDetach = eventHook();

  const chrome = {
    storage: {
      session: {
        async get(key) { return { [key]: sessionStore[key] }; },
        async set(value) { sessionStore = { ...sessionStore, ...value }; }
      },
      local: {
        async get(key) { return { [key]: localStore[key] }; },
        async set(value) { Object.assign(localStore, value); }
      }
    },
    permissions: { async contains() { return true; } },
    tabs: {
      async query() { return [...tabs.values()].map(tab => ({ ...tab })); },
      async get(id) {
        const tab = tabs.get(id);
        if (!tab) throw new Error(`No tab with id: ${id}`);
        return { ...tab };
      },
      async create({ url, active }) {
        const tab = { id: nextTabId++, url, title: '', active, pinned: false, status: 'complete' };
        tabs.set(tab.id, tab);
        return { ...tab };
      },
      async remove(id) { tabs.delete(id); onRemoved.dispatch(id); },
      onUpdated,
      onRemoved
    },
    scripting: {
      async executeScript() { throw new Error('arbitrary scripting must not be needed for attached-tab control'); }
    },
    debugger: {
      onEvent: onDebuggerEvent,
      onDetach: onDebuggerDetach,
      async attach({ tabId }) { attachCalls.push(tabId); attached.add(tabId); },
      async detach({ tabId }) { detachCalls.push(tabId); attached.delete(tabId); },
      async getTargets() {
        return [...tabs.values()].map(tab => ({ tabId: tab.id, attached: attached.has(tab.id) }));
      },
      async sendCommand(target, method, params = {}) {
        debuggerCommands.push({ target, method, params });
        if (onDebuggerCommand) await onDebuggerCommand({ target, method, params, tabs });
        if (method === 'Page.getFrameTree') {
          if (delayFrameTreeMs > 0) await new Promise(resolve => setTimeout(resolve, delayFrameTreeMs));
          const tab = tabs.get(target.tabId);
          return { frameTree: { frame: { id: `frame-${target.tabId}`, url: tab?.url || 'about:blank' }, childFrames: [] } };
        }
        if (method === 'Page.createIsolatedWorld') return { executionContextId: 42 };
        if (method === 'Runtime.evaluate') return { result: { value: true } };
        if (method === 'Page.getNavigationHistory') return navigationHistory || { currentIndex: 0, entries: [{ id: 1, url: tabs.get(target.tabId)?.url || 'about:blank' }] };
        if (method === 'Page.getLayoutMetrics') return {
          cssVisualViewport: { pageX: 0, pageY: 0, clientWidth: 1200, clientHeight: 800, scale: 1 },
          cssContentSize: { x: 0, y: 0, width: 1200, height: 2400 }
        };
        if (method === 'Page.captureScreenshot') {
          if (failCapture) throw new Error('capture failed');
          return { data: 'ZmFrZQ==' };
        }
        return {};
      }
    }
  };

  const crypto = {
    getRandomValues: nodeCrypto.webcrypto.getRandomValues.bind(nodeCrypto.webcrypto),
    randomUUID: nodeCrypto.randomUUID
  };
  const context = vm.createContext({
    console,
    chrome,
    crypto,
    URL,
    TextEncoder,
    setTimeout,
    clearTimeout,
    navigator: { userAgent: 'Chrome/140' }
  });
  context.globalThis = context;
  vm.runInContext(pageSource, context, { filename: 'user-browser-page.js' });
  vm.runInContext(controlSource, context, { filename: 'user-browser-control.js' });

  const transport = async (_path, options = {}) => {
    const body = typeof options.body === 'string' ? JSON.parse(options.body) : (options.body || {});
    transportCalls.push({ body, options: { ...options } });
    if (body.action === 'poll') {
      if (failPollStatus !== null) return { ok: false, status: failPollStatus, data: { error: 'bridge unavailable' } };
      return { ok: true, status: 200, data: { epoch: 'epoch-1', requests: pending ? [pending.id] : [], policy: { read: true, write: true } } };
    }
    if (body.action === 'claim') {
      if (!pending || pending.id !== body.id) return { ok: false, status: 409, data: {} };
      const command = pending;
      pending = null;
      return { ok: true, status: 200, data: { command } };
    }
    if (body.action === 'check') return { ok: true, status: 200, data: { allowed: true } };
    if (body.action === 'result') {
      if (failResultStatus !== null) return { ok: false, status: failResultStatus, data: { error: 'result ack unavailable' } };
      results.push(body.result);
      return { ok: true, status: 200, data: { ok: true } };
    }
    throw new Error(`unexpected transport action ${body.action}`);
  };

  const control = context.MoonDeskUserBrowserControl.create(chrome, transport, protectedTab);
  const command = (tool, args, owner = 'owner-a') => ({
    id: nodeCrypto.randomUUID(),
    epoch: 'epoch-1',
    owner,
    tool,
    args,
    expiresAt: Date.now() + 10_000
  });
  return {
    control,
    command,
    tabs,
    attachCalls,
    detachCalls,
    debuggerCommands,
    results,
    transportCalls,
    localStore,
    getSessionStore() { return { ...sessionStore }; },
    onDebuggerEvent,
    setPollFailure(status = 0) { failPollStatus = status; },
    clearPollFailure() { failPollStatus = null; },
    setResultFailure(status = 0) { failResultStatus = status; },
    clearResultFailure() { failResultStatus = null; }
  };
}

test('user-browser identity survives extension reload when session storage is cleared', async () => {
  const localStore = {};
  const first = loadControl({ sharedLocalStore: localStore });
  await first.control.heartbeat();
  const firstPoll = first.transportCalls.find(call => call.body.action === 'poll');
  assert.ok(firstPoll?.body?.browserId);
  assert.equal(localStore.moondeskUserBrowserIdentityV1, firstPoll.body.browserId);

  const second = loadControl({ sharedLocalStore: localStore });
  await second.control.heartbeat();
  const secondPoll = second.transportCalls.find(call => call.body.action === 'poll');
  assert.equal(secondPoll?.body?.browserId, firstPoll.body.browserId, 'extension reload must not rotate browser-control identity');
});

test('legacy session browser id migrates into persistent browser identity', async () => {
  const legacyId = nodeCrypto.randomUUID();
  const localStore = {};
  const env = loadControl({
    sharedLocalStore: localStore,
    initialSessionStore: {
      moondeskUserBrowserControlV1: { browserId: legacyId, epoch: null, receipt: null, tabs: [] }
    }
  });
  await env.control.heartbeat();
  const poll = env.transportCalls.find(call => call.body.action === 'poll');
  assert.equal(poll?.body?.browserId, legacyId);
  assert.equal(localStore.moondeskUserBrowserIdentityV1, legacyId);
});

test('first persistent browser identity can reuse the existing Companion installation id', async () => {
  const companionId = nodeCrypto.randomUUID();
  const localStore = {
    moondeskWorkerCompanionV1: { clientId: companionId }
  };
  const env = loadControl({ sharedLocalStore: localStore });
  await env.control.heartbeat();
  const poll = env.transportCalls.find(call => call.body.action === 'poll');
  assert.equal(poll?.body?.browserId, companionId);
  assert.equal(localStore.moondeskUserBrowserIdentityV1, companionId);
});

test('protected ChatGPT tabs refuse interactive attachment before debugger ownership', async () => {
  const env = loadControl({
    initialTabs: { 1: { url: 'https://chatgpt.com/c/core', title: 'Core', active: true, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => new URL(url).hostname === 'chatgpt.com'
  });
  await env.control.pump();
  await assert.rejects(
    env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 1 })),
    /BROWSER_EXECUTOR_TAB/
  );
  assert.deepEqual(env.attachCalls, []);
});

test('controlled tab is retired when its main frame navigates into protected ChatGPT', async () => {
  const env = loadControl({
    initialTabs: { 2: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => {
      try { return new URL(url).hostname === 'chatgpt.com'; } catch { return false; }
    }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 2 }));
  const transitionStart = env.debuggerCommands.length;
  env.tabs.get(2).url = 'https://chatgpt.com/c/protected';
  await env.control.event({ tabId: 2 }, 'Page.frameNavigated', { frame: { id: 'frame-2', url: 'https://chatgpt.com/c/protected' } });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(env.detachCalls, [2]);
  assert.equal(
    env.debuggerCommands.slice(transitionStart).some(call => call.method === 'Runtime.evaluate'),
    false,
    'retiring a protected destination must not inject cleanup JavaScript into that page'
  );
  await assert.rejects(
    env.control.execute(env.command('browser_snapshot', { tabId: 2 })),
    /BROWSER_TAB_NOT_OWNED/
  );
});

test('pending protected navigation retires custody before commit instead of refreshing the glow', async () => {
  const env = loadControl({
    initialTabs: { 11: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => {
      try { return new URL(url).hostname === 'chatgpt.com'; } catch { return false; }
    }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 11 }));
  const start = env.debuggerCommands.length;
  env.tabs.get(11).pendingUrl = 'https://chatgpt.com/c/protected';
  const listed = await env.control.execute(env.command('browser_tabs', { action: 'list' }));
  assert.equal(listed.value.tabs.find(tab => tab.tabId === 11)?.access.snapshot, 'protected');
  await env.control.heartbeat();
  assert.deepEqual(env.detachCalls, [11]);
  assert.equal(
    env.debuggerCommands.slice(start).some(call => call.method === 'Runtime.evaluate'),
    false,
    'a pending protected destination must detach before page-script indicator refresh'
  );
});

test('direct user-browser navigation cannot enter protected ChatGPT or fall through unknown actions', async () => {
  const env = loadControl({
    initialTabs: { 8: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => {
      try { return new URL(url).hostname === 'chatgpt.com'; } catch { return false; }
    },
    navigationHistory: {
      currentIndex: 0,
      entries: [
        { id: 1, url: 'https://example.com/' },
        { id: 2, url: 'https://chatgpt.com/c/protected' }
      ]
    }
  });
  await env.control.pump();
  const attached = await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 8 }));
  const start = env.debuggerCommands.length;
  await assert.rejects(
    env.control.execute(env.command('browser_navigate', {
      tabId: 8,
      pageId: attached.value.pageId,
      action: 'url',
      url: 'https://chatgpt.com/c/protected'
    })),
    /BROWSER_EXECUTOR_TAB/
  );
  await assert.rejects(
    env.control.execute(env.command('browser_navigate', {
      tabId: 8,
      pageId: attached.value.pageId,
      action: 'forward'
    })),
    /BROWSER_EXECUTOR_TAB/
  );
  await assert.rejects(
    env.control.execute(env.command('browser_navigate', {
      tabId: 8,
      pageId: attached.value.pageId,
      action: 'sideways'
    })),
    /BROWSER_NAVIGATION_ACTION_UNKNOWN/
  );
  assert.equal(
    env.debuggerCommands.slice(start).some(call => ['Page.navigate','Page.navigateToHistoryEntry'].includes(call.method)),
    false,
    'blocked/unknown navigation must not dispatch a navigation command'
  );
  assert.equal(env.tabs.get(8).url, 'https://example.com/');
});

test('unknown user-browser action is rejected before any input dispatch', async () => {
  const env = loadControl({
    initialTabs: { 12: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  const attached = await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 12 }));
  const shot = await env.control.execute(env.command('browser_screenshot', { tabId: 12, fullPage: false }));
  const start = env.debuggerCommands.length;
  await assert.rejects(
    env.control.execute(env.command('browser_action', {
      tabId: 12,
      pageId: attached.value.pageId,
      action: 'sideways',
      x: 100,
      y: 100,
      screenshotId: shot.value.screenshotId
    })),
    /BROWSER_ACTION_UNKNOWN/
  );
  assert.equal(
    env.debuggerCommands.slice(start).some(call => call.method.startsWith('Input.')),
    false,
    'unknown actions must fail before dispatching input'
  );
});

test('user-browser new refuses a protected ChatGPT destination before creating a tab', async () => {
  const env = loadControl({
    initialTabs: { 9: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => {
      try { return new URL(url).hostname === 'chatgpt.com'; } catch { return false; }
    }
  });
  await env.control.pump();
  const before = env.tabs.size;
  await assert.rejects(
    env.control.execute(env.command('browser_tabs', { action: 'new', url: 'https://chatgpt.com/' })),
    /BROWSER_EXECUTOR_TAB/
  );
  assert.equal(env.tabs.size, before);
});

test('unsupported top-level navigation retires debugger custody instead of leaving invisible control', async () => {
  const env = loadControl({
    initialTabs: { 10: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 10 }));
  const transitionStart = env.debuggerCommands.length;
  env.tabs.get(10).url = 'file:///tmp/local.html';
  await env.control.event({ tabId: 10 }, 'Page.frameNavigated', { frame: { id: 'frame-10', url: 'file:///tmp/local.html' } });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(env.detachCalls, [10]);
  assert.equal(
    env.debuggerCommands.slice(transitionStart).some(call => call.method === 'Runtime.evaluate'),
    false,
    'unsupported destinations must be detached without page-script cleanup'
  );
});

test('permission checks bound loopback transport by the remaining command lifetime', async () => {
  const env = loadControl({
    initialTabs: { 3: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  env.transportCalls.length = 0;
  const command = env.command('browser_tabs', { action: 'list' });
  command.expiresAt = Date.now() + 500;
  await env.control.execute(command);
  const check = env.transportCalls.find(call => call.body.action === 'check');
  assert.ok(check, 'browser command must re-check host authority before dispatch');
  assert.ok(check.options.timeoutMs > 0);
  assert.ok(check.options.timeoutMs <= 500, 'permission check must not outlive the command deadline');
});

test('unattached DOM inspection path is not shipped around protected-tab ownership checks', () => {
  assert.doesNotMatch(controlSource, /async function inspect\(/);
  assert.doesNotMatch(pageSource, /operation === ['"]inspect['"]/);
});

test('all compound input events retain command authority checks', () => {
  assert.doesNotMatch(controlSource, /send\(state,['"]Input\.dispatch/);
});

test('release cleanup avoids another host round trip and retires custody if the page becomes protected after press', async () => {
  const env = loadControl({
    initialTabs: { 5: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    protectedTab: (_id, url) => new URL(url).hostname === 'chatgpt.com',
    onDebuggerCommand: async ({ target, method, params, tabs }) => {
      if (target.tabId === 5 && method === 'Input.dispatchKeyEvent' && params.type === 'keyDown') {
        tabs.get(5).url = 'https://chatgpt.com/c/protected';
      }
    }
  });
  await env.control.pump();
  const attached = await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 5 }));
  const checksBefore = env.transportCalls.filter(call => call.body.action === 'check').length;
  await assert.rejects(
    env.control.execute(env.command('browser_action', { tabId: 5, pageId: attached.value.pageId, action: 'key', key: 'Enter' })),
    /BROWSER_EXECUTOR_TAB/
  );
  const checksAfter = env.transportCalls.filter(call => call.body.action === 'check').length;
  assert.equal(checksAfter - checksBefore, 2, 'cleanup release must not add another host authorization round trip');
  assert.deepEqual(
    env.debuggerCommands.filter(call => call.method === 'Input.dispatchKeyEvent').map(call => call.params.type),
    ['keyDown']
  );
  assert.deepEqual(env.detachCalls, [5]);
});

test('network diagnostics retain request body size but never raw POST data', async () => {
  const secretBody = 'username=alice&password=correct-horse-battery-staple';
  const env = loadControl({
    initialTabs: { 6: { url: 'https://example.com/login', title: 'Login', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 6 }));
  await env.control.event(
    { tabId: 6 },
    'Network.requestWillBeSent',
    {
      requestId: 'request-1',
      request: {
        url: 'https://example.com/session',
        method: 'POST',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        postData: secretBody
      },
      type: 'Fetch',
      timestamp: 10
    }
  );
  const result = await env.control.execute(
    env.command('browser_network', { tabId: 6, requestId: 'main:request-1' })
  );
  assert.equal(result.value.postDataBytes, new TextEncoder().encode(secretBody).length);
  assert.equal(Object.hasOwn(result.value, 'postData'), false);
  assert.doesNotMatch(JSON.stringify(result), /correct-horse-battery-staple/);
});

test('snapshot CDP reads stop at the broker command deadline instead of continuing in the background', async () => {
  const env = loadControl({
    initialTabs: { 7: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    delayFrameTreeMs: 20
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 7 }));
  const expiring = {
    ...env.command('browser_snapshot', { tabId: 7 }),
    expiresAt: Date.now() + 5
  };
  await assert.rejects(
    env.control.execute(expiring),
    /BROWSER_(?:CDP_TIMEOUT|EXPIRED)/
  );
  assert.ok(env.detachCalls.includes(7), 'an expired in-flight CDP read must retire debugger custody');
});

test('eligible tab attach injects the lightweight animated aurora glow, enforces one owner, and release detaches', async () => {
  const env = loadControl({
    initialTabs: { 2: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  const attached = await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 2 }));
  assert.equal(attached.value.tabId, 2);
  assert.equal(attached.value.attached, true);
  assert.deepEqual(env.attachCalls, [2]);
  const overlayEval = env.debuggerCommands.find(entry => entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff'));
  assert.ok(overlayEval, 'attach must inject the visible blue controlled-tab glow');
  assert.match(overlayEval.params.expression, /data-moondesk-browser-control/);
  assert.match(overlayEval.params.expression, /#3984ff/);
  assert.match(overlayEval.params.expression, /#7c3aed/);
  assert.match(overlayEval.params.expression, /#ec4899/);
  assert.match(overlayEval.params.expression, /#f59e0b/);
  assert.match(overlayEval.params.expression, /md-flow-x/);
  assert.match(overlayEval.params.expression, /md-flow-y/);
  assert.match(overlayEval.params.expression, /md-breathe/);
  assert.match(overlayEval.params.expression, /md-bloom/);
  assert.match(overlayEval.params.expression, /md-corner-pulse/);
  assert.match(overlayEval.params.expression, /height:14px/);
  assert.match(overlayEval.params.expression, /width:14px/);
  assert.match(overlayEval.params.expression, /top:-11px/);
  assert.match(overlayEval.params.expression, /left:-11px/);
  assert.match(overlayEval.params.expression, /filter:blur\(6px\)/);
  assert.match(overlayEval.params.expression, /prefers-reduced-motion/);
  const overlaySource = pageSource.match(/if \(operation === 'overlay'\) \{([\s\S]*?)if \(operation === 'removeOverlay'\)/)?.[1] || '';
  assert.doesNotMatch(overlaySource, /requestAnimationFrame|canvas/i, 'glow must stay CSS-only and lightweight');
  assert.match(overlayEval.params.expression, /overlayTimer/);
  assert.match(overlayEval.params.expression, /6000/);
  const overlayCountBeforeHeartbeat = env.debuggerCommands.filter(entry =>
    entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff')
  ).length;
  await env.control.heartbeat();
  const overlayCountAfterHeartbeat = env.debuggerCommands.filter(entry =>
    entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff')
  ).length;
  assert.ok(overlayCountAfterHeartbeat > overlayCountBeforeHeartbeat, 'active control must refresh the self-expiring glow lease');

  await assert.rejects(
    env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 2 }, 'owner-b')),
    /BROWSER_TAB_OWNED/
  );
  const released = await env.control.execute(env.command('browser_tabs', { action: 'release', tabId: 2 }));
  assert.equal(released.value.released, true);
  assert.deepEqual(env.detachCalls, [2]);
  assert.ok(env.debuggerCommands.some(entry => entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('node.remove')));
});

test('browser screenshot hides the human-only control glow before capture and restores it afterward', async () => {
  const env = loadControl({
    initialTabs: { 4: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 4 }));
  const start = env.debuggerCommands.length;
  const shot = await env.control.execute(env.command('browser_screenshot', { tabId: 4, fullPage: false }));
  assert.equal(shot.value.width, 1200);
  assert.equal(shot.value.height, 800);

  const suspend = env.debuggerCommands.findIndex((entry, index) =>
    index >= start && entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('("suspendOverlay",')
  );
  const capture = env.debuggerCommands.findIndex((entry, index) => index >= start && entry.method === 'Page.captureScreenshot');
  const resume = env.debuggerCommands.findIndex((entry, index) =>
    index >= start && entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('("resumeOverlay",')
  );
  assert.ok(suspend >= start, 'screenshot must suspend the human-only glow');
  assert.ok(capture > suspend, 'capture must happen only after the glow is hidden');
  assert.ok(resume > capture, 'glow must be restored after capture');
  assert.match(pageSource, /overlayCaptureDepth/);
  assert.match(pageSource, /visibility', 'hidden'/);
  assert.match(pageSource, /getComputedStyle\(host\)\.visibility !== 'hidden'/);
  assert.doesNotMatch(pageSource.match(/if \(operation === 'suspendOverlay'\) \{([\s\S]*?)if \(operation === 'resumeOverlay'\)/)?.[1] || '', /requestAnimationFrame|setTimeout/, 'screenshot suspension must not depend on throttled animation timers');
});

test('failed browser screenshot still restores the human-only control glow', async () => {
  const env = loadControl({
    initialTabs: { 5: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    failCapture: true
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 5 }));
  const start = env.debuggerCommands.length;
  await assert.rejects(
    env.control.execute(env.command('browser_screenshot', { tabId: 5, fullPage: false })),
    /capture failed/
  );
  const capture = env.debuggerCommands.findIndex((entry, index) => index >= start && entry.method === 'Page.captureScreenshot');
  const resume = env.debuggerCommands.findIndex((entry, index) =>
    index >= start && entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('("resumeOverlay",')
  );
  assert.ok(capture >= start, 'capture should have been attempted');
  assert.ok(resume > capture, 'finally must restore the glow after capture failure');
  assert.deepEqual(env.detachCalls, []);
});

test('one transient overlay refresh failure is retried before releasing debugger custody', async () => {
  let failOverlayCount = 0;
  const env = loadControl({
    initialTabs: { 4: { url: 'https://example.org/', title: 'Example Org', active: false, pinned: false, status: 'complete' } },
    onDebuggerCommand: async ({ method, params }) => {
      if (method === 'Runtime.evaluate' && String(params.expression || '').includes('"overlay"') && failOverlayCount > 0) {
        failOverlayCount -= 1;
        throw new Error('transient renderer context loss');
      }
    }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 4 }));
  failOverlayCount = 1;
  await env.control.heartbeat();
  assert.deepEqual(env.detachCalls, [], 'one transient overlay error must be repaired without invalidating the tab handle');
});

test('persistent overlay refresh failure still releases debugger custody fail-closed', async () => {
  let failOverlayCount = 0;
  const env = loadControl({
    initialTabs: { 4: { url: 'https://example.org/', title: 'Example Org', active: false, pinned: false, status: 'complete' } },
    onDebuggerCommand: async ({ method, params }) => {
      if (method === 'Runtime.evaluate' && String(params.expression || '').includes('"overlay"') && failOverlayCount > 0) {
        failOverlayCount -= 1;
        throw new Error('persistent renderer context loss');
      }
    }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 4 }));
  failOverlayCount = 2;
  await env.control.heartbeat();
  assert.deepEqual(env.detachCalls, [4], 'MoonDesk must not retain invisible debugger custody when the overlay cannot be restored');
});

test('transient browser-control bridge failure preserves exact debugger custody', async () => {
  const env = loadControl({
    initialTabs: { 3: { url: 'https://example.net/', title: 'Example Net', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 3 }));
  assert.deepEqual(env.attachCalls, [3]);
  env.setPollFailure(0);
  await env.control.pump();
  assert.deepEqual(env.detachCalls, [], 'a loopback timeout must not invalidate Codex tab handles');
});

test('authoritative browser-control rejection revokes debugger custody', async () => {
  const env = loadControl({
    initialTabs: { 3: { url: 'https://example.net/', title: 'Example Net', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 3 }));
  env.setPollFailure(401);
  await env.control.pump();
  assert.deepEqual(env.detachCalls, [3], 'credential/auth revocation must fail closed');
});

test('transient result acknowledgement failure preserves attachment and durable receipt', async () => {
  const pendingCommand = {
    id: 'request-attach-transient-ack',
    epoch: 'epoch-1',
    owner: 'owner-a',
    tool: 'user_browser_tabs',
    args: { action: 'attach', tabId: 9 },
    expiresAt: Date.now() + 10_000
  };
  const env = loadControl({
    initialTabs: { 9: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } },
    pendingCommand
  });
  env.setResultFailure(0);
  await env.control.pump();
  assert.deepEqual(env.attachCalls, [9]);
  assert.deepEqual(env.detachCalls, [], 'lost result ACK must not invalidate the exact attached tab');
  assert.ok(env.getSessionStore().moondeskUserBrowserControlV1?.receipt, 'unacknowledged result must survive MV3 service-worker suspension for retry');
  env.clearResultFailure();
  await env.control.pump();
  assert.equal(env.getSessionStore().moondeskUserBrowserControlV1?.receipt ?? null, null, 'later successful ACK clears the session receipt');
  assert.deepEqual(env.detachCalls, []);
});

test('poll loop maps user_browser tool names into the tab executor and returns the result once', async () => {
  const pendingCommand = {
    id: 'request-1',
    epoch: 'epoch-1',
    owner: 'owner-a',
    tool: 'user_browser_tabs',
    args: { action: 'list' },
    expiresAt: Date.now() + 10_000
  };
  const env = loadControl({
    initialTabs: { 7: { url: 'https://example.org/', title: 'Example Org', active: false, pinned: false, status: 'complete' } },
    pendingCommand
  });
  await env.control.pump();
  assert.equal(env.results.length, 1);
  assert.equal(env.results[0].value.tabs.length, 1);
  assert.equal(env.results[0].value.tabs[0].tabId, 7);
  assert.equal(env.results[0].value.tabs[0].url, 'https://example.org/');
});