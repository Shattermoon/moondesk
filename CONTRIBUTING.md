# Contributing to MoonDesk

Thanks for contributing to MoonDesk. The project is small enough that focused pull requests, reproducible bug reports, and careful platform-specific fixes have a large impact.

MoonDesk is also unusually sensitive to regressions because it combines a local MCP server, filesystem tools, shell/process management, browser tooling, a terminal UI, multi-workspace routing, and a native-binary npm distribution path. Please prefer small, well-tested changes over broad rewrites unless the rewrite is necessary.

## Before you start

For anything beyond a tiny documentation fix:

1. Check existing issues and pull requests to avoid duplicating work.
2. Keep the change scoped to one problem or feature whenever practical.
3. Preserve existing behavior unless the PR explicitly intends to change it.
4. Add or update tests for behavior that can be tested deterministically.
5. Treat workspace isolation, process cleanup, update logic, and release code as security/reliability-sensitive areas.

Never include a real MoonDesk workspace MCP URL, ngrok authtoken, npm credential, GitHub token, local credential, or other secret in an issue, test fixture, screenshot, commit, or pull request.

## Development requirements

MoonDesk currently requires:

- Rust **1.88 or newer** (`edition = 2024`)
- Node.js **18 or newer** for the npm wrapper/runtime
- Git

For the closest match to CI, use the current stable Rust toolchain with `rustfmt` and `clippy`. CI validates the npm runtime on both Node 18 and Node 24.

Some optional integration tests require platform-specific software:

- Windows tests may require PowerShell and standard developer tools.
- Browser/DevTools tests require a locally installed supported Chromium browser.
- Browser control itself uses `chrome-devtools-mcp` through `npx`.
- Running MoonDesk end-to-end through a public MCP endpoint requires ngrok configuration.

## Getting the repository running

Clone the repository and enter it:

```bash
git clone https://github.com/Shattermoon/moondesk.git
cd moondesk
```

Build a development binary:

```bash
cargo build --locked
```

Run MoonDesk from source:

```bash
cargo run --locked
```

You can also exercise the non-interactive ClippyMoon path without starting the TUI:

```bash
cargo run --locked -- clippymoon --help
cargo run --locked -- clippymoon export --seed 0000000000000042 --out ./tmp-clippymoon
```

Remove any generated test/export files before committing.

## Required validation

Before opening or updating a pull request, run the checks that apply to your change. For normal Rust changes, the expected baseline is:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --bin moondesk --locked -- \
  -D warnings \
  -D clippy::unwrap_used \
  -D clippy::expect_used \
  -D clippy::panic \
  -D clippy::unreachable
cargo test --locked
```

On Windows, run the test suite serially to match CI's process-heavy validation:

```powershell
cargo test --locked -- --test-threads 1
```

If your change touches process execution, Windows environment handling, browser launching/detection, or DevTools lifecycle code, run the relevant ignored integration smokes when your machine supports them:

```bash
cargo test --locked windows_developer_toolchain_smoke_uses_normal_host_environment -- --ignored
cargo test --locked remote_browser_termination_confirms_child_exit -- --ignored
cargo test --locked windows_devtools_stop_terminates_npx_process_tree -- --ignored
```

Additional ignored tests may require a live Chromium remote-debug endpoint. Do not weaken or delete environment-specific tests merely to make them run in an environment that does not satisfy their stated prerequisites.

## npm wrapper and distribution checks

If you touch `npm/`, `package.json`, release scripts, update behavior, binary bootstrap logic, or related GitHub Actions, also run:

```bash
node --check .github/scripts/npm-oidc-preflight.mjs
node --check .github/scripts/verify-npm-provenance.mjs
node --check npm/moondesk.js
node --check npm/install-binary.js
node --check npm/update-manager.js
node --check .github/scripts/verify-npm-package.mjs
node --test npm/install-binary.test.js npm/update-manager.test.js npm/moondesk.test.js
node .github/scripts/verify-npm-package.mjs
```

MoonDesk intentionally does **not** use npm `preinstall`, `install`, or `postinstall` lifecycle scripts. Do not introduce one without first understanding the npm 12/default-deny installation model documented in [`docs/RELEASING.md`](docs/RELEASING.md).

The npm wrapper downloads the native release binary on first run, verifies it against the release's SHA-256 checksums, and keeps it outside `node_modules`. Changes to that path should preserve atomic installation, checksum verification, concurrency safety, and Windows update behavior.

## Release-build smoke test

For changes that affect startup, rendering/export, dependencies, platform code, packaging, or release behavior, a local release build is strongly recommended:

```bash
cargo build --release --locked
```

Then exercise the built binary's non-interactive path. On Unix-like systems:

```bash
./target/release/moondesk clippymoon --help
```

On Windows:

```powershell
.\target\release\moondesk.exe clippymoon --help
```

## Architecture and safety rules

### Workspace filesystem boundary

Dedicated file tools (`read`, `search`, `write`, `edit`, and `delete`) are workspace-confined. They reject path traversal and filesystem indirection that escapes the registered workspace root.

When changing path handling:

- preserve canonicalization/normalization behavior;
- test nonexistent descendants as well as existing paths;
- consider symlinks on Unix and junction/reparse/short-path aliases on Windows;
- do not replace filesystem-aware checks with lexical string-prefix checks;
- preserve rejection of duplicate and parent/child-overlapping workspace roots.

### Shell commands are intentionally not an OS sandbox

`run_command` and `start_command` execute the user's normal developer shell with the workspace as the working directory. They intentionally inherit the user's normal PATH, home directory, environment, credentials, SDKs, and OS permissions.

Do not describe CWD confinement as an OS sandbox. If a change intends to introduce stronger command isolation, treat that as a separate architectural/security feature and document the compatibility tradeoffs.

### Multi-workspace isolation

A single MoonDesk host can serve multiple workspace endpoints. A change in one workspace must not accidentally expose another workspace's:

- filesystem root;
- secret MCP slug;
- command jobs;
- preserved command output;
- connection/runtime state;
- normal per-workspace quotas/history.

Browser/DevTools control is intentionally shared by the host and should remain clearly distinguished from workspace-local state.

### Process lifecycle

Process handling is cross-platform and easy to get subtly wrong. Changes to commands, background jobs, browser launching, or DevTools launching should account for:

- cancellation before spawn;
- cancellation during execution;
- timeout;
- parent exit with descendants still alive;
- bounded stdout/stderr handling;
- cleanup after errors;
- Windows process trees/Job Objects;
- Unix process groups;
- temporary directory/profile cleanup.

A test that only confirms the immediate shell process exited is not sufficient when the feature can spawn descendants.

### Secrets and logging

Workspace MCP slugs are credentials. Avoid adding logs, health responses, errors, analytics, or UI state that expose secret workspace paths unnecessarily. Tests should use synthetic values.

## Pull request guidelines

A good MoonDesk PR should:

- explain the user-visible problem or goal;
- explain the important implementation choice when it is not obvious;
- list the validation performed;
- call out platform-specific behavior or untested environments;
- include screenshots/GIFs for meaningful TUI changes when useful;
- avoid unrelated formatting, dependency, or refactor churn;
- leave the working tree free of generated artifacts.

Draft PRs intentionally do not run the normal review CI automatically. Mark the PR ready for review when it is ready for the full checks.

When responding to automated review feedback, verify the finding against the current code before changing anything. Do not apply suggestions mechanically if they would regress existing behavior or conflict with the architecture above.

## Commit messages

MoonDesk uses conventional-style commit subjects. Keep commits concise and consistent with nearby history, for example:

```text
feat: surface DevTools page count
fix: own DevTools process tree on Windows
ci: add RustSec dependency audit
docs: update contribution guide
```

Common prefixes include `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `ci:`, and `chore:`.

Breaking conventional commits (`feat!:` / `fix!:` or a `BREAKING CHANGE:` footer) affect automatic release version selection, so use them only for genuinely breaking changes.

## Versioning and releases

Do **not** manually bump `package.json`, `Cargo.toml`, or the root MoonDesk version in `Cargo.lock` in a normal contribution. Do not create a release tag from a feature/fix PR.

MoonDesk's release pipeline runs after a PR is merged to `main`. By default, conventional commits determine the version bump:

- breaking change -> major;
- `feat:` -> minor;
- other changes -> patch.

Maintainers can explicitly override the bump by applying exactly one of:

- `release:patch`
- `release:minor`
- `release:major`

The automated release pipeline validates the merged source, creates a versioned candidate, builds and smoke-tests all supported release targets, creates the GitHub Release/checksums, publishes npm through Trusted Publishing/OIDC, verifies provenance, and performs a fresh-install bootstrap test.

See [`docs/RELEASING.md`](docs/RELEASING.md) before modifying release automation.

## Documentation changes

Documentation should describe current behavior, not an implementation plan that has already shipped. When a feature is complete, update the README/user docs and remove obsolete planning material instead of leaving two competing sources of truth.

For command/tool documentation, keep names and argument behavior synchronized with the actual MCP schemas and tests.

## Final checklist

Before requesting review, confirm:

- [ ] The change is scoped and the reason is clear.
- [ ] New/changed behavior has appropriate tests.
- [ ] `cargo fmt --check` passes for Rust changes.
- [ ] Clippy passes with warnings denied for Rust changes.
- [ ] Relevant Rust tests pass.
- [ ] npm wrapper/package tests pass when npm/release code changed.
- [ ] Platform-specific smokes were run where applicable, or the PR states what could not be tested.
- [ ] No secrets, real workspace URLs, generated binaries, temp profiles, or export artifacts were committed.
- [ ] README/docs reflect user-visible changes.
- [ ] Package versions were not manually bumped for a normal PR.

Thanks for helping make MoonDesk safer, more reliable, and easier to use.
