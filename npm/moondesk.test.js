const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { cleanManagedUpdateEnv, orchestrate, runNative } = require("./moondesk");
const { UPDATE_EXIT_CODE, changelogNoticePath, currentVersion } = require("./update-manager");

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

function orchestrateWithoutRefresh(options) {
  return orchestrate({
    refreshUpdateRequestToLatestImpl: async (request) => request,
    ...options,
  });
}

test("native launch keeps the npm wrapper alive across parent SIGINT until the child exits", async () => {
  const child = new EventEmitter();
  const signalTarget = new EventEmitter();
  let settled = false;
  const running = runNative("/fake/moondesk", [], {
    signalTarget,
    spawnImpl: () => child,
  }).then((result) => {
    settled = true;
    return result;
  });

  assert.equal(signalTarget.listenerCount("SIGINT"), 1);
  signalTarget.emit("SIGINT");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(settled, false, "parent SIGINT must not terminate the wrapper while native MoonDesk owns shutdown");

  child.emit("exit", 0, null);
  assert.deepEqual(await running, { code: 0, signal: null });
  assert.equal(signalTarget.listenerCount("SIGINT"), 0, "SIGINT listener must be removed after native exit");
});

test("native launch removes the parent SIGINT listener when spawn fails", async () => {
  const child = new EventEmitter();
  const signalTarget = new EventEmitter();
  const running = runNative("/fake/moondesk", [], {
    signalTarget,
    spawnImpl: () => child,
  });

  assert.equal(signalTarget.listenerCount("SIGINT"), 1);
  child.emit("error", new Error("spawn failed"));
  await assert.rejects(running, /spawn failed/);
  assert.equal(signalTarget.listenerCount("SIGINT"), 0);
});

test("managed update environment variables never leak into npm or the restarted wrapper", () => {
  assert.deepEqual(
    cleanManagedUpdateEnv({
      PATH: "/bin",
      MOONDESK_NPM_MANAGED: "stale",
      MOONDESK_UPDATE_REQUEST_PATH: "stale-request",
      MOONDESK_UPDATE_STATE_PATH: "stale-state",
      MOONDESK_CHANGELOG_NOTICE_PATH: "stale-changelog",
    }),
    { PATH: "/bin" },
  );
});

test("normal native exit leaves npm untouched and stops the update monitor", async () => {
  const dir = tempDir();
  const requestPath = path.join(dir, "request.json");
  const sequence = [];
  try {
    const result = await orchestrateWithoutRefresh({
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
    const result = await orchestrateWithoutRefresh({
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
        assert.equal(options.env.MOONDESK_CHANGELOG_NOTICE_PATH, changelogNoticePath());
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
      writePostUpdateNoticeImpl: (request, version, options) => {
        sequence.push("changelog");
        assert.equal(request.targetVersion, targetVersion);
        assert.equal(version, targetVersion);
        assert.equal(options, undefined);
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
      "changelog",
      "restart",
    ]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a stale pending update jumps directly to a newer version before installing", async () => {
  const dir = tempDir();
  const staleTarget = nextVersion();
  const [major, minor, patch] = staleTarget.split(".").map(Number);
  const latestTarget = `${major}.${minor}.${patch + 1}`;
  const sequence = [];
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
      readUpdateRequestImpl: () => ({
        schemaVersion: 1,
        currentVersion,
        targetVersion: staleTarget,
        releaseNotes: ["Cached intermediate release"],
        releaseUrl: `https://github.com/Shattermoon/moondesk/releases/tag/v${staleTarget}`,
      }),
      acquireUpdateLockImpl: async () => {
        sequence.push("lock");
        return () => sequence.push("unlock");
      },
      refreshUpdateRequestToLatestImpl: async (request) => {
        sequence.push("refresh");
        assert.equal(request.targetVersion, staleTarget);
        return {
          ...request,
          targetVersion: latestTarget,
          releaseNotes: [
            `v${latestTarget}: Newest release`,
            `v${staleTarget}: Intermediate release`,
          ],
          releaseUrl: `https://github.com/Shattermoon/moondesk/releases/tag/v${latestTarget}`,
        };
      },
      installedWrapperVersionImpl: () => currentVersion,
      installExactVersionImpl: async (version) => {
        sequence.push(`install:${version}`);
        assert.equal(version, latestTarget);
      },
      verifyInstalledWrapperVersionImpl: (version) => {
        sequence.push(`verify:${version}`);
        assert.equal(version, latestTarget);
      },
      writePostUpdateNoticeImpl: (request, version) => {
        sequence.push("notice");
        assert.equal(request.targetVersion, latestTarget);
        assert.equal(version, latestTarget);
        assert.deepEqual(request.releaseNotes, [
          `v${latestTarget}: Newest release`,
          `v${staleTarget}: Intermediate release`,
        ]);
      },
      restartUpdatedWrapperImpl: async () => {
        sequence.push("restart");
        return { code: 0, signal: null };
      },
    });

    assert.deepEqual(result, { code: 0, signal: null });
    assert.deepEqual(sequence, [
      "lock",
      "refresh",
      `install:${latestTarget}`,
      `verify:${latestTarget}`,
      "unlock",
      "notice",
      "restart",
    ]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a failed last-second refresh still installs the validated cached target", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  const installed = [];
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
      refreshUpdateRequestToLatestImpl: async () => {
        throw new Error("simulated registry outage");
      },
      installedWrapperVersionImpl: () => currentVersion,
      installExactVersionImpl: async (version) => installed.push(version),
      verifyInstalledWrapperVersionImpl: (version) => assert.equal(version, targetVersion),
      writePostUpdateNoticeImpl: () => {},
      restartUpdatedWrapperImpl: async () => ({ code: 0, signal: null }),
    });

    assert.deepEqual(result, { code: 0, signal: null });
    assert.deepEqual(installed, [targetVersion]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("post-update notice failure never blocks a successful restart", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  let restartCalled = false;
  try {
    const result = await orchestrateWithoutRefresh({
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
      installExactVersionImpl: async () => {},
      verifyInstalledWrapperVersionImpl: () => {},
      writePostUpdateNoticeImpl: () => {
        throw new Error("simulated notice write failure");
      },
      restartUpdatedWrapperImpl: async () => {
        restartCalled = true;
        return { code: 0, signal: null };
      },
    });

    assert.deepEqual(result, { code: 0, signal: null });
    assert.equal(restartCalled, true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("post-update notice is persisted before restart so manual relaunch can still show it", async () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  const sequence = [];
  try {
    const result = await orchestrateWithoutRefresh({
      cwd: dir,
      logger: quietLogger(),
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => "/fake/moondesk",
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => ({ code: UPDATE_EXIT_CODE, signal: null }),
      readUpdateRequestImpl: () => ({
        schemaVersion: 1,
        currentVersion,
        targetVersion,
        releaseNotes: ["Persistent changelog"],
      }),
      acquireUpdateLockImpl: async () => () => {},
      installedWrapperVersionImpl: () => currentVersion,
      installExactVersionImpl: async () => {},
      verifyInstalledWrapperVersionImpl: () => {},
      writePostUpdateNoticeImpl: () => sequence.push("notice"),
      restartUpdatedWrapperImpl: async () => {
        sequence.push("restart");
        throw new Error("simulated restart failure");
      },
    });

    assert.deepEqual(result, { code: 1, signal: null });
    assert.deepEqual(sequence, ["notice", "restart"]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("special exit without a valid request cannot trigger npm", async () => {
  const dir = tempDir();
  let installCalled = false;
  try {
    const result = await orchestrateWithoutRefresh({
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
    const result = await orchestrateWithoutRefresh({
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
    const normal = await orchestrateWithoutRefresh({
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
    const failed = await orchestrateWithoutRefresh({
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

test("Windows quarantine between verification and spawn reports actionable security guidance", async () => {
  const dir = tempDir();
  const binaryPath = path.join(dir, "moondesk.exe");
  const errors = [];
  try {
    fs.writeFileSync(binaryPath, "verified-native-binary");
    const result = await orchestrateWithoutRefresh({
      cwd: dir,
      platform: "win32",
      logger: {
        log() {},
        warn() {},
        error(message) {
          errors.push(message);
        },
      },
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => binaryPath,
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => {
        fs.rmSync(binaryPath, { force: true });
        const error = new Error("spawn UNKNOWN");
        error.code = "UNKNOWN";
        throw error;
      },
    });

    assert.deepEqual(result, { code: 1, signal: null });
    const text = errors.join("\n");
    assert.match(text, /MoonDesk failed to start: spawn UNKNOWN/);
    assert.match(text, /native binary disappeared before Windows could launch it/);
    assert.match(text, /Windows Security or another antivirus may have quarantined/);
    assert.match(text, /Protection history/);
    assert.match(text, new RegExp(binaryPath.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("Windows ENOENT with an existing binary does not blame endpoint security", async () => {
  const dir = tempDir();
  const binaryPath = path.join(dir, "moondesk.exe");
  const errors = [];
  try {
    fs.writeFileSync(binaryPath, "verified-native-binary");
    const result = await orchestrateWithoutRefresh({
      cwd: dir,
      platform: "win32",
      logger: {
        log() {},
        warn() {},
        error(message) {
          errors.push(message);
        },
      },
      updateStatePath: path.join(dir, "state.json"),
      updateRequestPath: path.join(dir, "request.json"),
      ensureBinaryImpl: async () => binaryPath,
      cleanupOldBinaryVersionsImpl: () => {},
      cleanupOldUpdateVersionsImpl: () => {},
      startUpdateMonitorImpl: () => () => {},
      runNativeImpl: async () => {
        const error = new Error("spawn ENOENT");
        error.code = "ENOENT";
        throw error;
      },
    });

    assert.deepEqual(result, { code: 1, signal: null });
    const text = errors.join("\n");
    assert.match(text, /MoonDesk failed to start: spawn ENOENT/);
    assert.doesNotMatch(text, /Windows Security|antivirus|endpoint security|Protection history/);
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
    const result = await orchestrateWithoutRefresh({
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
      writePostUpdateNoticeImpl: () => {},
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
    const result = await orchestrateWithoutRefresh({
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
    const result = await orchestrateWithoutRefresh({
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
    const result = await orchestrateWithoutRefresh({
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
        assert.equal(options.env.MOONDESK_CHANGELOG_NOTICE_PATH, undefined);
        return { code: 0, signal: null };
      },
    });
    assert.deepEqual(result, { code: 0, signal: null });
    assert.equal(monitorStarted, false);
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /disabled in-app self-update/);
    assert.match(warnings[0], /read-only home/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
