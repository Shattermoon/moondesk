# MoonDesk Worker Companion (Experimental)

This unpacked Chromium extension is the browser-side transport for MoonDesk Workers V1.

It intentionally owns only a small surface:

- automatically discover and pair with the local MoonDesk companion bridge on loopback ports 47650-47654;
- keep multiple browser installations paired independently instead of making one browser take over another;
- report only bounded routing presence for open ChatGPT conversations (exact conversation ID, optional exact Project ID, active/focused state);
- redeem durable managed-chat launch commands for the browser that owns the exact Anchor or durable worker thread;
- process up to four fresh worker launches concurrently while tracking failures independently; MoonDesk recommends 1-4 workers for normal use because higher totals can hit ChatGPT/provider rate limits, especially alongside other active conversations;
- open/recover a worker ChatGPT tab;
- preserve the Anchor's exact ChatGPT Project when one exists, while supporting ordinary non-Project chats;
- select and read back the requested model + reasoning effort;
- insert the worker bootstrap and click Send once;
- reconcile ambiguous sends by looking for the durable task marker, never by blindly clicking Send again.

It does **not** match ChatGPT Projects, MoonDesk workspaces, connectors, or local folders by display name. Workspace authority remains the exact MoonDesk connector route / WorkspaceId. It also does **not** record transcripts, spawn nested workers, mirror agent state, or use the workspace MCP URL as an authentication credential.

## Local install

1. Build/run the matching experimental MoonDesk branch.
2. Open `chrome://extensions` (or `edge://extensions`), enable Developer mode, and choose **Load unpacked**.
3. Select `extensions/moondesk-worker-companion`.
4. Open the extension popup. The extension should automatically discover and pair with the local MoonDesk companion bridge. Manual repair is only a fallback when this installation's stored credential can no longer be accepted.
5. Open the ChatGPT conversation you want to use as the Anchor. No Project/workspace binding step is required.
6. Click **Discover available ChatGPT models**, choose a confirmed model and reasoning effort, and save the worker profile.

Placement is automatic. If the Anchor is inside a ChatGPT Project, a fresh worker starts in that exact `g-p-...` Project ID. If the Anchor is an ordinary ChatGPT conversation, a fresh worker starts as an ordinary ChatGPT conversation. Later tasks for the same durable worker reuse its exact confirmed worker conversation.

Multiple Chromium browser installations can remain paired at once. Fresh workers are routed to the browser that positively observes the exact Anchor conversation; once a worker thread exists, reuse stays pinned to the browser that owns that exact worker thread unless a safe recovery path is established.

Workers remain experimental until the signed-in normal-chat connector/tool-context and multi-browser browser-E2E matrix pass.
