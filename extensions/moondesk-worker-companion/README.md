# MoonDesk Worker Companion (Experimental)

This unpacked Chromium extension is the browser-side transport for MoonDesk Workers V1.

It intentionally owns only a small surface:

- automatically discover and pair with the local MoonDesk companion bridge on loopback ports 47650-47654;
- keep multiple browser installations paired independently instead of making one browser take over another;
- report only bounded routing presence for open ChatGPT conversations (exact conversation ID, optional exact Project ID, active/focused state);
- redeem durable managed-chat launch commands for the browser that owns the exact Core or durable worker thread;
- process up to four fresh worker launches concurrently while tracking failures independently; MoonDesk recommends 1-4 workers for normal use because higher totals can hit ChatGPT/provider rate limits, especially alongside other active conversations;
- open/recover a worker ChatGPT tab;
- publish tab closure quickly so MoonDesk can mark a running worker as `detached`/no-tab without treating browser disappearance as task completion; the same durable worker can still report/finish server-side and reattaches when its exact conversation page returns;
- create every fresh worker as an ordinary ChatGPT conversation, regardless of the Core's Project membership;
- select and read back the requested model + reasoning effort;
- insert the worker bootstrap and click Send once;
- reconcile ambiguous sends by looking for the durable task marker, never by blindly clicking Send again;
- never expose failed worker launches as delayed manual replay buttons; proven pre-Send fresh-launch failures automatically free their worker slot, while failed durable-worker wakes return that worker to idle;
- provide an explicit **Clear workers** reset for the current Core that removes idle/retired and proven pre-Send-failed MoonDesk worker history/bindings without deleting ChatGPT conversations, while refusing active or ambiguous work.

It does **not** match ChatGPT Projects, MoonDesk workspaces, connectors, or local folders by display name. Workspace authority remains the exact MoonDesk connector route / WorkspaceId. It also does **not** record transcripts, spawn nested workers, mirror agent state, or use the workspace MCP URL as an authentication credential.

## Distribution

MoonDesk distributes the Worker Companion as a local unpacked extension. Production setup enables browser Developer mode once and loads MoonDesk's stable companion folder with **Load unpacked**. GitHub Releases also include the exact checksummed `moondesk-worker-companion.zip` as a beta/recovery fallback. Distribution and onboarding requirements are tracked in [`docs/WORKER_COMPANION_DISTRIBUTION.md`](../../docs/WORKER_COMPANION_DISTRIBUTION.md).

## Local install

The steps below are for experimental/developer builds only.

1. Build/run the matching experimental MoonDesk branch.
2. Open `chrome://extensions` (or `edge://extensions`), enable Developer mode, and choose **Load unpacked**.
3. Select `extensions/moondesk-worker-companion`.
4. Open the extension popup. The extension should automatically discover and pair with the local MoonDesk companion bridge. Manual repair is only a fallback when this installation's stored credential can no longer be accepted.
5. Open the ChatGPT conversation you want to use as the Core. No Project/workspace binding step is required.
6. Click **Discover available ChatGPT models**, choose a confirmed model and reasoning effort, and save the worker profile. Discovery reads ChatGPT's provider-owned picker state and supports the current effort lanes (`Instant`, `Minimal`, `Low`, `Medium`, `High`, `Extra High`, `Max`, `Ultra`, and `Pro`) when the account offers them. Existing `Extra High` profiles remain compatible with the provider's `xhigh`/`max` migration.

Workers V1 deliberately keeps fresh placement simple: every fresh worker starts as an ordinary ChatGPT conversation, even when the Core is inside a ChatGPT Project. Core Project metadata is used only for exact Core routing/diagnostics; MoonDesk never clones the Core conversation as a worker bootstrap. Later tasks for the same durable worker reuse its exact confirmed worker conversation, including legacy worker conversations that were previously created inside a Project.

Multiple Chromium browser installations can remain paired at once. Fresh workers are routed to the browser that positively observes the exact Core conversation; once a worker thread exists, reuse stays pinned to the browser that owns that exact worker thread unless a safe recovery path is established. A running worker whose tab is closed remains running with `attachmentState: detached`; MoonDesk does not automatically declare it idle from silence alone because Workers V1 does not yet carry the turn/request-level provenance needed to distinguish a late old-turn call from a new assignment safely.

Workers remain experimental until the signed-in normal-chat connector/tool-context and multi-browser browser-E2E matrix pass.
