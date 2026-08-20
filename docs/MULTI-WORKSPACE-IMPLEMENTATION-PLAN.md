# MoonDesk Multi-Workspace Host — Implementation Plan

Status: source-of-truth implementation plan for `feat/multi-workspace-host`

## 1. Goal

Allow one MoonDesk process to serve multiple project roots concurrently so different ChatGPT chats can work on different projects at the same time.

Target architecture:

```text
One MoonDesk process
├── one Axum server on one local port
├── one ngrok tunnel / public domain
├── one shared browser + DevTools bridge
└── many workspace endpoints
    ├── /<secret-A>/mcp -> Workspace A root
    ├── /<secret-B>/mcp -> Workspace B root
    └── /<secret-C>/mcp -> Workspace C root
```

Each workspace receives a stable internal ID and its own secret MCP slug. ChatGPT is configured with one MoonDesk app/connector per workspace. The endpoint itself selects the workspace; tool schemas do not gain a `workspace` argument.

## 2. Primary optimization boundary

The feature must preserve the optimization work that keeps ChatGPT conversations lightweight.

The main performance constraint is **ChatGPT-side conversation/tool state**, not local PC RAM at any cost.

Therefore:

- Do not send workspace registries, other workspace names, logs, command history, job lists, URLs, or routing metadata to ChatGPT.
- Do not add a `workspace` property to every MCP tool.
- Do not add per-workspace UI metadata to normal MCP responses.
- Keep local command output bounded exactly as today, but do not divide one workspace's previous local resource allowance among all workspaces.
- Local PC-side state may scale with the number of active workspaces where preserving existing per-project capability requires it.

### Compatibility invariant

Multi-workspace support must not reduce the effective capabilities or normal resource allowance that one workspace had before this feature.

Current effective allowances that must remain available **per workspace** unless separately changed by a future intentional feature:

- up to 8 active background jobs
- up to 64 retained jobs
- up to 4 MiB decoded live output per background job
- up to 32 MiB decoded terminal-output retention budget
- up to 64 MiB archived output per command
- up to 128 KiB returned by one poll/read-output call
- terminal job retention for up to 1 hour
- up to 500 local TUI log entries
- up to 300 local command-activity entries

These are local MoonDesk resources and must not be divided globally simply because one process now hosts multiple workspaces.

## 3. Resource ownership model

### Host-global resources

Only resources that are naturally singleton infrastructure should remain shared:

- Tokio runtime
- Axum HTTP server
- local port
- ngrok session/tunnel/domain/token
- TUI renderer
- theme
- ClippyMoon
- selected browser
- remote-debugging browser instance
- chrome-devtools-mcp child / DevTools bridge
- config file writer
- stale-output-root cleanup mechanism

### Workspace-scoped resources

Each workspace owns or receives an independent quota/state partition for:

- stable workspace ID
- display name
- canonical root
- secret MCP slug
- availability state
- in-flight request count / request leases
- remote connection/activity status
- flow/bootstrap status
- request count
- logs (bounded per workspace)
- command activity history (bounded per workspace)
- active background-job quota
- retained-job quota
- decoded terminal-output quota
- retry/idempotency state
- command job ownership
- command output ownership

### Shared implementation, partitioned state

A resource may use one implementation object while still enforcing per-workspace limits. In particular, `CommandJobManager` should remain one shared manager implementation but store workspace ownership and apply quotas per workspace.

This avoids duplicating cleanup/process-management code while preventing one workspace from consuming another workspace's normal allowance.

## 4. Workspace identity model

Persist two different identities:

```text
WorkspaceConfig
├── id        stable internal identity
├── name      local human-readable label
├── root      canonical absolute project root
└── mcp_slug  rotatable secret credential
```

The slug must never be used as the permanent workspace identity because rotating a leaked secret must not orphan jobs, UI state, metrics, or history.

Suggested persisted shape:

```toml
configVersion = 2
ngrokDomain = "example.ngrok-free.dev"

[[workspaces]]
id = "<uuid>"
name = "MoonDesk"
root = "D:\\CatDesk"
mcpSlug = "<random-secret>"
```

A practical hard maximum such as 32 registered workspaces is acceptable as a config-abuse/UX guard, but it must not reduce per-workspace operational quotas.

## 5. Backward compatibility and migration

Current MoonDesk has one persisted `mcpSlug`, while the active workspace root comes from:

1. `WORKSPACE_ROOT`, if explicitly set
2. otherwise the process current directory

Migration must preserve the existing connector URL.

On first config-v2 migration:

1. Resolve the legacy effective workspace root using the exact old rule.
2. Canonicalize and validate it.
3. Create the first `WorkspaceConfig`.
4. Reuse the existing `mcpSlug` unchanged.
5. Generate a stable workspace ID.
6. Derive a sensible local display name from the directory leaf, with a deterministic fallback.
7. Persist atomically.

Result: an already configured ChatGPT connector keeps using the exact same URL after upgrade.

After a v2 workspace registry exists, process CWD must no longer silently repoint an existing connector. `WORKSPACE_ROOT` may remain relevant only for legacy migration/first-workspace bootstrap unless explicitly redesigned later.

## 6. Workspace root validation

When adding or loading a workspace:

- require a directory path
- canonicalize it
- normalize Windows verbatim path forms
- keep the canonical absolute root
- reject duplicate canonical roots
- reject duplicate workspace IDs
- reject duplicate MCP slugs
- validate slug format and non-empty name
- reject parent/child overlapping workspace roots for V1

Example rejected overlap:

```text
D:\Projects
D:\Projects\KUBA
```

This prevents two supposedly independent connectors from having overlapping dedicated-file-tool authority.

Missing roots at later startup must not be automatically deleted. A workspace on an unplugged drive or unavailable mount should remain registered and show `unavailable`. Requests should fail clearly until the root becomes available again.

Do not create one filesystem watcher per workspace. Validate availability on startup/UI refresh/request boundaries as needed.

## 7. HTTP routing model

Axum already routes `/{slug}/mcp`. Preserve that route shape.

Replace single-secret authorization with bounded registry resolution:

```text
slug
  -> resolve workspace by secret
  -> obtain WorkspaceRequestContext
  -> execute MCP request against that root
```

Suggested immutable request context:

```text
WorkspaceRequestContext
├── workspace_id
├── name/reference for local observability
├── root
└── request lease
```

Secret comparison should retain constant-time behavior. Because the registry is bounded, comparing against all configured slugs is acceptable. Do not leak whether a slug almost matched or which workspace exists; unknown/disabled secrets return the same 404-style response.

Do not rebuild the Axum router when workspaces are added or removed.

## 8. Request leases and workspace removal safety

Removing a workspace must be safe under concurrent ChatGPT activity.

A valid request acquires a lightweight lease before beginning work. Removal follows:

1. mark workspace disabled/revoked
2. reject all new requests for its slug immediately
3. cancel background jobs owned by that workspace
4. allow already-running foreground/file operations to complete
5. wait until its in-flight request count reaches zero
6. purge retained command/output state belonging to that workspace as appropriate
7. remove the workspace from persisted config

Do not abort a file write halfway through solely because the workspace was removed from the UI.

Workspace mutation operations (add/remove/rotate) are security-sensitive and should be transactional with persistence: do not publish an in-memory credential change that failed to persist.

## 9. CommandJobManager changes

Keep one manager implementation, but attach `workspace_id` to every owned object/state entry.

APIs should become workspace-aware conceptually:

```text
start(workspace_id, ...)
poll(workspace_id, job_id, ...)
cancel(workspace_id, job_id)
read_output(workspace_id, output_id, ...)
cancel_workspace(workspace_id)
```

Ownership mismatch should behave like an unknown/expired ID rather than reveal cross-workspace existence.

### Per-workspace quotas

Apply existing limits independently for each workspace:

- 8 active jobs/workspace
- 64 retained jobs/workspace
- 32 MiB terminal decoded-output budget/workspace

The existing per-job/per-call limits stay unchanged.

### Retry/idempotency

Include workspace identity in retry/dedupe ownership. Reused JSON-RPC IDs in different workspaces must never interact.

### One-shot `run_command` archives

`output_id` entries must also record workspace ownership. `read_command_output` from another workspace must reject the ID.

### Cleanup

Cleanup should prune per-workspace retained jobs/output according to current semantics without evicting Workspace A simply because Workspace B is busy.

A future very-high host emergency ceiling may be considered separately, but it is not a replacement for the per-workspace compatibility invariant and is not required for V1.

## 10. Filesystem isolation semantics

Preserve the existing hardened dedicated-tool containment code in `command.rs` and `workspace_tools.rs` as much as possible.

Dedicated file tools must continue rejecting:

- `..` traversal
- absolute paths outside the selected workspace
- symlink/junction escapes
- nonexistent-leaf paths whose nearest existing ancestor escapes the root

Do not weaken canonicalization for convenience.

### Explicit shell limitation

`run_command` / `start_command` execute a real shell with the workspace as CWD. CWD is not an OS sandbox. A deliberately written command can still access another absolute path on the machine. This is pre-existing behavior and should remain documented rather than falsely presented as filesystem isolation.

OS-level containment would require a separate VM/container/sandbox project.

## 11. MCP payload compatibility

The ChatGPT-facing contract should stay effectively identical to current MoonDesk.

No normal tool should receive or return:

- all-workspace lists
- workspace routing metadata
- other workspaces' status
- local logs/history
- secret URLs
- job lists

Normal tool schemas should not gain a workspace parameter.

The selected MCP endpoint is the workspace authority boundary.

Existing bounded read/search/poll/output behavior must remain unchanged unless a separate bug is discovered and intentionally fixed.

## 12. HTTP request/body limits

Multi-workspace work must **not reduce any existing tool limit** as part of this feature.

There is currently no equivalent existing 4 MiB MCP POST-body cap to preserve; the 4 MiB value in the code is command-output retention. Do not conflate the two.

For this feature:

- do not introduce a tighter body limit that can regress current calls
- if an explicit defensive HTTP body limit is later added, choose it only after testing all current local and DevTools tool payloads and keep it comfortably above valid usage
- a value such as 16–32 MiB may be evaluated separately, but is not required for the core multi-workspace implementation

## 13. Local observability and TUI state

Move logs, command activities, flows, connection status, and request count under workspace runtime state so one project's activity does not evict another project's normal history allowance.

Per workspace retain current local history capability:

- max 500 logs
- max 300 command activities

Store workspace IDs in events/history and resolve current display names locally so renaming a workspace does not duplicate old names in memory.

Example command panel:

```text
[SITEAI]
Succeeded · npm run build

[KUBA]
Running · npm run build
```

## 14. UI event transport hardening

The current server-to-TUI transport is an unbounded channel. Multi-chat concurrency increases the chance that transient observability events can accumulate faster than the TUI consumes them.

This queue is not authoritative command/job storage and should not be allowed to grow without bound.

Replace or wrap it with a bounded/non-blocking observability sink, but preserve meaningful state:

- MCP execution must never block waiting for the TUI
- command output/jobs remain authoritative in their managers
- repetitive/intermediate visual updates may be coalesced/dropped under pressure
- terminal/final state should be recoverable from authoritative state
- optionally expose a local dropped/coalesced UI-event counter

Do not use queue bounding as an excuse to reduce per-workspace command/job/history retention.

## 15. Flow and connection isolation

Current flow identity is globally `stateless`. That is insufficient for multiple connectors.

Use workspace-scoped flow identities/state. A DELETE/disconnect from Workspace A must not mark Workspace B disconnected or close B's flow.

Top-level UI can derive aggregate state such as `2 connected`, but the authoritative connection/activity status remains per workspace.

## 16. Browser / DevTools behavior

Browser control remains host-global for V1.

All workspace connectors in Browser/Both mode expose the same selected browser/DevTools bridge.

Do not create one Chromium profile/process or one `chrome-devtools-mcp` child per workspace.

### Initialization

Multiple connectors may independently send MCP `initialize`. The global DevTools bridge must initialize idempotently so repeated workspace handshakes do not repeatedly initialize or destabilize the same child process.

### Shutdown hardening

Make DevTools lifecycle explicit. Do not rely only on dropped stdio or an unused child field.

Host shutdown order should explicitly handle:

1. stop accepting new work
2. cancel all workspace command jobs/process trees
3. stop DevTools bridge/child
4. stop ngrok and await/abort owned task cleanly
5. stop MoonDesk-launched browser child if applicable
6. clean service handles/state
7. flush config

## 17. ngrok behavior

Keep one tunnel. It already forwards request paths to the local Axum server, so all workspace endpoints naturally share the same public host.

No additional tunnel, domain, local port, or proxy layer is required.

Changing the ngrok domain is a global operation because it changes the public host for every workspace URL. Workspace secret slugs remain independent.

Do not log raw secret paths in routine request logs. Prefer local labels/IDs, e.g. `workspace=SiteAI`, while keeping secret URL reveal/copy behind the existing masked/reveal UX.

## 18. TUI workspace manager

Add a dedicated workspace management screen, accessible from the main TUI (proposed key: `w`).

Capabilities:

- list registered workspaces
- show root and availability
- add workspace
- rename workspace
- show details
- show recommended ChatGPT app name such as `MoonDesk · SiteAI`
- masked MCP URL
- reuse existing 10-second reveal/copy UX
- rotate only the selected workspace secret
- remove workspace safely

The selected row is UI-only. There is no global "active workspace" that changes request routing.

Move the current single global slug controls out of Settings into per-workspace details/actions. Keep ngrok domain global in Settings.

## 19. Second-instance behavior

One running MoonDesk host should own the normal local port.

If a second `moondesk` process is launched and port 3200 is already occupied:

- probe the local health endpoint when feasible
- if MoonDesk is detected, show a clear message that the host is already running and the folder should be added from the workspace manager
- do not silently start a second tunnel/process on another port as the default architecture

No daemon/IPC registration mechanism is required for V1.

## 20. Config corruption / fail-closed rules

On load, fail clearly rather than choose ambiguous entries if config contains:

- duplicate workspace IDs
- duplicate MCP slugs
- duplicate roots
- overlapping roots
- malformed IDs/slugs
- empty/invalid names
- too many registered workspaces

Security-sensitive ambiguity must not resolve by "first match wins".

Existing atomic config writing must remain intact.

## 21. Implementation phases

### P0 — Branch baseline and architecture types

- freeze known-good baseline
- add `src/workspaces.rs`
- define workspace ID/config/runtime/request-context types
- define registry validation helpers
- no externally visible routing change

Acceptance:

- existing tests still pass
- no MCP response/schema change

### P1 — Config V2 and legacy migration

- add `configVersion`/workspace registry persistence
- migrate current single slug + effective legacy root into Workspace #1
- preserve old slug exactly
- preserve all unrelated settings and usage data
- add migration/round-trip/corruption tests

Acceptance:

- existing connector URL is unchanged after migration
- migration is atomic and idempotent

### P2 — Multi-workspace slug routing

- resolve slug -> workspace request context
- route dedicated MCP operations using that root
- keep unknown/disabled slugs indistinguishable
- no tool schema changes

Acceptance:

- two secrets can concurrently read/write their own roots
- dedicated file tools cannot cross roots

### P3 — Workspace-partitioned command jobs/output

- add workspace ownership to jobs, run outputs, dedupe state
- enforce existing job/output quotas per workspace
- add `cancel_workspace`
- keep existing per-job/per-call limits

Acceptance:

- Workspace B cannot poll/read/cancel Workspace A IDs
- A's heavy job history does not consume B's 8/64/32-MiB allowance

### P4 — Workspace lifecycle and transactional mutations

- add/rename/rotate/remove backend operations
- add request leases / revoke-and-drain semantics
- support unavailable roots without deleting registration
- persist security mutations transactionally

Acceptance:

- remove/rotate under concurrent requests has deterministic safe behavior

### P5 — Per-workspace local observability

- partition logs, command history, flows, connection/activity state
- attach workspace ID to UI events
- remove raw secret paths from routine logs
- preserve 500 logs + 300 command entries per workspace

Acceptance:

- one workspace's activity does not evict another's normal local history

### P6 — Bounded/coalesced UI event transport

- replace unbounded transient observability queue with bounded/non-blocking design
- ensure MCP execution never waits for TUI rendering
- ensure final command state remains recoverable/accurate

Acceptance:

- stress traffic cannot create unbounded UI-event memory growth
- no command/job/output data loss

### P7 — Workspace manager TUI

- add `w` workspace screen
- add/list/rename/details/reveal/rotate/remove
- show availability and connection state
- keep URL masking/reveal protections
- replace single-workspace dashboard fields with appropriate summary/details

Acceptance:

- workspace management is usable without changing routing through UI selection

### P8 — Shared browser/ngrok/service lifecycle hardening

- idempotent DevTools initialization
- explicit DevTools shutdown
- aggregate/per-workspace remote status correctness
- clear second-instance detection/message
- keep one ngrok tunnel and one browser bridge

Acceptance:

- repeated connector initialization is safe
- all child processes terminate on shutdown
- one workspace disconnect does not affect another

### P9 — Full concurrency/security/regression suite

Required tests include:

- A+B simultaneous reads
- A+B simultaneous writes
- same JSON-RPC ID used independently in A and B
- A polling B job rejected
- A reading B output ID rejected
- A cancelling B job rejected
- per-workspace 8-active-job limit independence
- per-workspace 64-retained-job independence
- per-workspace 32-MiB terminal-output budget independence
- secret rotation affects only selected workspace
- workspace DELETE/disconnect isolation
- removal during active background job
- removal while foreground request already holds a lease
- new request after revocation rejected
- unavailable workspace retained cleanly
- symlink/junction escape rejection
- duplicate/overlapping roots rejected
- duplicate secret rejected
- repeated DevTools initialize only initializes underlying bridge once
- ngrok restart preserves all workspace routes
- bounded UI-event stress test
- multi-workspace shutdown terminates all process trees
- legacy config migration keeps old endpoint URL

Run full current CI validation:

- `cargo fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- strict production Clippy deny list
- `cargo test --locked`
- `npm pack --dry-run`

### P10 — Documentation and release migration notes

Update README/usage docs to explain:

- first/legacy workspace migration
- add more projects through Workspaces UI
- one ChatGPT app/connector per workspace
- one process / port / ngrok domain
- browser is shared
- secret rotation implications
- unavailable and overlapping-root behavior
- shell is not an OS sandbox
- `WORKSPACE_ROOT` post-migration semantics

## 22. Existing areas to preserve carefully

Avoid unnecessary rewrites of hardened code:

- `src/command.rs`
- `src/workspace_tools.rs`
- `src/process_runner.rs`

They currently contain important traversal, symlink/junction, output, timeout, and process-tree safeguards.

Primary files expected to change:

- `src/workspaces.rs` (new)
- `src/state.rs`
- `src/server.rs`
- `src/mcp.rs`
- `src/command_jobs.rs`
- `src/main.rs`
- `src/devtools.rs`
- `src/ngrok.rs`
- `README.md`
- tests/fixtures as required

## 23. Explicit non-goals for V1

Do not implement:

- multiple MoonDesk host processes as the normal design
- multiple local MCP ports
- multiple ngrok tunnels
- one browser/DevTools child per workspace
- workspace argument on every MCP tool
- a global active-workspace switch
- a daemon/broker/IPC architecture
- automatic filesystem watchers per workspace
- broad parent-directory workspace as a routing workaround
- automatic repointing of a persisted secret based on launch CWD
- OS-level shell sandboxing

## 24. Definition of done

The feature is complete when multiple ChatGPT chats can concurrently use separate workspace-specific MoonDesk endpoints through one running MoonDesk process, with:

- deterministic endpoint-to-root routing
- unchanged lightweight ChatGPT-facing tool contract
- preserved existing per-workspace local resource capability
- strict dedicated-file-tool containment
- workspace-owned job/output access
- safe add/remove/rotate lifecycle
- isolated local observability
- bounded transient UI-event memory growth
- one ngrok tunnel and one browser bridge
- backward-compatible legacy connector migration
- clean multi-workspace shutdown
- full existing and new regression tests passing
