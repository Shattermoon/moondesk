#!/usr/bin/env node

const fs = require("node:fs");
const { spawn } = require("node:child_process");
const { cleanupOldBinaryVersions, ensureBinary } = require("./install-binary");
const {
  UPDATE_EXIT_CODE,
  acquireUpdateLock,
  cleanupOldUpdateVersions,
  compareStableVersions,
  createUpdateRequestPath,
  createUpdateStatePath,
  installExactVersion,
  installedWrapperVersion,
  readUpdateRequest,
  restartUpdatedWrapper,
  startUpdateMonitor,
  verifyInstalledWrapperVersion,
} = require("./update-manager");

function cleanManagedUpdateEnv(source = process.env) {
  const env = { ...source };
  delete env.MOONDESK_NPM_MANAGED;
  delete env.MOONDESK_UPDATE_REQUEST_PATH;
  delete env.MOONDESK_UPDATE_STATE_PATH;
  return env;
}

function cleanupEphemeralUpdateFiles(statePath, requestPath) {
  fs.rmSync(statePath, { force: true });
  fs.rmSync(requestPath, { force: true });
}

function runNative(binaryPath, args, options = {}) {
  const spawnImpl = options.spawnImpl ?? spawn;
  return new Promise((resolve, reject) => {
    const child = spawnImpl(binaryPath, args, {
      cwd: options.cwd ?? process.cwd(),
      env: options.env ?? process.env,
      stdio: "inherit",
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code: code ?? 1, signal }));
  });
}

async function orchestrate(options = {}) {
  const logger = options.logger ?? console;
  const originalArgs = options.args ?? process.argv.slice(2);
  const originalCwd = options.cwd ?? process.cwd();
  const baseEnv = cleanManagedUpdateEnv(options.env ?? process.env);
  const updateStatePath = options.updateStatePath ?? createUpdateStatePath();
  const updateRequestPath = options.updateRequestPath ?? createUpdateRequestPath();
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
  const restartUpdatedWrapperImpl = options.restartUpdatedWrapperImpl ?? restartUpdatedWrapper;
  const wrapperPath = options.wrapperPath ?? __filename;

  const stopUpdateMonitor = startUpdateMonitorImpl({
    statePath: updateStatePath,
    cwd: originalCwd,
    env: baseEnv,
  });

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

  const childEnv = {
    ...baseEnv,
    MOONDESK_NPM_MANAGED: "1",
    MOONDESK_UPDATE_REQUEST_PATH: updateRequestPath,
    MOONDESK_UPDATE_STATE_PATH: updateStatePath,
  };

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
    return { code: 1, signal: null };
  }

  stopUpdateMonitor();
  fs.rmSync(updateStatePath, { force: true });

  if (result.signal) {
    fs.rmSync(updateRequestPath, { force: true });
    return result;
  }

  if (result.code !== UPDATE_EXIT_CODE) {
    fs.rmSync(updateRequestPath, { force: true });
    return { code: result.code ?? 1, signal: null };
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

  logger.log(`MoonDesk ${restartVersion} is installed. Restarting...`);
  try {
    return await restartUpdatedWrapperImpl(wrapperPath, originalArgs, {
      cwd: originalCwd,
      env: baseEnv,
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
