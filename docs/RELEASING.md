# MoonDesk automated releases

MoonDesk releases are built and published by `.github/workflows/release.yml`.

## What happens after a merge to `main`

A normal push to `main` (including a merged PR) starts the release workflow. Release commits created by GitHub Actions are ignored so the workflow cannot recursively release itself.

The pipeline:

1. checks formatting;
2. runs normal Clippy with warnings denied;
3. runs the stricter production Clippy policy that rejects `unwrap`, `expect`, `panic`, and `unreachable` in the MoonDesk binary;
4. runs the full Rust test suite;
5. checks the npm package contents;
6. chooses the release version;
7. creates a temporary release-candidate commit containing the exact npm/Cargo version;
8. builds and smoke-tests that candidate on Linux x64, Linux arm64, macOS Intel, macOS arm64, and Windows x64;
9. verifies `main` did not move while the matrix was building;
10. atomically advances `main` to the tested release commit and creates `vX.Y.Z`;
11. creates the GitHub Release with all five binaries and `SHA256SUMS`;
12. publishes the same version to npm;
13. removes the temporary release-candidate branch.

The npm package is deliberately published only after the GitHub Release exists because `npm/postinstall.js` downloads the platform binary and checksum file from that matching GitHub Release.

If another commit reaches `main` while a release is building, the older release refuses to overwrite the newer branch. The later queued run can release the combined changes instead.

## Version selection

The first release uses the version already present in both `package.json` and `Cargo.toml`.

After a release tag exists, automatic releases inspect commits since the latest `vX.Y.Z` tag:

- a conventional breaking commit (`feat!:` / `fix!:` / `BREAKING CHANGE:`) -> major;
- `feat:` -> minor;
- everything else -> patch.

The workflow can also be started manually from GitHub Actions with an explicit `patch`, `minor`, or `major` bump.

`package.json`, `Cargo.toml`, and the root `moondesk` entry in `Cargo.lock` are kept at the same version. The Rust binary embeds `CARGO_PKG_VERSION`, so the binaries are compiled from the versioned release-candidate commit rather than from an unversioned merge checkout.

## One-time npm bootstrap

At the time this automation was added, `moondesk` had not been published to npm yet. npm Trusted Publishing can only be configured for a package that already exists, so the first publish needs a normal npm publishing credential.

Before merging the release-automation PR:

1. Create a granular npm access token that can publish `moondesk` and is suitable for non-interactive CI publishing.
2. In GitHub, open `Shattermoon/moondesk` -> **Settings** -> **Secrets and variables** -> **Actions**.
3. Add a repository secret named `NPM_TOKEN` containing that token.
4. Merge the release-automation PR. Its merge to `main` will run the pipeline and publish the first release.

The workflow prints a clear error at the npm step if the first publication is attempted without an npm credential. It will never publish npm before all binary builds and the GitHub Release are successful.

## Switch to tokenless npm Trusted Publishing

After the first npm package exists:

1. Open the `moondesk` package settings on npmjs.com.
2. Under **Trusted Publisher**, choose **GitHub Actions**.
3. Configure:
   - organization/user: `Shattermoon`
   - repository: `moondesk`
   - workflow filename: `release.yml`
   - allowed action: `npm publish`
4. Do not set an npm environment name unless the GitHub workflow is changed to use the same environment.
5. Trigger a later release and verify npm shows the trusted/provenance publication.
6. Remove the GitHub `NPM_TOKEN` secret once OIDC publishing has been verified.
7. Optionally configure npm publishing access to disallow traditional tokens after Trusted Publishing is working.

The publish job uses a GitHub-hosted runner, Node 24, npm 12, and `id-token: write`, so npm can authenticate the workflow through OIDC. If a version is already present on npm, the job treats it as idempotently complete instead of attempting to republish an immutable npm version.

## Recovery

The release workflow also supports manual dispatch from GitHub Actions. Candidate branches are temporary. They are cleaned after a build failure or after the tested release refs have been published successfully; if publishing the release refs succeeds but GitHub Release creation itself fails, the candidate branch is intentionally retained so a rerun can recover from the exact tested commit.

If a matrix build fails, no release tag, GitHub Release, or npm version is created.

If `main` moves during a build, the release stops before updating `main` or creating the tag.

If GitHub Release creation succeeds but npm publication fails, fix npm authentication and rerun the failed job. The publish job first checks whether that exact npm version already exists, so it is safe to retry.
