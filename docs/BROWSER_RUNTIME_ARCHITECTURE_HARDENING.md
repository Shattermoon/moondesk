# Browser runtime architecture

## Purpose

MoonDesk provides one browser capability surface to agents while keeping browser process ownership,
workspace storage, conversation tabs, and local file access under MoonDesk's authority.

The current browser runtime is **native Rust CDP**. MoonDesk does not use Playwright,
`chrome-devtools-mcp`, Browser Use, or a Node/Python browser sidecar for normal browser control.
Those earlier adapter designs are historical implementation context, not runtime dependencies.

## Runtime shape

```text
agent / moondesk browser CLI
        |
        v
MoonDesk browser capability layer
  - browser_state
  - browser_tabs
  - browser_navigate
  - browser_dom
  - browser_cua
  - browser_viewport
  - browser_command / view_page
        |
        v
BrowserRuntime
  - request deadline + serialization
  - workspace/session authority
  - logical-page mapping
  - popup attribution
  - file staging/output publication
  - presentation/restart policy
        |
        v
BrowserCdpTransport (Rust)
  - owned Chromium process tree
  - browser-level CDP WebSocket
  - request/response correlation
  - flattened target sessions
  - CDP event history
        |
        v
MoonDesk-managed Chrome for Testing
  - one browser process per MoonDesk host
  - one BrowserContext per MoonDesk workspace
  - conversation/CLI-owned logical page sets
```

Constructing `BrowserRuntime` never starts Chromium. The first real browser operation starts the
runtime lazily. `browser_state` can report ambient state without launching the browser.

## Managed browser provisioning

MoonDesk pins the browser artifacts in `browser/managed-browser.json`. The manifest records the
Chrome for Testing version, platform URL, exact archive size, SHA-256 digest, and expected executable
path for every MoonDesk release platform.

On first browser use:

1. MoonDesk checks for an already verified managed browser.
2. If it is absent or fails verification, MoonDesk acquires a cross-process installation lock.
3. The pinned archive is downloaded over HTTPS from the Chrome for Testing origin.
4. MoonDesk enforces the pinned content length and a hard archive-size ceiling while streaming.
5. The complete archive SHA-256 is verified before extraction.
6. ZIP entries are constrained to the staging root; unsafe paths and escaping symlinks fail closed.
7. The browser is extracted to a randomized staging directory.
8. The expected executable is verified and MoonDesk records a verified inventory of every regular browser file plus required symlinks, including file sizes, SHA-256 digests, modification metadata, and Unix permission bits where applicable.
9. The staged install is atomically published. A previous install is retained as a backup until the
   new verification marker is safely written, so a failed replacement can be rolled back.
10. Subsequent browser starts always re-hash the executable and cheaply verify the complete install inventory. Missing or size-changed support files and Unix permission changes fail verification immediately; support files whose modification metadata changed are re-hashed before reuse. A malformed verification marker or damaged cached browser is therefore reprovisioned instead of being retried indefinitely.

The normal npm package therefore stays small. Chromium is a managed first-use runtime artifact rather
than hundreds of megabytes embedded in each npm tarball.

`MOONDESK_BROWSER_PATH` is an explicit developer/test override. Normal production startup uses the
verified managed browser, not a detected personal Chrome installation.

## CDP ownership

MoonDesk starts Chromium with a private temporary user-data directory and an ephemeral loopback
DevTools endpoint. It reads Chromium's `DevToolsActivePort` file, connects to the browser WebSocket,
and owns the WebSocket and Chromium process tree for the complete runtime generation. Headless
Chromium is launched with `--no-startup-window` so Chrome does not create an unrelated startup
`about:blank` BrowserContext/page before MoonDesk creates the first workspace context. MoonDesk
still retires any unexpected headless startup target/context defensively before publishing the
transport.

CDP request IDs, target IDs, session IDs, BrowserContext IDs, and raw event data are internal
implementation details. They are not caller authority.

For page-scoped operations MoonDesk attaches with flattened target sessions and enables the required
Page, Runtime, DOM, Network, and Log domains. Headless pages receive deterministic device metrics so
the initial agent viewport is 1280x800 regardless of host window-manager behavior.

## Isolation and logical tabs

The browser process is host-shared for efficiency, but authority is deliberately split into two
levels:

- **Workspace BrowserContext** — cookies, localStorage, IndexedDB, service workers, and other browser
  storage are shared only by conversations belonging to the same MoonDesk workspace.
- **Logical browser session** — tabs, active-tab authority, and logical page IDs belong to the exact
  MCP conversation or the local `moondesk browser` CLI session.

MoonDesk maps workspace UUIDs to internal BrowserContexts and maps upstream CDP targets to
conversation-local page IDs. Callers cannot supply BrowserContext names or use raw target/page IDs
to cross those boundaries.

When an action creates a popup/new page, reconciliation attributes newly observed targets to the
conversation that initiated the action. Newly created targets are allowed a short metadata-settle
window so a popup is not exposed as a permanently blank logical tab merely because Chrome reported
the target before its URL/title update.

Removing a workspace closes its pages and disposes the entire BrowserContext. If safe cleanup cannot
be completed, MoonDesk resets the shared runtime fail-closed rather than retaining reachable browser
state from the removed workspace.

## Capability surface

Normal agent browsing uses the stable high-level facade:

- `browser_state` — presentation, runtime status, conversation tabs, selected tab, capabilities.
- `browser_tabs` — list/select/open/close conversation-owned tabs.
- `browser_navigate` — goto/back/forward/reload.
- `browser_dom` — accessibility snapshots, UID click/fill/form/hover/drag, evaluation, waits, upload.
- `browser_cua` — rendered screenshots and physical coordinate mouse/keyboard/wheel input.
- `browser_viewport` — viewport inspection and deterministic emulation.
- `set_browser_presentation` — headless/visible process presentation with restart confirmation.
- `view_page` — direct rendered-pixel helper.
- `browser_command` — lower-level native-CDP operations when the facade is insufficient.

`src/browser_contract.json` is MoonDesk's native low-level command contract. It contains only
commands and arguments implemented by the Raw-CDP engine. Unsupported compatibility flags are not
accepted and silently ignored.

The native advanced surface includes console/network inspection, emulation, screenshots,
performance trace capture, and V8 heap snapshots. Adapter-specific extension lifecycle commands,
Lighthouse wrapper commands, WebMCP/third-party-tool discovery, trace-insight wrappers, and
screencast commands from the retired `chrome-devtools-mcp` integration are intentionally not
advertised as native MoonDesk capabilities.

## DOM and computer-use paths

MoonDesk intentionally supports both structural and visual control.

### DOM path

`take_snapshot` uses Chrome's accessibility/DOM data and assigns opaque MoonDesk UIDs to elements.
UIDs are generation-scoped; navigation, substantial DOM changes, viewport/emulation changes, and
runtime restarts can invalidate them. Agents should take a fresh snapshot before continuing UID
interactions after those transitions.

### Visual computer-use path

Visual input is not implemented as JavaScript shims. Coordinate click, key input, typing, drag, and
scroll are dispatched through CDP Input commands. In particular, scrolling uses a real
`mouseWheel` event rather than `window.scrollBy`, preserving browser input semantics for visual
interfaces.

Viewport screenshots capture the current viewport; full-page screenshots explicitly opt into
capture beyond the viewport. `view_page` and `browser_cua action=screenshot` return native image
content to the model.

## Presentation

Headless and visible modes are two launch presentations of the **same** browser architecture and expose the same page-local browser capability set. Browser-global operations such as performance tracing remain subject to the recording-isolation rules below and can be rejected when another managed context/session or an unowned/default-context page is present.

Presentation is process-global. Changing it while Chromium is live requires explicit confirmation
because Chromium must restart and every temporary workspace BrowserContext, logical tab, page state,
and snapshot UID is lost. MoonDesk does not run separate parallel "headless browser" and "visible
browser" backends.

## Deadlines, cancellation, and recovery

Each operation has one MoonDesk-owned absolute deadline covering queueing, browser startup, CDP
requests, staging, and output publication.

A transport timeout/disconnect invalidates the affected runtime generation so an ambiguous
state-changing operation cannot continue invisibly after MoonDesk reports failure. The next browser
operation starts a fresh generation. MoonDesk does not automatically replay an ambiguous mutation.

Operation-level conditions are different from transport loss. For example, a `wait_for` text
timeout returns a normal browser-tool error and does not kill a healthy Chromium/CDP connection.

## Browser-global recording state

CDP performance tracing is browser-global. MoonDesk therefore starts a trace only when the requesting workspace is the sole active managed BrowserContext, the requester is the only logical browser session that has used that BrowserContext in the current Chromium generation, every managed page belongs to the requester, and Chromium has no unowned/default-context page target that could contribute unrelated trace data. The active trace is then leased to the exact session and page that started it. Other browser sessions are blocked from browser actions until the trace stops, another conversation cannot stop or replace the trace, and the owning page cannot be closed while recording is active. A trace protocol error resets the shared browser runtime rather than leaving browser-global recording state ambiguous.

If the page owning the active trace disappears unexpectedly, MoonDesk invalidates the shared
runtime rather than risking recording-state leakage across logical sessions.

## Local file boundary

Browser file access is mediated by MoonDesk, not handed arbitrary host paths.

- Relative browser input files remain workspace-scoped.
- In Both/CLI mode, an explicitly addressed absolute regular input file readable by the MoonDesk
  user may be copied into a private temporary staging directory before Chromium sees it.
- Output files remain workspace-bound.
- Existing output targets and parents are canonicalized and checked for symlink/reparse-point
  escapes.
- Browser output is written to randomized staging paths and atomically published to the workspace
  only while the request deadline remains valid.
- The exact MoonDesk-owned temporary path used by `view_page` receives a narrow managed exception;
  ordinary browser commands cannot use that exception.

The native contract has no browser directory-input or directory-output commands, so the retired
extension/Lighthouse directory staging machinery is intentionally absent.

## Security properties

The browser design preserves these invariants:

- no personal browser profile, cookies, or login state are inherited;
- the DevTools endpoint is loopback-only and tied to MoonDesk's private temporary profile;
- raw BrowserContext/target/session IDs are not exposed as caller authority;
- BrowserContext storage is isolated across MoonDesk workspaces;
- active-page authority is isolated across conversations;
- browser outputs cannot escape the active workspace;
- browser inputs cannot use symlinks/reparse points to bypass file validation;
- browser process descendants remain MoonDesk-owned and are terminated on invalidation/shutdown;
- browser install archives are pinned and cryptographically verified before use.

## Regression coverage

The browser unit/integration suite covers, among other things:

- ambient `browser_state` without Chromium startup;
- native command-contract parsing and read-only policy;
- workspace-scoped file staging/output publication;
- conversation logical-page isolation;
- BrowserContext storage isolation across workspaces;
- popup ownership and metadata settling;
- local CLI vs MCP logical-session separation;
- lazy headless startup without an unrelated startup window/context, defensive startup-context retirement, and recovery after owned-child loss;
- timeout cancellation of dispatched mutations;
- headless/visible presentation confirmation and restart;
- exact viewport emulation;
- DOM UIDs and form interactions;
- physical coordinate CUA and mouse-wheel scrolling;
- rendered viewport/full-page image capture;
- native performance traces and V8 heap snapshots;
- empty-cache managed-browser download, verification, extraction, and first launch.

The Windows browser smokes are serialized because they intentionally exercise real process/runtime
ownership rather than mocks.

## Historical note

MoonDesk previously used `chrome-devtools-mcp@1.7.0` as an internal browser-control transport. The
stable capability facade and workspace/conversation authority model were retained, but the transport
was replaced with MoonDesk's native CDP engine. The old Node/MCP transport, local personal-browser
detection, and its 50-command compatibility registry are not part of the current runtime.
