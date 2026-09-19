#!/usr/bin/env node

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { compareStableVersions, parseStableVersion } = require("./update-manager");

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

const MIN_LINUX_GLIBC_VERSION = "2.34";
const UNSUPPORTED_RUNTIME_ERROR_CODE = "MOONDESK_UNSUPPORTED_RUNTIME";

function unsupportedRuntimeError(message) {
  const error = new Error(message);
  error.code = UNSUPPORTED_RUNTIME_ERROR_CODE;
  return error;
}

function parseLibcVersion(value) {
  if (typeof value !== "string") return null;
  const match = /^(\d+)\.(\d+)(?:\.(\d+))?$/.exec(value.trim());
  if (!match) return null;
  return match.slice(1).map((part) => Number(part ?? 0));
}

function compareLibcVersions(left, right) {
  const a = parseLibcVersion(left);
  const b = parseLibcVersion(right);
  if (!a || !b) return null;
  for (let index = 0; index < Math.max(a.length, b.length); index += 1) {
    const leftPart = a[index] ?? 0;
    const rightPart = b[index] ?? 0;
    if (leftPart < rightPart) return -1;
    if (leftPart > rightPart) return 1;
  }
  return 0;
}

function detectedLinuxGlibcVersion(processReport = process.report) {
  try {
    if (!processReport || typeof processReport.getReport !== "function") return null;
    const version = processReport.getReport().header?.glibcVersionRuntime;
    return typeof version === "string" ? version : null;
  } catch {
    return null;
  }
}

function assertLinuxRuntimeCompatibility(platform = process.platform, options = {}) {
  if (platform !== "linux") return null;

  const glibcVersion = Object.prototype.hasOwnProperty.call(options, "glibcVersion")
    ? options.glibcVersion
    : detectedLinuxGlibcVersion(options.processReport);

  if (!parseLibcVersion(glibcVersion)) {
    throw unsupportedRuntimeError(
      `MoonDesk prebuilt Linux binaries require detectable glibc ${MIN_LINUX_GLIBC_VERSION} or newer; musl-based or unknown-libc environments such as Alpine are not currently supported.`,
    );
  }

  if (compareLibcVersions(glibcVersion, MIN_LINUX_GLIBC_VERSION) < 0) {
    throw unsupportedRuntimeError(
      `MoonDesk prebuilt Linux binaries require glibc ${MIN_LINUX_GLIBC_VERSION} or newer; detected glibc ${glibcVersion}.`,
    );
  }

  return glibcVersion;
}

function resolveTarget(platform = process.platform, arch = process.arch) {
  const target = `${platform}-${arch}`;
  if (!supportedTargets.has(target)) {
    throw unsupportedRuntimeError(
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

function stableTagIsOlder(candidate, current) {
  if (!candidate.startsWith("v") || !current.startsWith("v")) return false;
  const candidateVersion = candidate.slice(1);
  const currentVersionText = current.slice(1);
  if (!parseStableVersion(candidateVersion) || !parseStableVersion(currentVersionText)) return false;
  return compareStableVersions(candidateVersion, currentVersionText) < 0;
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

function reportDownloadProgress(callback, downloadedBytes, totalBytes, done = false) {
  if (typeof callback !== "function") return;
  try {
    callback({ downloadedBytes, totalBytes, done });
  } catch {
    // Download reporting is best-effort and must never make a verified install fail.
  }
}

async function readResponseBuffer(response, url, maxBytes, onProgress) {
  const declaredLength = Number(response.headers.get("content-length"));
  const totalBytes = Number.isFinite(declaredLength) && declaredLength >= 0 ? declaredLength : null;
  if (totalBytes !== null && totalBytes > maxBytes) {
    throw new Error(`${url} is unexpectedly large (${totalBytes} bytes)`);
  }

  reportDownloadProgress(onProgress, 0, totalBytes);

  if (!response.body || typeof response.body.getReader !== "function") {
    const buffer = Buffer.from(await response.arrayBuffer());
    if (buffer.length > maxBytes) {
      throw new Error(`${url} exceeded the ${maxBytes}-byte download limit`);
    }
    reportDownloadProgress(onProgress, buffer.length, totalBytes, true);
    return buffer;
  }

  const reader = response.body.getReader();
  const chunks = [];
  let downloadedBytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = Buffer.from(value);
      downloadedBytes += chunk.length;
      if (downloadedBytes > maxBytes) {
        try {
          await reader.cancel();
        } catch {
          // Preserve the size-limit error below if cancellation itself fails.
        }
        throw new Error(`${url} exceeded the ${maxBytes}-byte download limit`);
      }
      chunks.push(chunk);
      reportDownloadProgress(onProgress, downloadedBytes, totalBytes);
    }
  } finally {
    reader.releaseLock?.();
  }

  reportDownloadProgress(onProgress, downloadedBytes, totalBytes, true);
  return Buffer.concat(chunks, downloadedBytes);
}

async function fetchRequired(
  fetchImpl,
  url,
  maxBytes,
  timeoutMs = METADATA_TIMEOUT_MS,
  onProgress,
) {
  const response = await fetchImpl(url, {
    headers: {
      "User-Agent": `moondesk-npm/${version}`,
    },
    signal: AbortSignal.timeout(timeoutMs),
  });

  if (!response.ok) {
    throw new Error(`${url} returned HTTP ${response.status}`);
  }

  return readResponseBuffer(response, url, maxBytes, onProgress);
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
  assertLinuxRuntimeCompatibility(targetInfo.platform, options);
  const installDir = options.installDir ?? defaultInstallDir(targetInfo.target);
  const releaseBaseUrl = options.releaseBaseUrl ?? defaultReleaseBaseUrl;
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;

  if (typeof fetchImpl !== "function") {
    throw new Error("MoonDesk requires Node.js ^20.19.0 || ^22.12.0 || >=23 so the native binary can be downloaded securely");
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
      options.onDownloadProgress,
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

function formatMiB(bytes) {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function formatProgressBar(percent, width = 10) {
  const clampedPercent = Math.max(0, Math.min(100, percent));
  const filled = Math.round((clampedPercent / 100) * width);
  return `[${"█".repeat(filled)}${"░".repeat(width - filled)}]`;
}

function createDownloadProgressReporter(writer = process.stderr) {
  let lastPercentBucket = -10;
  let lastUnknownBytes = 0;
  let lastLineLength = 0;
  const percentReportStep = 10;
  const unknownReportStep = 5 * 1024 * 1024;

  const render = (line, done) => {
    if (!writer || typeof writer.write !== "function") return;

    // A redirected stderr cannot repaint one terminal row. Emit only the completed state there so
    // logs stay compact; interactive terminals get the live in-place progress experience.
    if (writer.isTTY !== true) {
      if (done) writer.write(`${line}\n`);
      return;
    }

    const padding = " ".repeat(Math.max(0, lastLineLength - line.length));
    writer.write(`\r${line}${padding}${done ? "\n" : ""}`);
    lastLineLength = done ? 0 : line.length;
  };

  // Progress is diagnostic stderr output: stdout stays reserved for native command results.
  return ({ downloadedBytes, totalBytes, done = false }) => {
    if (!Number.isFinite(downloadedBytes) || downloadedBytes < 0) return;

    if (Number.isFinite(totalBytes) && totalBytes > 0) {
      const percent = Math.min(100, Math.floor((downloadedBytes / totalBytes) * 100));
      const bucket = percent === 100 ? 100 : Math.floor(percent / percentReportStep) * percentReportStep;
      if (bucket > lastPercentBucket || done) {
        lastPercentBucket = Math.max(lastPercentBucket, bucket);
        const transferred = formatMiB(Math.min(downloadedBytes, totalBytes));
        render(
          `MoonDesk ${formatProgressBar(bucket)} ${String(bucket).padStart(3)}%  ${transferred} / ${formatMiB(totalBytes)}`,
          done,
        );
      }
      return;
    }

    if (downloadedBytes === 0 || downloadedBytes - lastUnknownBytes >= unknownReportStep || done) {
      lastUnknownBytes = downloadedBytes;
      render(
        done
          ? `MoonDesk [██████████] done  ${formatMiB(downloadedBytes)}`
          : `MoonDesk [░░░░░░░░░░] downloading…  ${formatMiB(downloadedBytes)}`,
        done,
      );
    }
  };
}

module.exports = {
  MIN_LINUX_GLIBC_VERSION,
  UNSUPPORTED_RUNTIME_ERROR_CODE,
  assertLinuxRuntimeCompatibility,
  cleanupOldBinaryVersions,
  createDownloadProgressReporter,
  ensureBinary,
  expectedSha256,
  resolveTarget,
  sha256Buffer,
};

if (require.main === module) {
  ensureBinary({ onDownloadProgress: createDownloadProgressReporter() })
    .then((binaryPath) => {
      console.log(`MoonDesk native binary ready at ${binaryPath}`);
    })
    .catch((error) => {
      console.error(`MoonDesk binary install failed: ${error.message}`);
      process.exit(1);
    });
}
