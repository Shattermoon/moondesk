import fs from "node:fs";
import path from "node:path";

const MANIFEST_PATH = path.join("browser", "managed-browser.json");
const EXPECTED_PLATFORMS = [
  "linux-x64",
  "linux-arm64",
  "darwin-x64",
  "darwin-arm64",
  "win32-x64",
];
const MAX_BROWSER_ARCHIVE_BYTES = 256 * 1024 * 1024;

function fail(message) {
  throw new Error(`Invalid managed-browser manifest: ${message}`);
}

const manifest = JSON.parse(fs.readFileSync(MANIFEST_PATH, "utf8"));

if (manifest.schemaVersion !== 1) {
  fail(`schemaVersion must be 1, received ${manifest.schemaVersion}`);
}
if (typeof manifest.version !== "string" || !/^\d+\.\d+\.\d+\.\d+$/.test(manifest.version)) {
  fail("version must be a four-component Chrome version");
}
if (typeof manifest.revision !== "string" || !/^\d+$/.test(manifest.revision)) {
  fail("revision must be a decimal string");
}
if (!manifest.platforms || typeof manifest.platforms !== "object" || Array.isArray(manifest.platforms)) {
  fail("platforms must be an object");
}

const actualPlatforms = Object.keys(manifest.platforms).sort();
const expectedPlatforms = [...EXPECTED_PLATFORMS].sort();
if (actualPlatforms.join("\n") !== expectedPlatforms.join("\n")) {
  fail(
    `platform set must be exactly ${EXPECTED_PLATFORMS.join(", ")}; received ${actualPlatforms.join(", ")}`,
  );
}

for (const platform of EXPECTED_PLATFORMS) {
  const artifact = manifest.platforms[platform];
  if (!artifact || typeof artifact !== "object" || Array.isArray(artifact)) {
    fail(`${platform} artifact must be an object`);
  }

  let url;
  try {
    url = new URL(artifact.url);
  } catch {
    fail(`${platform} url is not a valid URL`);
  }
  if (url.protocol !== "https:" || url.hostname !== "storage.googleapis.com") {
    fail(`${platform} url must use the official Chrome for Testing HTTPS origin`);
  }
  if (
    url.username ||
    url.password ||
    !url.pathname.startsWith(
      `/chrome-for-testing-public/${manifest.version}/`,
    )
  ) {
    fail(`${platform} url must be pinned under version ${manifest.version}`);
  }

  if (typeof artifact.sha256 !== "string" || !/^[0-9a-f]{64}$/i.test(artifact.sha256)) {
    fail(`${platform} sha256 must be a 64-character hexadecimal digest`);
  }
  if (
    !Number.isSafeInteger(artifact.size) ||
    artifact.size <= 0 ||
    artifact.size > MAX_BROWSER_ARCHIVE_BYTES
  ) {
    fail(`${platform} size must be between 1 and ${MAX_BROWSER_ARCHIVE_BYTES} bytes`);
  }
  if (typeof artifact.executable !== "string" || artifact.executable.length === 0) {
    fail(`${platform} executable must be a non-empty relative path`);
  }
  const normalizedExecutable = artifact.executable.replaceAll("\\", "/");
  if (
    normalizedExecutable.startsWith("/") ||
    /^[A-Za-z]:\//.test(normalizedExecutable) ||
    normalizedExecutable.split("/").some((segment) => segment === ".." || segment.length === 0)
  ) {
    fail(`${platform} executable must be a safe relative path`);
  }
}

console.log(
  `Verified managed browser manifest ${manifest.version} for ${EXPECTED_PLATFORMS.length} platforms.`,
);
