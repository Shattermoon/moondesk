const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { EventEmitter } = require("node:events");
const { PassThrough } = require("node:stream");
const test = require("node:test");

const {
  acquireUpdateLock,
  atomicWriteJson,
  checkForUpdate,
  cleanupOldUpdateVersions,
  compareStableVersions,
  createUpdateStatePath,
  currentVersion,
  installExactVersion,
  isGlobalPackageInstall,
  parseStableVersion,
  readUpdateRequest,
  resolveGlobalNpmRoot,
  restartUpdatedWrapper,
  startUpdateMonitor,
  verifyInstalledWrapperVersion,
} = require("./update-manager");

function tempDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "moondesk-update-test-"));
}

function nextVersion(version = currentVersion) {
  const [major, minor, patch] = version.split(".").map(Number);
  return `${major}.${minor}.${patch + 1}`;
}

function responseJson(value) {
  const body = Buffer.from(JSON.stringify(value));
  return new Response(body, {
    status: 200,
    headers: { "content-length": String(body.length) },
  });
}

function npmMetadata(version) {
  return {
    name: "moondesk",
    version,
    dist: { integrity: "sha512-dGVzdC1tb29uZGVzaw==" },
  };
}

function fakeChild(exitCode = 0, signal = null) {
  const child = new EventEmitter();
  queueMicrotask(() => child.emit("exit", exitCode, signal));
  return child;
}

function fakeStdoutChild(stdoutText, exitCode = 0) {
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.kill = () => {};
  queueMicrotask(() => {
    child.stdout.end(stdoutText);
    child.emit("exit", exitCode, null);
  });
  return child;
}

test("stable version parsing and comparison are strict", () => {
  assert.deepEqual(parseStableVersion("1.2.3"), [1n, 2n, 3n]);
  assert.equal(parseStableVersion("01.2.3"), null);
  assert.equal(parseStableVersion("1.2.3-beta.1"), null);
  assert.equal(compareStableVersions("1.2.3", "1.2.4"), -1);
  assert.equal(compareStableVersions("2.0.0", "1.99.99"), 1);
  assert.equal(compareStableVersions("3.4.5", "3.4.5"), 0);
});

test("update state is isolated by installed version and wrapper process", () => {
  const first = createUpdateStatePath().replaceAll("\\", "/");
  const second = createUpdateStatePath().replaceAll("\\", "/");
  const pattern = new RegExp(
    `/\\.moondesk/updates/v${currentVersion.replaceAll(".", "\\.")}/state/${process.pid}-[0-9a-f]{24}\\.json$`,
  );
  assert.match(first, pattern);
  assert.match(second, pattern);
  assert.notEqual(first, second);
});

test("npm global-root detection enables self-update only for the exact global package", async () => {
  const globalRoot = path.join(tempDir(), "global-node-modules");
  const calls = [];
  const spawnImpl = (command, args, options) => {
    calls.push({ command, args, options });
    return fakeStdoutChild(`${globalRoot}\n`);
  };

  const resolved = await resolveGlobalNpmRoot({ platform: "linux", spawnImpl });
  assert.equal(resolved, globalRoot);
  assert.equal(calls[0].command, "npm");
  assert.deepEqual(calls[0].args, ["root", "--global"]);

  assert.equal(
    await isGlobalPackageInstall({
      platform: "linux",
      packageRoot: path.join(globalRoot, "moondesk"),
      spawnImpl,
    }),
    true,
  );
  assert.equal(
    await isGlobalPackageInstall({
      platform: "linux",
      packageRoot: path.join(globalRoot, "other", "node_modules", "moondesk"),
      spawnImpl,
    }),
    false,
  );

  fs.rmSync(path.dirname(globalRoot), { recursive: true, force: true });
});

test("stopping during npm global-root detection kills the probe immediately", async () => {
  const controller = new AbortController();
  let killed = false;
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.kill = () => {
    killed = true;
  };

  const pending = resolveGlobalNpmRoot({
    platform: "linux",
    signal: controller.signal,
    spawnImpl: () => child,
  });
  controller.abort();
  await assert.rejects(pending, /npm root --global was aborted/);
  assert.equal(killed, true);
});

test("local or npx-style wrappers never query npm latest for self-update", async () => {
  const dir = tempDir();
  const statePath = path.join(dir, "state.json");
  let fetched = false;
  try {
    const stop = startUpdateMonitor({
      statePath,
      intervalMs: 60_000,
      isGlobalPackageInstallImpl: async () => false,
      fetchImpl: async () => {
        fetched = true;
        return responseJson(npmMetadata(nextVersion()));
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    stop();
    assert.equal(fetched, false);
    assert.equal(fs.existsSync(statePath), false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("old update metadata cleanup removes only older stable versions", () => {
  const dir = tempDir();
  try {
    for (const tag of ["v1.2.1", "v1.2.2", "v1.2.3", "v2.0.0", "notes"]) {
      fs.mkdirSync(path.join(dir, tag), { recursive: true });
      fs.writeFileSync(path.join(dir, tag, "marker"), tag);
    }
    const result = cleanupOldUpdateVersions({ root: dir, currentTag: "v1.2.3" });
    assert.deepEqual(result.skipped, []);
    assert.deepEqual(result.removed.sort(), ["v1.2.1", "v1.2.2"]);
    assert.equal(fs.existsSync(path.join(dir, "v1.2.3")), true);
    assert.equal(fs.existsSync(path.join(dir, "v2.0.0")), true);
    assert.equal(fs.existsSync(path.join(dir, "notes")), true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("atomic update state replacement overwrites an existing file", () => {
  const dir = tempDir();
  try {
    const statePath = path.join(dir, "state.json");
    atomicWriteJson(statePath, { version: 1 });
    atomicWriteJson(statePath, { version: 2 });
    assert.deepEqual(JSON.parse(fs.readFileSync(statePath, "utf8")), { version: 2 });
    assert.equal(
      fs.readdirSync(dir).some((name) => name.includes(".tmp-")),
      false,
      "temporary update state files must not be left behind",
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("update check writes an available exact npm latest version", async () => {
  const dir = tempDir();
  const statePath = path.join(dir, "update-state.json");
  try {
    const latest = nextVersion();
    const state = await checkForUpdate({
      statePath,
      managedInstall: true,
      fetchImpl: async () => responseJson(npmMetadata(latest)),
      registryUrl: "https://registry.example.invalid/moondesk/latest",
    });
    assert.equal(state.currentVersion, currentVersion);
    assert.equal(state.latestVersion, latest);
    assert.equal(state.available, true);
    const persisted = JSON.parse(fs.readFileSync(statePath, "utf8"));
    assert.equal(persisted.latestVersion, latest);
    assert.equal(persisted.available, true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("update check records no update when npm latest is the current version", async () => {
  const dir = tempDir();
  try {
    const state = await checkForUpdate({
      statePath: path.join(dir, "state.json"),
      managedInstall: true,
      fetchImpl: async () => responseJson(npmMetadata(currentVersion)),
    });
    assert.equal(state.available, false);
    assert.equal(state.latestVersion, currentVersion);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("update check rejects wrong package, prerelease, and missing integrity metadata", async () => {
  const dir = tempDir();
  const statePath = path.join(dir, "state.json");
  try {
    await assert.rejects(
      checkForUpdate({
        statePath,
        fetchImpl: async () => responseJson({ ...npmMetadata(nextVersion()), name: "other" }),
      }),
      /unexpected package metadata/,
    );
    await assert.rejects(
      checkForUpdate({
        statePath,
        fetchImpl: async () => responseJson(npmMetadata("999.0.0-beta.1")),
      }),
      /invalid MoonDesk version/,
    );
    await assert.rejects(
      checkForUpdate({
        statePath,
        fetchImpl: async () => responseJson({ name: "moondesk", version: nextVersion() }),
      }),
      /without a sha512 package integrity value/,
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("update metadata streaming enforces the byte limit without content-length", async () => {
  const dir = tempDir();
  try {
    const fetchImpl = async () => ({
      ok: true,
      status: 200,
      headers: { get: () => null },
      body: {
        async *[Symbol.asyncIterator]() {
          yield Buffer.alloc(64 * 1024);
          yield Buffer.from("x");
        },
      },
    });
    await assert.rejects(
      checkForUpdate({
        statePath: path.join(dir, "state.json"),
        managedInstall: true,
        fetchImpl,
      }),
      /metadata exceeded the download limit/,
    );
    assert.equal(fs.existsSync(path.join(dir, "state.json")), false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("stopping a monitor removes state even when a late response ignores abort", async () => {
  const dir = tempDir();
  const statePath = path.join(dir, "state.json");
  let releaseBody;
  let fetchStartedResolve;
  const fetchStarted = new Promise((resolve) => { fetchStartedResolve = resolve; });
  const bodyReleased = new Promise((resolve) => { releaseBody = resolve; });
  try {
    const fetchImpl = async () => {
      fetchStartedResolve();
      const payload = Buffer.from(JSON.stringify(npmMetadata(nextVersion())));
      return {
        ok: true,
        status: 200,
        headers: { get: () => null },
        body: {
          async *[Symbol.asyncIterator]() {
            await bodyReleased;
            yield payload;
          },
        },
      };
    };
    const stop = startUpdateMonitor({
      statePath,
      fetchImpl,
      intervalMs: 60_000,
      isGlobalPackageInstallImpl: async () => true,
    });
    await fetchStarted;
    stop();
    releaseBody();
    for (let attempt = 0; attempt < 40; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 5));
      if (!fs.existsSync(statePath)) break;
    }
    assert.equal(fs.existsSync(statePath), false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("stopping the update monitor aborts an in-flight registry request", async () => {
  const dir = tempDir();
  let requestStarted = false;
  let requestAborted = false;
  try {
    const fetchImpl = async (_url, options) => {
      requestStarted = true;
      return new Promise((_resolve, reject) => {
        options.signal.addEventListener(
          "abort",
          () => {
            requestAborted = true;
            reject(new Error("aborted"));
          },
          { once: true },
        );
      });
    };
    const stop = startUpdateMonitor({
      statePath: path.join(dir, "state.json"),
      fetchImpl,
      intervalMs: 60_000,
      isGlobalPackageInstallImpl: async () => true,
    });
    for (let attempt = 0; attempt < 20 && !requestStarted; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.equal(requestStarted, true);
    stop();
    for (let attempt = 0; attempt < 20 && !requestAborted; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.equal(requestAborted, true);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("update requests require the current package version and a newer target", () => {
  const dir = tempDir();
  try {
    const targetVersion = nextVersion();
    const requestPath = path.join(dir, "request.json");
    fs.writeFileSync(
      requestPath,
      JSON.stringify({ schemaVersion: 1, currentVersion, targetVersion }),
    );
    assert.deepEqual(readUpdateRequest(requestPath), {
      schemaVersion: 1,
      currentVersion,
      targetVersion,
    });
    assert.equal(fs.existsSync(requestPath), false, "request is consumed exactly once");

    fs.writeFileSync(
      requestPath,
      JSON.stringify({ schemaVersion: 1, currentVersion: "0.0.0", targetVersion }),
    );
    assert.equal(readUpdateRequest(requestPath), null);
    assert.equal(fs.existsSync(requestPath), false, "invalid requests are consumed too");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("global npm update lock serializes concurrent updaters and recovers stale locks", async () => {
  const dir = tempDir();
  const lockPath = path.join(dir, "npm-update.lock");
  try {
    const releaseFirst = await acquireUpdateLock({ lockPath, waitMs: 20, pollMs: 2 });
    assert.equal(fs.existsSync(lockPath), true);
    await assert.rejects(
      acquireUpdateLock({ lockPath, waitMs: 10, pollMs: 2 }),
      /another MoonDesk process is still updating/,
    );
    releaseFirst();
    assert.equal(fs.existsSync(lockPath), false);

    fs.writeFileSync(lockPath, "{}\n");
    const releaseRecovered = await acquireUpdateLock({ lockPath, waitMs: 20, pollMs: 2 });
    assert.equal(fs.existsSync(lockPath), true);
    releaseRecovered();
    assert.equal(fs.existsSync(lockPath), false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("exact updater invokes npm globally with scripts disabled", async () => {
  const targetVersion = nextVersion();
  const calls = [];
  const spawnImpl = (command, args, options) => {
    calls.push({ command, args, options });
    return fakeChild();
  };

  await installExactVersion(targetVersion, {
    platform: "linux",
    cwd: "/workspace",
    env: { PATH: "/bin" },
    spawnImpl,
  });

  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "npm");
  assert.deepEqual(calls[0].args, [
    "install",
    "--global",
    `moondesk@${targetVersion}`,
    "--ignore-scripts",
    "--no-audit",
    "--no-fund",
  ]);
  assert.equal(calls[0].options.shell, false);
});

test("exact updater refuses downgrade or same-version requests and reports npm failure", async () => {
  await assert.rejects(installExactVersion(currentVersion), /Refusing invalid MoonDesk update target/);
  assert.throws(() => compareStableVersions("1.2", "1.2.3"), /stable semantic versions/);
  await assert.rejects(
    installExactVersion(nextVersion(), {
      platform: "linux",
      spawnImpl: () => fakeChild(7),
    }),
    /npm install exited with code 7/,
  );
});

test("updated wrapper metadata must match the exact requested version", () => {
  const dir = tempDir();
  const targetVersion = nextVersion();
  const metadataPath = path.join(dir, "package.json");
  try {
    fs.writeFileSync(metadataPath, JSON.stringify({ name: "moondesk", version: targetVersion }));
    assert.equal(verifyInstalledWrapperVersion(targetVersion, { packageJsonPath: metadataPath }), true);

    fs.writeFileSync(metadataPath, JSON.stringify({ name: "moondesk", version: currentVersion }));
    assert.throws(
      () => verifyInstalledWrapperVersion(targetVersion, { packageJsonPath: metadataPath }),
      /npm reported success but installed/,
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("restart launches the wrapper with the same arguments and working directory", async () => {
  const calls = [];
  const result = await restartUpdatedWrapper("/package/npm/moondesk.js", ["--example"], {
    cwd: "/workspace",
    env: { PATH: "/bin" },
    spawnImpl: (command, args, options) => {
      calls.push({ command, args, options });
      return fakeChild(0);
    },
  });

  assert.deepEqual(result, { code: 0, signal: null });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, process.execPath);
  assert.deepEqual(calls[0].args, ["/package/npm/moondesk.js", "--example"]);
  assert.equal(calls[0].options.cwd, "/workspace");
});
