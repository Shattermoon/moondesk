# MoonDesk automated releases

MoonDesk releases are built and published by `.github/workflows/release.yml`.

## Release boundary: merge first, release second

The release workflow listens to `push` events on `main` because npm Trusted Publishing currently works reliably with the GitHub `push` OIDC identity and has an upstream failure mode with `pull_request_target`.

A `push` event by itself is **not** enough to release. The first job is a read-only gate that:

1. verifies the event is a push to `refs/heads/main`;
2. verifies the checked-out SHA is the current `origin/main` head;
3. asks GitHub which pull request is associated with that exact commit;
4. requires exactly one closed PR whose `merged_at` is set, whose base is `main`, and whose `merge_commit_sha` is exactly the pushed SHA;
5. reads the release bump label from that merged PR.

If a commit was pushed directly to `main`, is a release-bot version commit, or has already been superseded by a newer `main`, the gate reports `should_release=false` and every mutation/build/publish job is skipped. Opening, updating, approving, or otherwise reviewing a PR never invokes the Release workflow at all; normal PR validation is handled by `.github/workflows/ci.yml`.

GitHub does not recursively trigger normal `push` workflows for commits pushed with the repository `GITHUB_TOKEN`, and the merged-PR gate is an additional backstop if that platform behavior ever changes or a release commit is pushed by some other credential.

## Pipeline

After the merged-PR gate succeeds, the pipeline runs in this order:

1. **npm OIDC preflight** — before creating a candidate branch, tag, GitHub Release, or npm version, a minimal job with only `id-token: write` asks GitHub for an OIDC identity and exchanges it with npm's Trusted Publishing endpoint for `moondesk`. The short-lived npm token is validated and immediately discarded. If npm does not trust this workflow identity, release mutation never begins.
2. **Merged-source validation** — formatting, normal Clippy, strict production Clippy, all Rust tests, npm wrapper syntax, npm installer unit tests, repository metadata, and npm package contents are checked again on the exact merged SHA.
3. **Previous-gap repair** — before planning the newly merged PR, the workflow checks whether the current manifest/tag represents a modern GitHub-only release that never reached npm. If so, it publishes and smoke-tests that immutable older version first. Older tags that used lifecycle `postinstall` are intentionally left GitHub-only.
4. **Release planning** — the merged PR's optional release label selects patch/minor/major, otherwise conventional commits select the bump.
5. **Versioned candidate** — `package.json`, `Cargo.toml`, and the root MoonDesk entry in `Cargo.lock` are versioned together and committed to a temporary release-candidate branch.
6. **Five-platform matrix** — Linux x64, Linux arm64, macOS Intel, macOS arm64, and Windows x64 compile the exact candidate. Every binary runs a ClippyMoon help/export smoke test before becoming an artifact.
7. **Refs + GitHub Release** — `main` must still equal the original merged SHA. Only then does the workflow atomically advance `main` to the tested version commit and create the annotated `vX.Y.Z` tag. All five binaries are assembled, SHA-256 checksums are generated and checked, and the GitHub Release is created from those exact artifacts.
8. **npm publish** — a separate minimal OIDC job checks out the immutable release tag, uses a clean token-free npm configuration, publishes with explicit provenance, and confirms the immutable version appears in the registry.
9. **Fresh-install E2E** — the just-published npm package is installed with lifecycle scripts explicitly disabled. Running its CLI must bootstrap the correct GitHub Release binary, verify the checksum, and successfully export ClippyMoon. This proves npm + GitHub Release + checksum + wrapper + first-run bootstrap work as one system.
10. **Cleanup** — temporary candidate branches are removed once they are no longer required for safe recovery.

## Version selection

The first release uses the version already present in both `package.json` and `Cargo.toml`.

After a release tag exists, automatic releases inspect commits since the latest `vX.Y.Z` tag:

- a conventional breaking commit (`feat!:` / `fix!:` / `BREAKING CHANGE:`) -> major;
- `feat:` -> minor;
- everything else -> patch.

A merged PR may explicitly choose the bump by carrying exactly one of these labels before merge:

- `release:patch`
- `release:minor`
- `release:major`

Conflicting bump labels fail at the read-only merge gate before OIDC, builds, or repository mutation.

`package.json`, `Cargo.toml`, and the root `moondesk` entry in `Cargo.lock` stay on the same version. The Rust binary embeds `CARGO_PKG_VERSION`, so release binaries are always compiled from the versioned candidate/tag rather than from an unversioned source tree.

## Interrupted-release recovery

The pipeline is intentionally state-aware because Git refs, GitHub Releases, and npm cannot be updated atomically as one transaction.

If a modern MoonDesk tag already exists at the manifest version, is an ancestor of `main`, contains the lifecycle-script-free npm bootstrap, and that exact version is missing from npm, the next verified merged-PR release repairs that npm gap **before** planning the newly merged PR. It verifies the existing GitHub Release asset set, publishes the immutable tagged package with OIDC/provenance, performs a fresh-install smoke test, and then continues to create the new release for the current merge.

Older partial tags that predate `npm/install-binary.js` are deliberately not backfilled to npm because those packages relied on lifecycle `postinstall`. A later release gets a new version instead of rewriting the old public tag or publishing an obsolete installation model.

If `main` advances while a normal release matrix is building, the older release refuses to overwrite it. The newer queued `main` push can release the combined source instead.

If a matrix build fails, no new tag, GitHub Release, or npm version is created.

If GitHub Release creation succeeds but npm later encounters a transient problem, rerunning the failed npm job is the fastest recovery. If that is not done before another PR merges, the next release automatically repairs the missing modern npm version first and then continues with the newly merged release. npm versions are always checked before publishing, so repair is idempotent rather than attempting to republish an immutable version.

## npm Trusted Publishing

`moondesk` is published through npm Trusted Publishing using GitHub Actions OIDC. No long-lived npm publishing token is injected into the workflow.

The npm Trusted Publisher should be configured as:

- publisher: **GitHub Actions**;
- organization/user: `Shattermoon`;
- repository: `moondesk`;
- workflow filename: `release.yml`;
- environment: blank (unless both npm and this workflow are intentionally changed to the same GitHub Environment later);
- allowed action: `npm publish`.

The release uses a GitHub-hosted runner, Node 24, a pinned npm 12 CLI, `id-token: write`, and a clean npmrc containing only the npm registry and provenance setting. It deliberately does **not** use `actions/setup-node`'s `registry-url` option in the publish job, because that option can write an empty `_authToken=${NODE_AUTH_TOKEN}` placeholder that interferes with OIDC in some npm/setup-node combinations.

The early preflight performs the same GitHub-OIDC -> npm token exchange that `npm publish` relies on. This moves Trusted Publisher configuration failures before any irreversible release mutation.

Trusted Publishing automatically supports provenance; MoonDesk additionally requests `--provenance` explicitly so loss of provenance is treated as a release failure rather than silently degrading the supply-chain guarantee.

The obsolete GitHub Actions `NPM_TOKEN` secret has been removed. Any old npm access token that was used only to bootstrap publishing should also be revoked in npm account settings.

## npm 12 and native binary installation

MoonDesk no longer uses `preinstall`, `install`, or `postinstall` lifecycle scripts. npm 12 blocks dependency install scripts by default, so relying on `postinstall` would make a normal `npm install -g moondesk` incomplete unless the user explicitly approved scripts.

The npm package now contains a small JS wrapper plus `npm/install-binary.js`. On first CLI invocation it:

1. chooses the platform/architecture-specific release asset;
2. downloads `SHA256SUMS` and the matching native binary over HTTPS;
3. enforces download-size limits;
4. verifies SHA-256 before installation;
5. writes through a lock and temporary files so concurrent first launches cannot leave a partial binary;
6. stores the binary and its checksum in a versioned user cache under `~/.moondesk/npm-bin/vX.Y.Z/<platform-arch>/`;
7. verifies the cached binary before subsequent launches.

Keeping the native executable outside npm's `node_modules` also avoids the common Windows failure where `npm update -g` cannot replace/delete an `.exe` that is still running from inside the package directory.

## CI vs Release

`.github/workflows/ci.yml` is the review-time workflow. It runs Rust validation plus npm wrapper/installer unit tests and package checks. It never gets npm OIDC publishing permission.

`.github/workflows/release.yml` is the post-merge workflow. Its privileged capabilities are split by job:

- the merge gate is read-only;
- the npm preflight has `id-token: write` but checks out no project source;
- build jobs have no publishing identity and no repository write permission;
- the Git/tag/Release job has `contents: write` but no npm OIDC permission;
- the npm publish job has `id-token: write` and `contents: read`, with no repository write permission.

This separation reduces the blast radius of any single job and prevents build/test code from sharing a runner with the npm publishing identity.
