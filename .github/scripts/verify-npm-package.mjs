#!/usr/bin/env node

import { execFileSync } from "node:child_process";

const expected = [
  "LICENSE",
  "README.md",
  "npm/install-binary.js",
  "npm/moondesk-browser.js",
  "npm/moondesk.js",
  "npm/update-manager.js",
  "package.json",
  "skills/browser/SKILL.md",
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

let packageReport;
if (Array.isArray(report)) {
  if (report.length === 1) packageReport = report[0];
} else if (report && typeof report === "object") {
  const entries = Object.values(report);
  if (entries.length === 1) packageReport = entries[0];
}
if (!packageReport || !Array.isArray(packageReport.files)) {
  throw new Error("npm pack returned an unexpected report shape");
}

const actual = packageReport.files.map((entry) => entry.path).sort();
if (JSON.stringify(actual) !== JSON.stringify(expected)) {
  throw new Error(
    `unexpected npm package contents\nexpected: ${expected.join(", ")}\nactual:   ${actual.join(", ")}`,
  );
}

if (packageReport.entryCount !== expected.length) {
  throw new Error(`npm package entry count ${packageReport.entryCount} did not match ${expected.length}`);
}

console.log(`Verified npm package contents (${expected.length} files).`);
