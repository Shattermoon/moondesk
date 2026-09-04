#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { ensureBinary } = require("./install-binary");
const { version } = require("../package.json");

const BROWSER_CLI_FLAG = "--browser-cli";
const SKILL_PATH = path.join(__dirname, "..", "skills", "browser", "SKILL.md");

function usageText() {
  return [
    "MoonDesk browser CLI",
    "",
    "Usage:",
    "  moondesk-browser <chrome-devtools command> [...args]",
    "  moondesk-browser skill",
    "  moondesk-browser --help",
    "",
    "Examples:",
    "  moondesk-browser list_pages",
    "  moondesk-browser navigate_page --url=https://example.com",
    "  moondesk-browser take_snapshot",
    "  moondesk-browser click 1_23 --includeSnapshot",
    "  moondesk-browser evaluate_script \"() => document.title\"",
    "",
    "The CLI uses MoonDesk's pinned Chrome DevTools runtime and the same live",
    "isolated agent-browser session as MCP browser_command/view_page. Run",
    "`moondesk-browser skill` for the agent workflow and safety guidance.",
  ].join("\n");
}

function runNativeBrowser(binaryPath, args, options = {}) {
  const spawnImpl = options.spawnImpl ?? spawn;
  const cwd = options.cwd ?? process.cwd();
  const env = options.env ?? process.env;
  return new Promise((resolve, reject) => {
    const child = spawnImpl(binaryPath, [BROWSER_CLI_FLAG, ...args], {
      cwd,
      env,
      stdio: "inherit",
      windowsHide: false,
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code: code ?? 1, signal }));
  });
}

async function orchestrate(options = {}) {
  const args = options.args ?? process.argv.slice(2);
  const logger = options.logger ?? console;
  const readFileSyncImpl = options.readFileSyncImpl ?? fs.readFileSync;
  const ensureBinaryImpl = options.ensureBinaryImpl ?? ensureBinary;
  const runNativeBrowserImpl = options.runNativeBrowserImpl ?? runNativeBrowser;

  const first = args[0];
  if (args.length === 0 || first === "help" || first === "-h" || first === "--help") {
    logger.log(usageText());
    return { code: 0, signal: null };
  }
  if (first === "-v" || first === "--version") {
    logger.log(version);
    return { code: 0, signal: null };
  }
  if (first === "skill") {
    try {
      logger.log(readFileSyncImpl(SKILL_PATH, "utf8").trimEnd());
      return { code: 0, signal: null };
    } catch (error) {
      logger.error(`MoonDesk could not read the browser skill: ${error.message}`);
      return { code: 1, signal: null };
    }
  }

  let binaryPath;
  try {
    binaryPath = await ensureBinaryImpl();
  } catch (error) {
    logger.error(`MoonDesk could not prepare its native binary: ${error.message}`);
    return { code: 1, signal: null };
  }

  try {
    return await runNativeBrowserImpl(binaryPath, args, {
      cwd: options.cwd ?? process.cwd(),
      env: options.env ?? process.env,
    });
  } catch (error) {
    logger.error(`MoonDesk browser command failed to start: ${error.message}`);
    return { code: 1, signal: null };
  }
}

function finish(result) {
  if (result.signal) {
    try {
      process.kill(process.pid, result.signal);
    } catch (error) {
      console.error(`MoonDesk browser CLI could not propagate ${result.signal}: ${error.message}`);
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
      console.error(`MoonDesk browser CLI failed: ${error.message}`);
      process.exitCode = 1;
    });
}

module.exports = {
  BROWSER_CLI_FLAG,
  SKILL_PATH,
  orchestrate,
  runNativeBrowser,
  usageText,
};
