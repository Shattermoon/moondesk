# MoonDesk Worker Companion distribution plan

The Worker Companion is **optional for MoonDesk itself**. Normal MoonDesk features—files, shell commands, browser automation, workspaces, handoffs, and ordinary MCP use—do not require it. Only the experimental **Workers/sub-agents** feature depends on the Chromium companion for exact ChatGPT routing, model/effort confirmation, durable Send reconciliation, and worker-tab presence.

MoonDesk should **not** depend on browser-store approval for this companion. The production path is the same class of installation used by Chat On Steroids: MoonDesk ships the extension files itself, the user enables browser Developer mode once, and loads MoonDesk's stable local extension folder with **Load unpacked**.

## Distribution model

### Primary: embedded companion → stable local folder

The MoonDesk native binary embeds the exact Worker Companion runtime files from the same source revision. On startup MoonDesk materializes those bytes into the stable `~/.moondesk/worker-companion` directory. This works for npm-installed MoonDesk and direct native-binary installs without requiring a repository checkout.

The browser installation remains optional and explicit: normal MoonDesk does not require the companion. Users who want Workers choose **Open Worker Companion folder** in MoonDesk Settings, then load that stable folder with **Load unpacked**.

The stable directory itself is kept in place across MoonDesk updates so Chromium can keep remembering the same unpacked-extension path. On every MoonDesk startup the native binary synchronizes that directory to the exact companion runtime embedded in the running MoonDesk build. There is no separate companion-update action and no worker-idle gate: updating MoonDesk is the update mechanism for the bundled companion files.

A loaded Chromium extension can still be running an older service-worker generation while those files are replaced on disk. The companion therefore compares both the MoonDesk release version and a runtime revision from the local bridge handshake. When a previously loaded companion sees a different shipped version or runtime revision, it calls `chrome.runtime.reload()` once and reconnects using the newly synchronized files. A one-time manual Reload remains only a recovery fallback if Chromium refuses that self-reload; it is not part of the normal update flow.

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
2. Only if Workers are wanted, open **Settings → Workers** and choose **Open Worker Companion folder**.
3. MoonDesk opens its stable `~/.moondesk/worker-companion` folder.
4. In the browser used for ChatGPT, open `chrome://extensions`, `edge://extensions`, or `brave://extensions`.
5. Turn on **Developer mode** and click **Load unpacked**.
6. Select the Worker Companion folder MoonDesk opened.
7. Open ChatGPT in the same browser and sign in normally.
8. The companion discovers MoonDesk's loopback bridge and pairs automatically. There is no per-chat pairing code or token to copy.
9. Open the ChatGPT conversation that will act as the **Core**.
10. Models and reasoning efforts are discovered automatically once signed-in ChatGPT is available. Open the companion popup, choose from the confirmed catalog, and save the worker profile. **Refresh models** is only a repair/revalidation fallback.
11. Ask Core to create workers. MoonDesk recommends **1–4 simultaneous workers**; 8 remains the hard product ceiling, not the recommended everyday setting.

Fresh workers are ordinary ChatGPT conversations even when Core lives in a ChatGPT Project. Existing workers reuse their exact durable conversation.

## Updating the companion

App and companion versions move together. MoonDesk synchronizes the stable companion directory from the running native binary every time MoonDesk starts, so users do not perform a second companion-update step after updating MoonDesk.

If Chromium already has the unpacked extension loaded when MoonDesk starts with newer companion files, the old service worker may remain alive briefly. Release builds compare the loaded extension manifest version with MoonDesk's app version, and the bridge also carries a runtime revision for source/dev compatibility changes. Either mismatch makes the extension request one self-reload with `chrome.runtime.reload()` and then reconnect. The reload attempt is persisted so a broken/missing update cannot enter an infinite reload loop. If Chromium still serves the old generation after that single attempt, the popup reports an explicit one-time manual Reload instruction.

This deliberately favors a short recoverable interruption during an app upgrade over a permanent Settings-only update workflow. Worker Send remains fail-closed: a stale companion cannot silently send with an unconfirmed model/effort or route, and after reload the durable command/reconciliation state resumes from MoonDesk rather than replaying an uncertain Send.

## Pairing and compatibility

The extension discovers only MoonDesk's dedicated loopback bridge ports (`127.0.0.1:47650` through `47654`) and uses the companion protocol version to reject incompatible hosts.

Pairing is installation-scoped and automatic. Multiple browser installations may remain paired independently. MoonDesk never uses the workspace MCP URL as a browser-extension credential.

A runtime/protocol mismatch should be explicit and actionable. Runtime-revision mismatches self-reload once after MoonDesk synchronizes the stable folder; protocol mismatches explain that MoonDesk should be restarted and the extension reloaded once only if Chromium does not recover automatically. Do not expose a generic connection failure when the real issue is incompatible bytes.

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

The native binary now also embeds those exact runtime files and synchronizes them into the stable MoonDesk-owned `~/.moondesk/worker-companion` folder on startup. Settings exposes **Open Worker Companion folder**, so production users never need a repository checkout or Cargo/npm cache path.

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
