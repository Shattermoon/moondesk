#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

const args = process.argv.slice(2);
const apply = args.includes("--apply");
const bumpIndex = args.indexOf("--bump");
const requestedBump = bumpIndex >= 0 ? args[bumpIndex + 1] : "auto";
const allowedBumps = new Set(["auto", "patch", "minor", "major"]);

if (!allowedBumps.has(requestedBump)) {
  throw new Error(`unsupported release bump: ${requestedBump}`);
}

function git(...gitArgs) {
  return execFileSync("git", gitArgs, { encoding: "utf8" }).trim();
}

function parseVersion(value, label) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(value);
  if (!match) {
    throw new Error(`${label} must use stable x.y.z semver, got ${value}`);
  }
  return match.slice(1).map(Number);
}

function compareVersions(a, b) {
  for (let index = 0; index < 3; index += 1) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return 0;
}

function bumpVersion(version, kind) {
  const [major, minor, patch] = parseVersion(version, "current version");
  if (kind === "major") return `${major + 1}.0.0`;
  if (kind === "minor") return `${major}.${minor + 1}.0`;
  return `${major}.${minor}.${patch + 1}`;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function firstCommitLine(message) {
  return message.split(/\r?\n/, 1)[0] ?? "";
}

function latestReleaseTag() {
  const output = git("tag", "--merged", "HEAD", "--list", "v[0-9]*", "--sort=-version:refname");
  if (!output) return null;
  return output
    .split(/\r?\n/)
    .map((tag) => tag.trim())
    .find((tag) => /^v\d+\.\d+\.\d+$/.test(tag)) ?? null;
}

function automaticBumpSince(tag) {
  const log = git("log", `${tag}..HEAD`, "--format=%B%x00");
  const messages = log.split("\0").map((message) => message.trim()).filter(Boolean);
  if (messages.length === 0) {
    throw new Error(`nothing to release after ${tag}`);
  }

  const breaking = messages.some((message) => {
    const subject = firstCommitLine(message);
    return (
      /^[a-z]+(?:\([^)]+\))?!:/.test(subject) ||
      /^BREAKING[ -]CHANGE:/m.test(message)
    );
  });
  if (breaking) return "major";

  const feature = messages.some((message) =>
    /^feat(?:\([^)]+\))?:/.test(firstCommitLine(message)),
  );
  return feature ? "minor" : "patch";
}

function findTomlTableBody(text, tableName) {
  const headerPattern = new RegExp(`^\\[${escapeRegExp(tableName)}\\][ \\t]*\\r?$`, "m");
  const headerMatch = headerPattern.exec(text);
  if (!headerMatch) return null;

  const bodyStart = headerMatch.index + headerMatch[0].length;
  const rest = text.slice(bodyStart);
  const nextTable = /^[ \t]*\[[^\r\n]+\][ \t]*\r?$/m.exec(rest);
  const bodyEnd = nextTable ? bodyStart + nextTable.index : text.length;
  return {
    bodyStart,
    bodyEnd,
    body: text.slice(bodyStart, bodyEnd),
  };
}

function cargoPackageMetadata(text) {
  const table = findTomlTableBody(text, "package");
  if (!table) throw new Error("could not read [package] table from Cargo.toml");

  const nameMatch = /^name[ \t]*=[ \t]*"([^"]+)"[ \t]*(?:#.*)?$/m.exec(table.body);
  if (!nameMatch) throw new Error("could not read [package] name from Cargo.toml");

  const versionMatch = /^version[ \t]*=[ \t]*"([^"]+)"[ \t]*(?:#.*)?$/m.exec(table.body);
  if (!versionMatch) {
    throw new Error(
      "could not read a literal [package] version from Cargo.toml (workspace-based versions are not supported)",
    );
  }

  return {
    ...table,
    name: nameMatch[1],
    version: versionMatch[1],
  };
}

function rewriteCargoPackageVersion(text, nextVersion) {
  const table = cargoPackageMetadata(text);
  const updatedBody = table.body.replace(
    /^(version[ \t]*=[ \t]*")[^"]+(".*)$/m,
    `$1${nextVersion}$2`,
  );
  if (updatedBody === table.body) {
    throw new Error("failed to update [package] version in Cargo.toml");
  }
  return text.slice(0, table.bodyStart) + updatedBody + text.slice(table.bodyEnd);
}

const packagePath = "package.json";
const cargoPath = "Cargo.toml";
const lockPath = "Cargo.lock";

const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
if (typeof packageJson.name !== "string" || packageJson.name.length === 0) {
  throw new Error("package.json must contain a non-empty package name");
}
const packageName = packageJson.name;
const currentVersion = packageJson.version;
parseVersion(currentVersion, "package.json version");

const cargoText = readFileSync(cargoPath, "utf8");
const cargoPackage = cargoPackageMetadata(cargoText);
if (cargoPackage.name !== packageName) {
  throw new Error(`package.json (${packageName}) and Cargo.toml (${cargoPackage.name}) package names disagree`);
}
if (cargoPackage.version !== currentVersion) {
  throw new Error(`package.json (${currentVersion}) and Cargo.toml (${cargoPackage.version}) disagree`);
}

const lockText = readFileSync(lockPath, "utf8");
const escapedPackageName = escapeRegExp(packageName);
const lockVersionPattern = new RegExp(
  `(\\[\\[package\\]\\]\\r?\\nname = "${escapedPackageName}"\\r?\\nversion = ")([^"]+)(")`,
);
const lockVersionMatch = lockVersionPattern.exec(lockText);
if (!lockVersionMatch) throw new Error(`could not read ${packageName} version from Cargo.lock`);
if (lockVersionMatch[2] !== currentVersion) {
  throw new Error(`package.json (${currentVersion}) and Cargo.lock (${lockVersionMatch[2]}) disagree`);
}

const latestTag = latestReleaseTag();
let nextVersion = currentVersion;
let chosenBump = "current";

if (!latestTag) {
  if (requestedBump !== "auto") {
    chosenBump = requestedBump;
    nextVersion = bumpVersion(currentVersion, chosenBump);
  }
} else {
  const taggedVersion = latestTag.slice(1);
  const comparison = compareVersions(
    parseVersion(currentVersion, "current version"),
    parseVersion(taggedVersion, "latest tag"),
  );

  if (comparison < 0) {
    throw new Error(`manifest version ${currentVersion} is older than latest release ${latestTag}`);
  }

  if (comparison === 0) {
    chosenBump = requestedBump === "auto" ? automaticBumpSince(latestTag) : requestedBump;
    nextVersion = bumpVersion(currentVersion, chosenBump);
  } else if (requestedBump !== "auto") {
    throw new Error(
      `explicit ${requestedBump} bump cannot be applied because manifest version ${currentVersion} ` +
        `already exceeds latest release ${latestTag}; use --bump auto to release the manifest version as-is`,
    );
  }
}

const nextTag = `v${nextVersion}`;
let tagExists = false;
try {
  execFileSync("git", ["show-ref", "--verify", "--quiet", `refs/tags/${nextTag}`], {
    stdio: "ignore",
  });
  tagExists = true;
} catch {
  tagExists = false;
}
if (tagExists) {
  throw new Error(`release tag ${nextTag} already exists`);
}

if (apply && nextVersion !== currentVersion) {
  packageJson.version = nextVersion;
  writeFileSync(packagePath, `${JSON.stringify(packageJson, null, 2)}\n`);

  writeFileSync(cargoPath, rewriteCargoPackageVersion(cargoText, nextVersion));

  const updatedLock = lockText.replace(lockVersionPattern, `$1${nextVersion}$3`);
  if (updatedLock === lockText) {
    throw new Error(`failed to update ${packageName} version in Cargo.lock`);
  }
  writeFileSync(lockPath, updatedLock);
}

console.error(
  `release plan: current=${currentVersion} latest=${latestTag ?? "none"} bump=${chosenBump} next=${nextVersion}`,
);
console.log(nextVersion);
