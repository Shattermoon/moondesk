# MoonDesk Worker Companion distribution plan

MoonDesk Workers depend on the Chromium companion extension for exact ChatGPT routing, model/effort confirmation, durable Send reconciliation, and worker-tab presence. Production users should not be expected to enable Developer mode or manage an unpacked extension directory.

## Distribution channels

### Primary: browser stores

Use the same Manifest V3 extension source for both store listings:

- **Chrome Web Store** — primary install path for Google Chrome and Chromium browsers that support Chrome Web Store extensions, including Brave.
- **Microsoft Edge Add-ons** — primary install path for Microsoft Edge.

A store installation gives users a stable extension identity and automatic browser-managed updates. It also avoids the fragile production workflow of asking users to unzip files, enable Developer mode, and reload the extension after every MoonDesk update.

The Chrome and Edge listings should use the same release version as MoonDesk. The release workflow now synchronizes `extensions/moondesk-worker-companion/manifest.json` to the MoonDesk release version before creating the tested release candidate.

### Fallback: signed/checksummed GitHub Release asset

Every MoonDesk GitHub Release also carries `moondesk-worker-companion.zip`, built from the exact tested release candidate and included in `SHA256SUMS`.

This ZIP is a recovery/beta/developer fallback, not the normal production onboarding path. A user using this fallback must unzip it and load the folder as an unpacked extension; Chromium does not treat an arbitrary GitHub ZIP as a normal store-installed extension.

### Developer builds

Repository contributors can continue using `extensions/moondesk-worker-companion` with **Load unpacked**. Developer mode instructions belong in contributor/beta documentation, not the normal product onboarding flow.

## What the user should do

The production setup should be short:

1. Install or update MoonDesk.
2. Click **Install Worker Companion** in MoonDesk documentation/setup and install the extension from the browser's official store.
3. Start MoonDesk if it is not already running.
4. Open ChatGPT in the same browser and sign in normally.
5. Open the Worker Companion popup. It should discover MoonDesk's loopback companion bridge and pair automatically. There is no per-chat pairing code or token to copy.
6. Open the ChatGPT conversation that will act as the **Core**.
7. In the companion popup, click **Discover available ChatGPT models**, select the desired model/reasoning effort, and save the worker profile.
8. Ask Core to create workers. MoonDesk recommends **1–4 simultaneous workers**; 8 is the hard product limit and should be treated as high-load/advanced usage.

Fresh workers are normal ChatGPT conversations even when Core lives inside a ChatGPT Project. Users do not need to move workers into Projects or bind Project names to MoonDesk workspaces.

## Pairing and compatibility

The extension discovers only MoonDesk's dedicated loopback bridge ports (`127.0.0.1:47650` through `47654`) and uses the companion protocol version to reject incompatible hosts.

Pairing is installation-scoped and automatic. Multiple browser installations may remain paired independently. MoonDesk never uses the workspace MCP URL as a browser-extension credential.

Before store launch, the popup/setup UX should make protocol mismatch explicit with an actionable message such as **Update MoonDesk** or **Update Worker Companion** rather than surfacing a generic connection failure.

## Browser-store permission explanation

The store listing and privacy disclosure should explain each permission in plain language:

- `storage` — stores the companion's local pairing/profile state.
- `tabs` and `windows` — finds the exact open ChatGPT conversation and tracks whether that worker page is present.
- `scripting` — installs MoonDesk's ChatGPT-side routing/model helpers into `chatgpt.com` pages.
- `alarms` — wakes the Manifest V3 service worker for bounded command/presence polling.
- `https://chatgpt.com/*` — required because workers are created and controlled in ChatGPT's web UI.
- loopback `http://127.0.0.1/*` / `http://localhost/*` — required to communicate with the local MoonDesk companion bridge.

The companion is designed to report bounded routing state such as exact conversation IDs, Project IDs when present, focus/generation state, and provider correlation needed for worker routing. It does not act as a general browsing-history collector and does not need host access outside ChatGPT and the local MoonDesk bridge.

## Release automation implemented in this branch

The release pipeline now treats the Worker Companion as a first-class release artifact:

1. merged-source validation runs the companion JS syntax checks and full companion test suite;
2. the release version is written into `manifest.json` alongside the Rust/npm version update;
3. a dedicated release job checks out the exact candidate SHA and reruns the companion checks;
4. only runtime extension files are placed into `moondesk-worker-companion.zip`;
5. the ZIP is uploaded as a build artifact;
6. the GitHub Release requires the ZIP in its exact asset list;
7. `SHA256SUMS` covers the extension ZIP together with the five native binaries;
8. tag-context release verification requires the extension version to match the immutable MoonDesk tag and re-verifies the released checksum set before npm publication.

This gives us one exact extension package per MoonDesk release even before browser-store publishing is automated.

## External setup still required before automatic store publishing

Store publication cannot be completed only in repository code. We still need the external publisher identities and listing IDs:

- Chrome Web Store developer account and MoonDesk extension listing/item ID;
- Microsoft Partner Center / Edge Add-ons publisher account and product ID;
- store API credentials or approved CI authentication mechanism;
- production extension icons, screenshots, listing copy, support URL, and privacy-policy URL;
- final permission/privacy review and store approval.

Once those exist, add a post-release store-publish job that uploads **the exact `moondesk-worker-companion.zip` already checksummed on the GitHub Release**. Do not rebuild a separate store ZIP from `main`, because that would break the one-release/one-extension-bytes invariant.

Store upload should be retryable and idempotent. A transient Chrome/Edge review or API failure must not rewrite a Git tag or change the already-published MoonDesk/npm release.

## Production gate

Do not call Workers production-ready until all of these pass:

- fresh one-worker lifecycle;
- two simultaneous workers with distinct durable ChatGPT conversation IDs;
- exact existing-worker reuse;
- recommended four-worker concurrency;
- running-worker tab-close behavior (worker remains running/detached and can finish);
- extension reload/browser restart recovery;
- Chrome Web Store install/update smoke;
- Edge Add-ons install/update smoke;
- clear user-facing compatibility error for app/extension protocol mismatch;
- published privacy/support documentation matching the permissions actually requested by the manifest.
