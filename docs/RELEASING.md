# MoonDesk automated releases

MoonDesk releases are built and published by `.github/workflows/release.yml`.

## What happens after a merge to `main`

The release workflow listens only for a pull request being **closed against `main`**, and its release jobs run only when GitHub reports that pull request as **merged**. Opening a PR, pushing review updates, requesting review, approving it, or closing it without merging does not execute the release pipeline. Direct pushes to `main` also do not trigger a release. GitHub Actions release commits therefore cannot recursively release themselves.

The workflow uses `pull_request_target` for the post-merge event so a merged PR from a fork can still use the repository's release permissions. It never builds the untrusted PR head: after `merged == true`, it checks out the exact `merge_commit_sha` that GitHub says landed in `main`, verifies that SHA is now an ancestor of `origin/main`, and only then validates/builds it.

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

A merged PR may explicitly choose the bump by carrying exactly one of these labels before merge: `release:patch`, `release:minor`, or `release:major`. With no release label, the workflow uses the conventional-commit rules above. Conflicting release bump labels fail the release before any candidate is published.

`package.json`, `Cargo.toml`, and the root `moondesk` entry in `Cargo.lock` are kept at the same version. The Rust binary embeds `CARGO_PKG_VERSION`, so the binaries are compiled from the versioned release-candidate commit rather than from an unversioned merge checkout.

## npm Trusted Publishing

`moondesk` is published through npm Trusted Publishing using GitHub Actions OIDC. The npm package remains unscoped as `moondesk`, while publishing authority comes from the Shattermoon repository and release workflow.

The npm Trusted Publisher must be configured as:

- publisher: **GitHub Actions**;
- organization/user: `Shattermoon`;
- repository: `moondesk`;
- workflow filename: `release.yml`;
- environment: blank unless this workflow is later changed to use a matching GitHub Actions environment;
- allowed action: `npm publish`.

The publish job deliberately does not read or inject an `NPM_TOKEN`. It uses a GitHub-hosted runner, Node 24, npm 12, and `id-token: write`, so npm authenticates the exact `Shattermoon/moondesk` workflow through OIDC. If a version is already present on npm, the job treats it as idempotently complete instead of attempting to republish an immutable npm version.

Trusted Publishing has been verified with a real npm release and the release workflow has no `NPM_TOKEN` or `NODE_AUTH_TOKEN` consumer. The obsolete GitHub Actions `NPM_TOKEN` secret has been removed. The corresponding granular npm access token must also be revoked in npm account settings once it is no longer needed anywhere else. Package publishing access should stay on the most restrictive **Require two-factor authentication and disallow bypass 2FA tokens** option. Trusted Publishing remains compatible because it authenticates through OIDC rather than a traditional npm publishing token.

## Recovery

The release workflow intentionally has no manual-dispatch or review-time release trigger: normal releases originate only from a merged PR into `main`. Candidate branches are temporary. They are cleaned after a build failure, when the release stops before publishing refs (for example because `main` advanced), or after the tested release refs have been published successfully. If publishing the release refs succeeds but GitHub Release creation itself fails, the candidate branch is intentionally retained so GitHub's **Re-run failed jobs** action can recover from the exact tested commit and event payload without creating a second release source.

If a matrix build fails, no release tag, GitHub Release, or npm version is created.

If `main` moves during a build, the release stops before updating `main` or creating the tag.

If GitHub Release creation succeeds but npm publication fails, fix npm authentication and rerun the failed job. The publish job first checks whether that exact npm version already exists, so it is safe to retry.
