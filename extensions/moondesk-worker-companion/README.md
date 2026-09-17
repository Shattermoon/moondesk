# MoonDesk Worker Companion (Experimental)

This unpacked Chrome extension is the browser-side transport for MoonDesk Workers V1.

It intentionally owns only a small surface:

- pair with the local MoonDesk host using a one-time pairing code;
- bind one ChatGPT Project conversation to one MoonDesk workspace UUID;
- redeem durable managed-chat launch commands;
- open/recover a worker ChatGPT tab;
- select and read back the requested model + reasoning effort;
- insert the worker bootstrap and click Send once;
- reconcile ambiguous sends by looking for the durable task marker, never by blindly clicking Send again.

It does **not** record transcripts, spawn nested workers, mirror agent state, or use the workspace MCP URL as an authentication credential.

## Local install

1. Build/run the matching experimental MoonDesk branch.
2. Open `chrome://extensions` and enable Developer mode.
3. Choose **Load unpacked** and select `extensions/moondesk-worker-companion`.
4. Open the extension popup, enter the local MoonDesk URL (default `http://127.0.0.1:3200`) and the pairing code shown by MoonDesk.
5. Open an existing conversation inside the ChatGPT Project for a workspace and bind it in the popup.

Workers remain experimental until the real-browser model-picker and crash/reconciliation matrix pass.
