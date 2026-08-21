const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
  ensureBinary,
  resolveTarget,
  sha256Buffer,
} = require("./install-binary");

function tempDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "moondesk-install-test-"));
}

function makeFetch(binary, assetName, options = {}) {
  const expected = options.expected ?? sha256Buffer(binary);
  const calls = [];
  const delayMs = options.delayMs ?? 0;

  const fetchImpl = async (url) => {
    calls.push(url);
    if (delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }

    if (url.endsWith("/SHA256SUMS")) {
      const body = `${expected}  ${assetName}\n`;
      return new Response(body, {
        status: 200,
        headers: {
          "content-length": String(options.checksumContentLength ?? Buffer.byteLength(body)),
        },
      });
    }
    if (url.endsWith(`/${assetName}`)) {
      return new Response(binary, {
        status: 200,
        headers: { "content-length": String(binary.length) },
      });
    }
    return new Response("not found", { status: 404 });
  };

  return { calls, fetchImpl };
}

test("resolveTarget rejects unsupported targets", () => {
  assert.throws(
    () => resolveTarget("plan9", "mips"),
    /does not provide a prebuilt binary/,
  );
});

test("ensureBinary downloads, verifies, and reuses a cached binary", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from(`moondesk-test-${crypto.randomUUID()}\n`);
    const { calls, fetchImpl } = makeFetch(binary, target.assetName);

    const binaryPath = await ensureBinary({
      installDir: dir,
      releaseBaseUrl: "https://example.invalid/releases/v-test",
      fetchImpl,
    });

    assert.equal(fs.readFileSync(binaryPath).compare(binary), 0);
    assert.match(fs.readFileSync(`${binaryPath}.sha256`, "utf8"), /^[a-f0-9]{64}\n$/);
    assert.equal(calls.length, 2);

    const secondPath = await ensureBinary({
      installDir: dir,
      releaseBaseUrl: "https://example.invalid/releases/v-test",
      fetchImpl,
    });

    assert.equal(secondPath, binaryPath);
    assert.equal(calls.length, 2, "cached binary should not hit the network again");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("unchanged cached binaries use metadata without rehashing", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from(`moondesk-fast-cache-${crypto.randomUUID()}\n`);
    const { fetchImpl } = makeFetch(binary, target.assetName);
    const options = {
      installDir: dir,
      releaseBaseUrl: "https://example.invalid/releases/v-test",
      fetchImpl,
    };

    const binaryPath = await ensureBinary(options);
    const originalCreateHash = crypto.createHash;
    crypto.createHash = () => {
      throw new Error("cached fast path unexpectedly rehashed the native binary");
    };
    try {
      assert.equal(await ensureBinary(options), binaryPath);
    } finally {
      crypto.createHash = originalCreateHash;
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("ensureBinary refuses checksum mismatches without leaving a partial binary", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from("definitely-not-the-expected-binary");
    const { fetchImpl } = makeFetch(binary, target.assetName, {
      expected: "0".repeat(64),
    });

    await assert.rejects(
      ensureBinary({
        installDir: dir,
        releaseBaseUrl: "https://example.invalid/releases/v-test",
        fetchImpl,
      }),
      /Checksum mismatch/,
    );

    const executableName = target.platform === "win32" ? "moondesk.exe" : "moondesk";
    assert.equal(fs.existsSync(path.join(dir, executableName)), false);
    assert.equal(fs.existsSync(path.join(dir, `${executableName}.sha256`)), false);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("ensureBinary rejects an oversized declared checksum response", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from("small-binary");
    const { fetchImpl } = makeFetch(binary, target.assetName, {
      checksumContentLength: 2 * 1024 * 1024,
    });

    await assert.rejects(
      ensureBinary({
        installDir: dir,
        releaseBaseUrl: "https://example.invalid/releases/v-test",
        fetchImpl,
      }),
      /unexpectedly large/,
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("concurrent first runs share one atomic installation", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from(`moondesk-concurrent-${crypto.randomUUID()}\n`);
    const { calls, fetchImpl } = makeFetch(binary, target.assetName, { delayMs: 75 });

    const options = {
      installDir: dir,
      releaseBaseUrl: "https://example.invalid/releases/v-test",
      fetchImpl,
    };

    const [first, second] = await Promise.all([
      ensureBinary(options),
      ensureBinary(options),
    ]);

    assert.equal(first, second);
    assert.equal(fs.readFileSync(first).compare(binary), 0);
    assert.equal(
      calls.filter((url) => url.endsWith("/SHA256SUMS")).length,
      1,
      "only one installer should request release metadata",
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a corrupt abandoned install lock is recovered immediately", async () => {
  const dir = tempDir();
  try {
    const target = resolveTarget();
    const binary = Buffer.from(`moondesk-lock-recovery-${crypto.randomUUID()}\n`);
    const { fetchImpl } = makeFetch(binary, target.assetName);
    fs.writeFileSync(path.join(dir, ".install.lock"), "not-a-pid\n");

    const startedAt = Date.now();
    const binaryPath = await ensureBinary({
      installDir: dir,
      releaseBaseUrl: "https://example.invalid/releases/v-test",
      fetchImpl,
    });

    assert.equal(fs.readFileSync(binaryPath).compare(binary), 0);
    assert.ok(Date.now() - startedAt < 5_000, "lock recovery should not wait for the normal lock timeout");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
