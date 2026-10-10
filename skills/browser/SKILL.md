---
name: browser
description: Control MoonDesk's Codex-style agent browser with workspace-isolated storage, conversation-owned tabs, DOM actions, visual computer-use controls, viewport control, and advanced DevTools fallback.
---

# MoonDesk Browser

Use MoonDesk's browser runtime for local web-app inspection, UI testing, console/network debugging, accessibility snapshots, performance checks, and scripted browser flows.

## Interfaces

- **MCP `browser_state`**: read ambient browser state without launching Chromium: headless/visible presentation, runtime status, this conversation's logical tabs, selected tab, and available capability groups.
- **MCP `browser_tabs`**: list/select/open/close conversation-owned logical tabs. Raw upstream Chrome page IDs and other conversations' tabs are never exposed.
- **MCP `browser_navigate`**: goto/back/forward/reload on the selected tab.
- **MCP `browser_dom`**: the normal DOM-oriented path for snapshots, UID interactions, forms, evaluation, waits, and uploads.
- **MCP `browser_cua`**: the visual computer-use path for rendered screenshots, coordinate clicks, focused typing, keypresses, and scrolling. Prefer this for canvas, custom editors, maps, and other interfaces where DOM UIDs are unreliable.
- **MCP `browser_viewport`**: inspect or set the selected tab's viewport, using exact emulation when DPR/mobile/touch/landscape characteristics are requested.
- **MCP `set_browser_presentation`**: switch the same host-owned agent Chromium between headless and visible presentation. Prefer headless; changing presentation while Chromium is live restarts the host-wide browser runtime and therefore requires explicit user approval before retrying with `confirm_restart=true`.
- **MCP `browser_command`**: advanced escape hatch for MoonDesk-native CDP operations that are not represented by the capability facade. Do not use it as the default interaction model.
- **MCP `view_page`**: direct rendered-pixel helper retained for compatibility; `browser_cua action=screenshot` is the capability-facade equivalent.
- **`moondesk browser` CLI**: deterministic low-level scripting interface in Both mode. It calls the same MoonDesk Chromium runtime and workspace BrowserContext, but it intentionally owns a separate local-CLI logical tab session instead of borrowing a ChatGPT conversation's active page.

MoonDesk provisions its own pinned Chrome for Testing build and controls it directly over CDP. Do not launch a second Playwright/CDP/MCP browser-control stack for normal MoonDesk browser work.

## Core workflow

1. Call `browser_state` when ambient browser state matters, then use `browser_tabs` to select or create the target tab.
2. Navigate with `browser_navigate` and set the target size with `browser_viewport` before collecting interaction references.
3. For ordinary web UI, call `browser_dom action=snapshot`, then use the latest UIDs with DOM click/fill/hover/drag/upload actions. Navigation, viewport changes, and substantial DOM mutations can invalidate older UIDs.
4. For visual interfaces, use `browser_cua action=screenshot` followed by coordinate click/scroll/type/keypress actions instead of forcing an unreliable DOM path.
5. Take another DOM snapshot after structural state changes, or another visual screenshot after appearance-sensitive changes.
6. Use rendered pixels for visual judgment. Accessibility/text snapshots are structural evidence, not a substitute for seeing the page.
7. Drop to `browser_command` only for advanced console/network, emulation, native performance-trace, heap-snapshot, or lower-level page functionality not covered by the facade.

MoonDesk starts one host-owned managed Chromium lazily on the first browser operation and runs it headless by default at a deterministic 1280x800 initial viewport. On an empty cache it first downloads the pinned Chrome for Testing artifact, verifies its exact size and SHA-256, and installs it atomically. The Chromium process and native CDP connection are shared, but browser ownership is not: each registered workspace gets a named isolated BrowserContext for cookies/storage, and each ChatGPT conversation gets its own logical page set inside that workspace context. MoonDesk routes page-scoped operations by the conversation's owned page ID instead of trusting Chromium's globally selected tab. Conversations in the same workspace therefore share that project's login/storage state while keeping separate tabs; different workspaces do not share cookies/localStorage/IndexedDB/service-worker state. Always use the connector that owns the project for its browser work--do not switch to another workspace connector merely because it exposes browser tools.

Headless and visible presentation expose the same browser capabilities: tabs, navigation, DOM control, visual CUA, viewport control, screenshots, and advanced DevTools inspection. Users can switch the one agent Chromium to visible presentation from MoonDesk's dashboard. In `multi-tools`, use `set_browser_presentation` only when the user needs to see or manually interact with the browser, such as a login, CAPTCHA, or permission prompt; `browser_cua action=screenshot` and `view_page` already provide rendered pixels for agent inspection. Presentation is process-global. Changing it while Chromium is running closes **all** MoonDesk workspace BrowserContexts and conversation/CLI tabs, so the tool returns `confirmation_required` until the user explicitly approves that loss and the agent retries with `confirm_restart=true`. If the shared runtime is lost or a dispatched operation exceeds its deadline, MoonDesk invalidates that runtime before another browser operation can run; the next call starts a fresh Chromium generation and all callers must re-establish their page/snapshot state. MoonDesk never automatically replays an ambiguous state-changing action.

## Local dev-server verification

When an agent starts a local web server, browser verification is part of completing the task rather than a separate setup step:

1. Wait until the server reports its localhost URL as ready.
2. Navigate this conversation's selected tab to that URL with `browser_navigate`.
3. Set the viewport being tested with `browser_viewport`, then take a fresh `browser_dom` snapshot when DOM interactions are appropriate.
4. Exercise ordinary controls through `browser_dom`; use `browser_cua` for canvas/custom editors/maps or other visual interactions.
5. Inspect console/network output through the advanced DevTools path when debugging behavior.
6. Use `browser_cua action=screenshot` or `view_page` to verify the actual rendered result; do not declare visual success from DOM text alone.
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

Prefer the capability facade for interactive agent work. Use the CLI for deterministic orchestration, then return to `browser_cua action=screenshot` or `view_page` whenever the task requires actual visual inspection.

## Safety and lifecycle

- Do not run browser lifecycle commands (`start`, `status`, `stop`) through MCP `browser_command` or `moondesk browser`; the running MoonDesk host owns that lifecycle. `browser_state` observes lifecycle state without starting Chromium.
- `moondesk browser` is a lightweight localhost client to the running MoonDesk host. It does not own a separate browser process. CLI calls for one workspace share that workspace's BrowserContext/storage but use a dedicated local-CLI logical tab session, separate from ChatGPT conversations.
- ReadOnly mode narrows the facade to inspection-only actions: `browser_state`, tab list/selected, DOM snapshot/wait, visual screenshot, and viewport get. Navigation, arbitrary JavaScript execution, interaction, uploads, resizing, and other state-changing browser actions are blocked.
- Treat `evaluate_script` as code execution in this caller's currently active logical page. MoonDesk injects the owned upstream page ID; callers must not rely on Chromium's globally selected tab. Use scripts only when needed and keep them narrowly scoped.
- Browser file paths are local machine paths. Relative input paths stay inside the active workspace. Browser-only mode keeps browser inputs workspace-scoped. When Computer tools are also enabled (`Both` mode), an explicit absolute input-file path (for example, `upload_file`) may reference another regular file readable by the MoonDesk user; MoonDesk stages a private copy before Chromium sees it. File-producing/output paths remain workspace-bound.
- The native surface intentionally omits adapter-specific extension lifecycle, Lighthouse wrapper, WebMCP/third-party-tool discovery, and screencast commands. Native CDP performance tracing is browser-global, so MoonDesk leases the active trace to the conversation and exact page that started it.
