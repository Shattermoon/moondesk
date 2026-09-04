const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const test = require("node:test");

const {
  BROWSER_CLI_FLAG,
  orchestrate,
  runNativeBrowser,
  usageText,
} = require("./moondesk-browser");

function logger() {
  const logs = [];
  const errors = [];
  return {
    logs,
    errors,
    log(value) {
      logs.push(String(value));
    },
    error(value) {
      errors.push(String(value));
    },
  };
}

test("usage documents the stable browser CLI", () => {
  const usage = usageText();
  assert.match(usage, /moondesk-browser <chrome-devtools command>/);
  assert.match(usage, /moondesk-browser skill/);
  assert.match(usage, /same live/);
  assert.match(usage, /isolated agent-browser session/);
});

test("skill prints packaged guidance without preparing the native binary", async () => {
  const output = logger();
  let ensureCalls = 0;
  const result = await orchestrate({
    args: ["skill"],
    logger: output,
    readFileSyncImpl: () => "# Browser skill\nUse view_page.\n",
    ensureBinaryImpl: async () => {
      ensureCalls += 1;
      return "unused";
    },
  });
  assert.deepEqual(result, { code: 0, signal: null });
  assert.equal(ensureCalls, 0);
  assert.deepEqual(output.logs, ["# Browser skill\nUse view_page."]);
});

test("browser commands go through the verified MoonDesk native binary", async () => {
  const output = logger();
  const calls = [];
  const result = await orchestrate({
    args: ["click", "1_23", "--includeSnapshot"],
    cwd: "C:/repo",
    env: { TEST: "1" },
    logger: output,
    ensureBinaryImpl: async () => "C:/cache/moondesk.exe",
    runNativeBrowserImpl: async (binaryPath, args, options) => {
      calls.push({ binaryPath, args, options });
      return { code: 0, signal: null };
    },
  });
  assert.deepEqual(result, { code: 0, signal: null });
  assert.deepEqual(calls, [
    {
      binaryPath: "C:/cache/moondesk.exe",
      args: ["click", "1_23", "--includeSnapshot"],
      options: { cwd: "C:/repo", env: { TEST: "1" } },
    },
  ]);
});

test("runNativeBrowser preserves argument boundaries without a shell", async () => {
  const calls = [];
  const spawnImpl = (binaryPath, args, options) => {
    calls.push({ binaryPath, args, options });
    const child = new EventEmitter();
    queueMicrotask(() => child.emit("exit", 0, null));
    return child;
  };
  const result = await runNativeBrowser(
    "C:/cache/moondesk.exe",
    ["evaluate_script", "() => location.href.includes('&x=1')"],
    { spawnImpl, cwd: "C:/repo", env: { A: "B" } },
  );
  assert.deepEqual(result, { code: 0, signal: null });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].binaryPath, "C:/cache/moondesk.exe");
  assert.deepEqual(calls[0].args, [
    BROWSER_CLI_FLAG,
    "evaluate_script",
    "() => location.href.includes('&x=1')",
  ]);
  assert.equal(calls[0].options.stdio, "inherit");
  assert.equal(calls[0].options.cwd, "C:/repo");
});

test("native binary preparation failure is reported cleanly", async () => {
  const output = logger();
  const result = await orchestrate({
    args: ["list_pages"],
    logger: output,
    ensureBinaryImpl: async () => {
      throw new Error("release missing");
    },
  });
  assert.deepEqual(result, { code: 1, signal: null });
  assert.match(output.errors[0], /could not prepare its native binary/i);
});
