---
name: moondesk-browser
description: Control MoonDesk's shared Chromium session without exposing the full Chrome DevTools MCP schema to the model.
---

# MoonDesk Browser

Use MoonDesk's browser runtime for local web-app inspection, UI testing, console/network debugging, accessibility snapshots, performance checks, and scripted browser flows.

## Interfaces

- **MCP `browser_command`**: preferred for one browser action at a time and the only browser action primitive needed in Browser-only mode.
- **MCP `view_page`**: preferred whenever appearance matters. It returns the current rendered page as model-visible pixels.
- **`moondesk-browser` CLI**: preferred in Both mode when several deterministic browser actions are easier to express as a shell script or loop. It calls the same MoonDesk native browser runtime, so it shares the same isolated agent-browser session as MCP without touching the user's personal browser profile.

Do not invoke `npx chrome-devtools-mcp` directly. MoonDesk pins and manages the compatible Chrome DevTools runtime.

## Core workflow

1. Navigate or select the target page.
2. Run `take_snapshot` before DOM interactions. UIDs belong to the latest snapshot and can become stale after navigation or substantial DOM changes.
3. Use snapshot UIDs with `click`, `fill`, `hover`, `drag`, `upload_file`, etc.
4. Take another snapshot after a state-changing action when the page structure may have changed.
5. Use `view_page` for visual judgment. Accessibility/text snapshots are structural evidence, not a substitute for seeing the rendered page.
6. Inspect console/network/performance data only when it helps the task; do not collect large traces by default.

The browser starts lazily on the first operation. Normal commands reuse the existing session. MoonDesk retries once when the browser connection itself has died; ordinary action errors such as a stale UID are returned without restarting the browser.

## Common CLI commands

```text
moondesk-browser list_pages
moondesk-browser new_page https://example.com
moondesk-browser navigate_page --url=https://example.com
moondesk-browser take_snapshot
moondesk-browser click 1_23 --includeSnapshot
moondesk-browser fill 1_31 "hello"
moondesk-browser press_key Enter --includeSnapshot
moondesk-browser resize_page 390 844
moondesk-browser list_console_messages
moondesk-browser list_network_requests
moondesk-browser evaluate_script "() => ({title: document.title, href: location.href})"
```

Use `moondesk-browser <command> --help` when a command's positional arguments or flags are unclear.

## Scripted flows

For repetitive deterministic work in Both mode, keep the browser operations in one shell script instead of spending one MCP schema/tool call per action. Example:

```powershell
moondesk-browser navigate_page --url=http://localhost:3000
moondesk-browser resize_page 390 844
moondesk-browser take_snapshot
moondesk-browser list_console_messages
```

Prefer the CLI for orchestration, but return to `view_page` whenever the task requires actual visual inspection.

## Safety and lifecycle

- Do not run browser lifecycle commands (`start`, `status`, `stop`) through MCP `browser_command`; MoonDesk owns that lifecycle.
- Do not stop the shared browser daemon from scripts unless the user explicitly asks to end/reset the browser session.
- ReadOnly mode permits inspection commands only; navigation, JavaScript execution, interaction, uploads, resizing, and other state-changing browser commands are blocked.
- Treat `evaluate_script` as code execution in the currently selected page. Use it only when needed and keep the function narrowly scoped.
- Browser file paths are local machine paths. Use verified workspace paths for uploads or file-producing commands.
