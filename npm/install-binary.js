#!/usr/bin/env node

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const packageRoot = path.resolve(__dirname, "..");
const packageJson = require(path.join(packageRoot, "package.json"));
const version = packageJson.version;
const releaseTag = `v${version}`;
const defaultReleaseBaseUrl = `https://github.com/Shattermoon/moondesk/releases/download/${releaseTag}`;

const MAX_BINARY_BYTES = 128 * 1024 * 1024;
const MAX_CHECKSUM_BYTES = 1024 * 1024;
const METADATA_TIMEOUT_MS = 60_000;
const BINARY_TIMEOUT_MS = 10 * 60_000;
const LOCK_STALE_MS = 15 * 60_000;
const LOCK_WAIT_MS = 15 * 60_000;
const LOCK_POLL_MS = 100;

const supportedTargets = new Set([
  "linux-x64",
  "linux-arm64",
  "darwin-x64",
  "darwin-arm64",
  "win32-x64",
]);

function resolveTarget(platform = process.platform, arch = process.arch) {
  const target = `${platform}-${arch}`;
  if (!supportedTargets.has(target)) {
    throw new Error(
      `MoonDesk does not provide a prebuilt binary for ${target}. Supported targets: ${Array.from(supportedTargets).join(", ")}`,
    );
  }

  return {
    platform,
    arch,
    target,
    assetName: platform === "win32" ? `moondesk-${target}.exe` : `moondesk-${target}`,
    executableName: platform === "win32" ? "moondesk.exe" : "moondesk",
  };
}

function defaultBinaryCacheRoot() {
  return path.join(os.homedir(), ".moondesk", "npm-bin");
}

function defaultInstallDir(target) {
  if (process.env.MOONDESK_BINARY_CACHE_DIR) {
    return path.resolve(process.env.MOONDESK_BINARY_CACHE_DIR);
  }

  return path.join(defaultBinaryCacheRoot(), releaseTag, target);
}

function stableTagParts(tag) {
  const match = tag.match(/^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/);
  return match ? match.slice(1).map((part) => BigInt(part)) : null;
}

function stableTagIsOlder(candidate, current) {
  const left = stableTagParts(candidate);
  const right = stableTagParts(current);
  if (!left || !right) return false;
  for (let index = 0; index < 3; index += 1) {
    if (left[index] < right[index]) return true;
    if (left[index] > right[index]) return false;
  }
  return false;
}

function cleanupOldBinaryVersions(options = {}) {
  if (process.env.MOONDESK_BINARY_CACHE_DIR && !options.cacheRoot) {
    return { removed: [], skipped: [] };
  }

  const cacheRoot = options.cacheRoot ?? defaultBinaryCacheRoot();
  const keepTag = options.keepTag ?? releaseTag;
  const removed = [];
  const skipped = [];

  let entries;
  try {
    entries = fs.readdirSync(cacheRoot, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT") {
      return { removed, skipped };
    }
    throw error;
  }

  for (const entry of entries) {
    if (!entry.isDirectory() || !stableTagIsOlder(entry.name, keepTag)) {
      continue;
    }
    const stalePath = path.join(cacheRoot, entry.name);
    try {
      fs.rmSync(stalePath, { recursive: true, force: true, maxRetries: 2, retryDelay: 100 });
      removed.push(entry.name);
    } catch {
      // Another still-running MoonDesk process may hold an older Windows binary open.
      // Leave it in place and retry on the next managed launch.
      skipped.push(entry.name);
    }
  }

  return { removed, skipped };
}

async function fetchRequired(fetchImpl, url, maxBytes, timeoutMs = METADATA_TIMEOUT_MS) {
  const response = await fetchImpl(url, {
    headers: {
      "User-Agent": `moondesk-npm/${version}`,
    },
    signal: AbortSignal.timeout(timeoutMs),
  });

  if (!response.ok) {
    throw new Error(`${url} returned HTTP ${response.status}`);
  }

  const contentLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(contentLength) && contentLength > maxBytes) {
    throw new Error(`${url} is unexpectedly large (${contentLength} bytes)`);
  }

  const buffer = Buffer.from(await response.arrayBuffer());
  if (buffer.length > maxBytes) {
    throw new Error(`${url} exceeded the ${maxBytes}-byte download limit`);
  }

  return buffer;
}

function expectedSha256(checksums, name) {
  for (const line of checksums.split(/\r?\n/)) {
    const match = line.trim().match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (match && path.basename(match[2]) === name) {
      return match[1].toLowerCase();
    }
  }

  throw new Error(`SHA256SUMS does not contain ${name}`);
}

function sha256Buffer(buffer) {
  return crypto.createHash("sha256").update(buffer).digest("hex");
}

function sha256File(filePath) {
  return sha256Buffer(fs.readFileSync(filePath));
}

function cacheMetadata(stat, expected) {
  return {
    version: 1,
    sha256: expected,
    size: stat.size,
    mtimeMs: stat.mtimeMs,
    ctimeMs: stat.ctimeMs,
  };
}

function writeCacheMetadata(metadataPath, binaryPath, expected) {
  const stat = fs.statSync(binaryPath);
  fs.writeFileSync(metadataPath, `${JSON.stringify(cacheMetadata(stat, expected))}\n`, {
    mode: 0o600,
  });
}

function validCachedBinary(binaryPath, checksumPath, metadataPath, platform) {
  if (!fs.existsSync(binaryPath) || !fs.existsSync(checksumPath)) {
    return false;
  }

  let stat = fs.statSync(binaryPath);
  if (!stat.isFile() || stat.size === 0 || stat.size > MAX_BINARY_BYTES) {
    return false;
  }

  const expected = fs.readFileSync(checksumPath, "utf8").trim().toLowerCase();
  if (!/^[a-f0-9]{64}$/.test(expected)) {
    return false;
  }

  let metadata;
  try {
    metadata = JSON.parse(fs.readFileSync(metadataPath, "utf8"));
  } catch {
    metadata = null;
  }

  const fastPathMatches =
    metadata?.version === 1 &&
    metadata.sha256 === expected &&
    metadata.size === stat.size &&
    metadata.mtimeMs === stat.mtimeMs &&
    metadata.ctimeMs === stat.ctimeMs;

  if (fastPathMatches) {
    return true;
  }

  if (sha256File(binaryPath) !== expected) {
    return false;
  }

  if (platform !== "win32" && (stat.mode & 0o111) === 0) {
    fs.chmodSync(binaryPath, 0o755);
    stat = fs.statSync(binaryPath);
  }

  fs.writeFileSync(metadataPath, `${JSON.stringify(cacheMetadata(stat, expected))}\n`, {
    mode: 0o600,
  });
  return true;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function lockOwnerIsAlive(lockPath) {
  try {
    const [pidText] = fs.readFileSync(lockPath, "utf8").split(/\r?\n/);
    const pid = Number(pidText);
    if (!Number.isSafeInteger(pid) || pid <= 0) {
      return false;
    }

    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      if (error.code === "ESRCH") {
        return false;
      }
      if (error.code === "EPERM") {
        return true;
      }
      throw error;
    }
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}

async function acquireInstallLock(
  lockPath,
  binaryPath,
  checksumPath,
  metadataPath,
  platform,
) {
  const deadline = Date.now() + LOCK_WAIT_MS;

  while (Date.now() < deadline) {
    if (validCachedBinary(binaryPath, checksumPath, metadataPath, platform)) {
      return null;
    }

    try {
      const fd = fs.openSync(lockPath, "wx", 0o600);
      fs.writeFileSync(fd, `${process.pid}\n${Date.now()}\n`);
      return fd;
    } catch (error) {
      if (error.code !== "EEXIST") {
        throw error;
      }

      try {
        const age = Date.now() - fs.statSync(lockPath).mtimeMs;
        if (!lockOwnerIsAlive(lockPath) || age > LOCK_STALE_MS) {
          fs.rmSync(lockPath, { force: true });
          continue;
        }
      } catch (statError) {
        if (statError.code !== "ENOENT") {
          throw statError;
        }
      }

      await sleep(LOCK_POLL_MS);
    }
  }

  if (validCachedBinary(binaryPath, checksumPath, metadataPath, platform)) {
    return null;
  }

  throw new Error("Timed out waiting for another MoonDesk process to finish installing the native binary");
}

async function ensureBinary(options = {}) {
  const targetInfo = resolveTarget(options.platform, options.arch);
  const installDir = options.installDir ?? defaultInstallDir(targetInfo.target);
  const releaseBaseUrl = options.releaseBaseUrl ?? defaultReleaseBaseUrl;
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;

  if (typeof fetchImpl !== "function") {
    throw new Error("MoonDesk requires Node.js 18 or newer so the native binary can be downloaded securely");
  }

  const binaryPath = path.join(installDir, targetInfo.executableName);
  const checksumPath = `${binaryPath}.sha256`;
  const metadataPath = `${binaryPath}.metadata.json`;
  const lockPath = path.join(installDir, ".install.lock");

  fs.mkdirSync(installDir, { recursive: true, mode: 0o700 });

  if (validCachedBinary(binaryPath, checksumPath, metadataPath, targetInfo.platform)) {
    return binaryPath;
  }

  const lockFd = await acquireInstallLock(
    lockPath,
    binaryPath,
    checksumPath,
    metadataPath,
    targetInfo.platform,
  );

  if (lockFd === null) {
    return binaryPath;
  }

  const nonce = `${process.pid}-${crypto.randomBytes(8).toString("hex")}`;
  const tempBinary = `${binaryPath}.tmp-${nonce}`;
  const tempChecksum = `${checksumPath}.tmp-${nonce}`;

  try {
    if (validCachedBinary(binaryPath, checksumPath, metadataPath, targetInfo.platform)) {
      return binaryPath;
    }

    const checksumsBuffer = await fetchRequired(
      fetchImpl,
      `${releaseBaseUrl}/SHA256SUMS`,
      MAX_CHECKSUM_BYTES,
    );
    const expected = expectedSha256(checksumsBuffer.toString("utf8"), targetInfo.assetName);
    const binary = await fetchRequired(
      fetchImpl,
      `${releaseBaseUrl}/${targetInfo.assetName}`,
      MAX_BINARY_BYTES,
      BINARY_TIMEOUT_MS,
    );
    const actual = sha256Buffer(binary);

    if (actual !== expected) {
      throw new Error(
        `Checksum mismatch for ${targetInfo.assetName}: expected ${expected}, got ${actual}`,
      );
    }

    fs.writeFileSync(tempBinary, binary, { mode: 0o755 });
    if (targetInfo.platform !== "win32") {
      fs.chmodSync(tempBinary, 0o755);
    }
    fs.writeFileSync(tempChecksum, `${expected}\n`, { mode: 0o600 });

    fs.rmSync(binaryPath, { force: true });
    fs.rmSync(checksumPath, { force: true });
    fs.rmSync(metadataPath, { force: true });
    fs.renameSync(tempBinary, binaryPath);
    fs.renameSync(tempChecksum, checksumPath);
    writeCacheMetadata(metadataPath, binaryPath, expected);

    if (!validCachedBinary(binaryPath, checksumPath, metadataPath, targetInfo.platform)) {
      throw new Error(`Installed ${targetInfo.assetName} failed its local checksum verification`);
    }

    return binaryPath;
  } finally {
    fs.rmSync(tempBinary, { force: true });
    fs.rmSync(tempChecksum, { force: true });
    try {
      fs.closeSync(lockFd);
    } finally {
      fs.rmSync(lockPath, { force: true });
    }
  }
}

module.exports = {
  cleanupOldBinaryVersions,
  ensureBinary,
  expectedSha256,
  resolveTarget,
  sha256Buffer,
};

if (require.main === module) {
  ensureBinary()
    .then((binaryPath) => {
      console.log(`MoonDesk native binary ready at ${binaryPath}`);
    })
    .catch((error) => {
      console.error(`MoonDesk binary install failed: ${error.message}`);
      process.exit(1);
    });
}
