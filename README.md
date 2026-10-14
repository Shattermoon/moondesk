<div align="center">

# MoonDesk

**Turn ChatGPT Chat into a local coding agent.**

Files, shell commands, browser automation, multiple workspaces, and session handoffs — through one local MCP host.

**No reverse engineering. No OpenAI API key. No separate agent service.**

[Quickstart](#quickstart) · [Features](#what-you-get) · [Safety](#safety) · [Contributing](#contributing)

```bash
npm install -g moondesk
```

</div>

> [!IMPORTANT]
> MoonDesk can execute commands and modify files on your computer. Use it only with workspaces and instructions you trust. For untrusted code, use a VM or container.

## What is MoonDesk?

MoonDesk is an open-source local MCP server that connects ChatGPT to your development environment.

Run it inside a project, connect the workspace URL to ChatGPT as a Custom Connector, and ChatGPT can work with that project using local tools.

```text
ChatGPT
   │
   │ Custom Connector / MCP
   ▼
MoonDesk
   ├─ Files
   ├─ Shell jobs
   ├─ Browser / DevTools
   ├─ Workspaces
   └─ Session handoffs
```

Your code stays on your machine unless a tool you run sends it somewhere else.

## What you get

| Capability | What it gives ChatGPT |
| --- | --- |
| **Files** | Read, search, edit, write, and delete inside a project workspace. |
| **Shell** | Run tests, builds, package installs, dev servers, Git, and other developer commands. |
| **Browser** | Control a dedicated Chromium session, inspect the DOM/console, emulate viewports, and visually inspect pages. |
| **Workspaces** | Serve multiple projects from one host, each with its own secret MCP URL. |
| **Handoffs** | Save continuation state when moving a long task to a fresh conversation. |
| **Permissions** | Use full local tools or a reduced read-only mode. |
| **Verified installs** | Download the matching native binary on first run and verify it against the release SHA-256. |

MoonDesk runs on **Windows, macOS, and Linux**. Prebuilt Linux binaries are currently provided for x64/arm64 systems with **glibc 2.34 or newer**; musl-based distributions such as Alpine are not yet supported.

## Why MoonDesk?

ChatGPT is already good at reasoning about code. MoonDesk gives it the local execution layer, so you can stop copying files into chat and pasting terminal output back and forth.

One MoonDesk process can serve several repositories at once:

```text
MoonDesk host
├── Project A ── secret MCP URL ──> D:\ProjectA
├── Project B ── secret MCP URL ──> D:\ProjectB
└── Project C ── secret MCP URL ──> D:\ProjectC
```

## Quickstart

### 1. Install

MoonDesk requires Node.js `^20.19.0 || ^22.12.0 || >=23`.

```bash
npm install -g moondesk
```

### 2. Start it in your project

```bash
cd your-project
moondesk
```

Choose `Control Computer`, `Control Browser`, or `Both`.

On first launch, MoonDesk asks for an **ngrok authtoken** and **static domain** and stores them in its local config.

### 3. Copy the workspace URL

Open `[w] Workspaces` in the TUI and copy the MCP URL:

```text
https://your-domain.ngrok-free.dev/<workspace-secret>/mcp
```

> Treat this URL like a credential.

### 4. Add it to ChatGPT

Create a Custom Connector:

```text
Name: MoonDesk · <project name>
MCP Server URL: <URL copied from MoonDesk>
Authentication: None
```

Allow write actions only when you trust the workspace and task.

### 5. Add this to ChatGPT Custom Instructions

In ChatGPT, open **Settings → Personalization → Custom Instructions** and add the following instruction so ChatGPT routes project work to the correct MoonDesk connector:

```text
The user may have multiple MoonDesk custom connectors, with each connector bound to a different project/workspace.

For any request involving local files, code, commands, or project operations, use the MoonDesk connector that matches the project/workspace being discussed. Never use a different project’s connector unless the user explicitly asks.

Before using a MoonDesk connector for the first time in a conversation, call its moondesk_instruction tool and follow the instructions it returns. If the connector tools are unavailable or stale, refresh that connector with api_tool.list_resources first, then call moondesk_instruction.

Workspace routing is determined by the connector itself. Do not ask for or invent a workspace argument.
```

Select the connector and start working.

## Experimental ChatGPT workers

> [!WARNING]
> The Companion extension is **optional for MoonDesk itself**. It is required for the experimental **Workers/sub-agents** feature and for the separate `user_browser_*` tools that deliberately control exact tabs in your existing Chromium browser. Files, shell, workspaces, handoffs, normal MCP use, and MoonDesk's isolated `browser_*` runtime continue to work without it. When Workers are used, the companion drives ChatGPT's web UI, so MoonDesk fails closed when it cannot positively confirm the requested model, reasoning effort, exact placement, or previous Send state.

Workers let one ChatGPT conversation act as the **Core** and delegate independent tasks to durable worker conversations in the same MoonDesk workspace. The configurable Worker value is **1-8** and defaults to **4**, with a hard safety ceiling of **8 active workers per Core family**. For a browser ChatGPT Core, that value remains the family target. For the machine-local Codex/IDE Core, it is the **Worker-slot capacity per online paired Companion browser**: fresh Workers are automatically placed on the least-loaded browser, so a value of 3 with two eligible online browsers yields up to 6 usable family slots, while one online browser yields 3. Offline-browser capacity never spills into another account, and the family-wide ceiling still applies even with additional browsers. Worker identity is bound to the route-resolved workspace plus ChatGPT's exact session metadata; a different conversation in the same workspace does not inherit Core or worker authority. ChatGPT Project names, connector names, MoonDesk workspace names, and folder names are display-only and are never used as routing or authorization keys.

### Optional Worker Companion setup

You do **not** need the extension for normal MoonDesk use. Install it only when you want Workers/sub-agents or explicit control of tabs in your existing Chromium browser through `user_browser_*`. MoonDesk embeds the exact companion files from its own build and synchronizes them into the stable `~/.moondesk/worker-companion` folder whenever MoonDesk starts; browser installation remains an explicit user opt-in, but later MoonDesk upgrades do not require a second companion-update step. An already-loaded companion can self-reload once when its MoonDesk release version or runtime revision no longer matches. The supported browser targets are **Google Chrome, Microsoft Edge, and Brave**. Current signed-in acceptance has been run in Chrome; Edge and Brave use the same Chromium Manifest V3 `chrome.*` extension APIs but have not been separately live-tested in this cycle. Other Chromium-based browsers may work but are not supported targets; Firefox and Safari are not currently supported. The companion requests Chromium's required `debugger` permission so MoonDesk can attach CDP only to exact user-browser tabs that `user_browser_*` deliberately opens or acquires. Chromium does not allow the `debugger` capability to be declared as an optional extension permission, so an existing unpacked Companion upgraded from an older MoonDesk release may require a one-time permission approval/re-enable on the browser's Extensions page. Each GitHub Release also carries the same checksummed `moondesk-worker-companion.zip` as a beta/recovery fallback. The full release/onboarding plan is in [`docs/WORKER_COMPANION_DISTRIBUTION.md`](docs/WORKER_COMPANION_DISTRIBUTION.md).

To enable Workers:

1. Start MoonDesk and open **Settings → Workers**.
2. Choose **Open Worker Companion folder**. MoonDesk opens its stable Worker Companion folder.
3. Open `chrome://extensions` (or `edge://extensions` / `brave://extensions`), enable **Developer mode**, choose **Load unpacked**, and select the folder MoonDesk opened.
4. Open ChatGPT in that same browser. The extension discovers MoonDesk's loopback-only bridge and pairs automatically; there is no per-chat token step. Multiple browser installations may remain paired independently.
5. Open the ChatGPT conversation you want to use as the Core. No Project/workspace binding step is required.
6. The companion automatically discovers the signed-in account's available ChatGPT models and reasoning efforts. Open the popup, choose from the confirmed catalog, and save the worker profile. **Refresh models** is only a repair/revalidation fallback. The companion reads ChatGPT's provider-owned picker state instead of relying on translated labels, and supports the current provider effort lanes (`Instant`, `Minimal`, `Low`, `Medium`, `High`, `Extra High`, `Max`, `Ultra`, and `Pro`) when the account actually offers them. Existing `Extra High` profiles remain compatible with the provider's `xhigh`/`max` migration.

Contributors running from source can still load `extensions/moondesk-worker-companion` directly. Source checkouts and the GitHub Release ZIP intentionally do **not** contain MoonDesk's private installation capability, so those fallback installs do not auto-pair. Open the companion popup → **Advanced → Manual Repair**, copy the current **Manual repair code** from **MoonDesk Settings → Workers**, paste it once, and pair that browser. The normal MoonDesk-prepared `~/.moondesk/worker-companion` folder remains the recommended path and auto-pairs without this step. If a browser installation is explicitly revoked, that revocation is durable and the installation cannot silently auto-pair itself again; restoring it requires **Manual Repair**.

Workers V1 always creates fresh workers as ordinary ChatGPT conversations, even when the Core is inside a ChatGPT Project. Project membership remains useful browser-Core routing metadata, but it is not a worker placement target and MoonDesk never clones the Core conversation to create a worker. Fresh workers are routed to the selected/paired Companion browser. After the worker conversation is confirmed, reuse remains attached to that durable thread/browser affinity and the Companion records a one-way fingerprint of the ChatGPT account that created it. Reuse/recovery fails closed before opening the durable Worker URL when that account proof is missing or the current ChatGPT account differs; MoonDesk never guesses across accounts. Legacy workers that were previously created inside a Project can still be reused only when their exact confirmed conversation and account binding remain available.

For local Codex/IDE Core, configure the machine-local `moondesk mcp` stdio server once. Configure the same installation-wide **generalized MoonDesk Worker connector** (copied from **MoonDesk Settings → Workers**) in each ChatGPT account/browser that you want to contribute Worker capacity. The connector is not bound to a project: a fresh Worker's one-time claim capability resolves its owning MoonDesk workspace, and the exact Worker conversation remains bound to that workspace for later project tools. When multiple eligible Companion browsers are online, MoonDesk load-balances fresh Worker slots across them; each durable Worker is then pinned to the exact Companion browser/account that created it and can never migrate on reuse. Account affinity is enforced per Companion browser, so different browsers may legitimately use different ChatGPT accounts while one browser cannot silently switch accounts underneath retained Workers. Once that exact Worker has claimed and started its task, the generalized connector also allows conversation-scoped `user_browser_*` QA only through that Worker's own Companion browser; it cannot cross-control the other Worker account's browser, and ChatGPT/OpenAI tabs remain protected. If one browser goes offline, its retained Workers remain assigned there and only that browser's currently usable capacity disappears; MoonDesk does not spill those slots into another account. Legacy local Workers created before browser affinity was persisted are backfilled from unambiguous durable launch history, while conflicting multi-browser history fails closed. `workers status` reports per-browser capacity/assignment and aggregate available slots. Workspace-local `clear` remains the terminal recovery path when safe; the Companion popup's **Clear workers** remains the stronger host-wide recovery when a browser-side Send must be fenced.

The last confirmed model catalog is stored in extension-local storage, so reopening the popup does not require rediscovery. MoonDesk still verifies the actual model and effort in ChatGPT before sending every worker assignment. On reuse, if the current ChatGPT state already proves the exact requested model and effort are selected, the Companion keeps that selection and does not reopen the picker just to wake the Worker; it opens the native picker only when a change or stronger proof is required. If MoonDesk has been upgraded but an existing Worker conversation does not expose the generalized connector's `workers` tool, refresh/reconnect that one Worker connector and start a fresh Worker conversation because ChatGPT may retain an older connector schema.

MoonDesk keeps retained Worker state bounded to 64 Core families. Automatic compaction never discards an idle reusable worker, an uncollected result, or a still-replayable non-empty `collect` receipt merely because another Core needs capacity. If an old Core conversation is gone and its idle family can no longer be retired from that chat, use **MoonDesk Settings → Workers → Release inactive Worker capacity**, choose the workspace, and type `CLEANUP` to confirm. This host-local action is workspace-scoped and only removes idle/retired families when both Worker state and managed browser-launch history prove there is no active or ambiguous Send, pending worker message/report, or uncollected task result. ChatGPT conversations are preserved; terminal MoonDesk launch history for the removed families is cleared with them. Because this is an explicit destructive recovery action, the confirmation also reports when retained `collect` replay receipts for those inactive families will be discarded.

### Worker lifecycle

The MCP `workers` tool supports Core operations such as spawn, status, send, collect, reuse, and retire, plus worker-side claim, inbox, report, start, and finish operations. For a fresh spawn, Core can pass a compact `context` briefing containing the overall goal, user constraints and settled decisions, branch/PR state, completed validation, and sibling-worker ownership; MoonDesk places that shared context before the worker-specific assignment so a new worker does not need to rediscover conversation-only decisions. Reuse wakes the same durable ChatGPT conversation and does not resend that shared briefing; the worker keeps its existing conversation history, while any newly relevant or changed facts belong directly in the new `task`. A newly opened worker claims its one-time capability and immediately calls `start` before project work; MoonDesk blocks normal project tools, progress reports, and `finish` while the Worker is only claimed. If that first claim arrives before the Companion's terminal launch acknowledgement, MoonDesk uses the browser-attested claim operation to atomically finish the same conversation registration instead of returning a registration race. Local IDE status exposes a derived `lifecycleStage`: `queued` means browser preparation has not crossed Send, `sent` means the assignment reached or may have reached ChatGPT but claim is incomplete, `claimed` means registration succeeded but `workers start` has not completed, and `working` means the durable start transition succeeded. `collect` uses a stable `operation_id` and returns reports, completed results, plus durable `failed` task records containing the failing stage and bounded underlying error; retrying an ambiguous collect with the same ID replays the same batch. Workers V1 intentionally keeps only the most recent **16 collection batches per Core family**, so this is a bounded retry window rather than an indefinite delivery-ACK protocol.

Workers survive MoonDesk/browser tab restarts as durable records. Closing the tab of a **running** worker does not end its task: MoonDesk keeps the worker `running`, marks its browser attachment as `detached`/no-tab, preserves the exact conversation binding, and still accepts that worker's reports and `finish` call if its server-side ChatGPT turn continues. Opening that exact worker conversation again restores the attachment. MoonDesk deliberately does not infer completion from tab closure or silence once a Worker is actually running. A different rule applies before claim: after a fresh assignment crosses Send, the Worker gets a bounded two-minute claim window. If claim never succeeds, MoonDesk invalidates the one-time claim capability, terminalizes the pending task as `worker_claim_timeout_after_send`, retires the Worker, and exposes that failure through `collect` even if the Companion browser went offline. Core may also `retire` that exact sent-but-unclaimed Worker immediately; this safely fences future claim and records `worker_registration_cancelled_after_send`, but it cannot unsend a message that ChatGPT already received. Completing a normal task leaves the worker idle so a later task can reopen the same confirmed ChatGPT conversation.

Browser commands use durable leases and acknowledgements. If MoonDesk cannot tell whether ChatGPT accepted a Send, the command moves to reconciliation and **never blindly sends the assignment again**. Proven launch failures are terminalized instead of disappearing into status: fresh-worker failures free that slot, failed reuse returns the durable Worker to idle, and `collect.failed` carries the stage/error. Companion pre-Send exceptions preserve a bounded sanitized underlying diagnostic instead of collapsing everything to `worker_browser_launch_exception`. The companion's **Clear workers** action is a global Workers reset across the MoonDesk host: it discards retained Worker ownership and managed launch history while leaving the ChatGPT conversations themselves untouched. MoonDesk publishes a durable reset epoch so every paired browser drops stale local worker history on reconnect/status/pump and rechecks that epoch immediately before a browser Send after `send_started`. If another paired browser had already crossed `send_started`, Clear Workers does not report success until that browser acknowledges the new epoch after invalidating its local worker generation; a missing/stuck browser leaves Clear safely retryable instead of allowing a later post-clear Send. A Send that already reached ChatGPT cannot be unsent; Clear Workers reports commands that had already crossed MoonDesk's Send boundary so the popup can warn about that possibility.

## Browser control

MoonDesk owns one lazy managed Chromium process instead of attaching to your personal browser profile or launching a separate browser for every project. On first browser use it provisions a pinned Chrome for Testing build for the current platform, verifies the exact archive size and SHA-256, records a verified inventory of the installed browser files including Unix permission bits where applicable, installs it atomically into MoonDesk's browser cache, and controls it directly from Rust over the Chrome DevTools Protocol (CDP)—no Playwright, `chrome-devtools-mcp`, or separate browser-control service is required. Later starts revalidate that inventory so a missing or damaged support file triggers reprovisioning instead of a persistent broken-browser loop. Inside that shared Chromium, each registered workspace gets its own isolated BrowserContext for cookies and site storage, while each ChatGPT conversation gets its own MoonDesk-routed logical tab set. Different workspaces therefore do not share cookies, localStorage, IndexedDB, or service-worker state, and separate conversations cannot accidentally act on each other's selected tabs.

The browser runs headless by default and can be switched to visible mode for human-assisted steps such as logins or permission prompts. Headless and visible are two presentations of the same agent browser and expose the same page-local tabs, navigation, DOM controls, visual computer-use controls, viewport management, screenshots, console/network inspection, and heap-snapshot capabilities. Browser-global performance tracing has stricter isolation preconditions and is rejected whenever another managed BrowserContext is active, another logical browser session has used the workspace BrowserContext in the current runtime generation, a managed page is not owned by the requester, or an unowned/default-context page could contribute unrelated trace data. Presentation belongs to the shared Chromium process, so changing it while the browser is live requires explicit confirmation because every workspace BrowserContext and logical tab is recreated. MoonDesk never attaches to or reuses your personal browser profile, cookies, or logged-in sessions.

For normal agent work MoonDesk exposes a Codex-style capability facade: `browser_state` for ambient state, `browser_tabs` for conversation-owned tabs, `browser_navigate` for goto/back/forward/reload, `browser_dom` for accessibility/DOM interactions, `browser_cua` for rendered screenshots plus physical coordinate/keyboard/wheel input, and `browser_viewport` for responsive/device sizing. `browser_command` remains available as an advanced native-CDP escape hatch for console/network inspection, emulation, performance traces, V8 heap snapshots, screenshots, and lower-level page operations that are not represented by the facade.

When the optional Companion extension is installed, MoonDesk also exposes a deliberately separate `user_browser_*` surface for exact tabs in the user's existing Chromium browser: `user_browser_tabs` lists, attaches, opens, releases, and explicitly closes tabs; `user_browser_snapshot` reads the controlled page, `user_browser_screenshot` captures rendered pixels, `user_browser_navigate` navigates it, `user_browser_action` performs bounded page input, `user_browser_evaluate` runs bounded page JavaScript, and `user_browser_console` / `user_browser_network` expose bounded diagnostics. This is **not PC control** and does not merge the personal profile into MoonDesk's managed `browser_*` BrowserContexts. Each attachment is leased to the exact ChatGPT conversation, old page references are invalidated on navigation, uncertain input is never blindly replayed, every HTTPS `chatgpt.com` / `chat.openai.com` tab is protected from all user-browser observation and interaction (a controlled tab that reaches one is detached), and an animated multicolor edge/corner glow shows while MoonDesk holds control. MoonDesk temporarily hides only that indicator while taking model-facing screenshots so the captured page pixels stay clean. Releasing a tab removes the glow and detaches CDP without closing the user's tab; closing is a separate explicit `user_browser_tabs` action.

```bash
moondesk browser navigate_page --url=http://localhost:3000
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
```

The `moondesk browser` CLI is the deterministic low-level scripting path. It shares the resolved workspace's BrowserContext/login state but uses its own logical tab session, so scripted browser commands cannot steal a ChatGPT conversation's active page. For interactive agent work prefer the capability facade; use `browser_cua action=screenshot` or `view_page` when the task depends on actual rendered pixels rather than only a text/accessibility snapshot.

For browser-runtime invariants and implementation details, see [`docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md`](docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md).

## Workspaces and handoffs

Each workspace keeps its own root, secret connector URL, command jobs, retained output, history, and handoff state. Use `[w] Workspaces` to add, rename, inspect, copy, rotate, or remove projects.

MoonDesk refuses filesystem roots and protected operating-system directories as workspace roots. If an older config already contains one, MoonDesk keeps it visible as **BLOCKED** while disabling its connector and workspace tools; add or select a valid project workspace, then remove the blocked entry. Launching `moondesk` from a protected directory never auto-attaches it to a running host, and a fresh interactive launch asks you to choose a project folder instead.

Handoffs are explicit checkpoints. `create_handoff` saves the task goal, completed work, decisions, validation, blockers, next steps, Git state, and current MoonDesk jobs. `resume_handoff` reloads the checkpoint and checks for drift; `complete_handoff` marks the continuation finished.

> [!CAUTION]
> Never put credentials, tokens, passwords, private keys, or other secrets in handoff text.

Stopping the MoonDesk host disconnects every active workspace connector, so shutdown from the live dashboard requires confirmation.

## Safety

Dedicated file tools stay inside the selected workspace and reject path traversal plus symlink/junction escapes.

Shell tools are different:

> `run_command` and `start_command` execute your normal developer shell with the workspace as its working directory. They inherit your normal environment, credentials, `PATH`, and OS permissions.

**The workspace directory is not an OS sandbox.**

Use read-only mode when mutation is unnecessary, and use a VM or container for untrusted code.

> [!CAUTION]
> Never share a workspace MCP URL. Anyone who can use it may be able to invoke the tools you exposed.

## Reference

<details>
<summary><strong>Tools</strong></summary>

**Guidance / handoffs:** `moondesk_instruction`, `create_handoff`, `resume_handoff`, `complete_handoff`

**Experimental workers:** `workers`

**Files:** `read`, `view_image`, `view_images`, `search`, `write`, `edit`, `delete`

**Commands:** `run_command`, `start_command`, `list_commands`, `poll_command`, `read_command_output`, `cancel_command`

**Browser:** isolated `browser_state`, `browser_tabs`, `browser_navigate`, `browser_dom`, `browser_cua`, `browser_viewport`, `set_browser_presentation`, `view_page`, plus advanced `browser_command`; with the optional Companion, exact user-browser tab control is exposed separately as `user_browser_tabs`, `user_browser_snapshot`, `user_browser_screenshot`, `user_browser_navigate`, `user_browser_action`, `user_browser_evaluate`, `user_browser_console`, and `user_browser_network`

Use `run_command` for short work. Use `start_command` + `poll_command` for builds, tests, installs, dev servers, and other long-running jobs.

</details>

<details>
<summary><strong>Configuration</strong></summary>

| Setting | Default / location |
| --- | --- |
| Config | `%USERPROFILE%\.moondesk\config.toml` on Windows; `$HOME/.moondesk/config.toml` on macOS/Linux |
| Port | `3200` |
| Port override | `PORT` |
| Initial workspace override | `WORKSPACE_ROOT` |
| Global instructions | `~/.moondesk/AGENTS.md` |
| Workspace instructions | `<workspace>/AGENTS.md` |
| Codex-compatible instructions | `~/.codex/AGENTS.md` |
| Session handoffs | `%USERPROFILE%\.moondesk\handoffs\<workspace-id>\` on Windows; `$HOME/.moondesk/handoffs/<workspace-id>/` on macOS/Linux |

Workspace `AGENTS.md` instructions take priority.

On macOS, MoonDesk keeps your existing terminal profile instead of installing or forcing its own. Older Apple Terminal versions without reliable truecolor support use a stable 256-color compatibility palette for theme and ClippyMoon rendering; Terminal.app 2.15+ and other truecolor-capable terminals keep full RGB output. When upgrading from older MoonDesk versions, a tab still using the legacy `MoonDesk` Terminal.app profile is restored to your Terminal default settings. Set `MOONDESK_SKIP_MACOS_TERMINAL_PROFILE=1` only if you want to skip that legacy-profile migration.

</details>

<details>
<summary><strong>Windows antivirus note</strong></summary>

MoonDesk verifies the downloaded native executable against the SHA-256 published with the matching GitHub Release before launching it.

If Windows Security or another antivirus quarantines the verified executable, update security definitions and check **Protection history**, then run `moondesk` again. Do not disable antivirus protection or exclude the entire MoonDesk directory just to bypass a detection.

</details>

## Stack

| Part | Technology |
| --- | --- |
| Core | Rust |
| Server / async runtime | Axum + Tokio |
| TUI | Ratatui |
| Tunnel | ngrok |
| MCP server | Custom implementation |
| MCP protocol | `2025-11-25` |
| Browser runtime | MoonDesk-managed Chrome for Testing + native Rust CDP |
| Distribution | npm + verified native binaries |

## Contributing

Contributions are welcome.

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — development setup, required checks, and PR rules
- [`docs/RELEASING.md`](docs/RELEASING.md) — release pipeline and maintainer guidance

## Disclaimer

MoonDesk is an independent open-source project and is not affiliated with or endorsed by OpenAI. It can execute powerful local actions. Review permissions carefully and use it at your own risk.

## License

[MIT](LICENSE)
