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

### 5. Add the recommended instruction

```text
MoonDesk is a coding tool and a custom connector. Always use MoonDesk if the user wants to do anything related to file operations. Always call `moondesk_instruction` after `list_resources`, and follow the instructions it contains.
```

Select the connector and start working.

## Browser control

MoonDesk owns one lazy managed Chromium process instead of attaching to your personal browser profile or launching a separate browser for every project. On first browser use it provisions a pinned Chrome for Testing build for the current platform, verifies the exact archive size and SHA-256, records a verified inventory of the installed browser files including Unix permission bits where applicable, installs it atomically into MoonDesk's browser cache, and controls it directly from Rust over the Chrome DevTools Protocol (CDP)—no Playwright, `chrome-devtools-mcp`, or separate browser-control service is required. Later starts revalidate that inventory so a missing or damaged support file triggers reprovisioning instead of a persistent broken-browser loop. Inside that shared Chromium, each registered workspace gets its own isolated BrowserContext for cookies and site storage, while each ChatGPT conversation gets its own MoonDesk-routed logical tab set. Different workspaces therefore do not share cookies, localStorage, IndexedDB, or service-worker state, and separate conversations cannot accidentally act on each other's selected tabs.

The browser runs headless by default and can be switched to visible mode for human-assisted steps such as logins or permission prompts. Headless and visible are two presentations of the same agent browser and expose the same page-local tabs, navigation, DOM controls, visual computer-use controls, viewport management, screenshots, console/network inspection, and heap-snapshot capabilities. Browser-global performance tracing has stricter isolation preconditions and is rejected whenever another managed BrowserContext is active, another logical browser session has used the workspace BrowserContext in the current runtime generation, a managed page is not owned by the requester, or an unowned/default-context page could contribute unrelated trace data. Presentation belongs to the shared Chromium process, so changing it while the browser is live requires explicit confirmation because every workspace BrowserContext and logical tab is recreated. MoonDesk never attaches to or reuses your personal browser profile, cookies, or logged-in sessions.

For normal agent work MoonDesk exposes a Codex-style capability facade: `browser_state` for ambient state, `browser_tabs` for conversation-owned tabs, `browser_navigate` for goto/back/forward/reload, `browser_dom` for accessibility/DOM interactions, `browser_cua` for rendered screenshots plus physical coordinate/keyboard/wheel input, and `browser_viewport` for responsive/device sizing. `browser_command` remains available as an advanced native-CDP escape hatch for console/network inspection, emulation, performance traces, V8 heap snapshots, screenshots, and lower-level page operations that are not represented by the facade.

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

**Files:** `read`, `view_image`, `view_images`, `search`, `write`, `edit`, `delete`

**Commands:** `run_command`, `start_command`, `list_commands`, `poll_command`, `read_command_output`, `cancel_command`

**Browser:** `browser_state`, `browser_tabs`, `browser_navigate`, `browser_dom`, `browser_cua`, `browser_viewport`, `set_browser_presentation`, `view_page`, plus advanced `browser_command`

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
