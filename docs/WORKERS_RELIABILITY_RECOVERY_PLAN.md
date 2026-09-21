# Workers Reliability Recovery Plan

## Goal

A worker is not considered launched until MoonDesk can trace one durable lifecycle from Anchor spawn through ChatGPT send acceptance, worker claim, task finish, and Anchor collection.

The implementation must remain correct across:
- ChatGPT SPA/full-document navigation during first send;
- MoonDesk restart;
- extension service-worker restart/reload;
- browser tab closure;
- stale/revoked companion installations;
- 1-8 workers, with 4 recommended;
- ambiguous send boundaries without duplicate sends.

## Root causes found in the audit

1. WorkerBroker and ManagedChatBroker persist separate state with no lifecycle join or compensating rollback.
2. Extension launchRecords/threadRecords/blockedCommands act as a third source of truth.
3. ManagedChat `Leased` does not distinguish pre-Send work from a crossed Send boundary.
4. The content script clicks Send and waits for post-navigation DOM evidence in the same runtime message. Navigation can destroy that message channel.
5. Send success is primarily inferred from the task marker appearing in a rendered user message.
6. Worker status does not expose browser launch progress/failure; `Provisioning + Absent` is ambiguous.
7. Companion revocation does not settle commands pinned to that browser.
8. Existing tests do not exercise the whole spawn -> browser navigation -> claim -> finish -> collect lifecycle.

## Target architecture

### A. Server is authoritative

Extension local state is only browser recovery metadata. It never decides whether a launch is globally retryable/blocked.

ManagedChat command owns browser-delivery state. Worker record owns worker/task state. They are durably linked by command ID / worker ID / task ID.

### B. Explicit managed-chat send boundary

Use explicit command phases:

- `Queued`: safe to dispatch/re-dispatch.
- `Leased`: browser is preparing placement/model/prompt; Send has NOT been crossed. Lease expiry returns to Queued.
- `SendStarted`: server durably recorded that the extension is about to commit Send. Lease expiry becomes NeedsReconcile.
- `NeedsReconcile`: possible Send; automatic fresh Send forbidden.
- `Paused`: ambiguous reconciliation intentionally stopped; not auto-redeemable.
- `Succeeded`: ChatGPT conversation/send acceptance confirmed.
- `Failed`: proven terminal pre-Send/invalid launch.

No overloading `Failed + reconcileHistory` to mean paused ambiguity.

### C. Split prepare from commit

Content script API becomes:

1. `PREPARE_WORKER`
   - confirm placement;
   - select/read back model + effort;
   - insert and verify exact prompt;
   - DO NOT click Send;
   - return ready metadata.

2. Background calls server `send_started` endpoint while lease is active.

3. `COMMIT_WORKER_SEND`
   - validate the exact prepared marker/prompt is still present;
   - respond to background BEFORE navigation can destroy the message port;
   - schedule exactly one Send click;
   - never wait for post-Send DOM evidence in this RPC.

4. Background monitors the tab independently after the click:
   - recover same tab by tab ID/launch token;
   - survive route/document replacement;
   - wait for a canonical conversation URL;
   - reinject content after navigation as needed;
   - confirm acceptance using multiple independent signals.

### D. Acceptance evidence

For a new thread, success requires a canonical conversation URL plus at least one acceptance signal:
- task marker visible in a user turn; OR
- generation started after commit; OR
- composer cleared and user-turn count increased.

Before commit capture:
- source URL / starting conversation ID;
- user-turn count;
- exact prompt fingerprint.

For an existing thread, require same canonical conversation URL and a new-turn/generation/marker signal.

A route transition alone is not enough; a task marker alone on an unrelated conversation is not enough.

### E. Durable worker launch state

Add a launch lifecycle to the worker/task record:
- queued
- preparing
- send_started
- waiting_claim
- paused
- failed
- claimed

Also persist:
- launch command ID;
- confirmed conversation URL/ID when known;
- last launch error/reason.

`workers.status` exposes these fields.

Managed-chat transitions update the linked worker launch state.

### F. Spawn compensation

Spawn must never leave an unlinked ghost.

After WorkerBroker creates the pending worker:
- if managed-chat enqueue fails, compensate immediately by retiring/aborting that pending worker;
- once enqueue succeeds, persist the command ID link in the worker record;
- idempotent spawn retry returns the same linked lifecycle.

### G. Restart/reload semantics

- Leased before send boundary -> safe Queue again after lease expiry/restart.
- SendStarted -> NeedsReconcile after lease expiry.
- NeedsReconcile/Paused never creates a new thread.
- Extension reload does not authorize old reconciliation automatically.
- Explicit user Retry moves Paused -> NeedsReconcile only.
- Browser-local state loss cannot create a fresh thread for an ambiguous command.

### H. Companion installation lifecycle

Revoking a stale browser:
1. revoke its auth first;
2. then settle commands pinned to it:
   - Queued/pre-Send -> clear target and allow exact-anchor failover;
   - SendStarted/NeedsReconcile/leased-after-boundary -> Paused;
   - terminal commands unchanged.
3. update linked worker launch state.
4. current browser cannot revoke itself.

### I. End-to-end tests required before release

Rust integration lifecycle:
1. spawn
2. command queued + linked
3. redeem/preparing
4. send_started
5. accepted with conversation
6. worker waiting_claim
7. child claim
8. child finish/report
9. Anchor collect
10. worker idle

Recovery matrix:
- restart before send boundary -> safe redispatch
- restart after send boundary -> reconcile-only
- extension reload while paused -> zero tabs
- stale browser revoke -> safe settlement
- enqueue failure -> no ghost worker
- reconciliation missing local record -> zero tabs
- first-send navigation destroys old document -> background still confirms new conversation

Concurrency:
- 4 fresh workers start independently
- one launch failure does not block the other 3
- 8 active worker capacity is enforced
- worker 9 is rejected without partial state

## Release acceptance

Do not call the feature complete based on unit tests.

Acceptance order:
1. one real worker: launch -> claim -> finish -> collect
2. restart/reload and confirm zero unsolicited tabs
3. four real workers concurrently: all four claim/finish/collect
4. one induced launch failure while three succeed
5. eight-worker capacity run

Only after step 3 succeeds is generalized multi-worker spawning considered working.
