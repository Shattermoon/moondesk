---
name: browser
description: Control MoonDesk's shared Chromium runtime with workspace-isolated storage and conversation-owned tabs without exposing the full Chrome DevTools MCP schema to the model.
---

# MoonDesk Browser

Use MoonDesk's browser runtime for local web-app inspection, UI testing, console/network debugging, accessibility snapshots, performance checks, and scripted browser flows.

## Interfaces

- **MCP `set_browser_presentation`**: available in `multi-tools` mode for the rare case where the user needs the shared agent Chromium to become visible for human input. Prefer headless; changing presentation while Chromium is live restarts the host-wide browser runtime and therefore requires explicit user approval before retrying with `confirm_restart=true`.
- **MCP `browser_command`**: preferred for one browser action at a time and the only page-action primitive needed in Browser-only mode.
- **MCP `view_page`**: preferred whenever appearance matters. It returns the current rendered page as model-visible pixels.
- **`moondesk browser` CLI**: preferred in Both mode when several deterministic browser actions are easier to express as a shell script or loop. It calls the same MoonDesk Chromium runtime and workspace BrowserContext, but it intentionally owns a separate local-CLI logical tab session instead of borrowing a ChatGPT conversation's active page.

Do not invoke `npx chrome-devtools-mcp` directly. MoonDesk pins and manages the compatible Chrome DevTools runtime.

## Core workflow

1. Navigate or select the target page.
2. Set the target viewport before taking interaction UIDs. Use `resize_page` for normal desktop window sizes; use `emulate --viewport=<width>x<height>x<dpr>[,mobile][,touch]` for exact tablet/mobile responsive testing because Chromium may clamp very narrow desktop windows.
3. After navigation or viewport emulation, run `take_snapshot`. Those operations can recreate the page context, so older UIDs may be stale.
4. Use snapshot UIDs with `click`, `fill`, `hover`, `drag`, `upload_file`, etc.
5. Take another snapshot after a state-changing action when the page structure may have changed.
6. Use `view_page` for visual judgment. Accessibility/text snapshots are structural evidence, not a substitute for seeing the rendered page.
7. Inspect console/network/performance data when it helps the task; do not collect large traces by default.

MoonDesk starts one host-owned Chromium lazily on the first browser operation and runs it headless by default at a deterministic 1280x800 initial viewport. The expensive Chromium/MCP process is shared, but browser ownership is not: each registered workspace gets a named isolated BrowserContext for cookies/storage, and each ChatGPT conversation gets its own logical page set inside that workspace context. MoonDesk routes page-scoped operations by the conversation's owned page ID instead of trusting Chromium's globally selected tab. Conversations in the same workspace therefore share that project's login/storage state while keeping separate tabs; different workspaces do not share cookies/localStorage/IndexedDB/service-worker state. Always use the connector that owns the project for its browser work--do not switch to another workspace connector merely because it exposes browser tools.

Headless mode still supports snapshots, screenshots, `view_page`, console/network inspection, interaction, and responsive emulation. Users can switch the one agent Chromium to visible presentation from MoonDesk's dashboard. In `multi-tools`, use `set_browser_presentation` only when the user needs to see or manually interact with the browser, such as a login, CAPTCHA, or permission prompt; `view_page` already provides rendered pixels for agent inspection. Presentation is process-global. Changing it while Chromium is running closes **all** MoonDesk workspace BrowserContexts and conversation/CLI tabs, so the tool returns `confirmation_required` until the user explicitly approves that loss and the agent retries with `confirm_restart=true`. If the shared runtime is lost or a dispatched operation exceeds its deadline, MoonDesk invalidates that runtime before another browser operation can run; the next call starts a fresh Chromium generation and all callers must re-establish their page/snapshot state. MoonDesk never automatically replays an ambiguous state-changing action.

## Local dev-server verification

When an agent starts a local web server, browser verification is part of completing the task rather than a separate setup step:

1. Wait until the server reports its localhost URL as ready.
2. Navigate this conversation's project browser page to that URL.
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
- `moondesk browser` is a lightweight localhost client to the running MoonDesk host. It does not own a separate browser process. CLI calls for one workspace share that workspace's BrowserContext/storage but use a dedicated local-CLI logical tab session, separate from ChatGPT conversations.
- ReadOnly mode permits inspection commands only; navigation, JavaScript execution, interaction, uploads, resizing, and other state-changing browser commands are blocked.
- Treat `evaluate_script` as code execution in this caller's currently active logical page. MoonDesk injects the owned upstream page ID; callers must not rely on Chromium's globally selected tab. Use scripts only when needed and keep them narrowly scoped.
- Browser file paths are local machine paths. Relative input paths stay inside the active workspace. Browser-only mode keeps browser inputs workspace-scoped. When Computer tools are also enabled (`Both` mode), an explicit absolute input-file path (for example, `upload_file`) may reference another regular file readable by the MoonDesk user; MoonDesk stages a private copy before Chromium sees it. Input directories and file-producing/output paths remain workspace-bound.
- Browser-global extension lifecycle operations are intentionally unavailable through the shared runtime because installing/reloading an extension would mutate every workspace. Performance traces and screencasts are globally singleton upstream resources, so MoonDesk leases them to the conversation and exact page that started them.
