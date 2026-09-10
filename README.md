# MoonDesk

**Turn ChatGPT Chat into a local coding agent.**

MoonDesk is an open-source local MCP server that gives ChatGPT tools to read and edit files, run commands, manage long-running jobs, and control Chromium-based browsers — without using the OpenAI API.

```bash
npm install -g moondesk
```

> [!IMPORTANT]
> MoonDesk runs tools locally on your computer. Review commands before running them, and use an isolated environment for untrusted projects or code.

## Why MoonDesk?

MoonDesk lets you use the ChatGPT subscription you already have for local coding work. ChatGPT connects to MoonDesk through a Custom Connector, and MoonDesk exposes your project as a set of MCP tools.

```text
ChatGPT Chat
     │
     │ Custom Connector
     ▼
  MoonDesk
  ├─ Files
  ├─ Shell jobs
  ├─ Workspaces
  └─ Browser / DevTools
```

No reverse engineering. No API key. No separate agent service.

## Features

- **Local file tools** — read, search, write, edit, and delete inside a workspace.
- **Shell commands** — run short commands or start background jobs with polling, preserved output, and cancellation.
- **Multiple workspaces** — serve several projects from one MoonDesk process, each with its own secret MCP URL.
- **Headless agent browser by default** — a small `set_browser_presentation` + `browser_command` + `view_page` surface backed by a pinned Chrome DevTools runtime. Selecting Browser/Both does not launch Chrome; the shared isolated browser starts lazily on first use without opening a desktop window unless the user or agent switches it to visible mode.
- **Read-only mode** — expose only safe local read tools when mutation is unnecessary.
- **Cross-platform** — Windows, macOS, and Linux.
- **Native binary distribution** — install with npm; MoonDesk downloads and verifies the matching release binary on first run.
- **Self-update** — global npm installs can update and restart from the TUI after confirmation.

## Quickstart

### 1. Install

MoonDesk requires Node.js `^20.19.0 || ^22.12.0 || >=23`, matching the pinned browser runtime. Node 21 and Node 22.0-22.11 are not supported.

```bash
npm install -g moondesk
```

### 2. Run

Start MoonDesk inside the project you want to use:

```bash
cd your-project
moondesk
```

Choose:

- `Control Computer`
- `Control Browser`
- `Both`

On first launch, MoonDesk asks for your **ngrok authtoken** and **static domain**. These are stored in `~/.moondesk/config.toml`. You can update either value later from Settings; the authtoken editor is masked and Settings only shows whether a token is configured.

### 3. Copy the workspace URL

Open `[w] Workspaces` in the TUI and copy the MCP URL for your project.

Each workspace has its own secret URL, for example:

```text
https://your-domain.ngrok-free.dev/<workspace-secret>/mcp
```

### 4. Create the ChatGPT connector

Open ChatGPT's Custom Connector settings and create a connector with:

```text
Name: MoonDesk · <project name>
MCP Server URL: <URL copied from MoonDesk>
Authentication: None
```

For full coding-agent behavior, allow write actions only when you trust the current workspace and task.

### 5. Add the recommended instruction

Add this to your ChatGPT custom instructions:

```text
MoonDesk is a coding tool and a custom connector. Always use MoonDesk if the user wants to do anything related to file operations. Always call `moondesk_instruction` after `list_resources`, and follow the instructions it contains.
```

That's it. Select the MoonDesk connector in a ChatGPT conversation and start working.

### Windows security software

MoonDesk verifies the downloaded native binary against the SHA-256 published with the matching GitHub Release before launching it. If Windows Security or another antivirus quarantines that verified executable, update its security definitions and check Protection History, then run `moondesk` again. Do not disable antivirus protection or exclude the whole MoonDesk directory just to bypass a detection; report suspected false positives with the exact release version and SHA-256 instead.

## Multiple projects

One MoonDesk host can serve several project roots at once:

```text
one MoonDesk process
one local server :3200
one ngrok domain

├── Project A -> /<secret-A>/mcp -> D:\ProjectA
├── Project B -> /<secret-B>/mcp -> D:\ProjectB
└── Project C -> /<secret-C>/mcp -> D:\ProjectC
```

Each workspace keeps its own file boundary, command jobs, retained output, history, and secret connector URL.

Use `[w] Workspaces` to add, rename, inspect, copy, rotate, or remove projects. On Windows, `[b] Explorer` opens the native Explorer folder picker for adding a workspace; `[a] Path` remains available for manual path entry. Launching `moondesk` from another project while a host is already running can attach that directory to the existing host instead of starting another server.

Browser control is shared by the host. Workspaces using browser mode share one lazy **isolated agent browser** session that starts only on first use. It runs **headless by default** at a deterministic 1280×800 initial viewport, so normal agent work does not open a Chrome window, while `view_page` and screenshots still inspect the browser's rendered pixels. Agents can still resize or emulate the target viewport for responsive QA. Press `[v]` in the live dashboard to switch between hidden/headless and visible presentation; the Browser status row shows the current mode and the toggle hint. In `multi-tools` mode an agent can also call `set_browser_presentation`, primarily when a login, CAPTCHA, permission prompt, or other step needs human input. If changing presentation would close a live session, the tool refuses the change and reports `confirmation_required`; the agent must get explicit user approval before retrying with `confirm_restart=true`. Changing presentation while the browser is running closes that temporary session first, so its tabs, cookies, storage, page state, and snapshot UIDs are discarded. Switching to visible starts a fresh empty visible browser immediately; if that headful launch fails, MoonDesk reverts the setting to headless so later agent browser work remains usable. A human-assisted visible session should stay visible while its entered state is still needed; switching back to headless closes that session and returns to lazy hidden startup on the next browser action. MoonDesk never attaches to or reuses your personal browser profile, cookies, or logged-in sessions.

Because every workspace shares this host and public tunnel, stopping MoonDesk disconnects all active workspace connectors. Pressing `q` or `Ctrl+C` in the live dashboard therefore opens a shutdown confirmation instead of stopping the host immediately; `Enter` confirms and `Esc` keeps MoonDesk running.

## Tools

In `multi-tools` mode MoonDesk exposes 12 local tools:

| Tool | Purpose |
| --- | --- |
| `moondesk_instruction` | MoonDesk usage guidance |
| `read` | Read workspace files |
| `search` | Search workspace text |
| `write` | Create or overwrite files |
| `edit` | Replace exact text |
| `delete` | Delete files or directories |
| `run_command` | Run a short shell command |
| `start_command` | Start a background command |
| `list_commands` | List current and retained jobs |
| `poll_command` | Read incremental job output |
| `read_command_output` | Read preserved command output |
| `cancel_command` | Stop a job and its process tree |

Use `run_command` for short work. Use `start_command` + `poll_command` for builds, tests, package installs, dev servers, and other long-running commands. Polls long-wait by default and report elapsed, idle, and timeout timing so agents can avoid rapid blind polling.

`read-only` mode removes local mutation/shell tools. In Browser/Both mode it still permits bounded browser inspection, while state-changing browser commands and browser file-output flags remain blocked.

Browser mode has a stable tool catalog instead of forwarding the full Chrome DevTools MCP schema:

| Browser tool | Purpose |
| --- | --- |
| `set_browser_presentation` | In `multi-tools`, request headless or visible presentation; live-session changes require explicit restart confirmation |
| `browser_command` | Run one browser/DevTools CLI operation in the shared lazy session |
| `view_page` | Attach the current rendered page directly to the model as bounded image content |

For one-off actions, use `browser_command`. Navigate first, set the target viewport, then run `take_snapshot` before element interactions and use UIDs from the latest snapshot. Use `resize_page` for ordinary desktop window sizes. For exact tablet/mobile QA, use `emulate --viewport=390x844x1,mobile,touch` (or another target size); Chromium can clamp very narrow desktop windows, and viewport emulation can recreate the page context, so take a fresh snapshot afterward. For visual layout/rendering checks, use `view_page`; text/accessibility snapshots do not replace pixel inspection.

The same `moondesk` CLI also has a `browser` subcommand for deterministic scripted flows in `Both` mode:

```bash
moondesk browser skill
moondesk browser navigate_page --url=http://localhost:3000
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
```

The `browser` subcommand is handled by MoonDesk itself and acts as a lightweight authenticated localhost client to the **running MoonDesk host**. It does not launch a separate browser runtime, so separate shell commands, MCP `browser_command`, and MCP `view_page` all operate on the same host-owned agent-browser session. MoonDesk directly owns the pinned `chrome-devtools-mcp` stdio process tree and its isolated Chromium child instead of relying on the upstream detached CLI daemon. The browser starts headless by default; visible mode changes only Chromium presentation, not the browser tool surface or isolation model. Each agent-browser session uses an isolated temporary profile, so personal cookies/logins are never inherited and browser state is discarded when that session ends. Sensitive network headers are redacted, CrUX URL lookups and usage statistics are disabled, local-file navigation is blocked, and a lost runtime is invalidated so the next browser operation starts a fresh isolated session without replaying the ambiguous failed action.

The browser runtime is intentionally pinned to `chrome-devtools-mcp@1.7.0`. Version `1.8.0` changed required CLI argument shapes for commands MoonDesk currently invokes with the 1.7 contract, so upgrading the pin requires an explicit command-contract migration and the full browser regression matrix rather than a blind dependency bump.

## Workspace security

Dedicated file tools are confined to the selected workspace. MoonDesk rejects path traversal and symlink/junction escapes outside that root.

Shell commands are different. `run_command` and `start_command` launch your normal developer shell with the workspace as its working directory. They inherit your normal environment, credentials, PATH, and OS permissions.

**The working directory is not an OS sandbox.** A shell command can access anything your user account can access.

Use:

- `read-only` mode when write/shell access is unnecessary;
- a VM or container when you need OS-level isolation;
- secret rotation from `[w] Workspaces` if a workspace MCP URL is ever exposed.

> [!CAUTION]
> Never share a workspace MCP URL. Treat it like a credential.

## Configuration

| Setting | Default / location |
| --- | --- |
| Config | `~/.moondesk/config.toml` |
| Port | `3200` |
| Port override | `PORT` |
| Initial workspace override | `WORKSPACE_ROOT` |
| Global instructions | `~/.moondesk/AGENTS.md` |
| Codex-compatible instructions | `~/.codex/AGENTS.md` |

MoonDesk also checks `AGENTS.md` in the current workspace. Workspace instructions take priority.

On macOS Terminal.app, MoonDesk can manage a dedicated terminal profile. Set `MOONDESK_SKIP_MACOS_TERMINAL_PROFILE=1` to disable that behavior.

## Stack

| Part | Technology |
| --- | --- |
| Core | Rust |
| Async runtime / server | Tokio + Axum |
| TUI | Ratatui |
| Tunnel | ngrok |
| MCP server | Custom implementation |
| MCP protocol | `2025-11-25` |
| Browser runtime | pinned `chrome-devtools-mcp@1.7.0` stdio child owned by MoonDesk, lazy + headless by default with opt-in visible presentation |
| Distribution | npm + native binaries |

## Contributing

Contributions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup, required checks, PR rules, and security-sensitive invariants.

Release maintainers should also read [`docs/RELEASING.md`](docs/RELEASING.md).

## ClippyMoon

<p align="center">
  <img src="docs/images/clippymoon.gif" alt="ClippyMoon" width="420"><br>
  <em>ClippyMoon!</em>
</p>

## Disclaimer

MoonDesk is an independent open-source project and is not affiliated with or endorsed by OpenAI. It can execute powerful local actions. Review permissions carefully and use it at your own risk.

## License

[MIT](LICENSE)
