# Browser Runtime Architecture Hardening

Status: implemented and locally validated for PR #42 (`refactor/lazy-browser-skill-runtime`)

## Purpose

This document captures the browser-runtime architecture review performed after Ashpeak's second review of PR #42. The goal is not merely to clear individual review comments. The goal is to preserve MoonDesk's intended browser product model while removing lifecycle, timeout, security, compatibility, and performance weaknesses introduced by relying on the experimental detached `chrome-devtools-mcp@1.7.0` CLI daemon as MoonDesk's internal transport.

The findings below describe the pre-hardening baseline at commit `2608f37`. They are retained as the rationale for the refactor. The current branch implementation no longer uses the detached CLI daemon for normal browser operations.

## Implementation status

The hardening work described in this document is implemented on the current branch:

- **Owned runtime transport - done.** `BrowserRuntime` lazily starts exact `chrome-devtools-mcp@1.7.0` as a direct stdio MCP child. MoonDesk owns the complete process tree using the same Windows Job Object / Unix process-group lifecycle primitives used by command execution. Daemon session IDs, daemon PID files, and `start/status/stop` subprocess orchestration are removed.
- **Post-dispatch timeout cancellation - done.** A timed-out MCP request invalidates and terminates the exact owned MCP/Chromium tree while browser serialization is still held. The Windows regression `windows_browser_timeout_cancels_dispatched_mutation` proves a delayed mutation that was genuinely dispatched cannot occur after MoonDesk returns timeout.
- **Crash/runtime-loss recovery - done.** A dead owned child is discarded and the next browser operation starts a fresh isolated runtime. MoonDesk does not automatically replay the ambiguous operation that observed the loss.
- **Host-file navigation boundary - done.** MoonDesk owns URL validation for `navigate_page` and `new_page`; local filesystem paths and unsafe local/internal schemes such as `file:`, `view-source:file:`, `chrome:`, and `javascript:` fail before reaching Chromium. Normal HTTP(S), localhost, `data:`, safe `view-source:https:`, and HTTP(S)-origin `blob:` navigation remain available.
- **Node compatibility contract - done.** The npm engine range, wrapper preflight, installer diagnostics, README, contributor docs, and CI matrix now match exact `chrome-devtools-mcp@1.7.0` support: `^20.19.0 || ^22.12.0 || >=23`. CI also launches the exact pinned browser runtime with `--help` at supported boundary/latest versions so wrapper-only tests cannot hide a future engine mismatch.
- **Single deadline model - done.** Direct stdio removes the upstream CLI client's hidden 60-second socket deadline. MoonDesk's absolute request deadline now governs queueing, runtime startup, MCP stdin lock/write/newline/flush, response wait, staging, and output publication. A blocked child stdin fails as `Timeout`; cleanup terminates the owned process tree before dropping buffered stdin so shutdown cannot wait behind a stuck writer.
- **Bounded MCP framing - done.** Browser stdout is read as newline-delimited JSON-RPC with a 16 MiB frame ceiling before allocation/parsing can grow without bound. Oversized or malformed protocol frames invalidate the transport and fail pending requests explicitly. Stderr is consumed with a bounded per-line buffer while oversized tails are discarded at ingestion.
- **Staging/output deadline hardening - done.** Potentially large staging work runs off Tokio workers; file copying checks the operation deadline in bounded chunks; file output is copied to a randomized sibling temporary file and atomically published only while the deadline remains valid. On Unix the staging root/directories are created as `0700` and staged/temp files as `0600` before the first byte is copied, preventing transient permission broadening. Existing traversal, symlink/reparse-point, and outside-workspace checks remain fail-closed.
- **Pinned command contract - done.** `src/browser_contract_v1_7.json` records all 50 commands from exact v1.7 generated CLI metadata, and `src/browser_contract.rs` parses the existing `command + args[]` surface into MCP tool arguments while preserving aliases, booleans, arrays, enums/defaults, and MoonDesk-owned `--output-format`.
- **CLI response parity - done.** MoonDesk mirrors v1.7 CLI rendering for markdown, structured JSON, MCP tool errors, and image responses rather than depending on the detached CLI renderer. Oversized JSON remains syntactically valid by returning a bounded `_moondesk.truncated` envelope with the original/limit byte counts instead of splicing plaintext into serialized JSON.
- **Deferred trace-output semantics - fail-closed.** `performance_start_trace --autoStop=false --filePath=...` is rejected by MoonDesk's command contract before transport startup/dispatch because exact v1.7 does not write that start-call path. Manual traces remain supported by starting without `filePath` and supplying `--filePath` to `performance_stop_trace`.
- **Capability parity - done.** The direct MCP server keeps the old CLI safety/runtime defaults; feature-gated extension tooling is not silently enabled.
- **Stable product surface - preserved.** The public architecture remains one `moondesk` executable, one lazy host-shared browser session, and only MCP `browser_command` + `view_page`. The host-shared browser remains an explicit trust-domain decision.

Two cleanup ideas from the audit are intentionally **not required for this PR**: fully moving every path/read-only metadata table into the checked-in command registry, and splitting `browser_runtime.rs` into a deeper module tree. The current path and ReadOnly policies remain centralized enough to be fail-closed and are covered by regression tests; those structural cleanups can be performed separately without reopening lifecycle semantics.

## Product invariants to preserve

MoonDesk should continue to provide:

- one `moondesk` executable;
- a lazy browser runtime that starts only on first browser use;
- one host-owned browser session shared by MCP `browser_command`, MCP `view_page`, and `moondesk browser ...`;
- a clean isolated Chromium profile that never attaches to the user's personal browser profile, cookies, extensions, or history;
- a deliberately small MCP browser surface (`browser_command` and `view_page`) rather than dynamically forwarding the full upstream Chrome DevTools MCP schema;
- safe workspace staging/copy-back for browser file inputs/outputs while keeping upstream unrestricted filesystem access disabled;
- ReadOnly browser policy enforced by MoonDesk rather than trusting upstream defaults;
- automatic recovery after a browser/runtime loss, but without replaying ambiguous state-changing actions.

The host-shared browser is an explicit trust-domain decision. Filesystem and command tooling remain workspace-scoped, while browser tabs/cookies/page state are host-scoped. If MoonDesk later needs mutually untrusted concurrent project sessions, browser isolation should move to a per-workspace model.

## Current architecture and why it is fragile

At PR HEAD `2608f37`, BrowserRuntime uses the following internal chain:

```text
MoonDesk BrowserRuntime
    -> launches `npx chrome-devtools <command>` for each operation
    -> CLI sends one socket/named-pipe request
    -> detached, unref'd chrome-devtools CLI daemon
    -> daemon owns a stdio chrome-devtools-mcp server
    -> server owns isolated Chromium
```

This is a poor ownership boundary for MoonDesk because MoonDesk is already the persistent host process. The detached CLI daemon exists primarily so independent shell commands can share state; MoonDesk does not need another persistence layer between its host and the actual MCP server.

The implemented replacement is:

```text
ChatGPT / `moondesk browser`
        -> MoonDesk host
        -> BrowserRuntime
             - command contract / argument translation
             - ReadOnly + URL policy
             - workspace staging
             - deadline + cancellation ownership
             - direct owned MCP stdio transport
        -> pinned chrome-devtools-mcp server child
        -> isolated Chromium
```

This is not a restoration of the deleted legacy browser architecture. MoonDesk should not restore raw schema forwarding, personal remote-debug attachment, old browser picker state, or dynamic upstream tool exposure. Only the process-ownership principle is reused: MoonDesk owns the exact subprocess tree that implements its browser runtime.

## Confirmed blockers

### 1. Dispatched operations survive MoonDesk timeout

Ashpeak's finding is valid. `tokio::time::timeout` currently cancels only MoonDesk's local future and the short-lived CLI process. Exact upstream v1.7 runs `mcpClient.callTool()` inside a detached daemon and does not cancel it when the CLI socket closes.

A state-changing click/fill/navigation/evaluate action can therefore complete after MoonDesk has returned a timeout. Releasing MoonDesk's operation mutex at that point can admit a second command while the first is still running upstream.

Required invariant:

> If an operation times out after dispatch, MoonDesk must invalidate and terminate the exact runtime that is executing it before releasing browser serialization or returning control to the caller.

With a directly owned stdio child, timeout recovery should terminate the owned MCP/Chromium process tree, wait for teardown, clear readiness, and only then release the operation lock.

### 2. Detached daemon ownership is not durable across host death/start timeout

Ashpeak's second finding is also valid. The current random `session_id` is in-memory only, while the upstream daemon is detached. Abrupt MoonDesk termination can leave an orphan daemon/browser that a later host can no longer identify. A start timeout can similarly leave a daemon in the current session namespace before `owned_daemon_pid` is committed, causing the same runtime to classify its own daemon as unowned.

A direct owned stdio child removes the need for daemon session IDs, daemon PID files, durable daemon lease recovery, `status/start/stop` CLI subprocesses, and stale namespace adoption logic. On normal host death, stdio EOF and OS process-tree ownership should tear down the runtime; on Windows a kill-on-close job object provides an additional hard ownership boundary.

### 3. `file://` bypasses the workspace filesystem boundary

This issue was discovered during the architecture audit and is not covered by the existing review threads.

The current staging layer protects explicit browser path arguments such as `upload_file`, screenshot output, heap snapshots, Lighthouse output, and network body output. It does not protect filesystem access encoded as a browser URL.

A real probe against exact v1.7 with unrestricted upstream paths disabled successfully navigated the isolated browser to a harmless `file:///C:/...` path outside the MoonDesk workspace, and `take_snapshot` returned the file contents.

Required invariant:

> Browser navigation must not provide a second filesystem API that bypasses MoonDesk workspace policy.

MoonDesk must own URL validation for URL-bearing browser commands. At minimum, local/browser-internal schemes such as `file:`, `filesystem:`, `view-source:file:`, `chrome:`, `chrome-extension:`, and equivalent unsafe local-resource forms must fail closed unless MoonDesk deliberately introduces a safe workspace-backed local-file navigation abstraction later.

HTTP(S), localhost development URLs, and other explicitly supported schemes should remain available.

### 4. Node 18 support contradicts the pinned browser runtime

MoonDesk currently declares `node >=18` and runs npm compatibility tests on Node 18. Exact `chrome-devtools-mcp@1.7.0` declares:

```text
^20.19.0 || ^22.12.0 || >=23
```

A real Node 18.20.8 run fails at startup on unsupported JavaScript syntax after npm emits the engine warning. Therefore Browser mode is not actually functional on a MoonDesk installation that the package currently advertises as supported.

Required invariant:

> MoonDesk's advertised runtime support must match all first-class product features.

Preferred fix: raise MoonDesk's Node engine floor and CI/runtime documentation to a version supported by the pinned browser dependency. If MoonDesk intentionally keeps Node 18 for Computer-only mode, Browser mode must perform an explicit compatibility preflight and the documentation/package metadata must make the feature split clear; a silent first-use failure is not acceptable.

### 5. MoonDesk's 120s timeout conflicts with upstream CLI's 60s daemon timeout

MoonDesk advertises a browser request budget up to 120 seconds. Exact upstream v1.7's CLI socket client hardcodes a 60-second send-command timeout. That means the current effective chain is:

```text
MoonDesk deadline: 120s
upstream CLI request deadline: 60s
upstream detached daemon: may keep executing after client timeout
```

This is both a contract mismatch and another source of late mutations. Direct stdio transport should use only MoonDesk's deadline model and remove this hidden 60-second layer.

### 6. Synchronous staging/copy work is not truly deadline-preemptible

Current browser staging and output publication perform synchronous filesystem work (`std::fs::copy`, recursive directory copying, output commit) inside the async request future. Tokio deadlines cannot preempt a blocking filesystem call.

Required invariant:

- potentially large staging/copy operations must not block Tokio worker threads;
- no output should be published to the workspace after the operation has timed out or been invalidated;
- publication should remain transactional/fail-closed.

Implementation should move blocking filesystem work to bounded blocking tasks and check the operation deadline before final workspace publication. Large output publication may require chunked/deadline-aware copying or a clearly defined post-execution commit budget.

### 7. Generic retry can replay non-idempotent operations

Current BrowserRuntime retries a command after a connectivity failure by restarting the browser runtime and sending the same command again. For state-changing operations, a connection failure after execution but before response creates ambiguous completion. Retrying can duplicate side effects. After a full browser restart, UID-based actions are also semantically stale.

Required invariant:

> MoonDesk must never automatically replay an operation whose execution may already have occurred.

On runtime loss during/after dispatch, invalidate/restart the runtime and return a session-lost error requiring the caller to take a fresh snapshot / re-establish page state. If automatic retry remains at all, it must be limited to a small explicit set of context-free, demonstrably idempotent inspection operations.

## Additional architectural findings

### Per-operation CLI process overhead

Even with a warm detached daemon, every browser operation currently launches a fresh `npx`/Node CLI process. Local measurements of repeated warm `list_pages` calls were roughly 1.4-1.9 seconds each. A directly owned MCP child removes this avoidable process-launch tax and should materially improve agent browser latency.

### `browser_runtime.rs` has accumulated too many responsibilities

The file now combines lifecycle, process execution, daemon ownership, deadline handling, path parsing, workspace security, staging, output publication, CLI error interpretation, PID discovery, platform details, and integration tests.

As part of or immediately after the transport refactor, prefer a structure such as:

```text
src/browser/
    mod.rs
    runtime.rs
    transport.rs
    contract.rs
    policy.rs
    staging.rs
```

The migration can remain incremental to keep the PR reviewable.

### MoonDesk needs an owned browser command contract

The MCP surface is stable at the tool-name level, but `browser_command` currently exposes the pinned v1.7 experimental CLI's command/argument vocabulary directly. v1.8 has already demonstrated that upstream command argument shapes can change.

MoonDesk should own a checked-in command registry for the supported pinned contract. It should define, per command:

- required positional arguments;
- optional argument names and value types;
- path input/output metadata;
- URL-bearing arguments;
- ReadOnly classification;
- whether an operation is context/session dependent;
- whether a command is safe to auto-retry (default false).

That registry should drive argument parsing/translation, ReadOnly policy, path staging, URL policy, help/validation, and future upstream migrations.

## Implementation record

### Phase A - replace detached CLI daemon transport ? completed

1. Introduce a lazy owned `BrowserTransport` that starts exact `chrome-devtools-mcp@1.7.0` as a direct stdio MCP server child with the current safe isolated server flags.
2. Own its full process tree using MoonDesk's existing process-ownership infrastructure; ensure host exit/drop cannot leave descendants alive.
3. Perform MCP initialize once per runtime generation and keep the child alive across browser operations.
4. Serialize browser operations at BrowserRuntime as today because selected page, snapshots, and UIDs are session-global.
5. On timeout/cancellation after dispatch, terminate the runtime tree, clear state, wait for teardown, and return timeout/session-lost without replay.
6. Remove daemon session ID/PID-file/start/status/stop logic once the new transport is proven.

### Phase B - command contract and policy ? completed for merge scope

1. Add a checked-in v1.7 command registry and parser that converts MoonDesk's existing `command + args[]` contract into upstream MCP tool `arguments`.
2. Preserve current CLI argument compatibility where practical so users do not need to change `moondesk browser` scripts.
3. Move path metadata from scattered match statements into the command contract.
4. Add URL metadata/policy and block unsafe local/internal schemes.
5. Keep ReadOnly fail-closed and derived from MoonDesk contract metadata, with explicit special handling where necessary (for example Lighthouse snapshot-only).

### Phase C - staging/output deadline hardening ? completed

1. Move potentially blocking staging/copy work off Tokio worker threads.
2. Ensure no workspace output is committed after timeout/runtime invalidation.
3. Keep symlink/reparse-point, traversal, and outside-workspace rejection behavior.
4. Preserve managed `view_page` temp output handling.

### Phase D - compatibility cleanup ? completed

1. Align `package.json`, README, CI, release docs, and tests with the real Node floor required by Browser mode.
2. Add a browser-runtime compatibility smoke for the minimum supported Node version so wrapper-only tests cannot hide future upstream engine changes.

## Required regressions before merge

At minimum:

- timeout after tool dispatch kills the owned runtime and proves the page cannot mutate afterward;
- a second request cannot enter while timeout cleanup is still terminating the previous runtime;
- abrupt parent/runtime teardown leaves no owned MCP/Chromium descendants;
- startup timeout/failure cannot leave an unrecoverable detached browser process;
- `file://` navigation outside the workspace is rejected before reaching Chromium;
- browser URL policy rejects unsafe local/internal schemes and preserves normal HTTP(S)/localhost navigation;
- no automatic replay of ambiguous state-changing commands after transport loss;
- workspace upload and file-producing command staging/copy-back remain green;
- outside-workspace/traversal/symlink/reparse-point path tests remain green;
- ReadOnly inspection policy remains green, including Lighthouse snapshot-only behavior;
- `view_page` still returns native bounded image content and cleans managed temp files;
- one runtime remains shared across MCP `browser_command`, MCP `view_page`, and `moondesk browser`;
- two MoonDesk hosts remain independent at the process/runtime level;
- minimum supported Node version can actually launch the pinned browser runtime;
- Windows and Linux Clippy/test matrices remain clean.

## Merge criterion

PR #42 is merge-ready only when MoonDesk, not the detached experimental upstream CLI daemon, is the authoritative lifecycle owner of the browser runtime; browser timeouts cannot leave late mutations; browser navigation cannot bypass workspace filesystem policy; the advertised Node/runtime contract is truthful; and the full real-browser regression matrix passes on the exact PR HEAD.

## Validation record

Validated locally on the final implementation state before push:

| Validation | Result |
| --- | --- |
| Windows Rust tests | **311 passed, 0 failed, 7 ignored** |
| Windows all-target Clippy (`-D warnings`) | **PASS** |
| Windows strict production Clippy (`unwrap` / `expect` / `panic` / `unreachable` denied) | **PASS** |
| Windows developer-tool smoke | **PASS** |
| Windows owned-browser lifecycle/recovery smoke | **PASS** |
| Windows dispatched-timeout late-mutation smoke | **PASS** |
| Windows host CLI + MCP shared-session / workspace staging smoke | **PASS** |
| Windows native `view_page` vision smoke | **PASS** |
| Linux stable Rust 1.98.0 format + both Clippy gates | **PASS** |
| Linux Rust tests | **309 passed, 0 failed** |
| Unix private staging permissions (`0700` root/dirs, `0600` staged file) | **PASS** |
| Node 20.19 npm tests + package verification | **59/59 + exact 7-file package** |
| Node 22.12 npm tests + package verification | **59/59 + exact 7-file package** |
| Node 24 npm tests + package verification | **59/59 + exact 7-file package** |
| Pinned `chrome-devtools-mcp@1.7.0 --help` on Node 20.19, Node 22.12, and Node 24 | **PASS** |

GitHub CI must still pass on the pushed PR HEAD before review threads are considered fully closed.
