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
  changelogNoticePath,
  compareStableVersions,
  createUpdateStatePath,
  currentVersion,
  fetchReleaseChangelog,
  installExactVersion,
  isGlobalPackageInstall,
  normalizeReleaseNotes,
  parseStableVersion,
  readUpdateRequest,
  refreshUpdateRequestToLatest,
  resolveGlobalNpmRoot,
  restartUpdatedWrapper,
  startUpdateMonitor,
  verifyInstalledWrapperVersion,
  writePostUpdateNotice,
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

test("GitHub release notes normalize into concise terminal changelog items", () => {
  const notes = normalizeReleaseNotes(`## What's Changed
* feat: add native model vision tools by @nkcbuilds in https://github.com/Shattermoon/moondesk/pull/35
* fix(update): keep read-only path semantics consistent by @nkcbuilds in https://github.com/Shattermoon/moondesk/pull/37

**Full Changelog**: https://github.com/Shattermoon/moondesk/compare/v0.7.0...v0.7.1`);
  assert.deepEqual(notes, [
    "Add native model vision tools",
    "Keep read-only path semantics consistent",
  ]);
});

test("release-note normalization is bounded, de-duplicated, and ignores markdown noise", () => {
  assert.deepEqual(normalizeReleaseNotes(""), []);
  assert.deepEqual(normalizeReleaseNotes(null), []);

  const oversized = `feat: ${"x".repeat(300)}`;
  const body = [
    "# Release heading",
    "## What's Changed",
    "* feat: same change",
    "- feat: same change",
    `* ${oversized}`,
    "**Full Changelog**: https://github.com/Shattermoon/moondesk/compare/v1.0.0...v1.0.1",
    "https://example.invalid/raw-url",
    ...Array.from({ length: 20 }, (_, index) => `* fix: change ${index}`),
  ].join("\n");

  const notes = normalizeReleaseNotes(body);
  assert.equal(notes[0], "Same change");
  assert.equal(notes.filter((note) => note === "Same change").length, 1);
  assert.ok(notes[1].length <= 180);
  assert.equal(notes.length, 12);
  assert.equal(notes.some((note) => note.includes("Full Changelog")), false);
  assert.equal(notes.some((note) => note.startsWith("http")), false);
});

test("release selection ignores draft prerelease and wrong tags and canonicalizes the target URL", async () => {
  const fromVersion = "1.2.3";
  const toVersion = "1.2.4";
  const changelog = await fetchReleaseChangelog(fromVersion, toVersion, {
    releasesApiUrl: "https://api.example.invalid/releases",
    fetchImpl: async () => responseJson([
      {
        tag_name: `v${toVersion}`,
        draft: true,
        prerelease: false,
        html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${toVersion}`,
        body: "* draft change",
      },
      {
        tag_name: `v${toVersion}`,
        draft: false,
        prerelease: true,
        html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${toVersion}`,
        body: "* prerelease change",
      },
      {
        tag_name: "v9.9.9",
        draft: false,
        prerelease: false,
        html_url: "https://github.com/Shattermoon/moondesk/releases/tag/v9.9.9",
        body: "* wrong tag",
      },
      {
        tag_name: `v${toVersion}`,
        draft: false,
        prerelease: false,
        html_url: "https://example.invalid/not-the-release",
        body: "## What's Changed\n* fix: valid target release",
      },
    ]),
  });

  assert.deepEqual(changelog.releaseNotes, ["Valid target release"]);
  assert.equal(
    changelog.releaseUrl,
    `https://github.com/Shattermoon/moondesk/releases/tag/v${toVersion}`,
  );
});

test("skipped releases are folded into one bounded changelog", async () => {
  const fromVersion = "0.8.0";
  const toVersion = "0.9.1";
  const releases = [
    {
      tag_name: "v0.9.1",
      draft: false,
      prerelease: false,
      html_url: "https://github.com/Shattermoon/moondesk/releases/tag/v0.9.1",
      body: "## What's Changed\n* fix: polish updater behavior by @nkcbuilds in https://github.com/Shattermoon/moondesk/pull/102",
    },
    {
      tag_name: "v0.9.0",
      draft: false,
      prerelease: false,
      html_url: "https://github.com/Shattermoon/moondesk/releases/tag/v0.9.0",
      body: "## What's Changed\n* feat: add changelog UI by @nkcbuilds in https://github.com/Shattermoon/moondesk/pull/101",
    },
    {
      tag_name: "v0.8.0",
      draft: false,
      prerelease: false,
      html_url: "https://github.com/Shattermoon/moondesk/releases/tag/v0.8.0",
      body: "## What's Changed\n* old release that should not be repeated",
    },
  ];

  const changelog = await fetchReleaseChangelog(fromVersion, toVersion, {
    releasesApiUrl: "https://api.example.invalid/releases",
    fetchImpl: async () => responseJson(releases),
  });

  assert.deepEqual(changelog.releaseNotes, [
    "v0.9.1: Polish updater behavior",
    "v0.9.0: Add changelog UI",
  ]);
  assert.equal(
    changelog.releaseUrl,
    "https://github.com/Shattermoon/moondesk/releases/tag/v0.9.1",
  );
});

test("target release tag is used when the recent release list lags", async () => {
  const fromVersion = "0.8.0";
  const toVersion = "0.8.1";
  const changelog = await fetchReleaseChangelog(fromVersion, toVersion, {
    releasesApiUrl: "https://api.example.invalid/releases",
    releaseTagApiBase: "https://api.example.invalid/releases/tags",
    fetchImpl: async (url) => {
      if (String(url).endsWith("/releases")) return responseJson([]);
      return responseJson({
        tag_name: `v${toVersion}`,
        draft: false,
        prerelease: false,
        html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${toVersion}`,
        body: "## What's Changed\n* fix: target release fallback",
      });
    },
  });
  assert.deepEqual(changelog.releaseNotes, ["Target release fallback"]);
  assert.equal(
    changelog.releaseUrl,
    `https://github.com/Shattermoon/moondesk/releases/tag/v${toVersion}`,
  );
});

test("available update carries GitHub changelog but survives release API failure", async () => {
  const dir = tempDir();
  const latest = nextVersion();
  try {
    const statePath = path.join(dir, "with-notes.json");
    const state = await checkForUpdate({
      statePath,
      managedInstall: true,
      registryUrl: "https://registry.example.invalid/moondesk/latest",
      releasesApiUrl: "https://api.example.invalid/releases",
      fetchImpl: async (url) => {
        if (String(url).includes("registry.example.invalid")) {
          return responseJson(npmMetadata(latest));
        }
        return responseJson([{
          tag_name: `v${latest}`,
          draft: false,
          prerelease: false,
          html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${latest}`,
          body: "## What's Changed\n* feat: polished update changelog by @nkcbuilds in https://github.com/Shattermoon/moondesk/pull/99",
        }]);
      },
    });
    assert.equal(state.available, true);
    assert.deepEqual(state.releaseNotes, ["Polished update changelog"]);
    assert.equal(
      state.releaseUrl,
      `https://github.com/Shattermoon/moondesk/releases/tag/v${latest}`,
    );

    const fallback = await checkForUpdate({
      statePath: path.join(dir, "without-notes.json"),
      managedInstall: true,
      registryUrl: "https://registry.example.invalid/moondesk/latest",
      releasesApiUrl: "https://api.example.invalid/releases",
      fetchImpl: async (url) => {
        if (String(url).includes("registry.example.invalid")) {
          return responseJson(npmMetadata(latest));
        }
        throw new Error("GitHub unavailable");
      },
    });
    assert.equal(fallback.available, true);
    assert.deepEqual(fallback.releaseNotes, []);
    assert.equal(fallback.releaseUrl, null);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("post-update changelog notice is bounded and version-specific", () => {
  const dir = tempDir();
  const latest = nextVersion();
  const noticePath = path.join(dir, "post-update.json");
  try {
    assert.ok(
      changelogNoticePath("1.2.4").endsWith(path.join("updates", "v1.2.4", "post-update.json")),
    );
    assert.throws(() => changelogNoticePath("1.2.4-beta.1"), /stable semantic version/);
    assert.equal(
      writePostUpdateNotice(
        { currentVersion: "1.2.3", targetVersion: "1.2.4", releaseNotes: [] },
        "1.2.5",
        { noticePath },
      ),
      null,
    );
    writePostUpdateNotice(
      {
        currentVersion,
        targetVersion: latest,
        releaseNotes: ["First change", "Second change"],
        releaseUrl: `https://github.com/Shattermoon/moondesk/releases/tag/v${latest}`,
      },
      latest,
      { noticePath },
    );
    const notice = JSON.parse(fs.readFileSync(noticePath, "utf8"));
    assert.equal(notice.schemaVersion, 1);
    assert.equal(notice.packageName, "moondesk");
    assert.equal(notice.fromVersion, currentVersion);
    assert.equal(notice.toVersion, latest);
    assert.deepEqual(notice.releaseNotes, ["First change", "Second change"]);
    assert.equal(
      notice.releaseUrl,
      `https://github.com/Shattermoon/moondesk/releases/tag/v${latest}`,
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("refreshing a pending request jumps directly to the newest stable npm version", async () => {
  const [major, minor, patch] = currentVersion.split(".").map(Number);
  const staleTarget = `${major}.${minor}.${patch + 1}`;
  const latestTarget = `${major}.${minor}.${patch + 2}`;
  const request = {
    schemaVersion: 1,
    currentVersion,
    targetVersion: staleTarget,
    releaseNotes: ["Stale cached note"],
    releaseUrl: `https://github.com/Shattermoon/moondesk/releases/tag/v${staleTarget}`,
  };

  const refreshed = await refreshUpdateRequestToLatest(request, {
    registryUrl: "https://registry.example.invalid/moondesk/latest",
    releasesApiUrl: "https://api.example.invalid/releases",
    fetchImpl: async (url) => {
      if (String(url).includes("registry.example.invalid")) {
        return responseJson(npmMetadata(latestTarget));
      }
      return responseJson([
        {
          tag_name: `v${latestTarget}`,
          draft: false,
          prerelease: false,
          html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${latestTarget}`,
          body: "## What's Changed\n* fix: newest release",
        },
        {
          tag_name: `v${staleTarget}`,
          draft: false,
          prerelease: false,
          html_url: `https://github.com/Shattermoon/moondesk/releases/tag/v${staleTarget}`,
          body: "## What's Changed\n* feat: intermediate release",
        },
      ]);
    },
  });

  assert.equal(refreshed.targetVersion, latestTarget);
  assert.deepEqual(refreshed.releaseNotes, [
    `v${latestTarget}: Newest release`,
    `v${staleTarget}: Intermediate release`,
  ]);
  assert.equal(
    refreshed.releaseUrl,
    `https://github.com/Shattermoon/moondesk/releases/tag/v${latestTarget}`,
  );
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
      releaseNotes: [],
      releaseUrl: null,
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

test("restart re-enters the updated wrapper in the same Node process", async () => {
  const dir = tempDir();
  const npmDir = path.join(dir, "npm");
  const wrapperPath = path.join(npmDir, "moondesk.js");
  const helperPath = path.join(npmDir, "helper.js");
  try {
    fs.mkdirSync(npmDir, { recursive: true });
    fs.writeFileSync(helperPath, 'module.exports = "old";\n');
    fs.writeFileSync(
      wrapperPath,
      [
        'const helper = require("./helper");',
        "module.exports.orchestrate = async (options) => ({",
        "  code: helper === \"new\" ? 0 : 9,",
        "  signal: null,",
        "  pid: process.pid,",
        "  args: options.args,",
        "  cwd: options.cwd,",
        "  pathValue: options.env.PATH,",
        "  wrapperPath: options.wrapperPath,",
        "});",
        "",
      ].join("\n"),
    );

    const seeded = require(wrapperPath);
    assert.equal(
      (
        await seeded.orchestrate({
          args: [],
          cwd: dir,
          env: { PATH: "/seed" },
          wrapperPath,
        })
      ).code,
      9,
      "test must seed the old package cache",
    );
    fs.writeFileSync(helperPath, 'module.exports = "new";\n');

    const result = await restartUpdatedWrapper(wrapperPath, ["--example"], {
      cwd: "/workspace",
      env: { PATH: "/bin" },
    });

    assert.deepEqual(result, {
      code: 0,
      signal: null,
      pid: process.pid,
      args: ["--example"],
      cwd: "/workspace",
      pathValue: "/bin",
      wrapperPath,
    });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
