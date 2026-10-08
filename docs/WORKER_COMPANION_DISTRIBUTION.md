# MoonDesk Worker Companion distribution plan

The Worker Companion is **optional for MoonDesk itself**. Normal MoonDesk features—files, shell commands, browser automation, workspaces, handoffs, and ordinary MCP use—do not require it. Only the experimental **Workers/sub-agents** feature depends on the Chromium companion for exact ChatGPT routing, model/effort confirmation, durable Send reconciliation, and worker-tab presence.

MoonDesk should **not** depend on browser-store approval for this companion. The production path is the same class of installation used by Chat On Steroids: MoonDesk ships the extension files itself, the user enables browser Developer mode once, and loads MoonDesk's stable local extension folder with **Load unpacked**.

## Distribution model

### Primary: bundled local extension folder

The normal MoonDesk installation should materialize a stable local folder containing the exact Worker Companion version shipped with that MoonDesk release.

The user should never need to clone the repository or copy source files manually. MoonDesk setup should expose the folder directly (for example, **Open Worker Companion folder**) and explain the browser steps.

The folder must be stable across MoonDesk updates. Do not point Chrome at an ephemeral build, temporary extraction directory, Cargo target directory, npm cache entry, or version-specific release folder that disappears on update.

A future update path may replace the files in that stable folder and ask/reload the extension only when no worker/Core browser operation is busy. Updating the bytes and reloading the running Manifest V3 extension are separate operations; MoonDesk must never claim the new companion is active merely because the files on disk changed.

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

1. Install/update MoonDesk and start it.
2. In MoonDesk, choose **Set up Worker Companion**.
3. Choose the browser used for ChatGPT: Chrome, Edge, or Brave.
4. MoonDesk opens that browser's extensions page:
   - Chrome: `chrome://extensions`
   - Edge: `edge://extensions`
   - Brave: `brave://extensions`
5. Turn on **Developer mode**.
6. Click **Load unpacked**.
7. In MoonDesk, click **Open Worker Companion folder** and choose that exact folder in the browser picker.
8. Open ChatGPT in the same browser and sign in normally.
9. The companion discovers MoonDesk's loopback bridge and pairs automatically. There is no per-chat pairing code or token to copy.
10. Open the ChatGPT conversation that will act as the **Core**.
11. In the companion popup, click **Discover available ChatGPT models**, select model/reasoning effort, and save the worker profile.
12. Ask Core to create workers. MoonDesk recommends **1–4 simultaneous workers**; 8 remains the hard product ceiling, not the recommended everyday setting.

Fresh workers are ordinary ChatGPT conversations even when Core lives in a ChatGPT Project. Existing workers reuse their exact durable conversation.

## Updating the companion

App and companion versions should move together.

For the first production version, the safe update UX is:

1. MoonDesk updates/replaces the files in its stable Worker Companion folder.
2. If the browser is still running the previous companion build, MoonDesk says **Reload Worker Companion**.
3. The user opens the extensions page and clicks Reload for MoonDesk Worker Companion, then refreshes open ChatGPT tabs if required.

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

The next distribution implementation step is to materialize those exact release companion files into a stable MoonDesk-owned local folder and expose **Open Worker Companion folder** from MoonDesk setup. That is preferable to a browser-store dependency.

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
