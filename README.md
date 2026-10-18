<div align="center">

# MoonDesk

**Turn ChatGPT into a local coding agent.**

Files, shell commands, browser automation, multiple workspaces, durable workers, and session handoffs — through one local MCP host.

**No reverse engineering. No OpenAI API key. No separate agent service.**

[Quickstart](#quickstart) · [Features](#what-you-get) · [Workers](#experimental-chatgpt-workers) · [Browser](#browser-control) · [Safety](#safety) · [Contributing](#contributing)

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
   ├─ Experimental Workers
   └─ Session handoffs
```

MoonDesk does not upload your repository by itself. Only content returned by invoked tools—or sent by programs and browser activity you run—leaves your machine.

## What you get

| Capability | What it gives ChatGPT |
| --- | --- |
| **Files** | Read, search, edit, write, and delete inside a project workspace. |
| **Shell** | Run tests, builds, package installs, dev servers, Git, and other developer commands. |
| **Browser** | Control MoonDesk's isolated managed Chromium, inspect the DOM/console, emulate viewports, and visually inspect pages. |
| **Workspaces** | Serve multiple projects from one host, each with its own secret MCP URL. |
| **Handoffs** | Save continuation state when moving a long task to a fresh conversation. |
| **Workers (experimental)** | Delegate parallel tasks to durable ChatGPT Worker conversations through the optional Companion extension. |
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
> Workers are **experimental**. The optional Worker Companion extension is required for Workers/sub-agents and for the separate `user_browser_*` tools that control exact tabs in your existing Chromium browser.
>
> Normal files, shell, workspaces, handoffs, MCP use, and MoonDesk's isolated `browser_*` runtime do **not** require the extension.
>
> The Companion drives ChatGPT's web UI for Worker launches. MoonDesk fails closed if it cannot verify the requested model, reasoning effort, conversation placement, account binding, or previous Send state.

Workers let one conversation act as the **Core** and delegate independent work to durable **Worker** conversations in the same MoonDesk workspace.

```text
Core conversation
├─ spawn ──> Worker A ── task ──┐
├─ spawn ──> Worker B ── task ──┼─> collect results
└─ reuse ──> Worker C ── task ──┘
```

A Worker is a real ChatGPT conversation, not a disposable subprocess. When a task finishes, the Worker becomes idle and can be reused later with its conversation history intact.

### At a glance

| Setting / behavior | Current behavior |
| --- | --- |
| Worker target | Configurable from **1-8**, default **4** |
| Recommended range | **1-4** Workers |
| Hard limit | **8 active Workers per Core family** |
| Fresh launch concurrency | Up to **4** new Worker conversations at once |
| Placement | Fresh Workers are ordinary ChatGPT conversations, even when Core is inside a ChatGPT Project |
| Reuse | Reopens the same confirmed Worker conversation and browser affinity |
| Isolation | Bound to the resolved MoonDesk workspace and exact ChatGPT session metadata |

> Using **5-8 Workers** is supported, but it is a high-load mode and can hit ChatGPT/provider rate limits more easily, especially if the account already has other generations running.

ChatGPT Project names, connector names, MoonDesk workspace names, and folder names are display metadata only. MoonDesk does not use them as authorization keys.

### Set up the Worker Companion

You only need the Companion for Workers or `user_browser_*` control.

1. Start MoonDesk and open **Settings → Workers**.
2. Choose **Open Worker Companion folder**.
3. Open `chrome://extensions`, `edge://extensions`, or `brave://extensions`.
4. Enable **Developer mode**, choose **Load unpacked**, and select the folder MoonDesk opened.
5. Open ChatGPT in that browser. The Companion discovers MoonDesk's loopback-only bridge and pairs automatically.
6. Open the ChatGPT conversation you want to use as Core.
7. In the Companion popup, choose a confirmed ChatGPT model and reasoning effort, then save the Worker profile.

**Supported targets:**

- Google Chrome — signed-in acceptance tested.
- Microsoft Edge and Brave — supported through the same Chromium Manifest V3 APIs, but not separately live-tested in this cycle.
- Firefox and Safari — not supported.

MoonDesk keeps the prepared extension in `~/.moondesk/worker-companion` and refreshes those files on MoonDesk upgrades.

The Companion requests Chromium's `debugger` permission because exact-tab control uses CDP. An older unpacked installation may require a one-time permission approval or re-enable after upgrading.

<details>
<summary><strong>Source builds, manual repair, and fallback ZIP</strong></summary>

Contributors can load `extensions/moondesk-worker-companion` directly. Source checkouts and the GitHub Release ZIP do not contain MoonDesk's private installation capability, so those fallback installs do not auto-pair.

For a fallback install:

1. Open the Companion popup → **Advanced → Manual Repair**.
2. Copy the current **Manual repair code** from **MoonDesk Settings → Workers**.
3. Paste it once to pair that browser.

An explicitly revoked browser installation stays revoked and cannot silently auto-pair again; restoring it requires **Manual Repair**. Each GitHub Release also includes the checksummed `moondesk-worker-companion.zip` as a beta/recovery fallback.

See [`docs/WORKER_COMPANION_DISTRIBUTION.md`](docs/WORKER_COMPANION_DISTRIBUTION.md) for the full distribution and onboarding design.

</details>

### How delegation works

The `workers` MCP tool has two sides:

| Core operations | Purpose |
| --- | --- |
| `spawn` | Create a fresh Worker for a task |
| `status` | Inspect Worker/task state |
| `send` | Send follow-up instructions to a Worker |
| `collect` | Receive reports, completed results, and durable failures |
| `reuse` | Wake an idle Worker in its existing conversation |
| `recover` | Guardedly recover a stalled running Worker through the Companion |
| `retire` | Permanently stop reusing a Worker |

| Worker operations | Purpose |
| --- | --- |
| `claim` | Bind the newly opened ChatGPT conversation to its one-time Worker capability |
| `start` | Enter the durable working state before using project tools |
| `inbox` / `ack` | Receive and acknowledge Core messages |
| `report` | Send progress back to Core |
| `finish` | Complete the task and return the Worker to idle |

For a **fresh spawn**, Core can send both:

- `context`: a compact shared briefing with the overall goal, constraints, settled decisions, branch/PR state, completed validation, and sibling ownership.
- `task`: the Worker-specific assignment.

For **reuse**, MoonDesk does not resend the old shared briefing. The Worker already has its conversation history, so new or changed facts belong directly in the new `task`.

### Worker lifecycle

```text
spawn
  │
  ▼
queued ──> sent ──> claimed ──> working ──> finish ──> idle
            │
            └─ no claim within 2 min ──> failed + retired
```

Local IDE status exposes a derived `lifecycleStage` for the launch/start path:

| `lifecycleStage` | Meaning |
| --- | --- |
| `queued` | Browser preparation has not crossed ChatGPT Send |
| `sent` | The assignment reached, or may have reached, ChatGPT; claim is still incomplete |
| `claimed` | Conversation registration succeeded, but `workers start` has not completed |
| `working` | The durable start transition succeeded and project work may proceed |

A newly opened Worker claims its one-time capability and then calls `start`. While it is only claimed, MoonDesk blocks normal project tools, progress reports, and `finish`.

If claim races the Companion's final launch acknowledgement, MoonDesk uses the browser-attested claim operation to complete the same registration atomically instead of creating a second Worker.

`collect` is durable and idempotent when retried with the same `operation_id`. It returns progress reports, completed results, and `failed` task records with the failing stage plus a bounded underlying error.

MoonDesk retains the most recent **16 collection batches per Core family**, so replay is intentionally bounded rather than an indefinite delivery-ACK log.

### Durability and failure handling

| Situation | What MoonDesk does |
| --- | --- |
| A running Worker's browser tab closes | Keeps the Worker `running`, marks the attachment detached/no-tab, preserves the exact conversation binding, and still accepts `report` / `finish` if the server-side ChatGPT turn continues |
| The exact Worker conversation is opened again | Restores the browser attachment |
| Send may or may not have reached ChatGPT | Moves the launch into reconciliation and **never blindly sends the assignment again** |
| Fresh Worker launch is proven to have failed | Records a durable failure and frees that Worker slot |
| Reuse launch fails | Returns the durable Worker to idle instead of losing it |
| Fresh assignment was sent but never claims within two minutes | Invalidates the claim capability, records `worker_claim_timeout_after_send`, retires the Worker, and exposes the failure through `collect` |
| Core retires a sent-but-unclaimed Worker | Fences future claim and records `worker_registration_cancelled_after_send`; a message that already reached ChatGPT cannot be unsent |
| `collect` response is ambiguous | Retry with the same `operation_id` to replay the same retained batch |

MoonDesk does **not** infer completion from tab closure or silence after a Worker has entered the running state. A normal `finish` leaves the Worker idle for later reuse.

### Identity, account, and model safety

- Fresh Workers are routed to the selected paired Companion browser; Core is never cloned to manufacture a Worker.
- Reuse stays attached to the exact confirmed Worker thread/browser affinity.
- The Companion stores a one-way fingerprint of the ChatGPT account that created the Worker. Reuse/recovery fails closed if the proof is missing or the signed-in account differs.
- MoonDesk verifies the actual model and reasoning effort before every Worker assignment. The cached model catalog only avoids unnecessary rediscovery.
- If a Worker conversation still exposes an old cached connector schema after a MoonDesk upgrade, refresh/reconnect that Worker connector and start a fresh Worker conversation.
- Once a Worker has claimed **and** started, the generalized Worker connector can expose that conversation's `user_browser_*` tools for independent browser QA.
- Unclaimed, claimed-but-not-started, finished, and unrelated chats do not receive that authority. ChatGPT/OpenAI tabs remain protected.

### Local Codex / IDE Core

For a local IDE or Codex Core:

1. Configure the machine-local `moondesk mcp` stdio server once.
2. In the fixed Worker ChatGPT account, configure one **generalized MoonDesk Worker connector** using the installation-wide Worker connector URL from **MoonDesk Settings → Workers**.
3. Fresh Workers claim a one-time capability that resolves the owning MoonDesk workspace; that exact Worker conversation remains bound to the workspace afterward.

The generalized Worker connector is intentionally not tied to one project. Switching the ChatGPT account used by Codex does not require changing the Worker account or either connector configuration.

If a local Worker family is pinned to an offline Companion browser and its browser launch is already terminal, local IDE Core can use `workers` with `action=clear` to discard only that workspace-local family and choose an online Worker browser again.

MoonDesk refuses that scoped clear while any matching launch is still queued, leased, send-started, paused, or awaiting reconciliation.

<details>
<summary><strong>Capacity cleanup and host-wide reset</strong></summary>

MoonDesk retains up to **64 Core families**. Automatic compaction will not discard an idle reusable Worker, an uncollected result, or a still-replayable non-empty `collect` receipt merely to make room.

If an old Core conversation is gone and its idle family cannot be retired from that chat:

1. Open **MoonDesk Settings → Workers → Release inactive Worker capacity**.
2. Choose the workspace.
3. Type `CLEANUP` to confirm.

MoonDesk only removes idle/retired families when Worker state and browser-launch history prove there is no active or ambiguous Send, pending Worker message/report, or uncollected result. The ChatGPT conversations themselves are preserved.

The Companion popup's **Clear workers** action is a stronger, host-wide reset. It clears retained Worker ownership and managed launch history while leaving ChatGPT conversations untouched.

MoonDesk publishes a durable reset epoch so paired browsers discard stale Worker history before another Send.

If a browser already crossed `send_started`, Clear workers waits for that browser to acknowledge the new epoch. A missing or stuck browser leaves the reset retryable instead of allowing a later stale Send.

A Send that already reached ChatGPT cannot be unsent. Clear workers reports commands that had already crossed MoonDesk's Send boundary so the UI can warn about that possibility.

</details>

## Browser control

MoonDesk provides two deliberately separate browser surfaces:

| Surface | What it controls | Profile / storage | Requires Companion? |
| --- | --- | --- | --- |
| `browser_*` | MoonDesk's managed Chrome for Testing runtime | MoonDesk-owned; isolated per workspace | No |
| `user_browser_*` | Exact tabs in your existing Chrome / Edge / Brave browser | Your existing browser profile | Yes |

### Managed browser (`browser_*`)

MoonDesk owns **one lazy Chromium process per host** and controls it directly from Rust over Chrome DevTools Protocol (CDP). It does not use Playwright, `chrome-devtools-mcp`, or your personal browser profile.

```text
MoonDesk-managed Chromium
├─ Workspace A BrowserContext
│  ├─ Conversation 1 tabs
│  └─ Conversation 2 tabs
└─ Workspace B BrowserContext
   └─ Conversation 3 tabs
```

| Boundary | Behavior |
| --- | --- |
| Browser process | Shared by the MoonDesk host |
| Cookies / localStorage / IndexedDB / service workers | Isolated per workspace BrowserContext |
| Selected tabs / page authority | Isolated per ChatGPT conversation |
| Personal browser profile | Never attached or reused |
| Startup | Lazy; Chromium starts on first browser operation |
| Default presentation | Headless |

On first use, MoonDesk downloads the pinned Chrome for Testing build for the current platform, verifies its exact archive size and SHA-256, records a verified inventory of installed files, and publishes the browser atomically into its cache.

Later launches revalidate that inventory so missing or damaged files trigger reprovisioning instead of a persistent broken-browser state.

#### Agent-facing browser tools

| Tool | Typical use |
| --- | --- |
| `browser_state` | Inspect browser/runtime state |
| `browser_tabs` | List, open, select, or close conversation-owned tabs |
| `browser_navigate` | Go to a URL, back, forward, or reload |
| `browser_dom` | Accessibility snapshots, forms, clicks, text input, DOM interaction |
| `browser_cua` | Rendered screenshots and coordinate/keyboard/wheel input |
| `browser_viewport` | Desktop/mobile viewport and device emulation |
| `view_page` | Inspect actual rendered pixels |
| `browser_command` | Advanced native-CDP operations not covered by the normal facade |

Use `browser_dom` for ordinary web controls. Use `browser_cua` or `view_page` when the task depends on what the page actually looks like.

#### Headless and visible mode

Headless and visible are two presentations of the **same managed browser** and expose the same page-local capabilities. Visible mode is useful for logins, permission prompts, CAPTCHAs, or other human-assisted steps.

> [!IMPORTANT]
> Presentation is process-global. Switching presentation while Chromium is live requires explicit confirmation because every MoonDesk workspace BrowserContext and logical tab is recreated.

<details>
<summary><strong>Advanced tracing isolation</strong></summary>

Browser-global performance tracing is allowed only when MoonDesk can prove that unrelated work cannot contribute data.

Tracing is rejected when any of these isolation conditions fail:

- another managed BrowserContext is active;
- another logical session used the workspace BrowserContext in the current runtime generation;
- the page is not owned by the requester; or
- an unowned/default-context page could contribute unrelated trace data.

</details>

### Existing-browser control (`user_browser_*`)

When the optional Companion extension is installed, MoonDesk can deliberately attach to **specific tabs** in your existing Chromium browser.

| Tool | Purpose |
| --- | --- |
| `user_browser_tabs` | List, attach, open, release, or explicitly close tabs |
| `user_browser_snapshot` | Read the controlled page structure |
| `user_browser_screenshot` | Capture rendered pixels |
| `user_browser_navigate` | Navigate the controlled tab |
| `user_browser_action` | Perform bounded page input |
| `user_browser_evaluate` | Run bounded page JavaScript |
| `user_browser_console` / `user_browser_network` | Read bounded diagnostics |

This surface is **not PC control** and does not merge your personal browser profile into the managed `browser_*` runtime.

Safety rules for attached user tabs:

- Attachments are leased to the exact ChatGPT conversation.
- Navigation invalidates old page references.
- Uncertain input is not blindly replayed.
- HTTPS tabs on `chatgpt.com` and `chat.openai.com` are protected from observation and interaction; reaching one detaches the controlled tab.
- A multicolor edge/corner glow shows while MoonDesk holds control.
- Releasing removes control without closing the tab; closing is a separate explicit action.

### Browser CLI

The `moondesk browser` CLI is the deterministic scripting path for the managed browser:

```bash
moondesk browser navigate_page --url=http://localhost:3000
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
```

It shares the resolved workspace's BrowserContext/login state but uses its own logical tab session, so scripts cannot steal a ChatGPT conversation's selected page.

For deeper runtime invariants, see [`docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md`](docs/BROWSER_RUNTIME_ARCHITECTURE_HARDENING.md).

## Workspaces and handoffs

### Workspaces

One MoonDesk host can serve multiple projects while keeping their local authority separate.

| Workspace-owned state | Isolation |
| --- | --- |
| Root directory | Per workspace |
| Secret MCP URL | Per workspace |
| Command jobs and retained output | Per workspace |
| History / quotas | Per workspace |
| Handoff state | Per workspace |
| Managed browser site storage | Per workspace BrowserContext |

Use `[w] Workspaces` to add, rename, inspect, copy, rotate, or remove projects.

> [!IMPORTANT]
> MoonDesk refuses filesystem roots and protected operating-system directories as workspace roots. Older invalid entries remain visible as **BLOCKED**, but their connector and workspace tools are disabled until you move to a valid project workspace.

Launching `moondesk` from a protected directory never silently attaches that directory to a running host. On a fresh interactive launch, MoonDesk asks you to choose a valid project folder instead.

### Handoffs

Handoffs are explicit checkpoints for moving unfinished work to another ChatGPT conversation.

| Tool | Purpose |
| --- | --- |
| `create_handoff` | Save the goal, completed work, decisions, validation, blockers, next steps, Git state, and current MoonDesk jobs |
| `resume_handoff` | Reload the checkpoint and compare saved state with the current workspace |
| `complete_handoff` | Mark the continuation finished |

> [!CAUTION]
> Never put credentials, tokens, passwords, private keys, or other secrets in handoff text.

Stopping the MoonDesk host disconnects every active workspace connector, so shutdown from the live dashboard requires confirmation.

## Safety

| Tool surface | Boundary |
| --- | --- |
| Dedicated file tools | Stay inside the selected workspace and reject path traversal plus symlink/junction escapes |
| Shell tools | Run your normal developer shell with the workspace only as the working directory |
| Managed browser | Uses MoonDesk-owned browser storage; never inherits your personal browser profile |
| User-browser control | Requires the Companion and explicit tab attachment |

> [!WARNING]
> `run_command` and `start_command` inherit your normal environment, credentials, `PATH`, and OS permissions. **The workspace directory is not an OS sandbox.**

Use read-only mode when mutation is unnecessary. For untrusted code, use a VM or container.

> [!CAUTION]
> Never share a workspace MCP URL. Treat it as a credential: anyone who can use it may be able to invoke the tools you exposed.

## Reference

<details>
<summary><strong>Tools</strong></summary>

| Category | Tools |
| --- | --- |
| Guidance / handoffs | `moondesk_instruction`, `create_handoff`, `resume_handoff`, `complete_handoff` |
| Experimental Workers | `workers` |
| Files | `read`, `view_image`, `view_images`, `search`, `write`, `edit`, `delete` |
| Commands | `run_command`, `start_command`, `list_commands`, `poll_command`, `read_command_output`, `cancel_command` |
| Managed browser | `browser_state`, `browser_tabs`, `browser_navigate`, `browser_dom`, `browser_cua`, `browser_viewport`, `set_browser_presentation`, `view_page`, advanced `browser_command` |
| Existing-browser Companion | `user_browser_tabs`, `user_browser_snapshot`, `user_browser_screenshot`, `user_browser_navigate`, `user_browser_action`, `user_browser_evaluate`, `user_browser_console`, `user_browser_network` |

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

**macOS terminal behavior**

- MoonDesk keeps your existing terminal profile instead of installing or forcing its own.
- Older Apple Terminal versions without reliable truecolor support use a stable 256-color compatibility palette for theme and ClippyMoon rendering.
- Terminal.app 2.15+ and other truecolor-capable terminals keep full RGB output.
- When upgrading from older MoonDesk versions, a tab still using the legacy `MoonDesk` Terminal.app profile is restored to your Terminal default settings.
- Set `MOONDESK_SKIP_MACOS_TERMINAL_PROFILE=1` only if you want to skip that legacy-profile migration.

</details>

<details>
<summary><strong>Windows antivirus note</strong></summary>

MoonDesk verifies the downloaded native executable against the SHA-256 published with the matching GitHub Release before launching it.

If Windows Security or another antivirus quarantines the verified executable, update security definitions and check **Protection history**, then run `moondesk` again.

Do not disable antivirus protection or exclude the entire MoonDesk directory just to bypass a detection.

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
