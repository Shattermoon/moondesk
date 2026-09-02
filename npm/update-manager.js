#!/usr/bin/env node

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const packageRoot = path.resolve(__dirname, "..");
const packageJsonPath = path.join(packageRoot, "package.json");
const packageJson = require(packageJsonPath);
const currentVersion = packageJson.version;

const UPDATE_EXIT_CODE = 75;
const UPDATE_STATE_SCHEMA_VERSION = 1;
const UPDATE_REQUEST_SCHEMA_VERSION = 1;
const CHANGELOG_NOTICE_SCHEMA_VERSION = 1;
const REGISTRY_LATEST_URL = "https://registry.npmjs.org/moondesk/latest";
const GITHUB_REPOSITORY = "Shattermoon/moondesk";
const GITHUB_RELEASES_API_URL = `https://api.github.com/repos/${GITHUB_REPOSITORY}/releases?per_page=20`;
const GITHUB_RELEASE_TAG_API_BASE = `https://api.github.com/repos/${GITHUB_REPOSITORY}/releases/tags`;
const GITHUB_RELEASE_WEB_BASE = `https://github.com/${GITHUB_REPOSITORY}/releases/tag`;
const GITHUB_PULL_WEB_BASE = `https://github.com/${GITHUB_REPOSITORY}/pull`;
const UPDATE_CHECK_INTERVAL_MS = 15 * 60_000;
const UPDATE_CHECK_TIMEOUT_MS = 15_000;
const MAX_UPDATE_METADATA_BYTES = 64 * 1024;
const MAX_CHANGELOG_METADATA_BYTES = 512 * 1024;
const MAX_CHANGELOG_ITEMS = 12;
const MAX_CHANGELOG_ITEM_CHARS = 180;
const MAX_UPDATE_REQUEST_BYTES = 16 * 1024;
const MAX_NPM_ROOT_BYTES = 16 * 1024;
const NPM_ROOT_TIMEOUT_MS = 10_000;
const UPDATE_LOCK_WAIT_MS = 60_000;
const UPDATE_LOCK_POLL_MS = 200;

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

const GITHUB_PULL_ATTRIBUTION_RE = new RegExp(
  `\\s+by\\s+@[A-Za-z0-9_-]+\\s+in\\s+${escapeRegExp(GITHUB_PULL_WEB_BASE)}\\/\\d+\\s*$`,
  "i",
);

function parseStableVersion(input) {
  if (typeof input !== "string") {
    return null;
  }
  const match = input.match(/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/);
  if (!match) {
    return null;
  }
  return match.slice(1).map((part) => BigInt(part));
}

function compareStableVersions(left, right) {
  const a = parseStableVersion(left);
  const b = parseStableVersion(right);
  if (!a || !b) {
    throw new Error(`MoonDesk update versions must be stable semantic versions: ${left} vs ${right}`);
  }
  for (let index = 0; index < 3; index += 1) {
    if (a[index] < b[index]) return -1;
    if (a[index] > b[index]) return 1;
  }
  return 0;
}

function updateRootDir() {
  return path.join(os.homedir(), ".moondesk", "updates");
}

function currentUpdateDir() {
  return path.join(updateRootDir(), `v${currentVersion}`);
}

function createUpdateStatePath() {
  const dir = path.join(currentUpdateDir(), "state");
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  return path.join(dir, `${process.pid}-${crypto.randomBytes(12).toString("hex")}.json`);
}

function cleanupOldUpdateVersions(options = {}) {
  const root = options.root ?? updateRootDir();
  const currentTag = options.currentTag ?? `v${currentVersion}`;
  const currentVersionText = currentTag.startsWith("v") ? currentTag.slice(1) : currentTag;
  const currentParts = parseStableVersion(currentVersionText);
  if (!currentParts) {
    return { removed: [], skipped: [] };
  }

  const removed = [];
  const skipped = [];
  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT") return { removed, skipped };
    throw error;
  }

  for (const entry of entries) {
    if (!entry.isDirectory() || !entry.name.startsWith("v")) continue;
    const candidate = entry.name.slice(1);
    const candidateParts = parseStableVersion(candidate);
    if (!candidateParts || compareStableVersions(candidate, currentVersionText) >= 0) continue;
    try {
      fs.rmSync(path.join(root, entry.name), {
        recursive: true,
        force: true,
        maxRetries: 2,
        retryDelay: 100,
      });
      removed.push(entry.name);
    } catch {
      skipped.push(entry.name);
    }
  }
  return { removed, skipped };
}

function changelogNoticePath(version = currentVersion) {
  if (!parseStableVersion(version)) {
    throw new Error(`MoonDesk changelog version must be a stable semantic version: ${version}`);
  }
  return path.join(updateRootDir(), `v${version}`, "post-update.json");
}

function normalizePersistedReleaseNotes(value) {
  if (!Array.isArray(value)) return [];
  const notes = [];
  for (const item of value) {
    if (typeof item !== "string") continue;
    const trimmed = item.trim();
    if (!trimmed || trimmed.length > MAX_CHANGELOG_ITEM_CHARS) continue;
    notes.push(trimmed);
    if (notes.length >= MAX_CHANGELOG_ITEMS) break;
  }
  return notes;
}

function normalizeReleaseUrl(value, version) {
  const expected = `${GITHUB_RELEASE_WEB_BASE}/v${version}`;
  return typeof value === "string" && value === expected ? value : null;
}

function writePostUpdateNotice(request, installedVersion, options = {}) {
  if (!request || request.targetVersion !== installedVersion || !parseStableVersion(installedVersion)) {
    return null;
  }
  const filePath = options.noticePath ?? changelogNoticePath(installedVersion);
  const notice = {
    schemaVersion: CHANGELOG_NOTICE_SCHEMA_VERSION,
    packageName: "moondesk",
    fromVersion: request.currentVersion,
    toVersion: installedVersion,
    releaseNotes: normalizePersistedReleaseNotes(request.releaseNotes),
    releaseUrl: normalizeReleaseUrl(request.releaseUrl, installedVersion),
    createdAt: new Date().toISOString(),
  };
  atomicWriteJson(filePath, notice, options);
  return filePath;
}

function updateRequestDir() {
  return path.join(currentUpdateDir(), "requests");
}

function createUpdateRequestPath() {
  const dir = updateRequestDir();
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  return path.join(dir, `${process.pid}-${crypto.randomBytes(12).toString("hex")}.json`);
}

function normalizePathForCompare(value, platform = process.platform) {
  const resolved = path.resolve(value);
  let canonical = resolved;
  try {
    canonical = fs.realpathSync.native(resolved);
  } catch {
    // The expected global package path may not exist in unit tests.
  }
  return platform === "win32" ? canonical.toLowerCase() : canonical;
}

function resolveGlobalNpmRoot(options = {}) {
  const spawnImpl = options.spawnImpl ?? spawn;
  const platform = options.platform ?? process.platform;
  const command = npmExecutable(platform);
  const env = options.env ?? process.env;
  const externalSignal = options.signal;

  return new Promise((resolve, reject) => {
    const child = spawnImpl(command, ["root", "--global"], {
      cwd: options.cwd ?? process.cwd(),
      env,
      stdio: ["ignore", "pipe", "ignore"],
      shell: platform === "win32",
      windowsHide: true,
    });

    let settled = false;
    let timer = null;
    let abortHandler = null;
    let output = Buffer.alloc(0);
    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      if (abortHandler) externalSignal?.removeEventListener("abort", abortHandler);
      if (error) reject(error);
      else resolve(value);
    };

    abortHandler = () => {
      child.kill?.();
      finish(new Error("npm root --global was aborted"));
    };
    if (externalSignal?.aborted) {
      abortHandler();
      return;
    }
    externalSignal?.addEventListener("abort", abortHandler, { once: true });

    timer = setTimeout(() => {
      child.kill?.();
      finish(new Error("npm root --global timed out"));
    }, NPM_ROOT_TIMEOUT_MS);
    timer.unref?.();

    child.once("error", (error) => finish(error));
    child.stdout?.on("data", (chunk) => {
      if (settled) return;
      output = Buffer.concat([output, Buffer.from(chunk)]);
      if (output.length > MAX_NPM_ROOT_BYTES) {
        child.kill?.();
        finish(new Error("npm root --global returned too much output"));
      }
    });
    child.once("exit", (code, signal) => {
      if (settled) return;
      if (signal) {
        finish(new Error(`npm root --global was terminated by ${signal}`));
        return;
      }
      if (code !== 0) {
        finish(new Error(`npm root --global exited with code ${code ?? "unknown"}`));
        return;
      }
      const root = output.toString("utf8").trim();
      if (!root) {
        finish(new Error("npm root --global returned an empty path"));
        return;
      }
      finish(null, root);
    });
  });
}

async function isGlobalPackageInstall(options = {}) {
  const root = await resolveGlobalNpmRoot(options);
  const platform = options.platform ?? process.platform;
  const actualRoot = options.packageRoot ?? packageRoot;
  const expectedRoot = path.join(root, "moondesk");
  return normalizePathForCompare(actualRoot, platform) === normalizePathForCompare(expectedRoot, platform);
}

function replaceFile(tempPath, destinationPath, platform = process.platform) {
  try {
    fs.renameSync(tempPath, destinationPath);
    return;
  } catch (error) {
    const replaceConflict = ["EEXIST", "EPERM", "EACCES"].includes(error.code);
    if (platform !== "win32" || !replaceConflict) {
      throw error;
    }
  }

  // Windows can reject rename-over-existing-file while another process briefly
  // has the destination open. The TUI treats a missing state file as "no update",
  // so a tiny replacement gap is safer than failing MoonDesk startup.
  fs.rmSync(destinationPath, { force: true });
  fs.renameSync(tempPath, destinationPath);
}

function atomicWriteJson(filePath, value, options = {}) {
  const dir = path.dirname(filePath);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const temp = `${filePath}.tmp-${process.pid}-${crypto.randomBytes(8).toString("hex")}`;
  try {
    fs.writeFileSync(temp, `${JSON.stringify(value)}\n`, { mode: 0o600 });
    replaceFile(temp, filePath, options.platform ?? process.platform);
  } finally {
    fs.rmSync(temp, { force: true });
  }
}

async function fetchJsonLimited(fetchImpl, url, externalSignal, maxBytes = MAX_UPDATE_METADATA_BYTES) {
  const controller = new AbortController();
  const abortFromParent = () => controller.abort();
  if (externalSignal) {
    if (externalSignal.aborted) {
      controller.abort();
    } else {
      externalSignal.addEventListener("abort", abortFromParent, { once: true });
    }
  }
  const timeout = setTimeout(() => controller.abort(), UPDATE_CHECK_TIMEOUT_MS);
  timeout.unref?.();

  try {
    const response = await fetchImpl(url, {
      headers: {
        Accept: "application/json",
        "User-Agent": `moondesk-npm/${currentVersion}`,
      },
      signal: controller.signal,
    });
    if (!response.ok) {
      throw new Error(`${url} returned HTTP ${response.status}`);
    }
    const contentLength = Number(response.headers.get("content-length"));
    if (Number.isFinite(contentLength) && contentLength > maxBytes) {
      controller.abort();
      throw new Error(`MoonDesk update metadata is unexpectedly large (${contentLength} bytes)`);
    }
    if (!response.body || typeof response.body[Symbol.asyncIterator] !== "function") {
      throw new Error("MoonDesk update metadata response did not include a readable body");
    }

    const chunks = [];
    let totalBytes = 0;
    for await (const chunk of response.body) {
      const buffer = Buffer.from(chunk);
      totalBytes += buffer.length;
      if (totalBytes > maxBytes) {
        controller.abort();
        throw new Error("MoonDesk update metadata exceeded the download limit");
      }
      chunks.push(buffer);
    }
    return JSON.parse(Buffer.concat(chunks, totalBytes).toString("utf8"));
  } finally {
    clearTimeout(timeout);
    externalSignal?.removeEventListener("abort", abortFromParent);
  }
}

function normalizeChangelogLine(line) {
  if (typeof line !== "string") return null;
  let value = line.trim();
  if (!value || value.startsWith("#") || /^\*\*Full Changelog\*\*/i.test(value)) return null;
  value = value.replace(/^[-*+]\s+/, "");
  value = value.replace(GITHUB_PULL_ATTRIBUTION_RE, "");
  value = value.replace(/^\[(.+?)\]\([^)]*\)$/, "$1");
  value = value.replace(/`([^`]+)`/g, "$1");
  value = value.replace(/^(?:feat|fix|chore|refactor|perf|docs|test|build|ci|style)(?:\([^)]*\))?!?:\s*/i, "");
  value = value.trim();
  if (!value || /^https?:\/\//i.test(value)) return null;
  value = `${value.charAt(0).toUpperCase()}${value.slice(1)}`;
  if (value.length > MAX_CHANGELOG_ITEM_CHARS) {
    value = `${value.slice(0, MAX_CHANGELOG_ITEM_CHARS - 3).trimEnd()}...`;
  }
  return value;
}

function normalizeReleaseNotes(body) {
  if (typeof body !== "string") return [];
  const notes = [];
  const seen = new Set();
  for (const line of body.split(/\r?\n/)) {
    const note = normalizeChangelogLine(line);
    if (!note || seen.has(note)) continue;
    seen.add(note);
    notes.push(note);
    if (notes.length >= MAX_CHANGELOG_ITEMS) break;
  }
  return notes;
}

function boundedChangelogItem(value) {
  const trimmed = String(value ?? "").trim();
  if (!trimmed) return null;
  if (trimmed.length <= MAX_CHANGELOG_ITEM_CHARS) return trimmed;
  return `${trimmed.slice(0, MAX_CHANGELOG_ITEM_CHARS - 3).trimEnd()}...`;
}

function stableReleaseVersion(release) {
  if (!release || release.draft === true || release.prerelease === true) return null;
  if (typeof release.tag_name !== "string" || !release.tag_name.startsWith("v")) return null;
  const version = release.tag_name.slice(1);
  return parseStableVersion(version) ? version : null;
}

async function fetchReleaseChangelog(fromVersion, toVersion, options = {}) {
  if (
    !parseStableVersion(fromVersion) ||
    !parseStableVersion(toVersion) ||
    compareStableVersions(toVersion, fromVersion) <= 0
  ) {
    return { releaseNotes: [], releaseUrl: null };
  }
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  if (typeof fetchImpl !== "function") return { releaseNotes: [], releaseUrl: null };

  const releasesUrl = options.releasesApiUrl ?? GITHUB_RELEASES_API_URL;
  const releases = await fetchJsonLimited(
    fetchImpl,
    releasesUrl,
    options.signal,
    MAX_CHANGELOG_METADATA_BYTES,
  );
  if (!Array.isArray(releases)) {
    throw new Error("GitHub returned invalid MoonDesk releases metadata");
  }

  const selected = [];
  for (const release of releases) {
    const version = stableReleaseVersion(release);
    if (!version) continue;
    if (
      compareStableVersions(version, fromVersion) > 0 &&
      compareStableVersions(version, toVersion) <= 0
    ) {
      selected.push({ release, version });
    }
  }

  if (!selected.some((item) => item.version === toVersion)) {
    const tagApiBase = options.releaseTagApiBase ?? GITHUB_RELEASE_TAG_API_BASE;
    try {
      const release = await fetchJsonLimited(
        fetchImpl,
        `${tagApiBase}/v${toVersion}`,
        options.signal,
        MAX_CHANGELOG_METADATA_BYTES,
      );
      if (stableReleaseVersion(release) === toVersion) {
        selected.push({ release, version: toVersion });
      }
    } catch {
      // The recent releases list may briefly lag the just-published npm tag.
      // Missing notes must never block a valid npm update.
    }
  }

  selected.sort((left, right) => compareStableVersions(right.version, left.version));
  const uniqueReleases = [];
  const seenVersions = new Set();
  for (const item of selected) {
    if (seenVersions.has(item.version)) continue;
    seenVersions.add(item.version);
    uniqueReleases.push(item);
  }

  const includeVersion = uniqueReleases.length > 1;
  const releaseNotes = [];
  const seenNotes = new Set();
  for (const { release, version } of uniqueReleases) {
    for (const note of normalizeReleaseNotes(release.body)) {
      const rendered = boundedChangelogItem(includeVersion ? `v${version}: ${note}` : note);
      if (!rendered || seenNotes.has(rendered)) continue;
      seenNotes.add(rendered);
      releaseNotes.push(rendered);
      if (releaseNotes.length >= MAX_CHANGELOG_ITEMS) break;
    }
    if (releaseNotes.length >= MAX_CHANGELOG_ITEMS) break;
  }

  const expectedUrl = `${GITHUB_RELEASE_WEB_BASE}/v${toVersion}`;
  const target = uniqueReleases.find((item) => item.version === toVersion)?.release;
  const releaseUrl = target ? expectedUrl : null;
  return { releaseNotes, releaseUrl };
}

async function fetchLatestPackageMetadata(options = {}) {
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  const registryUrl = options.registryUrl ?? REGISTRY_LATEST_URL;
  if (typeof fetchImpl !== "function") {
    throw new Error("MoonDesk update checks require a fetch implementation");
  }

  const metadata = await fetchJsonLimited(fetchImpl, registryUrl, options.signal);
  const latestVersion = metadata?.version;
  if (metadata?.name !== "moondesk") {
    throw new Error(`npm returned unexpected package metadata for ${String(metadata?.name)}`);
  }
  if (!parseStableVersion(latestVersion)) {
    throw new Error(`npm returned an invalid MoonDesk version: ${String(latestVersion)}`);
  }
  if (typeof metadata?.dist?.integrity !== "string" || !metadata.dist.integrity.startsWith("sha512-")) {
    throw new Error("npm returned MoonDesk metadata without a sha512 package integrity value");
  }
  return metadata;
}

async function refreshUpdateRequestToLatest(request, options = {}) {
  if (
    !request ||
    request.currentVersion !== currentVersion ||
    !parseStableVersion(request.targetVersion) ||
    compareStableVersions(request.targetVersion, currentVersion) <= 0
  ) {
    throw new Error("Refusing to refresh an invalid MoonDesk update request");
  }

  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  const metadata = await fetchLatestPackageMetadata({ ...options, fetchImpl });
  const latestVersion = metadata.version;
  if (compareStableVersions(latestVersion, request.targetVersion) <= 0) {
    return request;
  }

  let releaseNotes = [];
  let releaseUrl = null;
  try {
    ({ releaseNotes, releaseUrl } = await fetchReleaseChangelog(
      request.currentVersion,
      latestVersion,
      { ...options, fetchImpl },
    ));
  } catch {
    // The exact newest npm target is authoritative. GitHub notes remain optional.
  }

  return {
    ...request,
    targetVersion: latestVersion,
    releaseNotes,
    releaseUrl,
  };
}

async function checkForUpdate(options = {}) {
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  const statePath = options.statePath ?? createUpdateStatePath();
  const managedInstall = options.managedInstall === true;
  if (typeof fetchImpl !== "function") {
    return null;
  }

  const metadata = await fetchLatestPackageMetadata({ ...options, fetchImpl });
  const latestVersion = metadata.version;
  const available = managedInstall && compareStableVersions(latestVersion, currentVersion) > 0;
  let releaseNotes = [];
  let releaseUrl = null;
  if (available) {
    try {
      ({ releaseNotes, releaseUrl } = await fetchReleaseChangelog(currentVersion, latestVersion, {
        ...options,
        fetchImpl,
      }));
    } catch {
      // Release notes are optional metadata. A GitHub outage must never hide a valid npm update.
    }
  }
  const state = {
    schemaVersion: UPDATE_STATE_SCHEMA_VERSION,
    packageName: "moondesk",
    currentVersion,
    latestVersion,
    managedInstall,
    available,
    releaseNotes,
    releaseUrl,
    checkedAt: new Date().toISOString(),
  };
  atomicWriteJson(statePath, state, options);
  return state;
}

function startUpdateMonitor(options = {}) {
  const intervalMs = options.intervalMs ?? UPDATE_CHECK_INTERVAL_MS;
  const statePath = options.statePath ?? createUpdateStatePath();
  const globalInstallCheckImpl = options.isGlobalPackageInstallImpl ?? isGlobalPackageInstall;
  let managedInstall = null;
  let stopped = false;
  let checking = false;
  let timer = null;
  let controller = null;

  const run = async () => {
    if (stopped || checking) return;
    checking = true;
    controller = new AbortController();
    try {
      if (managedInstall === null) {
        managedInstall = await globalInstallCheckImpl({ ...options, signal: controller.signal });
      }
      if (!managedInstall || stopped) {
        fs.rmSync(statePath, { force: true });
        return;
      }
      await checkForUpdate({
        ...options,
        statePath,
        managedInstall: true,
        signal: controller.signal,
      });
    } catch {
      // Update checks are optional. Offline/npm/registry failures must never affect MoonDesk startup.
    } finally {
      if (stopped) {
        try {
          fs.rmSync(statePath, { force: true });
        } catch {
          // Removing ephemeral update state must never fail MoonDesk.
        }
      }
      controller = null;
      checking = false;
    }
  };

  void run();
  timer = setInterval(() => void run(), intervalMs);
  timer.unref?.();

  return () => {
    stopped = true;
    controller?.abort();
    if (timer) clearInterval(timer);
  };
}

function readUpdateRequest(requestPath) {
  let parsed;
  try {
    const stat = fs.statSync(requestPath);
    if (!stat.isFile() || stat.size <= 0 || stat.size > MAX_UPDATE_REQUEST_BYTES) {
      return null;
    }
    parsed = JSON.parse(fs.readFileSync(requestPath, "utf8"));
  } catch {
    return null;
  } finally {
    fs.rmSync(requestPath, { force: true });
  }

  if (
    parsed?.schemaVersion !== UPDATE_REQUEST_SCHEMA_VERSION ||
    parsed.currentVersion !== currentVersion ||
    !parseStableVersion(parsed.targetVersion) ||
    compareStableVersions(parsed.targetVersion, currentVersion) <= 0
  ) {
    return null;
  }
  return {
    ...parsed,
    releaseNotes: normalizePersistedReleaseNotes(parsed.releaseNotes),
    releaseUrl: normalizeReleaseUrl(parsed.releaseUrl, parsed.targetVersion),
  };
}

function updateLockPath() {
  return path.join(updateRootDir(), "npm-update.lock");
}

function lockOwnerIsAlive(lockPath) {
  try {
    const payload = JSON.parse(fs.readFileSync(lockPath, "utf8"));
    const pid = Number(payload?.pid);
    if (!Number.isSafeInteger(pid) || pid <= 0) return false;
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function acquireUpdateLock(options = {}) {
  const lockPath = options.lockPath ?? updateLockPath();
  const waitMs = options.waitMs ?? UPDATE_LOCK_WAIT_MS;
  const pollMs = options.pollMs ?? UPDATE_LOCK_POLL_MS;
  fs.mkdirSync(path.dirname(lockPath), { recursive: true, mode: 0o700 });
  const deadline = Date.now() + waitMs;

  while (true) {
    try {
      const fd = fs.openSync(lockPath, "wx", 0o600);
      try {
        fs.writeFileSync(
          fd,
          `${JSON.stringify({ pid: process.pid, startedAt: new Date().toISOString() })}\n`,
        );
        fs.fsyncSync(fd);
      } finally {
        fs.closeSync(fd);
      }
      let released = false;
      return () => {
        if (released) return;
        released = true;
        fs.rmSync(lockPath, { force: true });
      };
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      if (!lockOwnerIsAlive(lockPath)) {
        fs.rmSync(lockPath, { force: true });
        continue;
      }
      if (Date.now() >= deadline) {
        throw new Error("another MoonDesk process is still updating the global npm package");
      }
      await sleep(pollMs);
    }
  }
}

function installedWrapperVersion(options = {}) {
  const targetPackageJsonPath = options.packageJsonPath ?? packageJsonPath;
  let installed;
  try {
    installed = JSON.parse(fs.readFileSync(targetPackageJsonPath, "utf8"));
  } catch (error) {
    throw new Error(`could not read installed MoonDesk package metadata: ${error.message}`);
  }
  if (installed?.name !== "moondesk" || !parseStableVersion(installed.version)) {
    throw new Error(
      `installed package metadata is not a stable MoonDesk release: ${String(installed?.name)}@${String(installed?.version)}`,
    );
  }
  return installed.version;
}

function npmExecutable(platform = process.platform) {
  return platform === "win32" ? "npm.cmd" : "npm";
}

function installExactVersion(targetVersion, options = {}) {
  if (!parseStableVersion(targetVersion) || compareStableVersions(targetVersion, currentVersion) <= 0) {
    return Promise.reject(new Error(`Refusing invalid MoonDesk update target ${targetVersion}`));
  }

  const spawnImpl = options.spawnImpl ?? spawn;
  const platform = options.platform ?? process.platform;
  const command = npmExecutable(platform);
  const args = [
    "install",
    "--global",
    `moondesk@${targetVersion}`,
    "--ignore-scripts",
    "--no-audit",
    "--no-fund",
  ];

  return new Promise((resolve, reject) => {
    const child = spawnImpl(command, args, {
      cwd: options.cwd ?? process.cwd(),
      env: options.env ?? process.env,
      stdio: "inherit",
      shell: platform === "win32",
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (signal) {
        reject(new Error(`npm update was terminated by ${signal}`));
      } else if (code !== 0) {
        reject(new Error(`npm install exited with code ${code ?? "unknown"}`));
      } else {
        resolve();
      }
    });
  });
}

function verifyInstalledWrapperVersion(targetVersion, options = {}) {
  if (!parseStableVersion(targetVersion)) {
    throw new Error(`Refusing invalid MoonDesk verification target ${targetVersion}`);
  }
  const installedVersion = installedWrapperVersion(options);
  if (installedVersion !== targetVersion) {
    throw new Error(
      `npm reported success but installed moondesk@${installedVersion} instead of moondesk@${targetVersion}`,
    );
  }
  return true;
}

function moduleIsInsidePackage(packageRootPath, modulePath) {
  const relative = path.relative(packageRootPath, modulePath);
  return (
    relative === "" ||
    (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative))
  );
}

function clearPackageRequireCache(wrapperPath) {
  const packageRootPath = path.resolve(path.dirname(wrapperPath), "..");
  for (const modulePath of Object.keys(require.cache)) {
    if (moduleIsInsidePackage(packageRootPath, modulePath)) {
      delete require.cache[modulePath];
    }
  }
}

async function restartUpdatedWrapper(wrapperPath, args, options = {}) {
  const clearCacheImpl = options.clearCacheImpl ?? clearPackageRequireCache;
  const loadWrapperImpl = options.loadWrapperImpl ?? ((targetPath) => require(targetPath));

  clearCacheImpl(wrapperPath);
  const updatedWrapper = loadWrapperImpl(wrapperPath);
  if (typeof updatedWrapper?.orchestrate !== "function") {
    throw new Error("updated MoonDesk wrapper does not export orchestrate()");
  }

  return updatedWrapper.orchestrate({
    args,
    cwd: options.cwd ?? process.cwd(),
    env: options.env ?? process.env,
    logger: options.logger,
    wrapperPath,
  });
}

module.exports = {
  UPDATE_EXIT_CODE,
  acquireUpdateLock,
  atomicWriteJson,
  checkForUpdate,
  cleanupOldUpdateVersions,
  changelogNoticePath,
  compareStableVersions,
  createUpdateRequestPath,
  createUpdateStatePath,
  currentVersion,
  fetchReleaseChangelog,
  installExactVersion,
  installedWrapperVersion,
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
};
