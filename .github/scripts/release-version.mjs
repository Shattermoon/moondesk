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

  const breaking = messages.some(
    (message) =>
      /^[a-z]+(?:\([^)]+\))?!:/m.test(message) ||
      /^BREAKING[ -]CHANGE:/m.test(message),
  );
  if (breaking) return "major";

  const feature = messages.some((message) => /^feat(?:\([^)]+\))?:/m.test(message));
  return feature ? "minor" : "patch";
}

const packagePath = "package.json";
const cargoPath = "Cargo.toml";
const lockPath = "Cargo.lock";

const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
const currentVersion = packageJson.version;
parseVersion(currentVersion, "package.json version");

const cargoText = readFileSync(cargoPath, "utf8");
const cargoVersionMatch = cargoText.match(/\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m);
if (!cargoVersionMatch) throw new Error("could not read [package] version from Cargo.toml");
if (cargoVersionMatch[1] !== currentVersion) {
  throw new Error(`package.json (${currentVersion}) and Cargo.toml (${cargoVersionMatch[1]}) disagree`);
}

const lockText = readFileSync(lockPath, "utf8");
const lockVersionMatch = lockText.match(/\[\[package\]\]\r?\nname = "moondesk"\r?\nversion = "([^"]+)"/);
if (!lockVersionMatch) throw new Error("could not read moondesk version from Cargo.lock");
if (lockVersionMatch[1] !== currentVersion) {
  throw new Error(`package.json (${currentVersion}) and Cargo.lock (${lockVersionMatch[1]}) disagree`);
}

const latestTag = latestReleaseTag();
let nextVersion = currentVersion;
let chosenBump = "current";

if (latestTag) {
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

  const updatedCargo = cargoText.replace(
    /(\[package\][\s\S]*?^version\s*=\s*")[^"]+(".*$)/m,
    `$1${nextVersion}$2`,
  );
  if (updatedCargo === cargoText && nextVersion !== currentVersion) {
    throw new Error("failed to update Cargo.toml version");
  }
  writeFileSync(cargoPath, updatedCargo);

  const updatedLock = lockText.replace(
    /(\[\[package\]\]\r?\nname = "moondesk"\r?\nversion = ")[^"]+("\r?\n)/,
    `$1${nextVersion}$2`,
  );
  if (updatedLock === lockText && nextVersion !== currentVersion) {
    throw new Error("failed to update Cargo.lock version");
  }
  writeFileSync(lockPath, updatedLock);
}

console.error(
  `release plan: current=${currentVersion} latest=${latestTag ?? "none"} bump=${chosenBump} next=${nextVersion}`,
);
console.log(nextVersion);
