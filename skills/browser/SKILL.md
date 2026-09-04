---
name: browser
description: Control MoonDesk's shared Chromium session without exposing the full Chrome DevTools MCP schema to the model.
---

# MoonDesk Browser

Use MoonDesk's browser runtime for local web-app inspection, UI testing, console/network debugging, accessibility snapshots, performance checks, and scripted browser flows.

## Interfaces

- **MCP `browser_command`**: preferred for one browser action at a time and the only browser action primitive needed in Browser-only mode.
- **MCP `view_page`**: preferred whenever appearance matters. It returns the current rendered page as model-visible pixels.
- **`moondesk browser` CLI**: preferred in Both mode when several deterministic browser actions are easier to express as a shell script or loop. It calls the same MoonDesk native browser runtime, so it shares the same isolated agent-browser session as MCP without touching the user's personal browser profile.

Do not invoke `npx chrome-devtools-mcp` directly. MoonDesk pins and manages the compatible Chrome DevTools runtime.

## Core workflow

1. Navigate or select the target page.
2. Set the target viewport before taking interaction UIDs. Use `resize_page` for normal desktop window sizes; use `emulate --viewport=<width>x<height>x<dpr>[,mobile][,touch]` for exact tablet/mobile responsive testing because Chromium may clamp very narrow desktop windows.
3. After navigation or viewport emulation, run `take_snapshot`. Those operations can recreate the page context, so older UIDs may be stale.
4. Use snapshot UIDs with `click`, `fill`, `hover`, `drag`, `upload_file`, etc.
5. Take another snapshot after a state-changing action when the page structure may have changed.
6. Use `view_page` for visual judgment. Accessibility/text snapshots are structural evidence, not a substitute for seeing the rendered page.
7. Inspect console/network/performance data when it helps the task; do not collect large traces by default.

The browser starts lazily on the first operation. Normal commands reuse the existing session. MoonDesk retries once when the browser connection itself has died; ordinary action errors such as a stale UID are returned without restarting the browser.

## Local dev-server verification

When an agent starts a local web server, browser verification is part of completing the task rather than a separate setup step:

1. Wait until the server reports its localhost URL as ready.
2. Navigate the shared agent browser to that URL.
3. Set the viewport being tested, then take a fresh snapshot.
4. Exercise the user-visible flow with snapshot UIDs.
5. Inspect console/network output when debugging behavior.
6. Use `view_page` to verify the actual rendered result; do not declare visual success from DOM text alone.
7. Repeat at the relevant desktop/tablet/mobile viewport when the change is responsive.

## Common CLI commands

```text
moondesk browser list_pages
moondesk browser new_page https://example.com
moondesk browser navigate_page --url=https://example.com
moondesk browser take_snapshot
moondesk browser click 1_23 --includeSnapshot
moondesk browser fill 1_31 "hello"
moondesk browser press_key Enter --includeSnapshot
moondesk browser resize_page 1280 800
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
moondesk browser list_network_requests
moondesk browser evaluate_script "() => ({title: document.title, href: location.href})"
```

Use `moondesk browser <command> --help` when a command's positional arguments or flags are unclear.

## Scripted flows

For repetitive deterministic work in Both mode, keep the browser operations in one shell script instead of spending one MCP schema/tool call per action. Example:

```powershell
moondesk browser navigate_page --url=http://localhost:3000
moondesk browser emulate --viewport=390x844x1,mobile,touch
moondesk browser take_snapshot
moondesk browser list_console_messages
```

Prefer the CLI for orchestration, but return to `view_page` whenever the task requires actual visual inspection.

## Safety and lifecycle

- Do not run browser lifecycle commands (`start`, `status`, `stop`) through MCP `browser_command` or `moondesk browser`; the running MoonDesk host owns that lifecycle.
- `moondesk browser` is a lightweight localhost client to the running MoonDesk host. It does not own a separate browser daemon, so independent shell commands share the same agent-browser session as MCP.
- ReadOnly mode permits inspection commands only; navigation, JavaScript execution, interaction, uploads, resizing, and other state-changing browser commands are blocked.
- Treat `evaluate_script` as code execution in the currently selected page. Use it only when needed and keep the function narrowly scoped.
- Browser file paths are local machine paths. Use verified workspace paths for uploads or file-producing commands.
