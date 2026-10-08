# MoonDesk Worker Companion distribution plan

The Worker Companion is **optional for MoonDesk itself**. Normal MoonDesk features—files, shell commands, browser automation, workspaces, handoffs, and ordinary MCP use—do not require it. Only the experimental **Workers/sub-agents** feature depends on the Chromium companion for exact ChatGPT routing, model/effort confirmation, durable Send reconciliation, and worker-tab presence.

MoonDesk should **not** depend on browser-store approval for this companion. The production path is the same class of installation used by Chat On Steroids: MoonDesk ships the extension files itself, the user enables browser Developer mode once, and loads MoonDesk's stable local extension folder with **Load unpacked**.

## Distribution model

### Primary: embedded companion → stable local folder

The MoonDesk native binary embeds the exact Worker Companion runtime files from the same source revision. On startup MoonDesk materializes those bytes into the stable `~/.moondesk/worker-companion` directory. This works for npm-installed MoonDesk and direct native-binary installs without requiring a repository checkout.

The browser installation remains optional and explicit: normal MoonDesk does not require the companion. Users who want Workers choose **Set up Workers (open companion folder)** in MoonDesk Settings, then load that stable folder with **Load unpacked**.

The stable directory itself is kept in place across MoonDesk updates so Chromium can keep remembering the same unpacked-extension path. On startup MoonDesk materializes the folder only for a first install. If an existing folder differs from the newly shipped companion, MoonDesk leaves those loaded bytes untouched and shows **Update Worker Companion (when idle)** instead. That action refuses to update while any Worker or managed ChatGPT launch is live/ambiguous, then refreshes the known runtime files in place, publishes `manifest.json` last, and tells the user to click Reload on the browser extensions page before using Workers again.

### Release ZIP fallback

Every MoonDesk GitHub Release carries `moondesk-worker-companion.zip`, built from the exact tested release candidate and covered by `SHA256SUMS`.

The ZIP is useful for:

- beta/manual installations;
- recovery when the locally materialized folder is missing;
- contributors and support diagnostics;
- verifying that the installed companion bytes match the MoonDesk release.

A user installing from the ZIP must extract it first and choose that extracted folder with **Load unpacked**. Chromium cannot load the ZIP itself as an unpacked extension.

### Developer/source builds

Repository contributors may load `extensions/moondesk-worker-companion` directly. That is a development path, not the normal end-user path.

## User onboarding

The production instructions should be short and shown inside MoonDesk rather than buried in documentation:

1. Install/update MoonDesk and start it. No extension is required for normal MoonDesk use.
2. Only if Workers are wanted, open **Settings → Workers** and choose **Set up Workers (open companion folder)**.
3. MoonDesk opens its stable `~/.moondesk/worker-companion` folder.
4. In the browser used for ChatGPT, open `chrome://extensions`, `edge://extensions`, or `brave://extensions`.
5. Turn on **Developer mode** and click **Load unpacked**.
6. Select the Worker Companion folder MoonDesk opened.
7. Open ChatGPT in the same browser and sign in normally.
8. The companion discovers MoonDesk's loopback bridge and pairs automatically. There is no per-chat pairing code or token to copy.
9. Open the ChatGPT conversation that will act as the **Core**.
10. In the companion popup, click **Discover available ChatGPT models**, select model/reasoning effort, and save the worker profile.
11. Ask Core to create workers. MoonDesk recommends **1–4 simultaneous workers**; 8 remains the hard product ceiling, not the recommended everyday setting.

Fresh workers are ordinary ChatGPT conversations even when Core lives in a ChatGPT Project. Existing workers reuse their exact durable conversation.

## Updating the companion

App and companion versions should move together.

For the first production version, the safe update UX is:

1. MoonDesk notices that the stable folder differs from the companion embedded in the new app, but leaves the existing folder untouched on startup.
2. Settings shows **Update Worker Companion (when idle)**.
3. The update action refuses while any Worker or managed ChatGPT launch is active, crossing Send, or awaiting reconciliation.
4. Once idle, MoonDesk replaces the runtime files in the same stable folder and says **Reload Worker Companion**.
5. The user opens the extensions page and clicks Reload for MoonDesk Worker Companion, then refreshes open ChatGPT tabs if required.

Later, after live validation, MoonDesk can adopt the CoS-style convenience path: only when no Core/worker browser operation is busy, prepare the stable folder and let the extension call `chrome.runtime.reload()` on itself. That must remain guarded by exact activity/version evidence so an update cannot interrupt an active Send or worker turn.

## Pairing and compatibility

The extension discovers only MoonDesk's dedicated loopback bridge ports (`127.0.0.1:47650` through `47654`) and uses the companion protocol version to reject incompatible hosts.

Pairing is installation-scoped and automatic. Multiple browser installations may remain paired independently. MoonDesk never uses the workspace MCP URL as a browser-extension credential.

A version/protocol mismatch should be explicit and actionable: **MoonDesk and Worker Companion do not match — update/reload the companion from this MoonDesk installation.** Do not expose a generic connection failure when the real issue is incompatible bytes.

## Release automation implemented in this branch

The release pipeline treats the Worker Companion as a first-class release artifact:

1. merged-source validation runs companion JavaScript syntax checks and the full companion test suite;
2. the release version is written into `extensions/moondesk-worker-companion/manifest.json` alongside the Rust/npm version update;
3. a dedicated release job checks out the exact candidate SHA and reruns companion validation;
4. only runtime extension files are placed into `moondesk-worker-companion.zip`;
5. packaging is reproducible from identical source bytes;
6. the ZIP is uploaded with the release artifacts;
7. the GitHub Release requires the ZIP in its exact asset list;
8. `SHA256SUMS` covers the companion ZIP together with the five native binaries;
9. tag-context verification requires the companion version to match the immutable MoonDesk tag and re-verifies the checksum set before npm publication.

The native binary now also embeds those exact runtime files and materializes them into the stable MoonDesk-owned `~/.moondesk/worker-companion` folder. Settings exposes **Set up Workers (open companion folder)** / **Open Worker Companion folder**, so production users never need a repository checkout or Cargo/npm cache path.

## Production gate

Do not call Workers production-ready until all of these pass:

- fresh one-worker lifecycle;
- two simultaneous workers with distinct durable ChatGPT conversation IDs;
- exact existing-worker reuse, including reopening a closed idle worker tab;
- recommended four-worker concurrency;
- running-worker tab-close behavior: worker stays running/detached and can report/finish;
- extension reload/browser restart recovery;
- release/package installation from the stable local Worker Companion folder;
- update from one companion version to the next without changing the browser's loaded folder;
- explicit app/companion protocol mismatch guidance;
- user instructions for Chrome, Edge, and Brave Developer mode + Load unpacked.
