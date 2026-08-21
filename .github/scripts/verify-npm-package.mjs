#!/usr/bin/env node

import { execFileSync } from "node:child_process";

const expected = [
  "LICENSE",
  "README.md",
  "npm/install-binary.js",
  "npm/moondesk.js",
  "npm/update-manager.js",
  "package.json",
].sort();

const output = execFileSync(
  process.platform === "win32" ? "npm.cmd" : "npm",
  ["pack", "--dry-run", "--json", "--ignore-scripts"],
  {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
    windowsHide: true,
    shell: process.platform === "win32",
  },
);

let report;
try {
  report = JSON.parse(output);
} catch (error) {
  throw new Error(`npm pack did not return valid JSON: ${error.message}`);
}

if (!Array.isArray(report) || report.length !== 1 || !Array.isArray(report[0]?.files)) {
  throw new Error("npm pack returned an unexpected report shape");
}

const actual = report[0].files.map((entry) => entry.path).sort();
if (JSON.stringify(actual) !== JSON.stringify(expected)) {
  throw new Error(
    `unexpected npm package contents\nexpected: ${expected.join(", ")}\nactual:   ${actual.join(", ")}`,
  );
}

if (report[0].entryCount !== expected.length) {
  throw new Error(`npm package entry count ${report[0].entryCount} did not match ${expected.length}`);
}

console.log(`Verified npm package contents (${expected.length} files).`);
