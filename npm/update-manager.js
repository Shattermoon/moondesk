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
const REGISTRY_LATEST_URL = "https://registry.npmjs.org/moondesk/latest";
const UPDATE_CHECK_INTERVAL_MS = 15 * 60_000;
const UPDATE_CHECK_TIMEOUT_MS = 15_000;
const MAX_UPDATE_METADATA_BYTES = 64 * 1024;
const MAX_UPDATE_REQUEST_BYTES = 16 * 1024;
const MAX_NPM_ROOT_BYTES = 16 * 1024;
const NPM_ROOT_TIMEOUT_MS = 10_000;
const UPDATE_LOCK_WAIT_MS = 60_000;
const UPDATE_LOCK_POLL_MS = 200;

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

async function fetchJsonLimited(fetchImpl, url, externalSignal) {
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
    if (Number.isFinite(contentLength) && contentLength > MAX_UPDATE_METADATA_BYTES) {
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
      if (totalBytes > MAX_UPDATE_METADATA_BYTES) {
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

async function checkForUpdate(options = {}) {
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  const statePath = options.statePath ?? createUpdateStatePath();
  const registryUrl = options.registryUrl ?? REGISTRY_LATEST_URL;
  const managedInstall = options.managedInstall === true;
  if (typeof fetchImpl !== "function") {
    return null;
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

  const available = managedInstall && compareStableVersions(latestVersion, currentVersion) > 0;
  const state = {
    schemaVersion: UPDATE_STATE_SCHEMA_VERSION,
    packageName: "moondesk",
    currentVersion,
    latestVersion,
    managedInstall,
    available,
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
        fs.rmSync(statePath, { force: true });
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
  return parsed;
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

function restartUpdatedWrapper(wrapperPath, args, options = {}) {
  const spawnImpl = options.spawnImpl ?? spawn;
  return new Promise((resolve, reject) => {
    const child = spawnImpl(process.execPath, [wrapperPath, ...args], {
      cwd: options.cwd ?? process.cwd(),
      env: options.env ?? process.env,
      stdio: "inherit",
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code: code ?? 1, signal }));
  });
}

module.exports = {
  UPDATE_EXIT_CODE,
  acquireUpdateLock,
  atomicWriteJson,
  checkForUpdate,
  cleanupOldUpdateVersions,
  compareStableVersions,
  createUpdateRequestPath,
  createUpdateStatePath,
  currentVersion,
  installExactVersion,
  installedWrapperVersion,
  isGlobalPackageInstall,
  parseStableVersion,
  readUpdateRequest,
  resolveGlobalNpmRoot,
  restartUpdatedWrapper,
  startUpdateMonitor,
  verifyInstalledWrapperVersion,
};
