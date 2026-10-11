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

function loadControl({ initialTabs = {}, protectedTab = () => false, pendingCommand = null } = {}) {
  const tabs = new Map(Object.entries(initialTabs).map(([id, tab]) => [Number(id), { id: Number(id), ...tab }]));
  let nextTabId = Math.max(100, ...tabs.keys()) + 1;
  let sessionStore = {};
  const attached = new Set();
  const attachCalls = [];
  const detachCalls = [];
  const debuggerCommands = [];
  const results = [];
  let pending = pendingCommand ? { ...pendingCommand } : null;
  let failPoll = false;
  const onUpdated = eventHook();
  const onRemoved = eventHook();
  const onDebuggerEvent = eventHook();
  const onDebuggerDetach = eventHook();

  const chrome = {
    storage: {
      session: {
        async get(key) { return { [key]: sessionStore[key] }; },
        async set(value) { sessionStore = { ...sessionStore, ...value }; }
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
        if (method === 'Page.getFrameTree') {
          const tab = tabs.get(target.tabId);
          return { frameTree: { frame: { id: `frame-${target.tabId}`, url: tab?.url || 'about:blank' }, childFrames: [] } };
        }
        if (method === 'Page.createIsolatedWorld') return { executionContextId: 42 };
        if (method === 'Runtime.evaluate') return { result: { value: true } };
        if (method === 'Page.getNavigationHistory') return { currentIndex: 0, entries: [{ id: 1, url: tabs.get(target.tabId)?.url || 'about:blank' }] };
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
    if (body.action === 'poll') {
      if (failPoll) return { ok: false, status: 0, data: { error: 'bridge offline' } };
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
    onDebuggerEvent,
    setPollFailure(value = true) { failPoll = value; }
  };
}

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

test('eligible tab attach injects the blue glow, enforces one owner, and release detaches', async () => {
  const env = loadControl({
    initialTabs: { 2: { url: 'https://example.com/', title: 'Example', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  const attached = await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 2 }));
  assert.equal(attached.value.tabId, 2);
  assert.equal(attached.value.attached, true);
  assert.deepEqual(env.attachCalls, [2]);
  const overlayEval = env.debuggerCommands.find(entry => entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff99'));
  assert.ok(overlayEval, 'attach must inject the visible blue controlled-tab glow');
  assert.match(overlayEval.params.expression, /data-moondesk-browser-control/);
  assert.match(overlayEval.params.expression, /overlayTimer/);
  assert.match(overlayEval.params.expression, /6000/);
  const overlayCountBeforeHeartbeat = env.debuggerCommands.filter(entry =>
    entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff99')
  ).length;
  await env.control.pump();
  const overlayCountAfterHeartbeat = env.debuggerCommands.filter(entry =>
    entry.method === 'Runtime.evaluate' && entry.params.expression?.includes('#3984ff99')
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

test('lost browser-control bridge revokes debugger custody instead of leaving invisible control', async () => {
  const env = loadControl({
    initialTabs: { 3: { url: 'https://example.net/', title: 'Example Net', active: false, pinned: false, status: 'complete' } }
  });
  await env.control.pump();
  await env.control.execute(env.command('browser_tabs', { action: 'attach', tabId: 3 }));
  assert.deepEqual(env.attachCalls, [3]);
  env.setPollFailure();
  await env.control.pump();
  assert.deepEqual(env.detachCalls, [3]);
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