# Workers V1 — Generalized Routing & Multi-Browser Plan

Status: implementation plan after manual V1 testing on 2026-09-19.

Branch: `experimental/workers-v1`

Current baseline: `c649e9d fix(workers): harden companion browser lifecycle`

## Why this follow-up exists

The first Workers V1 E2E proved the durable worker broker, model/effort selection, safe browser send/reconciliation, and same-thread reuse for one paired browser and one manually bound ChatGPT Project.

Manual end-user testing exposed three product-level gaps that the controlled E2E did not challenge:

1. ChatGPT can retain an older MCP tool schema, so MoonDesk may advertise the `workers` tool while an existing ChatGPT conversation still cannot call it.
2. Worker placement currently requires a manual MoonDesk-workspace -> ChatGPT-Project binding even though the workspace is already authoritatively resolved by the MCP connector route. This wrongly makes ChatGPT Projects mandatory.
3. Companion authentication supports only one browser installation, so pairing Chrome prevents Edge from pairing.

This plan removes those assumptions without weakening the workspace/session isolation or duplicate-send safety already implemented.

## Non-negotiable invariants

1. **Never correlate by a human-readable name.**
   - ChatGPT Project display name is display-only.
   - ChatGPT Connector display name is display-only.
   - MoonDesk workspace name is display-only.
   - Local directory basename is display-only.
   - Similarity/fuzzy/name matching is forbidden for routing or authorization.

2. **Workspace authority remains server-side.**
   - The exact MoonDesk workspace comes from the workspace-specific MCP route and immutable `WorkspaceId`.
   - A path or workspace name included in a worker prompt is informational only.
   - The model never chooses or changes its authoritative workspace by writing a path/name in text.

3. **Anchor/worker authority remains exact-session based.**
   - Anchor and worker identities continue to derive from OpenAI session metadata and the route-resolved workspace.
   - A different conversation cannot become an Anchor or worker by claiming an ID in text.

4. **Projects become optional placement context, never authority.**
   - If the Anchor is in a ChatGPT Project, a new worker should preferably open in that exact Project ID.
   - If the Anchor is a normal ChatGPT conversation, a new worker should be a normal ChatGPT conversation.
   - No Project binding is required for either path.

5. **Multiple paired browsers are allowed, but one exact browser owns each in-flight send.**
   - Chrome, Edge, Brave, etc. may all remain authenticated concurrently.
   - A command lease is tied to one companion client.
   - A leased/ambiguous command is never concurrently handed to another browser.

6. **Never blind-resend after an ambiguous Send boundary.**
   - Existing `NeedsReconcile` semantics stay intact.
   - Browser failover is allowed only for proven pre-Send commands or exact, positively identified conversation recovery.

7. **Durable worker reuse means the same exact conversation.**
   - A reuse command must target the previously confirmed worker conversation.
   - Browser migration may occur only when another paired client positively observes that exact conversation.

8. **Persist the minimum necessary browser metadata.**
   - Do not persist chat titles, message text, Project names, connector names, or browsing history.
   - Presence is process-local unless durability is explicitly required.
   - Persist only the stable IDs needed for auth/routing/recovery.

## Phase 0 — Empirical contract checks before changing routing

These checks prevent us from baking an unverified ChatGPT implementation detail into MoonDesk.

### 0A. Prove Anchor session correlation

MoonDesk currently receives the Anchor as `_meta["openai/session"]` and hashes it into `ChatIdentity.session_digest`.

The extension independently sees the browser conversation ID from `/c/<conversation-id>`.

We must prove with a real signed-in ChatGPT conversation whether:

```
openai/session == URL conversation id
```

If yes:
- compare only a server-computed session digest; do not persist the raw Anchor session just for routing.
- extension reports `conversationId`; MoonDesk hashes it with the same session-digest function and matches exact digests.

If no:
- do **not** fall back to names.
- implement an explicit opaque correlation handshake before generalized routing proceeds.

Acceptance:
- automated digest-match unit tests;
- one normal chat and one Project chat verified live;
- mismatch path fails closed.

### 0B. Prove normal-chat connector availability

A worker cannot do local work merely because its bootstrap says `D:\project`. The new worker conversation must actually have the correct MoonDesk custom connector/tool surface.

Test:
- Anchor is a normal ChatGPT conversation using a MoonDesk workspace connector.
- Open a fresh normal ChatGPT conversation in the same browser.
- Determine whether the MoonDesk connector remains available/selected automatically.
- If not, inspect ChatGPT's current UI/runtime for a stable connector/plugin identifier that can be carried from the Anchor and selected in the worker chat.

Rules:
- connector **display name is never a routing key**.
- if ChatGPT exposes only a display name and no stable identifier, do not automate selection by text guessing; keep the feature gated until a safe mechanism exists.

Acceptance:
- worker fresh normal chat can call `workers claim` without manual connector setup.

### 0C. Connector schema refresh

Confirm after running the experimental host:
- live MCP `tools/list` contains `workers`;
- refreshing/reconnecting the Custom Connector and starting a fresh chat exposes `workers` to ChatGPT.

Add current `moondesk_instruction` guidance explaining:
- Workers require multi-tools mode;
- if MoonDesk says Workers are enabled but this chat lacks `workers`, refresh/reconnect the connector and start a new conversation because ChatGPT may retain an older schema.

## Phase 1 — Companion auth V2: real multi-browser pairing

### Current problem

`CompanionAuthState` stores one:

```
client_id
credential_hash
```

A second installation is intentionally rejected as a takeover.

### New persisted shape

Migrate to a bounded client registry, for example:

```text
schemaVersion: 2
clients:
  <clientId>:
    credentialHash
    extensionOrigin
    pairedAtMs
```

Constraints:
- maximum 8 paired clients by default;
- client IDs remain bounded and opaque;
- credential hashes only; never persist raw credentials;
- bind each credential to the authenticated `chrome-extension://<id>` Origin;
- preserve constant-time hash comparison;
- keep the existing auth file location and migrate schema V1 atomically so the existing Chrome installation is not silently logged out.

### Pairing semantics

Auto-pair:
- existing client + correct credential + same extension origin -> idempotent success;
- new client while capacity exists -> add client, do not revoke existing clients;
- same client + wrong credential -> reject;
- credential replay from a different extension origin -> reject;
- registry full -> actionable conflict requiring stale-client revocation.

Manual repair:
- repairs/upserts **this client only**;
- does not revoke other healthy browsers;
- manual token still rotates after use.

Add:
- revoke-current-client route/action;
- TUI/API ability to list paired client IDs/status safely and revoke stale clients;
- no raw credentials in UI/logs.

Tests:
- Chrome + Edge equivalent clients pair concurrently;
- restart preserves both;
- one client credential cannot authenticate as another;
- Origin mismatch rejected;
- V1 -> V2 migration;
- max-client cap;
- per-client repair/revoke does not affect peers.

## Phase 2 — Ephemeral browser/chat presence registry

Add an authenticated companion presence route and process-local registry.

Each paired client periodically reports a bounded snapshot of ChatGPT tabs containing only routing metadata:

```text
clientId        <- server derives from credential, never trusts body
browserFamily   <- display only
lastSeen
tabs:
  conversationId
  projectId?       <- exact g-p-... ID, optional
  projectUrl?      <- canonical navigation URL, optional
  active
  windowFocused
```

Do not report:
- chat titles;
- message text;
- Project names;
- connector names;
- unrelated page URLs.

The server derives `sessionDigest` from `conversationId` after Phase 0 proves the contract.

Presence is ephemeral:
- heartbeat timeout marks browser offline;
- no config write on every heartbeat;
- browser restart simply rebuilds presence.

Selection for a fresh Anchor command:
1. exact Anchor session-digest match;
2. if exactly one matching client -> use it;
3. if several match -> focused/active exact Anchor wins;
4. otherwise existing Anchor affinity wins;
5. otherwise fail with an actionable ambiguity instead of guessing.

No name participates.

Tests:
- same Project/name in two workspaces cannot cross-route;
- different names with exact IDs route correctly;
- stale presence is ignored;
- multiple browsers observing unrelated chats cannot steal an Anchor.

## Phase 3 — Durable managed-chat browser targeting

Current `redeem(client_id)` lets whichever single paired client exists take the command. Multi-browser requires explicit command ownership.

Extend managed-chat durability so a command has a target companion client.

Recommended model:

```text
ManagedChatCommand
  ...
  targetClientId
  terminalClientId?
```

Keep routing state separate from the immutable semantic launch payload where practical.

Rules:
- only `targetClientId` may redeem a queued/reconcile command;
- lease remains client-bound as today;
- ACK persists the successful executing client as `terminalClientId`;
- retargeting is allowed only before Send is proven possible (queued or explicit safe pre-Send failure);
- leased / NeedsReconcile commands cannot be moved merely because another browser is online.

For `thread_key = worker:<workerId>`:
- the first successful new-thread command establishes thread -> browser client affinity;
- an existing-thread reuse defaults to the last confirmed successful client for that thread.

Tests:
- wrong client cannot redeem;
- simultaneous Chrome/Edge polls lease once;
- restart preserves target/terminal client;
- duplicate retry does not produce cross-browser double send.

## Phase 4 — Remove mandatory Project binding; add automatic placement

Delete the normal-flow dependency on:

```
workspaceId -> projectId binding
```

and remove `workspace_not_bound` as a prerequisite for worker spawn.

### Fresh worker placement

Resolved from the exact Anchor presence:

```text
Anchor in Project:
  placement.kind = project
  placement.projectId = exact g-p-...
  placement.projectUrl = canonical observed project entry URL

Anchor in normal chat:
  placement.kind = normal
```

No workspace name/Project name comparison.

### Extension behavior

For `new_thread + project`:
- navigate to the exact stored Project entry;
- positively verify exact `projectId`;
- select/read back model + effort;
- prepare/send once.

For `new_thread + normal`:
- navigate to a fresh normal ChatGPT composer;
- positively verify there is no existing conversation and no Project context;
- ensure the correct MoonDesk connector/tool context from Phase 0B;
- select/read back model + effort;
- prepare/send once.

For `existing_thread`:
- ignore new-thread placement;
- open the exact confirmed worker conversation;
- verify conversation identity before typing.

Remove the normal popup's **Bind this Project** workflow.

Tests:
- Project names/workspace names/connector names all intentionally different;
- normal Anchor -> normal worker;
- Project Anchor -> exact same Project ID;
- two Projects with identical visible names do not cross;
- old manual binding data is ignored/migrated without breaking extension startup.

## Phase 5 — Preserve connector/tool context for normal workers

This phase is conditional on Phase 0B findings.

Goal:
- a fresh worker conversation must expose the exact workspace connector needed for `workers claim/start` and local tools.

Preferred order:
1. inherit existing ChatGPT behavior if it is stable and positively verified;
2. otherwise capture a stable ChatGPT connector/plugin identifier from the Anchor and select that exact identifier in the new worker UI;
3. never choose a connector based on visible/fuzzy name.

Before Send, fail closed if the required connector/tool context cannot be positively confirmed.

Add a specific pre-Send failure such as:

```
connector_context_unconfirmed
```

which is safe to retry after the user refreshes/repairs the ChatGPT connector.

## Phase 6 — Multi-browser recovery and safe failover

### Fresh commands

If target browser disappears before lease/Send:
- if command is still safely pre-Send, router may choose another exact Anchor-matching online client;
- recompute placement from that exact client's Anchor observation;
- persist retarget before lease.

### Durable worker reuse

Normal behavior:
- worker remains pinned to the browser client that owns its confirmed conversation.

If that browser goes offline:
- command waits; do not create a replacement worker.

Safe migration is allowed only if another paired client reports the **exact same worker conversation ID** and compatible ChatGPT context.

Then:
- atomically rebind the thread to that client;
- continue existing-thread reuse there.

Never:
- infer migration from Project name;
- infer migration from workspace name;
- open a fresh chat and pretend it is the same worker.

Tests:
- close Edge before Send -> Chrome safe takeover only when exact Anchor is observed there;
- close Edge after ambiguous Send -> no Chrome resend;
- exact worker conversation opened in Chrome -> safe reuse migration;
- no exact worker conversation -> wait/fail actionable.

## Phase 7 — Companion popup/TUI UX redesign

Companion popup becomes status/control, not setup ceremony.

Example:

```text
MoonDesk Workers
Connected

This browser
Edge · connected

Current chat
Normal conversation
or
Project: <short exact ID>

Routing
This chat is visible to MoonDesk

Worker profile
GPT-5.6 Sol · High
```

Optional advanced section:
- paired browsers;
- revoke this browser;
- stale client repair;
- model rediscovery;
- blocked/reconciliation command status.

Do not show **Bind this Project** in the normal flow.

MoonDesk TUI Settings:
- paired companion count;
- online companion count;
- worker profile;
- stale-client/revoke controls if practical;
- connector-refresh hint after upgrades that change MCP tool surface.

## Phase 8 — Regression, compatibility and manual acceptance matrix

Automated gates:
- JS syntax checks;
- companion JS tests;
- Rust fmt;
- Clippy all targets;
- strict binary Clippy;
- full Rust tests;
- git diff --check;
- secrets scan if repository provides one.

Browser E2E matrix:

### Placement
- normal Anchor / normal worker;
- Project Anchor / same exact Project ID worker;
- completely unrelated human-readable names;
- two Projects with same visible name;
- two workspaces with similar names;
- one workspace used from different ChatGPT Projects.

### Browsers
- Chrome only;
- Edge only;
- Chrome + Edge paired concurrently;
- Anchor in Chrome -> worker Chrome;
- Anchor in Edge -> worker Edge;
- same Anchor open in both -> deterministic focus/affinity rule;
- one browser closes before Send;
- one browser closes after possible Send.

### Durability
- extension reload;
- MoonDesk restart;
- browser restart;
- same-thread reuse;
- no duplicate prompt after restart;
- safe pre-Send retry;
- ambiguous reconciliation never blind-sends.

### Tool context
- fresh worker can claim;
- worker can call `moondesk_instruction`;
- exact workspace is enforced by connector route;
- another workspace/Anchor cannot claim or message the worker.

### Schema refresh
- upgraded MoonDesk exposes `workers`;
- old cached chat gets actionable `moondesk_instruction` guidance;
- refreshed connector + fresh conversation sees `workers`.

## Proposed implementation slices / commits

Keep each slice reviewable and independently testable.

1. **A — Capability discovery + empirical probes**
   - instruction/schema-refresh guidance;
   - diagnostic helpers/tests for session/conversation correlation;
   - normal-chat connector inheritance experiment.

2. **B — Multi-client companion auth**
   - auth schema V2 migration;
   - per-client Origin-bound credentials;
   - list/revoke/status;
   - Edge + Chrome can pair simultaneously.

3. **C — Browser presence + exact Anchor routing**
   - presence endpoint/registry;
   - bounded tab observations;
   - exact session-digest resolver;
   - no names.

4. **D — Managed-chat client targeting**
   - target/terminal client durability;
   - client-filtered redeem;
   - thread/browser affinity;
   - safe retarget rules.

5. **E — Automatic Project/normal placement**
   - placement payload;
   - remove binding requirement/UI;
   - normal chat support;
   - exact Project ID verification.

6. **F — Connector context preservation**
   - only after Phase 0B establishes the correct provider mechanism;
   - exact stable connector identity, never visible-name matching.

7. **G — Cross-browser recovery**
   - offline handling;
   - safe pre-Send failover;
   - exact-conversation worker migration.

8. **H — UX/docs/full regression**
   - popup/TUI cleanup;
   - stale schema guidance;
   - whole-workspace gates;
   - Chrome + Edge manual matrix.

## Stop/go criteria

Do not call generalized Workers complete until all are true:

- `workers` is visible in a refreshed ChatGPT connector conversation;
- Chrome and Edge can remain paired simultaneously;
- a normal non-Project Anchor can spawn a functional worker;
- a Project Anchor automatically places a worker in the same exact Project ID;
- no manual Project binding is required;
- visible names can all differ without affecting routing;
- same-thread reuse remains exact;
- browser loss cannot cause a duplicate Send;
- connector/tool context is positively confirmed in a normal fresh worker;
- full automated gates and multi-browser E2E pass.
