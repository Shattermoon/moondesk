#!/usr/bin/env node

const { spawn } = require("node:child_process");
const { ensureBinary } = require("./install-binary");

async function main() {
  let binaryPath;
  try {
    binaryPath = await ensureBinary();
  } catch (error) {
    console.error(`MoonDesk could not prepare its native binary: ${error.message}`);
    console.error("Check your network connection and the matching GitHub Release, then run MoonDesk again.");
    process.exit(1);
    return;
  }

  const child = spawn(binaryPath, process.argv.slice(2), {
    cwd: process.cwd(),
    env: process.env,
    stdio: "inherit",
  });

  child.on("error", (error) => {
    console.error(`MoonDesk failed to start: ${error.message}`);
    process.exit(1);
  });

  child.on("exit", (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 1);
  });
}

main().catch((error) => {
  console.error(`MoonDesk failed to start: ${error.message}`);
  process.exit(1);
});
