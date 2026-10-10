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

MoonDesk runs on **Windows, macOS, and Linux**.

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
> The Worker Companion extension is **optional for MoonDesk itself** and is required only for the experimental **Workers/sub-agents** feature. Files, shell, browser automation, workspaces, handoffs, and normal MCP use continue to work without it. When Workers are used, the companion drives ChatGPT's web UI, so MoonDesk fails closed when it cannot positively confirm the requested model, reasoning effort, exact placement, or previous Send state.

Workers let one ChatGPT conversation act as the **Core** and delegate independent tasks to durable worker conversations in the same MoonDesk workspace. The configurable target is **1-8 workers**, defaults to **4**, and has a hard maximum of **8 active workers per Core family**. **1-4 workers is the recommended operating range.** Using **5-8 workers** is supported but can trigger ChatGPT/provider rate limits, especially when the account already has other conversations generating at the same time, so higher counts are best treated as an advanced/high-load mode. MoonDesk launches up to four fresh worker conversations concurrently so the recommended group does not serialize behind one slow launch. Worker identity is bound to the route-resolved workspace plus ChatGPT's exact session metadata; a different conversation in the same workspace does not inherit Core or worker authority. ChatGPT Project names, connector names, MoonDesk workspace names, and folder names are display-only and are never used as routing or authorization keys.

### Optional Worker Companion setup

You do **not** need the extension for normal MoonDesk use. Install it only when you want Workers/sub-agents. MoonDesk embeds the exact companion files from its own build and synchronizes them into the stable `~/.moondesk/worker-companion` folder whenever MoonDesk starts; browser installation remains an explicit user opt-in, but later MoonDesk upgrades do not require a second companion-update step. An already-loaded companion can self-reload once when its MoonDesk release version or runtime revision no longer matches. The supported browser targets are **Google Chrome, Microsoft Edge, and Brave**. Current signed-in acceptance has been run in Chrome; Edge and Brave use the same Chromium Manifest V3 `chrome.*` extension APIs but have not been separately live-tested in this cycle. Other Chromium-based browsers may work but are not supported targets; Firefox and Safari are not currently supported. Each GitHub Release also carries the same checksummed `moondesk-worker-companion.zip` as a beta/recovery fallback. The full release/onboarding plan is in [`docs/WORKER_COMPANION_DISTRIBUTION.md`](docs/WORKER_COMPANION_DISTRIBUTION.md).

To enable Workers:

1. Start MoonDesk and open **Settings → Workers**.
2. Choose **Open Worker Companion folder**. MoonDesk opens its stable Worker Companion folder.
3. Open `chrome://extensions` (or `edge://extensions` / `brave://extensions`), enable **Developer mode**, choose **Load unpacked**, and select the folder MoonDesk opened.
4. Open ChatGPT in that same browser. The extension discovers MoonDesk's loopback-only bridge and pairs automatically; there is no per-chat token step. Multiple browser installations may remain paired independently.
5. Open the ChatGPT conversation you want to use as the Core. No Project/workspace binding step is required.
6. The companion automatically discovers the signed-in account's available ChatGPT models and reasoning efforts. Open the popup, choose from the confirmed catalog, and save the worker profile. **Refresh models** is only a repair/revalidation fallback. The companion reads ChatGPT's provider-owned picker state instead of relying on translated labels, and supports the current provider effort lanes (`Instant`, `Minimal`, `Low`, `Medium`, `High`, `Extra High`, `Max`, `Ultra`, and `Pro`) when the account actually offers them. Existing `Extra High` profiles remain compatible with the provider's `xhigh`/`max` migration.

Contributors running from source can still load `extensions/moondesk-worker-companion` directly. Source checkouts and the GitHub Release ZIP intentionally do **not** contain MoonDesk's private installation capability, so those fallback installs do not auto-pair. Open the companion popup → **Advanced → Manual Repair**, copy the current **Manual repair code** from **MoonDesk Settings → Workers**, paste it once, and pair that browser. The normal MoonDesk-prepared `~/.moondesk/worker-companion` folder remains the recommended path and auto-pairs without this step.

Workers V1 always creates fresh workers as ordinary ChatGPT conversations, even when the Core is inside a ChatGPT Project. Project membership remains useful Core routing metadata, but it is not a worker placement target and MoonDesk never clones the Core conversation to create a worker. Fresh workers are routed to the paired browser that positively observes the exact Core. After the worker conversation is confirmed, reuse remains attached to that durable thread/browser affinity; legacy workers that were previously created inside a Project can still be reused by their exact confirmed conversation binding.

The last confirmed model catalog is stored in extension-local storage, so reopening the popup does not require rediscovery. MoonDesk still re-verifies the actual model and effort in ChatGPT before sending every worker assignment. If MoonDesk has been upgraded but an existing ChatGPT conversation does not expose the `workers` tool, refresh/reconnect the Custom Connector and start a fresh conversation because ChatGPT may retain an older connector schema.

MoonDesk keeps retained Worker state bounded to 64 Core families. Automatic compaction never discards an idle reusable worker, an uncollected result, or a still-replayable non-empty `collect` receipt merely because another Core needs capacity. If an old Core conversation is gone and its idle family can no longer be retired from that chat, use **MoonDesk Settings → Workers → Release inactive Worker capacity**, choose the workspace, and type `CLEANUP` to confirm. This host-local action is workspace-scoped and only removes idle/retired families when both Worker state and managed browser-launch history prove there is no active or ambiguous Send, pending worker message/report, or uncollected task result. ChatGPT conversations are preserved; terminal MoonDesk launch history for the removed families is cleared with them. Because this is an explicit destructive recovery action, the confirmation also reports when retained `collect` replay receipts for those inactive families will be discarded.

### Worker lifecycle

The MCP `workers` tool supports Core operations such as spawn, status, send, collect, reuse, and retire, plus worker-side claim, inbox, report, start, and finish operations. For a fresh spawn, Core can pass a compact `context` briefing containing the overall goal, user constraints and settled decisions, branch/PR state, completed validation, and sibling-worker ownership; MoonDesk places that shared context before the worker-specific assignment so a new worker does not need to rediscover conversation-only decisions. Reuse wakes the same durable ChatGPT conversation and does not resend that shared briefing; the worker keeps its existing conversation history, while any newly relevant or changed facts belong directly in the new `task`. A newly opened worker must claim its one-time capability before MoonDesk binds that ChatGPT session to the durable worker record. `collect` uses a stable `operation_id`: if its response is dropped, retrying promptly with the same ID replays the same durable batch. Workers V1 intentionally keeps only the most recent **16 collection batches per Core family**, so this is a bounded retry window rather than an indefinite delivery-ACK protocol; Core should retry an ambiguous collect before issuing enough unrelated collects to evict that receipt.

Workers survive MoonDesk/browser tab restarts as durable records. Closing the tab of a **running** worker does not end its task: MoonDesk keeps the worker `running`, marks its browser attachment as `detached`/no-tab, preserves the exact conversation binding, and still accepts that worker's reports and `finish` call if its server-side ChatGPT turn continues. Opening that exact worker conversation again restores the attachment. MoonDesk deliberately does not infer completion or make the worker reusable from tab closure or a silence timer alone, because Workers V1 does not yet have CoS-style turn/request-level authority to distinguish a late old-turn call from a new assignment. Completing a task leaves the worker idle so a later task can reopen the same confirmed ChatGPT conversation. Retiring an idle worker frees its display slot; MoonDesk refuses retirement once a browser launch may have crossed the Send boundary.

Browser commands use durable leases and acknowledgements. If MoonDesk cannot tell whether ChatGPT accepted a Send, the command moves to reconciliation and **never blindly sends the assignment again**. Failed worker launches are not exposed as delayed manual replays: a proven pre-Send failure automatically frees a fresh worker slot, while a failed pre-Send wake returns an existing durable worker to idle so Core can choose the next assignment. The companion's **Clear workers** action is a global Workers reset across the MoonDesk host: it discards retained Worker ownership and managed launch history while leaving the ChatGPT conversations themselves untouched. MoonDesk publishes a durable reset epoch so every paired browser drops stale local worker history on reconnect/status/pump and rechecks that epoch immediately before a browser Send after `send_started`. If another paired browser had already crossed `send_started`, Clear Workers does not report success until that browser acknowledges the new epoch after invalidating its local worker generation; a missing/stuck browser leaves Clear safely retryable instead of allowing a later post-clear Send. A Send that already reached ChatGPT cannot be unsent; Clear Workers reports commands that had already crossed MoonDesk's Send boundary so the popup can warn about that possibility.

## Browser control

MoonDesk owns one lazy agent Chromium process instead of attaching to your personal browser profile or launching a separate browser for every project. Inside that shared Chromium, each registered workspace gets its own isolated BrowserContext for cookies and site storage, while each ChatGPT conversation gets its own MoonDesk-routed logical tab set. Different workspaces therefore do not share cookies, localStorage, IndexedDB, or service-worker state, and separate conversations cannot accidentally act on each other's selected tabs.

The browser runs headless by default and can be switched to visible mode for human-assisted steps such as logins or permission prompts. Presentation belongs to the shared Chromium process, so changing it while the browser is live requires explicit confirmation because every workspace BrowserContext and logical tab is recreated. MoonDesk never attaches to or reuses your personal browser profile, cookies, or logged-in sessions.

```bash
moondesk browser navigate_page --url=http://localhost:3000
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
```

The `moondesk browser` CLI shares the resolved workspace's BrowserContext/login state but uses its own logical tab session, so scripted browser commands cannot steal a ChatGPT conversation's active page. Use `view_page` when the task depends on actual rendered pixels rather than only a text/accessibility snapshot.

For browser-runtime invariants and implementation details, see [`docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md`](docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md).

## Workspaces and handoffs

Each workspace keeps its own root, secret connector URL, command jobs, retained output, history, and handoff state. Use `[w] Workspaces` to add, rename, inspect, copy, rotate, or remove projects.

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

**Browser:** `set_browser_presentation`, `browser_command`, `view_page`

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
| Browser runtime | pinned `chrome-devtools-mcp@1.7.0` |
| Distribution | npm + verified native binaries |

## Contributing

Contributions are welcome.

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — development setup, required checks, and PR rules
- [`docs/RELEASING.md`](docs/RELEASING.md) — release pipeline and maintainer guidance

## Disclaimer

MoonDesk is an independent open-source project and is not affiliated with or endorsed by OpenAI. It can execute powerful local actions. Review permissions carefully and use it at your own risk.

## License

[MIT](LICENSE)
