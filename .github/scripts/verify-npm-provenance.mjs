#!/usr/bin/env node

const [packageName, version, expectedRef, expectedSha, repository] = process.argv.slice(2);

if (!packageName || !version || !expectedRef || !expectedSha || !repository) {
  console.error(
    "usage: verify-npm-provenance.mjs <package> <version> <git-ref> <git-sha> <owner/repo>",
  );
  process.exit(2);
}

if (!/^[0-9a-f]{40}$/i.test(expectedSha)) {
  throw new Error(`Expected a full 40-character Git SHA, got ${expectedSha}`);
}

const predicateType = "https://slsa.dev/provenance/v1";
const workflowPath = ".github/workflows/release.yml";
const expectedRepository = `https://github.com/${repository}`;
const expectedDependencyUri = `git+https://github.com/${repository}@${expectedRef}`;
const attestationUrl = `https://registry.npmjs.org/-/npm/v1/attestations/${packageName}@${version}`;
const attempts = Number.parseInt(process.env.PROVENANCE_ATTEMPTS ?? "12", 10);
const retryDelayMs = Number.parseInt(process.env.PROVENANCE_RETRY_DELAY_MS ?? "5000", 10);

if (!Number.isSafeInteger(attempts) || attempts < 1 || attempts > 60) {
  throw new Error(`Invalid PROVENANCE_ATTEMPTS value: ${process.env.PROVENANCE_ATTEMPTS}`);
}
if (!Number.isSafeInteger(retryDelayMs) || retryDelayMs < 0 || retryDelayMs > 60_000) {
  throw new Error(
    `Invalid PROVENANCE_RETRY_DELAY_MS value: ${process.env.PROVENANCE_RETRY_DELAY_MS}`,
  );
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function decodeSlsa(attestations) {
  if (!Array.isArray(attestations)) {
    return null;
  }

  const attestation = attestations.find((entry) => entry?.predicateType === predicateType);
  const payload = attestation?.bundle?.dsseEnvelope?.payload;
  if (typeof payload !== "string" || payload.length === 0) {
    return null;
  }

  return JSON.parse(Buffer.from(payload, "base64").toString("utf8"));
}

function verify(statement) {
  const buildDefinition = statement?.predicate?.buildDefinition;
  const workflow = buildDefinition?.externalParameters?.workflow;
  const dependencies = buildDefinition?.resolvedDependencies;

  if (workflow?.ref !== expectedRef) {
    throw new Error(`Provenance ref mismatch: expected ${expectedRef}, got ${workflow?.ref}`);
  }
  if (workflow?.repository !== expectedRepository) {
    throw new Error(
      `Provenance repository mismatch: expected ${expectedRepository}, got ${workflow?.repository}`,
    );
  }
  if (workflow?.path !== workflowPath) {
    throw new Error(`Provenance workflow mismatch: expected ${workflowPath}, got ${workflow?.path}`);
  }
  if (!Array.isArray(dependencies)) {
    throw new Error("Provenance does not contain resolvedDependencies");
  }

  const source = dependencies.find(
    (entry) =>
      entry?.uri === expectedDependencyUri &&
      entry?.digest?.gitCommit?.toLowerCase() === expectedSha.toLowerCase(),
  );
  if (!source) {
    throw new Error(
      `Provenance does not resolve ${expectedDependencyUri} to ${expectedSha}`,
    );
  }
}

let lastUnavailable = "npm provenance endpoint did not return a usable SLSA attestation";

for (let attempt = 1; attempt <= attempts; attempt += 1) {
  try {
    const response = await fetch(attestationUrl, {
      headers: { "User-Agent": `moondesk-release-provenance/${version}` },
      signal: AbortSignal.timeout(30_000),
    });

    if (response.ok) {
      const statement = decodeSlsa((await response.json()).attestations);
      if (statement) {
        verify(statement);
        console.log(`Verified npm provenance for ${expectedRef} at ${expectedSha}.`);
        process.exit(0);
      }
      lastUnavailable = "npm returned attestations without a SLSA provenance statement";
    } else {
      lastUnavailable = `npm provenance endpoint returned HTTP ${response.status}`;
    }
  } catch (error) {
    if (
      error instanceof Error &&
      (error.message.startsWith("Provenance ") || error.message.startsWith("Provenance does not"))
    ) {
      throw error;
    }
    lastUnavailable = error instanceof Error ? error.message : String(error);
  }

  if (attempt < attempts) {
    await sleep(retryDelayMs);
  }
}

throw new Error(
  `SLSA provenance for ${packageName}@${version} was not available after ${attempts} attempts: ${lastUnavailable}`,
);
