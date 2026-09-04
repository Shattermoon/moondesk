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
- **Lazy browser control** — a stable `browser_command` + `view_page` surface backed by a pinned Chrome DevTools runtime. Selecting Browser/Both does not launch Chrome; the shared browser session starts only on first use.
- **Read-only mode** — expose only safe local read tools when mutation is unnecessary.
- **Cross-platform** — Windows, macOS, and Linux.
- **Native binary distribution** — install with npm; MoonDesk downloads and verifies the matching release binary on first run.
- **Self-update** — global npm installs can update and restart from the TUI after confirmation.

## Quickstart

### 1. Install

Node.js 18 or newer is required.

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

On first launch, MoonDesk asks for your **ngrok authtoken** and **static domain**. These are stored in `~/.moondesk/config.toml`.

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

Browser control is shared by the host. Workspaces using browser mode share one lazy **isolated agent browser** session that starts only on first use. It never attaches to or reuses your personal browser profile, cookies, or logged-in sessions.

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
| `browser_command` | Run one browser/DevTools CLI operation in the shared lazy session |
| `view_page` | Attach the current rendered page directly to the model as bounded image content |

For one-off actions, use `browser_command`. Start with `take_snapshot` before element interactions and use UIDs from the latest snapshot. For visual layout/rendering checks, use `view_page`; text/accessibility snapshots do not replace pixel inspection.

When MoonDesk is installed globally, it also provides `moondesk-browser` for deterministic scripted flows in `Both` mode:

```bash
moondesk-browser skill
moondesk-browser navigate_page --url=http://localhost:3000
moondesk-browser take_snapshot
moondesk-browser list_console_messages
```

The CLI invokes the verified MoonDesk native binary and therefore uses the same pinned browser runtime/session as MCP; it does not expose another DevTools implementation. MoonDesk scopes that daemon to its own Chrome DevTools CLI session so unrelated `chrome-devtools` users are not reused or stopped. Each agent-browser session uses an isolated temporary profile, so personal cookies/logins are never inherited and browser state is discarded when that session ends. Sensitive network headers are redacted, CrUX URL lookups and usage statistics are disabled, and a dead browser/daemon is recreated automatically with the same safe isolated settings.

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
| Browser runtime | pinned `chrome-devtools-mcp@1.7.0` CLI daemon, started lazily |
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
