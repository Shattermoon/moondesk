#!/usr/bin/env node

const fs = require("node:fs");
const { spawn } = require("node:child_process");
const { cleanupOldBinaryVersions, ensureBinary } = require("./install-binary");
const {
  UPDATE_EXIT_CODE,
  acquireUpdateLock,
  cleanupOldUpdateVersions,
  changelogNoticePath,
  compareStableVersions,
  createUpdateRequestPath,
  createUpdateStatePath,
  installExactVersion,
  installedWrapperVersion,
  readUpdateRequest,
  restartUpdatedWrapper,
  startUpdateMonitor,
  verifyInstalledWrapperVersion,
  writePostUpdateNotice,
} = require("./update-manager");

function cleanManagedUpdateEnv(source = process.env) {
  const env = { ...source };
  delete env.MOONDESK_NPM_MANAGED;
  delete env.MOONDESK_UPDATE_REQUEST_PATH;
  delete env.MOONDESK_UPDATE_STATE_PATH;
  delete env.MOONDESK_CHANGELOG_NOTICE_PATH;
  return env;
}

function cleanupEphemeralUpdateFiles(statePath, requestPath) {
  for (const filePath of [statePath, requestPath]) {
    if (!filePath) continue;
    try {
      fs.rmSync(filePath, { force: true });
    } catch {
      // Ephemeral updater metadata must never make normal MoonDesk startup or shutdown fail.
    }
  }
}

function runNative(binaryPath, args, options = {}) {
  const spawnImpl = options.spawnImpl ?? spawn;
  const signalTarget = options.signalTarget ?? process;
  return new Promise((resolve, reject) => {
    const child = spawnImpl(binaryPath, args, {
      cwd: options.cwd ?? process.cwd(),
      env: options.env ?? process.env,
      stdio: "inherit",
    });

    // The native TUI owns Ctrl+C confirmation once the shared host is running.
    // Without a listener, Node's default SIGINT behavior can terminate the npm
    // wrapper before the native child restores the terminal and closes ngrok.
    const ignoreParentSigint = () => {};
    signalTarget.on?.("SIGINT", ignoreParentSigint);
    const cleanupSignalHandler = () => {
      signalTarget.off?.("SIGINT", ignoreParentSigint);
    };

    child.once("error", (error) => {
      cleanupSignalHandler();
      reject(error);
    });
    child.once("exit", (code, signal) => {
      cleanupSignalHandler();
      resolve({ code: code ?? 1, signal });
    });
  });
}

function nativeStartFailureHints(error, binaryPath, options = {}) {
  const platform = options.platform ?? process.platform;
  if (platform !== "win32") return [];

  const existsSync = options.existsSync ?? fs.existsSync;
  let binaryExists = true;
  try {
    binaryExists = existsSync(binaryPath);
  } catch {
    // If the path cannot be checked, keep the original spawn error rather than
    // guessing that security software removed the binary.
  }

  if (!binaryExists) {
    return [
      "The verified MoonDesk native binary disappeared before Windows could launch it.",
      "Windows Security or another antivirus may have quarantined the executable. Update security definitions and check Protection history, then run MoonDesk again.",
      `Native binary: ${binaryPath}`,
    ];
  }

  const code = typeof error?.code === "string" ? error.code : "";
  const message = error?.message ?? "";
  if (
    ["UNKNOWN", "EPERM", "EACCES"].includes(code) ||
    /\b(?:UNKNOWN|EPERM|EACCES)\b/i.test(message)
  ) {
    return [
      "Windows refused to launch the verified MoonDesk native binary. Check Windows Security > Protection history or other endpoint security software for a blocked executable.",
      `Native binary: ${binaryPath}`,
    ];
  }

  return [];
}

async function orchestrate(options = {}) {
  const logger = options.logger ?? console;
  const originalArgs = options.args ?? process.argv.slice(2);
  const originalCwd = options.cwd ?? process.cwd();
  const baseEnv = cleanManagedUpdateEnv(options.env ?? process.env);
  const createUpdateStatePathImpl = options.createUpdateStatePathImpl ?? createUpdateStatePath;
  const createUpdateRequestPathImpl = options.createUpdateRequestPathImpl ?? createUpdateRequestPath;
  let updateStatePath = options.updateStatePath ?? null;
  let updateRequestPath = options.updateRequestPath ?? null;
  let selfUpdateEnabled = true;
  try {
    updateStatePath = updateStatePath ?? createUpdateStatePathImpl();
    updateRequestPath = updateRequestPath ?? createUpdateRequestPathImpl();
  } catch (error) {
    if (!options.updateStatePath) cleanupEphemeralUpdateFiles(updateStatePath, null);
    if (!options.updateRequestPath) cleanupEphemeralUpdateFiles(null, updateRequestPath);
    updateStatePath = null;
    updateRequestPath = null;
    selfUpdateEnabled = false;
    logger.warn?.(
      `MoonDesk disabled in-app self-update because it could not prepare update metadata: ${error.message}`,
    );
  }
  const ensureBinaryImpl = options.ensureBinaryImpl ?? ensureBinary;
  const cleanupOldBinaryVersionsImpl = options.cleanupOldBinaryVersionsImpl ?? cleanupOldBinaryVersions;
  const cleanupOldUpdateVersionsImpl = options.cleanupOldUpdateVersionsImpl ?? cleanupOldUpdateVersions;
  const startUpdateMonitorImpl = options.startUpdateMonitorImpl ?? startUpdateMonitor;
  const runNativeImpl = options.runNativeImpl ?? runNative;
  const readUpdateRequestImpl = options.readUpdateRequestImpl ?? readUpdateRequest;
  const acquireUpdateLockImpl = options.acquireUpdateLockImpl ?? acquireUpdateLock;
  const installedWrapperVersionImpl = options.installedWrapperVersionImpl ?? installedWrapperVersion;
  const installExactVersionImpl = options.installExactVersionImpl ?? installExactVersion;
  const verifyInstalledWrapperVersionImpl =
    options.verifyInstalledWrapperVersionImpl ?? verifyInstalledWrapperVersion;
  const writePostUpdateNoticeImpl = options.writePostUpdateNoticeImpl ?? writePostUpdateNotice;
  const restartUpdatedWrapperImpl = options.restartUpdatedWrapperImpl ?? restartUpdatedWrapper;
  const wrapperPath = options.wrapperPath ?? __filename;

  const stopUpdateMonitor = selfUpdateEnabled
    ? startUpdateMonitorImpl({
        statePath: updateStatePath,
        cwd: originalCwd,
        env: baseEnv,
      })
    : () => {};

  let binaryPath;
  try {
    binaryPath = await ensureBinaryImpl();
  } catch (error) {
    stopUpdateMonitor();
    cleanupEphemeralUpdateFiles(updateStatePath, updateRequestPath);
    logger.error(`MoonDesk could not prepare its native binary: ${error.message}`);
    logger.error(
      "Check your network connection and the matching GitHub Release, then run MoonDesk again.",
    );
    return { code: 1, signal: null };
  }

  try {
    cleanupOldBinaryVersionsImpl();
  } catch (error) {
    // Cache cleanup is best-effort. A locked old Windows executable must not stop MoonDesk.
    logger.warn?.(`MoonDesk could not remove an older native cache yet: ${error.message}`);
  }
  try {
    cleanupOldUpdateVersionsImpl();
  } catch (error) {
    logger.warn?.(`MoonDesk could not remove older update metadata yet: ${error.message}`);
  }

  const childEnv = { ...baseEnv };
  if (selfUpdateEnabled) {
    Object.assign(childEnv, {
      MOONDESK_NPM_MANAGED: "1",
      MOONDESK_UPDATE_REQUEST_PATH: updateRequestPath,
      MOONDESK_UPDATE_STATE_PATH: updateStatePath,
    });
    try {
      childEnv.MOONDESK_CHANGELOG_NOTICE_PATH = changelogNoticePath();
    } catch (error) {
      logger.warn?.(`MoonDesk could not prepare its optional changelog notice path: ${error.message}`);
    }
  }

  let result;
  try {
    result = await runNativeImpl(binaryPath, originalArgs, {
      cwd: originalCwd,
      env: childEnv,
    });
  } catch (error) {
    stopUpdateMonitor();
    cleanupEphemeralUpdateFiles(updateStatePath, updateRequestPath);
    logger.error(`MoonDesk failed to start: ${error.message}`);
    for (const hint of nativeStartFailureHints(error, binaryPath, {
      platform: options.platform,
      existsSync: options.binaryExistsSyncImpl,
    })) {
      logger.error(hint);
    }
    return { code: 1, signal: null };
  }

  stopUpdateMonitor();
  cleanupEphemeralUpdateFiles(updateStatePath, null);

  if (result.signal) {
    cleanupEphemeralUpdateFiles(null, updateRequestPath);
    return result;
  }

  if (result.code !== UPDATE_EXIT_CODE) {
    cleanupEphemeralUpdateFiles(null, updateRequestPath);
    return { code: result.code ?? 1, signal: null };
  }

  if (!selfUpdateEnabled || !updateRequestPath) {
    logger.error("MoonDesk requested an update restart, but in-app self-update is unavailable for this launch.");
    return { code: 1, signal: null };
  }

  const request = readUpdateRequestImpl(updateRequestPath);
  if (!request) {
    logger.error(
      "MoonDesk requested an update restart, but its validated update request was missing or invalid.",
    );
    return { code: 1, signal: null };
  }

  logger.log(`Updating MoonDesk ${request.currentVersion} -> ${request.targetVersion}...`);
  let releaseUpdateLock = null;
  let restartVersion = request.targetVersion;
  try {
    releaseUpdateLock = await acquireUpdateLockImpl({
      cwd: originalCwd,
      env: baseEnv,
    });

    const alreadyInstalled = installedWrapperVersionImpl();
    const comparison = compareStableVersions(alreadyInstalled, request.targetVersion);
    if (comparison < 0) {
      await installExactVersionImpl(request.targetVersion, {
        cwd: originalCwd,
        env: baseEnv,
      });
      verifyInstalledWrapperVersionImpl(request.targetVersion);
    } else if (comparison === 0) {
      logger.log(`MoonDesk ${request.targetVersion} was already installed by another process.`);
    } else {
      restartVersion = alreadyInstalled;
      logger.log(
        `MoonDesk ${alreadyInstalled} is already installed, so the updater will not downgrade it to ${request.targetVersion}.`,
      );
    }
  } catch (error) {
    logger.error(`MoonDesk update failed: ${error.message}`);
    logger.error(
      `Run 'npm install -g moondesk@${request.targetVersion}' manually to retry this exact version.`,
    );
    return { code: 1, signal: null };
  } finally {
    try {
      releaseUpdateLock?.();
    } catch (error) {
      logger.warn?.(`MoonDesk could not remove its npm update lock yet: ${error.message}`);
    }
  }

  if (restartVersion === request.targetVersion) {
    try {
      writePostUpdateNoticeImpl(request, restartVersion);
    } catch (error) {
      logger.warn?.(`MoonDesk updated, but could not persist its one-time changelog notice: ${error.message}`);
    }
  }

  logger.log(`MoonDesk ${restartVersion} is installed. Restarting...`);
  try {
    return await restartUpdatedWrapperImpl(wrapperPath, originalArgs, {
      cwd: originalCwd,
      env: baseEnv,
      logger,
    });
  } catch (error) {
    logger.error(`MoonDesk updated successfully but could not restart automatically: ${error.message}`);
    logger.error("Run 'moondesk' again to start the updated version.");
    return { code: 1, signal: null };
  }
}

function finish(result) {
  if (result.signal) {
    try {
      process.kill(process.pid, result.signal);
    } catch (error) {
      console.error(`MoonDesk could not propagate ${result.signal}: ${error.message}`);
      process.exitCode = 1;
    }
    return;
  }
  process.exitCode = result.code ?? 1;
}

if (require.main === module) {
  orchestrate()
    .then(finish)
    .catch((error) => {
      console.error(`MoonDesk failed to start: ${error.message}`);
      process.exitCode = 1;
    });
}

module.exports = {
  cleanManagedUpdateEnv,
  cleanupEphemeralUpdateFiles,
  orchestrate,
  runNative,
};
