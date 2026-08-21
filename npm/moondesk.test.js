const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { cleanManagedUpdateEnv, orchestrate } = require("./moondesk");
const { UPDATE_EXIT_CODE, currentVersion } = require("./update-manager");

function tempDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "moondesk-wrapper-test-"));
}

function nextVersion() {
  const [major, minor, patch] = currentVersion.split(".").map(Number);
  return `${major}.${minor}.${patch + 1}`;
}

function quietLogger() {
  return { log() {}, warn() {}, error() {} };
}

test("managed update environment variables never leak into npm or the restarted wrapper", () => {
  assert.deepEqual(
    cleanManagedUpdateEnv({
      PATH: "/bin",
      MOONDESK_NPM_MANAGED: "stale",
      MOONDESK_UPDATE_REQUEST_PATH: "stale-request",
      MOONDESK_UPDATE_STATE_PATH: "stale-state",
    }),
    { PATH: "/bin" },
  );
});

test("normal native exit leaves npm untouched and stops the update monitor", async () => {
  const dir = tempDir();
  const requestPath = path.join(dir, "request.json");
  const sequence = [];
  try {
    const result = await orchestrate({
      args: ["--example"],
      cwd: dir,
      env: { PATH: "/bin" },
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: requestPath,
      ensureBinaryImpl: async () => {
        sequence.push("ensure");
        return "/fake/moondesk";
      },
      cleanupOldBinaryVersionsImpl: () => sequence.push("cleanup"),
      cleanupOldUpdateVersionsImpl: () => sequence.push("update-cleanup"),
      startUpdateMonitorImpl: () => {
        sequence.push("monitor-start");
        return () => sequence.push("monitor-stop");
      },
      runNativeImpl: async (_binary, args, options) => {
        sequence.push("native");
        assert.deepEqual(args, ["--example"]);
        assert.equal(options.cwd, dir);
        assert.equal(options.env.MOONDESK_NPM_MANAGED, "1");
        assert.equal(options.env.MOONDESK_UPDATE_REQUEST_PATH, requestPath);
        return { code: 0, signal: null };
      },
      readUpdateRequestImpl: () => {
        throw new Error("normal exit must not consume an update request");
      },
      installExactVersionImpl: async () => {
        throw new Error("normal exit must not update npm");
      },
    });

    assert.deepEqual(result, { code: 0, signal: null });
    assert.deepEqual(sequence, ["monitor-start", "ensure", "cleanup", "update-cleanup", "native", "monitor-stop"]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("validated update exit installs the exact version, verifies it, and restarts in place", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  const requestPath = path.join(dir, "request.json");
  const statePath = path.join(dir, "state.json");
  const sequence = [];
  const baseEnv = {
    PATH: "/bin",
    KEEP_ME: "yes",
    MOONDESK_NPM_MANAGED: "stale",
    MOONDESK_UPDATE_REQUEST_PATH: "stale-request",
    MOONDESK_UPDATE_STATE_PATH: "stale-state",
  };
  try {
    const result = await orchestrate({
      args: ["arg-one"],
      cwd: dir,
      env: baseEnv,
      wrapperPath: "/global/node_modules/moondesk/npm/moondesk.js",
      logger: quietLogger(),
      updateStatePath: statePath,
      updateRequestPath: requestPath,
      ensureBinaryImpl: async () => {
        sequence.push("ensure");
        return "/cache/moondesk";
      },
      cleanupOldBinaryVersionsImpl: () => sequence.push("cleanup"),
      cleanupOldUpdateVersionsImpl: () => sequence.push("update-cleanup"),
      startUpdateMonitorImpl: (options) => {
        sequence.push("monitor-start");
        assert.equal(options.statePath, statePath);
        return () => sequence.push("monitor-stop");
      },
      runNativeImpl: async (_binary, args, options) => {
        sequence.push("native");
        assert.deepEqual(args, ["arg-one"]);
        assert.equal(options.cwd, dir);
        assert.equal(options.env.KEEP_ME, "yes");
        assert.equal(options.env.MOONDESK_NPM_MANAGED, "1");
        assert.equal(options.env.MOONDESK_UPDATE_STATE_PATH, statePath);
        assert.equal(options.env.MOONDESK_UPDATE_REQUEST_PATH, requestPath);
        return { code: UPDATE_EXIT_CODE, signal: null };
      },
      readUpdateRequestImpl: (seenPath) => {
        sequence.push("request");
        assert.equal(seenPath, requestPath);
        return { schemaVersion: 1, currentVersion, targetVersion };
      },
      acquireUpdateLockImpl: async () => {
        sequence.push("lock");
        return () => sequence.push("unlock");
      },
      installedWrapperVersionImpl: () => currentVersion,
      installExactVersionImpl: async (version, options) => {
        sequence.push("install");
        assert.equal(version, targetVersion);
        assert.equal(options.cwd, dir);
        assert.equal(options.env.KEEP_ME, "yes");
        assert.equal(options.env.MOONDESK_NPM_MANAGED, undefined);
        assert.equal(options.env.MOONDESK_UPDATE_REQUEST_PATH, undefined);
        assert.equal(options.env.MOONDESK_UPDATE_STATE_PATH, undefined);
      },
      verifyInstalledWrapperVersionImpl: (version) => {
        sequence.push("verify");
        assert.equal(version, targetVersion);
      },
      restartUpdatedWrapperImpl: async (wrapperPath, args, options) => {
        sequence.push("restart");
        assert.equal(wrapperPath, "/global/node_modules/moondesk/npm/moondesk.js");
        assert.deepEqual(args, ["arg-one"]);
        assert.equal(options.cwd, dir);
        assert.equal(options.env.KEEP_ME, "yes");
        assert.equal(options.env.MOONDESK_NPM_MANAGED, undefined);
        return { code: 0, signal: null };
      },
    });

    assert.deepEqual(result, { code: 0, signal: null });
    assert.deepEqual(sequence, [
      "monitor-start",
      "ensure",
      "cleanup",
      "update-cleanup",
      "native",
      "monitor-stop",
      "request",
      "lock",
      "install",
      "verify",
      "unlock",
      "restart",
    ]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("special exit without a valid request cannot trigger npm", async () => {
  const dir = tempDir();
  let installCalled = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => null,
      installExactVersionImpl: async () => {
        installCalled = true;
      },
    });
    assert.deepEqual(result, { code: 1, signal: null });
    assert.equal(installCalled, false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("failed npm update never restarts MoonDesk", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  let restartCalled = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => ({ schemaVersion: 1, currentVersion, targetVersion }),
      acquireUpdateLockImpl: async () => () => {},
      installedWrapperVersionImpl: () => currentVersion,
      installExactVersionImpl: async () => {
        throw new Error("simulated npm failure");
      },
      restartUpdatedWrapperImpl: async () => {
        restartCalled = true;
        return { code: 0, signal: null };
      },
    });
    assert.deepEqual(result, { code: 1, signal: null });
    assert.equal(restartCalled, false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("old-cache cleanup failure is non-fatal but native startup failure is fatal", async () => {
  const dir = tempDir();
  try {
    const normal = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state-a.json"),
      updateRequestPath: path.join(dir, "request-a.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {
        throw new Error("locked old cache");
      },
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: 0, signal: null }),
    });
    assert.deepEqual(normal, { code: 0, signal: null });

    let stopped = false;
    const failed = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state-b.json"),
      updateRequestPath: path.join(dir, "request-b.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {
        stopped = true;
      },
      runNativeImpl: async () => {
        throw new Error("spawn failed");
      },
    });
    assert.deepEqual(failed, { code: 1, signal: null });
    assert.equal(stopped, true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("another process installing the requested version skips duplicate npm work and restarts", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  let installCalled = false;
  let restartCalled = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => ({ schemaVersion: 1, currentVersion, targetVersion }),
      acquireUpdateLockImpl: async () => () => {},
      installedWrapperVersionImpl: () => targetVersion,
      installExactVersionImpl: async () => {
        installCalled = true;
      },
      restartUpdatedWrapperImpl: async () => {
        restartCalled = true;
        return { code: 0, signal: null };
      },
    });
    assert.deepEqual(result, { code: 0, signal: null });
    assert.equal(installCalled, false);
    assert.equal(restartCalled, true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a newer already-installed version is never downgraded by a stale updater", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  const [major, minor, patch] = targetVersion.split(".").map(Number);
  const newerVersion = `${major}.${minor}.${patch + 1}`;
  let installCalled = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => ({ schemaVersion: 1, currentVersion, targetVersion }),
      acquireUpdateLockImpl: async () => () => {},
      installedWrapperVersionImpl: () => newerVersion,
      installExactVersionImpl: async () => {
        installCalled = true;
      },
      restartUpdatedWrapperImpl: async () => ({ code: 0, signal: null }),
    });
    assert.deepEqual(result, { code: 0, signal: null });
    assert.equal(installCalled, false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("failure to acquire the npm update lock touches neither npm nor restart", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  let installCalled = false;
  let restartCalled = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => ({ schemaVersion: 1, currentVersion, targetVersion }),
      acquireUpdateLockImpl: async () => {
        throw new Error("simulated lock timeout");
      },
      installExactVersionImpl: async () => {
        installCalled = true;
      },
      restartUpdatedWrapperImpl: async () => {
        restartCalled = true;
        return { code: 0, signal: null };
      },
    });
    assert.deepEqual(result, { code: 1, signal: null });
    assert.equal(installCalled, false);
    assert.equal(restartCalled, false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});


test("self-update path failures disable updates without blocking MoonDesk startup", async () => {
  const dir = tempDir();
  const warnings = [];
  let monitorStarted = false;
  try {
    const result = await orchestrate({
      cwd: dir,
      logger: { log() {}, error() {}, warn(message) { warnings.push(message); } },
      createUpdateStatePathImpl: () => {
        const error = new Error("read-only home");
        error.code = "EROFS";
        throw error;
      },
      createUpdateRequestPathImpl: () => {
        throw new Error("request path should not be attempted after state path failure");
      },
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => {
        monitorStarted = true;
        return () => {};
      },
      runNativeImpl: async (_binary, _args, options) => {
        assert.equal(options.env.MOONDESK_NPM_MANAGED, undefined);
        assert.equal(options.env.MOONDESK_UPDATE_STATE_PATH, undefined);
        assert.equal(options.env.MOONDESK_UPDATE_REQUEST_PATH, undefined);
        return { code: 0, signal: null };
      },
    });
    assert.deepEqual(result, { code: 0, signal: null });
    assert.equal(monitorStarted, false);
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /disabled in-app self-update/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
