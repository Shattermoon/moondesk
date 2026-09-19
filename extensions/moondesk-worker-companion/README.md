# MoonDesk Worker Companion (Experimental)

This unpacked Chrome extension is the browser-side transport for MoonDesk Workers V1.

It intentionally owns only a small surface:

- automatically discover and pair with the local MoonDesk companion bridge on loopback ports 47650-47654;
- bind one ChatGPT Project conversation to one MoonDesk workspace UUID;
- redeem durable managed-chat launch commands;
- open/recover a worker ChatGPT tab;
- select and read back the requested model + reasoning effort;
- insert the worker bootstrap and click Send once;
- reconcile ambiguous sends by looking for the durable task marker, never by blindly clicking Send again.

It does **not** record transcripts, spawn nested workers, mirror agent state, or use the workspace MCP URL as an authentication credential.

## Local install

1. Build/run the matching experimental MoonDesk branch.
2. Open `chrome://extensions`, enable Developer mode, and choose **Load unpacked**.
3. Select `extensions/moondesk-worker-companion`.
4. Open the extension popup. The extension should automatically discover and pair with the local MoonDesk companion bridge. Manual repair is only a fallback when the stored installation credential can no longer be accepted.
5. Open an existing conversation inside the ChatGPT Project you want to associate with the workspace.
6. In the extension popup, select the workspace and click **Bind** once.

The existing conversation is used only to establish the Project binding. A fresh worker thread starts from the bound Project home; later tasks for the same durable worker thread reuse the confirmed worker conversation.

Workers remain experimental until the real-browser model-picker, launch/reuse, and crash/reconciliation matrix pass.
