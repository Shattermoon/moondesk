# Experimental Lightweight MCP and Local Command Observability Report

> **Status:** Experimental / not merged into `origin/main`  
> **Snapshot date:** 2026-08-19  
> **Experimental branch:** `feat-async-command-jobs`  
> **Experimental commits covered:**
>
> - `1315ddd` — `perf: reduce ChatGPT MCP payload overhead`
> - `df4f749` — `Add new features & Make it really light weight`
>
> **Baseline immediately before the experiment:** `017d682` — `fix: avoid spawning pre-cancelled jobs`  
> **Current upstream main at the time of this report:** `364391f` — `Release v0.1.10`

---

## 1. Purpose of this document

This document records the experimental CatDesk work that was performed after the async command-job feature was implemented. The goal is to preserve the reasoning, architectural changes, implementation details, behavior changes, advantages, trade-offs, risks, and future integration plan so the work can be revisited later without having to reverse-engineer the branch again.

The experiment is more significant than a terminal UI change. It changes the role CatDesk plays in a ChatGPT session.

The previous architecture treated CatDesk partly as an embedded MCP App: tool calls could advertise an HTML output template, ChatGPT could load a CatDesk dashboard resource, and CatDesk attached UI-specific metadata to tool results so that an in-chat widget could render file diffs, command output, token counters, mascot state, and other information.

The experiment moves to a different design:

- ChatGPT receives only the data needed to use CatDesk as a coding tool.
- CatDesk's own terminal becomes the place where the human sees operational status and shell activity.
- MCP payloads are deliberately bounded and compact.
- Large files and command outputs are paginated or archived instead of being pushed into a single tool result.
- UI-only state remains local instead of being inserted into ChatGPT's conversation state.

The intended outcome is lower browser/UI overhead, lower MCP payload overhead, less duplicated context, lower memory pressure, and clearer separation between **agent-facing data** and **human-facing observability**.

---

# 2. Executive summary

There are two experimental commits, and they serve different purposes.

## 2.1 `1315ddd`: remove the heavy in-chat MCP App and reduce payload size

This commit performs the major architectural simplification.

It removes the CatDesk web widget system, including:

- the embedded HTML dashboard;
- MCP resource discovery/read support used by the widget;
- `openai/outputTemplate` metadata;
- CatDesk widget `_meta` payloads;
- widget-specific token/history metadata;
- changed-file and diff payload generation used by the widget;
- web-widget CSP plumbing;
- widget-specific layout/detail settings;
- web-widget Binagotchy/mascot infrastructure;
- the second bootstrap phase that existed only to load widget resources.

At the same time it makes the tool protocol more bounded and recoverable:

- file reads are line-paginated and byte-paginated;
- search results are smaller and capped;
- background command polling becomes strictly incremental;
- complete command stdout/stderr can be preserved locally and retrieved later with a new `read_command_output` tool;
- token accounting stays local and large payloads use bounded sampling instead of full tokenization.

The commit changes **16 files** with approximately:

- **1,675 insertions**
- **12,300 deletions**

A major part of the deletion is the complete removal of:

```text
src/widget/catdesk_dashboard.html
```

which was approximately **6,887 lines**.

## 2.2 `df4f749`: replace lost in-chat observability with a local Shell Commands panel

The second commit adds a richer local terminal experience.

It introduces a dedicated **Shell Commands** pane beside the existing Logs pane on sufficiently wide terminals. Shell commands are recorded as local UI activity, updated through their lifecycle, and never injected into the MCP response merely for display purposes.

The panel tracks:

- command text;
- start time;
- running/succeeded/failed/cancelled/timed-out state;
- background/foreground status;
- job ID relationship internally;
- exit code;
- a small result/progress preview.

It also adds independent scrolling, selection, expansion, keyboard navigation, mouse interaction, and command deduplication for retried async jobs.

This commit changes **5 files** with approximately:

- **1,559 insertions**
- **80 deletions**

The main idea is that information useful to the person supervising CatDesk should be displayed by CatDesk itself instead of being sent through ChatGPT purely so an embedded widget can render it.

---

# 3. Problem the experiment is trying to solve

## 3.1 The old architecture mixed two different responsibilities

Before this experiment, a CatDesk tool result could simultaneously serve two consumers:

1. the model, which needs the actual tool result;
2. the CatDesk web widget, which needs presentation metadata.

That meant an MCP result could contain actual operation data plus a second layer of UI-oriented metadata.

Conceptually, the flow looked like this:

```text
ChatGPT calls CatDesk tool
        |
        v
CatDesk executes operation
        |
        +-------------------------------+
        |                               |
        v                               v
model-facing structured data      widget-facing metadata
                                        |
                                        v
                           ui://widget/catdesk-dashboard.html
                                        |
                                        v
                              embedded UI in ChatGPT
```

This creates several kinds of overhead:

- larger tool descriptors;
- larger tool results;
- more JSON serialization/deserialization;
- more data retained in the conversation/tool-call history;
- more HTML/JavaScript loaded by the ChatGPT client;
- more UI instances/cards in long coding sessions;
- additional MCP `resources/list` / `resources/read` traffic;
- additional server-side code for building widget payloads;
- duplicated state that CatDesk itself already knows locally.

## 3.2 Disabling visible detail is not equivalent to removing the MCP App contract

The previous code had a `ShowDetailMode::Disable` path intended to avoid attaching the widget template for tool calls.

However, the connector still contained the broader MCP App machinery:

- resource capability;
- widget resource URI;
- `resources/list`;
- `resources/read`;
- HTML dashboard;
- widget metadata types;
- connector/tool descriptors that could have already been discovered by ChatGPT.

There is also an important practical issue: ChatGPT can retain connector/tool metadata that was discovered earlier in the session or connector lifecycle. Changing a CatDesk-side preference does not necessarily force ChatGPT to forget a previously advertised output template immediately.

The experiment avoids this ambiguity entirely by making CatDesk no longer advertise an MCP App/widget at all.

## 3.3 Long coding sessions amplify small inefficiencies

CatDesk is intended for many tool calls in one coding session. Small repeated costs become large after dozens or hundreds of operations.

Examples include:

- repeating prior build output in every poll;
- returning hundreds of search results when only a few are needed;
- reading a very large file in one response;
- attaching UI metadata to every tool result;
- tokenizing megabytes of output merely to display a usage estimate;
- rendering an embedded web panel repeatedly even when the human can already see CatDesk's terminal.

The experiment attacks these costs at the protocol, execution, and UI layers rather than treating them as a single rendering bug.

---

# 4. Architecture before the experiment

The pre-experiment CatDesk architecture can be simplified as follows:

```text
                             ChatGPT
                                |
                         MCP tool request
                                |
                                v
                        CatDesk MCP server
                                |
                  +-------------+-------------+
                  |                           |
                  v                           v
          execute local tool           build UI metadata
                  |                           |
                  |                           +--> changed-file diff state
                  |                           +--> token counters
                  |                           +--> widget state
                  |                           +--> mascot / Binagotchy data
                  |                           +--> command presentation data
                  |                           +--> layout/detail settings
                  |                           |
                  +-------------+-------------+
                                |
                                v
                         MCP tool result
                                |
                 +--------------+--------------+
                 |                             |
                 v                             v
           model consumes                ChatGPT loads
           result data          ui://widget/catdesk-dashboard.html
                                               |
                                               v
                                  embedded CatDesk widget
```

The MCP server also advertised a `resources` capability because ChatGPT needed to retrieve the dashboard HTML resource.

A tool descriptor could contain UI metadata equivalent in purpose to:

```text
_meta
  openai/outputTemplate -> ui://widget/catdesk-dashboard.html?... 
  ui.resourceUri        -> ui://widget/catdesk-dashboard.html?... 
```

The tool result could then contain additional CatDesk metadata under `_meta`, including a `catdesk/widgetPayload` object.

---

# 5. Architecture after the experiment

The experiment changes the separation of responsibilities:

```text
                             ChatGPT
                                |
                         MCP tool request
                                |
                                v
                        CatDesk MCP server
                                |
                                v
                          execute tool
                                |
                                v
                    compact structured result
                                |
                                v
                             ChatGPT


          Human-facing local observability is separate:

                     CatDesk server events
                                |
                                v
                         local AppState
                                |
                     +----------+----------+
                     |                     |
                     v                     v
                   Logs              Shell Commands
                                         pane
```

The design principle becomes:

> **Send ChatGPT what the model needs. Keep CatDesk UI state inside CatDesk.**

This is the central architectural idea behind the experiment.

---

# 6. Commit `1315ddd`: `perf: reduce ChatGPT MCP payload overhead`

## 6.1 Complete removal of the embedded CatDesk dashboard

The commit deletes:

```text
src/widget/catdesk_dashboard.html
```

The file contained the embedded HTML/JavaScript dashboard and was roughly 6,887 lines.

Several dashboard-related images and documentation references are also removed.

This means CatDesk no longer needs to ship or render a web application inside ChatGPT for normal tool calls.

## 6.2 Removal of MCP widget resources

The previous MCP server exposed resource behavior such as:

```text
resources/list
resources/read
```

and advertised the widget resource URI:

```text
ui://widget/catdesk-dashboard.html
```

The experiment removes that resource capability from CatDesk's MCP initialization response and removes the resource handlers.

Before:

```text
capabilities
  tools
  resources
```

After:

```text
capabilities
  tools
```

This is important because ChatGPT no longer needs to discover and fetch a CatDesk UI resource during connector initialization.

## 6.3 Removal of `openai/outputTemplate`

The previous implementation could attach an output template to tool descriptors.

The experiment removes the helper logic that created and attached:

```text
openai/outputTemplate
```

and removes the corresponding `ui.resourceUri` metadata.

A test explicitly verifies the new contract:

```text
tools_list_does_not_attach_ui_templates
```

The intended guarantee is that local CatDesk tools are plain MCP tools rather than embedded UI tools.

## 6.4 Removal of CatDesk widget `_meta` payloads

The previous architecture contained a large amount of code for constructing widget payloads such as:

- read-file widgets;
- search widgets;
- write/edit/delete widgets;
- changed-file summaries;
- diff previews;
- command-output widgets;
- async command-job widgets;
- generic fallback widgets;
- token usage metadata;
- historical tool-call counts;
- mascot data;
- Binagotchy data;
- widget state and panel state.

The experiment removes this layer.

The MCP result is no longer enriched with CatDesk UI metadata merely to drive an embedded UI.

Tests verify that tool results do not contain CatDesk `_meta` UI payloads.

Examples include checks equivalent to:

```text
command results must not include CatDesk UI metadata
search result must not include CatDesk UI metadata
write result must not include CatDesk UI metadata
```

## 6.5 Token usage remains local

Token estimation itself is still useful for CatDesk's local counters.

Previously token usage could be inserted into the widget payload so the browser UI could render it. The experiment changes that relationship.

New behavior:

```text
MCP request/result
      |
      +--> locally estimate usage
      |
      +--> update AppState counters
      |
      +--> persist local totals
      |
      X--> do not add token accounting metadata to MCP result
```

The server directly calls the local estimator and records the result in CatDesk state.

A test verifies that accounting works **without returning token metadata to ChatGPT**.

This is a cleaner ownership model because usage counters are a CatDesk UI concern, not information the language model requires to complete a coding task.

---

# 7. Removal of widget-specific settings and server routes

The old application had settings and HTTP endpoints that existed primarily because the embedded widget needed to modify local CatDesk UI state.

The experiment removes widget-oriented configuration such as:

- `TokenStatsLayout`;
- `ShowDetailMode`;
- token statistics layout endpoints;
- show-detail endpoints;
- widget action CORS helpers;
- some widget-side agent path state actions;
- widget-side Binagotchy/partner controls.

The terminal settings UI is correspondingly simplified because there is no longer an embedded widget whose detail level needs to be configured.

The previous concept:

```text
Widget detail mode
  - disabled
  - expanded
  - collapsed
```

is no longer necessary because there is no web widget to configure.

---

# 8. Bootstrap flow becomes smaller

The pre-experiment connector initialization tracked two phases:

```text
Phase 1: Checking tools
Phase 2: Loading widgets
```

The widget phase involved resource reads for multiple CatDesk tool templates.

The experiment removes the second phase entirely.

The bootstrap now only needs the tool-discovery sequence.

This reduces both implementation complexity and connector initialization work.

Conceptually:

```text
Before
------
initialize
notifications/initialized
tools/list
resources/read:run_command
resources/read:catdesk_instruction
resources/read:read
resources/read:search
resources/read:write
resources/read:edit
resources/read:delete

After
-----
initialize
notifications/initialized
tools/list
```

The exact network behavior is controlled by ChatGPT, but CatDesk itself no longer requires the widget-resource phase.

---

# 9. File reads are changed from large one-shot reads to bounded pagination

## 9.1 Previous behavior

The previous `read` implementation could read up to approximately:

```text
512 KiB
```

of file data in one operation.

This is potentially expensive for a coding-agent conversation because a single read can inject a large amount of text into context even when the model only needed a small section.

## 9.2 Experimental behavior

The experiment introduces line-based pagination.

Defaults and limits:

```text
DEFAULT_READ_LINES = 200
MAX_READ_LINES     = 1000
MAX_READ_BYTES     = 128 KiB
```

A normal response contains continuation information such as:

```text
startLine
endLine
nextStartLine
text
```

Example flow:

```text
read(path)
  -> lines 1-200
  -> nextStartLine = 201

read(path, start_line=201)
  -> next section
```

This lets the model inspect only the amount it needs.

## 9.3 Long single-line files

Line pagination is not sufficient for files such as:

- minified JavaScript;
- generated JSON;
- source maps;
- large machine-generated single-line assets.

If one logical line itself exceeds the response byte budget, the experiment changes to byte continuation.

The response can contain:

```text
startByte
endByte
nextStartByte
```

The implementation preserves UTF-8 character boundaries when choosing byte chunks.

This prevents cutting a multibyte character in half and allows the entire file to remain recoverable through repeated bounded reads.

---

# 10. Search results are more compact and explicitly bounded

The search tool is also changed to reduce accidental context explosion.

Key experimental limits:

```text
DEFAULT_SEARCH_LIMIT       = 50
MAX_SEARCH_RESPONSE_BYTES  = 128 KiB
MAX_SEARCH_LINE_BYTES      = 4 KiB
```

The result renderer groups matches by path instead of repeatedly printing the path for every line.

Instead of conceptually producing:

```text
src/main.rs:10: first
src/main.rs:11: second
src/main.rs:12: third
```

it can produce a more compact representation:

```text
src/main.rs
10: first
11: second
12: third
```

If the result exceeds the response budget, CatDesk marks it as truncated and asks the caller to narrow the search.

The purpose is not to make search less capable. The purpose is to keep one broad search from consuming a disproportionate amount of model context.

---

# 11. Async command polling becomes strictly incremental

The async command-job system already existed before this experiment. The experimental commit tightens its protocol to avoid repeated output.

## 11.1 `after` becomes explicit/required

`poll_command` is changed so the caller is expected to provide an output cursor.

First call:

```text
after = 0
```

Response:

```text
output = A
nextCursor = 17
```

Next call:

```text
after = 17
```

Response:

```text
output = B
nextCursor = 31
```

The desired behavior is:

```text
poll 1 -> A
poll 2 -> B
poll 3 -> C
```

rather than:

```text
poll 1 -> A
poll 2 -> A+B
poll 3 -> A+B+C
```

Repeating old compiler/build output in every tool response would waste both MCP bandwidth and model context.

## 11.2 Poll response size is bounded

The async command manager continues to keep a bounded live history and returns bounded poll responses.

Relevant experimental limits include approximately:

```text
MAX_OUTPUT_BYTES_PER_JOB = 4 MiB
MAX_POLL_OUTPUT_BYTES    = 128 KiB
```

Old output can fall out of the live incremental buffer when the command is extremely noisy.

That leads to the next major change: complete local output archival.

---

# 12. Complete command output can be preserved and recovered

One of the strongest parts of the experiment is that it separates **what is returned inline** from **what is retained locally**.

## 12.1 Problem

A tool result must be bounded, but permanently throwing away excess compiler/test output is undesirable.

The model may later need a section of output that was no longer available in the live poll buffer.

## 12.2 Experimental solution

CatDesk writes complete stdout and stderr to local temporary archive files while still keeping the MCP responses small.

For a one-shot `run_command`:

```text
run_command
    |
    +--> bounded stdout/stderr returned inline
    |
    +--> complete stdout/stderr written locally
    |
    +--> if inline data was truncated, return outputId
```

For an async command:

```text
start_command
    |
    +--> jobId
    |
    +--> live bounded event buffer
    |
    +--> complete stdout archive
    +--> complete stderr archive
```

If `poll_command` reports that the requested cursor has fallen behind, the full output can still be recovered from the local archive.

## 12.3 New tool: `read_command_output`

The experiment adds:

```text
read_command_output
```

This tool can read a bounded chunk of a preserved stdout/stderr archive.

Inputs include:

```text
output_id
stream       = stdout | stderr
start_byte
max_bytes
```

The result provides continuation using `nextStartByte`.

The maximum read chunk is approximately:

```text
128 KiB
```

This makes large output fully recoverable without forcing CatDesk to return all of it in a single tool call and without rerunning the command.

## 12.4 Temporary output lifecycle

Archived command output is written under a CatDesk-specific temporary output root owned by the command manager.

The root is removed when the manager is dropped/session ends.

This avoids making the output a permanent project artifact.

A future production implementation should still add an explicit per-job disk cap; see the Risks section.

---

# 13. Process runner changes required for archival

The process runner is modified so it can simultaneously:

1. capture a bounded amount for the immediate MCP result;
2. stream the complete bytes to an archive file.

Conceptually:

```text
child stdout
    |
    +--> bounded in-memory capture
    |
    +--> complete archive file

child stderr
    |
    +--> bounded in-memory capture
    |
    +--> complete archive file
```

Archive failures are recorded separately so CatDesk can distinguish:

- command execution failure;
- normal inline truncation;
- failure to preserve the full archive.

This is an important reliability distinction.

---

# 14. Large token estimates use bounded sampling

CatDesk tracks approximate token usage for its own UI/cost counters.

The old behavior could tokenize the complete serialized payload even when the payload was extremely large.

The experiment introduces:

```text
MAX_EXACT_TOKEN_ESTIMATE_BYTES = 4 KiB
```

Behavior:

```text
payload <= 4 KiB
  -> tokenize exactly

payload > 4 KiB
  -> take ~2 KiB from beginning
  -> take ~2 KiB from end
  -> tokenize the sample
  -> calculate token density
  -> scale estimate to full payload size
```

This prevents local token accounting from becoming a response-path CPU bottleneck.

Trade-off: very large payloads are now estimates and can be less accurate if the middle of the content has substantially different token density from the sampled ends.

For a local usage indicator this is considered an acceptable trade-off.

---

# 15. Commit `df4f749`: local Shell Commands observability

After removing the in-chat dashboard, the second commit adds a better place for human supervision: the CatDesk terminal itself.

## 15.1 New command activity state

`state.rs` introduces local command activity records.

A command activity contains information equivalent to:

```text
id
start time
command
background flag
job ID
state
exit code
preview
```

Possible states include:

```text
Running
Succeeded
Failed
Cancelled
TimedOut
```

The history is intentionally bounded:

```text
MAX_COMMAND_ACTIVITIES = 300
```

This prevents the TUI from accumulating an unbounded command history during a very long session.

---

# 16. Command activity is generated by local server events, not by changing MCP output

This is an important implementation decision.

When `server.rs` receives a CatDesk command tool call, it observes the request and emits local UI events.

For `run_command` / `start_command`, CatDesk emits a command-start event immediately after the MCP request has been parsed.

Conceptually:

```text
MCP tools/call
    |
    v
server parses command
    |
    +--> emit ServerUiEvent::CommandStarted
    |
    +--> execute command normally
```

When the tool result is available, the server derives:

- activity state;
- exit code;
- bounded preview;
- background job binding.

It then emits update events to the TUI state.

The response sent back to ChatGPT is not modified simply to drive the local terminal UI.

A dedicated test verifies this separation:

```text
command_ui_events_track_background_lifecycle_without_mutating_mcp_response
```

This enforces the architectural rule that **local observability must stay local**.

---

# 17. Async command lifecycle is shown as one command, not many polls

A naïve terminal implementation could create a separate entry for:

```text
start_command
poll_command
poll_command
poll_command
cancel_command
```

The experiment instead binds the initial command activity to its async `jobId`.

Example:

```text
start_command("cargo test")
        |
        v
Shell Commands
> cargo test
  running [bg]
```

Later:

```text
poll_command(jobId)
        |
        v
same row updated
> cargo test
  running [bg] · Compiling ...
```

Finally:

```text
poll_command(jobId)
        |
        v
same row updated
✓ cargo test
  succeeded exit 0 · test result: ok...
```

`poll_command` itself does not create a new visible command row.

This produces a view of **actual command executions**, not a view of MCP polling mechanics.

---

# 18. Retried async starts are deduplicated visually

The async command manager can deduplicate repeated `start_command` requests and return the same underlying job.

Without special handling, the TUI could still display two command rows even though only one process actually ran.

The experiment detects when a newly created local activity becomes bound to a job ID that already belongs to an existing activity. In that case the duplicate visual row is removed.

This means the Shell Commands pane tries to represent real executions rather than HTTP/MCP retries.

A state test verifies this behavior.

---

# 19. Shell command result previews are intentionally small

The local command activity stores only a short preview, not the complete stdout/stderr.

The preview is capped at approximately:

```text
240 characters
```

The server checks useful fields such as:

```text
stderr
output
stdout
message
```

and chooses a compact tail/preview for display.

This keeps the TUI state lightweight while complete command output remains available through the command archive mechanisms when necessary.

---

# 20. New split terminal layout

On a sufficiently wide terminal, the bottom section is split into two panes:

```text
+------------------------------+------------------------------------------+
| Logs                         | Shell Commands                           |
|                              |                                          |
| MCP/server events            | actual executed shell commands           |
| warnings                     | running/completed state                   |
| connection events            | exit code / short preview                 |
+------------------------------+------------------------------------------+
```

The command pane is shown when the available terminal region satisfies approximately:

```text
width  >= 100
height >= 5
```

On smaller terminals CatDesk falls back to the existing logs-only presentation.

---

# 21. Independent Logs and Shell Commands navigation

The two panes maintain separate state.

The experiment adds support for:

```text
Tab / Shift+Tab    switch pane focus
Up / Down          move selection
PageUp / PageDown  page through entries
Home / End         jump to first/latest
Mouse wheel        scroll the pane under the pointer
Mouse click        select a rendered entry
Enter / Space      expand selected entry
Esc                collapse expanded entry
```

Long entries stay compact by default and use an explicit ellipsis.

When expanded, the full locally stored command/log text is wrapped across lines.

An intentional blank line is inserted between command entries to keep executions visually distinct.

The key behavior is that scrolling through shell history does **not** change the Logs pane's position and vice versa.

Tests cover independent scroll state, variable-height entries, hit testing, selection navigation, and expanded wrapping.

---

# 22. What functionality is deliberately removed from ChatGPT

The experiment removes the following in-chat CatDesk UI behavior:

| Previous feature | Experimental result |
| --- | --- |
| CatDesk HTML dashboard | Removed |
| MCP widget resource | Removed |
| Rich read-file card | Removed |
| Rich search card | Removed |
| Command output widget | Removed |
| Changed-file widget | Removed |
| Syntax-highlighted web diff panel | Removed |
| Widget token counters | Removed |
| Widget history counters | Removed |
| Expanded/collapsed widget setting | Removed |
| Widget token-layout setting | Removed |
| Widget CSP machinery | Removed |
| Widget Binagotchy controls | Removed |
| `resources/list` / `resources/read` for CatDesk UI | Removed |
| `openai/outputTemplate` metadata | Removed |

The coding capabilities remain available as MCP tools.

The key difference is where their presentation lives.

---

# 23. Coding functionality retained or improved

The experiment does not remove CatDesk's core purpose.

The local coding tools remain available, including the relevant set of:

```text
catdesk_instruction
read
search
write
edit
delete
run_command
start_command
poll_command
cancel_command
read_command_output
```

Browser/devtools tools remain provided by the browser bridge when browser mode is enabled.

Several core capabilities are actually improved:

- long file reads are continuable;
- long single-line files are continuable;
- search is less likely to flood context;
- command polls do not repeat old output;
- complete command output remains recoverable after truncation;
- command supervision becomes clearer in the TUI.

---

# 24. Expected performance and usability impact

The experiment reduces overhead in several independent layers.

## 24.1 Source/runtime complexity

Large amounts of widget-only Rust and HTML code are removed.

This reduces:

- code surface area;
- maintenance burden;
- widget-specific failure modes;
- server routes used only by the widget;
- state synchronization between embedded UI and CatDesk.

## 24.2 Connector initialization

There is no CatDesk widget resource to discover and load.

The bootstrap only needs the MCP tool contract.

## 24.3 MCP payload size

Tool results no longer carry redundant UI metadata.

Token history, widget state, diff display metadata, mascot data, and related data remain local.

## 24.4 Model context

Pagination and incremental command polling reduce accidental context growth.

The model receives less repeated or irrelevant material.

## 24.5 Browser rendering

ChatGPT no longer needs to instantiate a CatDesk web application for each relevant tool call.

This is expected to reduce UI/rendering/memory pressure during long CatDesk conversations.

## 24.6 Human supervision

The Shell Commands pane makes actual local process activity visible immediately in CatDesk without depending on ChatGPT's rendering path.

---

# 25. Important trade-offs

The experiment is intentionally opinionated. It is not a free optimization.

## 25.1 Loss of rich in-chat diffs

A user no longer gets the CatDesk-specific changed-files/diff dashboard in the conversation.

If this presentation is valuable in the future, it should be reconsidered as a separate opt-in feature rather than automatically attaching a large widget to ordinary tool calls.

## 25.2 Loss of web-widget controls

Controls that existed only inside the widget disappear.

Any control still considered important should be moved to:

- the CatDesk terminal settings UI;
- a lightweight local web admin UI that is not embedded in every ChatGPT tool call;
- or a dedicated explicit tool/action.

## 25.3 Large token counts become approximate

Payloads larger than the exact-tokenization threshold are sampled.

The usage UI therefore prioritizes responsiveness over perfect accuracy for unusually large payloads.

## 25.4 `poll_command` contract changes

Requiring an explicit `after` cursor is safer and more efficient, but it is a protocol change.

Old instructions/clients that call `poll_command` without `after` need to be updated.

## 25.5 Shell Commands pane depends on terminal size

Small terminal windows cannot show the split layout.

The logs-only fallback remains necessary.

---

# 26. Risks and issues to address before production integration

## 26.1 Complete output archives need a hard disk ceiling

The live in-memory command buffers and MCP chunks are bounded, but the complete stdout/stderr archives are intentionally preserved on disk.

A pathological command could continuously generate enormous output and consume a large amount of temporary disk space.

Recommended production addition:

```text
MAX_ARCHIVED_OUTPUT_PER_JOB
```

Possible behavior after the limit is reached:

```text
archiveTruncated = true
archiveLimitBytes = ...
```

CatDesk should still preserve as much useful output as possible while preventing unbounded disk growth.

## 26.2 Connector metadata can be cached by ChatGPT

Removing `openai/outputTemplate` from CatDesk does not guarantee that an already-configured ChatGPT connector immediately forgets a previously discovered descriptor.

Therefore the production migration should deliberately trigger the existing connector-refresh/re-add flow once when the contract changes.

Current `origin/main` already contains infrastructure for connector contract revisions and a refresh notice. That should be preserved and reused.

## 26.3 Experimental branch is behind current main

The experiment is based on the async-command branch and still identifies as package version `0.1.8`.

At the time of this report, upstream main is `v0.1.10` and contains changes not present in the experiment, including:

- connector refresh/re-add flow;
- public route protection;
- TUI version display;
- sensitive log reveal isolation;
- documentation updates;
- removal of the old auto-approval extension;
- release/version changes.

The experimental branch should therefore **not be merged wholesale**.

## 26.4 Heavy overlap with files changed on main

The experiment substantially modifies:

```text
src/main.rs
src/mcp.rs
src/server.rs
src/state.rs
```

These files also continued evolving on main after the async command PR was merged.

A future integration should port the changes concept-by-concept onto a fresh branch from current main rather than blindly merging the stale branch.

---

# 27. Validation performed on the experimental branch

The full Rust test suite was run from:

```text
E:\CatDesk\Experiment\catdesk
```

Result:

```text
running 120 tests

test result: ok.
120 passed
0 failed
0 ignored
```

Relevant coverage includes tests for:

- no UI templates on tool descriptors;
- no CatDesk UI metadata in command/search/write results;
- local token accounting without returned token metadata;
- incremental command polling by cursor;
- large background command output recovery;
- `read_command_output` pagination;
- large one-shot `run_command` output recovery;
- line-based file pagination;
- byte-based continuation for very long lines;
- UTF-8-safe byte boundaries;
- compact/bounded search output;
- local Shell Commands lifecycle tracking;
- no MCP response mutation for TUI observability;
- duplicate async command activity suppression;
- bounded command activity history;
- independent bottom-panel scrolling;
- mouse/keyboard selection and hit testing;
- large token estimate sampling;
- process cancellation and timeout behavior.

The experiment is therefore internally coherent and tested, even though it has not been integrated with the newer `v0.1.10` main branch.

---

# 28. Recommended future integration strategy

When this work is resumed, the recommended path is:

## Step 1: start from the latest `origin/main`

Do not continue development by merging current main into the stale experimental branch and resolving everything blindly.

Create a fresh feature branch from the latest main.

## Step 2: port the lightweight MCP architecture first

Port the conceptual pieces from `1315ddd` in controlled groups:

1. remove widget resource capability and handlers;
2. remove `openai/outputTemplate` / UI descriptor metadata;
3. remove widget `_meta` result enrichment;
4. keep token accounting local;
5. remove widget-only server routes/settings/state;
6. simplify bootstrap to tool discovery;
7. add bounded file reads;
8. add bounded search output;
9. add complete command archive support;
10. add `read_command_output`;
11. enforce incremental poll cursors;
12. add bounded large-payload token estimation.

Run tests after each logical group.

## Step 3: preserve all newer `v0.1.10` behavior

Do not regress newer main-branch functionality.

In particular preserve:

- MCP slug/public-route protection;
- connector refresh revision tracking;
- version header/display;
- sensitive URL/log reveal handling;
- current config migration behavior;
- current release/build packaging behavior.

## Step 4: explicitly bump the connector contract revision

Because the tool descriptors and resource contract change significantly, existing ChatGPT connectors should be prompted to refresh/remove-and-readd once.

Reuse the `CURRENT_CHATGPT_CONNECTOR_REVISION` mechanism on current main rather than inventing a second migration path.

## Step 5: port the Shell Commands TUI

After the protocol changes are stable, port `df4f749`'s local command observability:

- `CommandActivityState`;
- bounded command activity history;
- command start/bind/update UI events;
- split Logs/Shell Commands layout;
- independent scrolling;
- selection/expand behavior;
- async job visual deduplication.

## Step 6: add a per-job archive size limit

Before release, add a disk safety ceiling for command output archives.

Add tests proving:

- archive size stops growing at the limit;
- inline/poll behavior remains correct;
- the model is informed that complete output was no longer preserved past the limit;
- cleanup still removes temporary files.

## Step 7: verify real ChatGPT behavior

After local tests pass, test with a fresh/updated ChatGPT connector and verify:

- no CatDesk widget/template is loaded;
- no blank CatDesk card remains;
- connector discovery still succeeds;
- every local tool remains callable;
- browser/devtools bridging still works;
- long coding sessions remain responsive;
- command activity is visible locally;
- token counters still update locally;
- connector refresh notice appears only when required.

## Step 8: benchmark before/after

For a useful release note, compare current main and lightweight build using the same scripted workload.

Potential measurements:

- number of MCP requests during connector bootstrap;
- average tool-result JSON byte size;
- ChatGPT browser memory after 25 / 50 / 100 tool calls;
- CatDesk process memory;
- time to render long conversations;
- total estimated tool input/output tokens;
- file-read payload size;
- repeated bytes across async command polls;
- temp archive disk usage during large builds.

This will turn the qualitative improvement into measurable evidence.

---

# 29. Suggested design principle for future CatDesk work

A useful rule to preserve from this experiment is:

> **CatDesk should not send data through MCP merely because CatDesk wants to display that data.**

Before adding anything to an MCP tool result, ask:

```text
Does ChatGPT/the model need this information to perform the task?
```

If yes, return it through MCP.

If no, but the human should see it, prefer:

```text
CatDesk local state -> CatDesk TUI
```

This principle prevents presentation features from silently increasing model context, browser memory usage, connector complexity, and protocol surface area.

---

# 30. Appendix A: files most affected by `1315ddd`

The first experimental commit touched the following major areas:

```text
README.md
src/command.rs
src/command_jobs.rs
src/main.rs
src/mascot.rs
src/mcp.rs
src/process_runner.rs
src/server.rs
src/state.rs
src/workspace_tools.rs
src/widget/catdesk_dashboard.html   [deleted]
```

Additional widget images/fixtures/documentation were also removed or adjusted.

Approximate high-level change profile:

```text
src/widget/catdesk_dashboard.html   ~6887 lines removed
src/mcp.rs                          major simplification
src/server.rs                       major simplification
src/mascot.rs                       widget-only generation removed
src/workspace_tools.rs              bounded read/search improvements
src/command_jobs.rs                 output archival/recovery additions
src/process_runner.rs               dual capture + archive support
```

---

# 31. Appendix B: files changed by `df4f749`

The local observability commit changes:

```text
README.md
src/main.rs
src/mcp.rs
src/server.rs
src/state.rs
```

Primary responsibilities:

```text
main.rs
  -> split pane UI, focus, scrolling, expansion, mouse/keyboard behavior

server.rs
  -> observe command MCP requests/results and emit local UI events

state.rs
  -> command activity model and bounded history

mcp.rs
  -> bounded token-estimation optimization

README.md
  -> documents Shell Commands behavior and local-only nature
```

---

# 32. Appendix C: npm update `EPERM` warning investigation

During installation of the released CatDesk version, npm displayed a Windows cleanup warning similar to:

```text
npm warn cleanup Failed to remove some directories
EPERM: operation not permitted, unlink
...\.catdesk-<temporary-name>\npm\bin\catdesk.exe
```

The installation itself completed successfully:

```text
changed 1 package
```

The globally installed package was verified as:

```text
catdesk@0.1.10
```

and the running executable path was:

```text
C:\Users\chnav\AppData\Roaming\npm\node_modules\catdesk\npm\bin\catdesk.exe
```

The likely cause is normal Windows executable locking during npm's package replacement flow:

```text
old catdesk package
      |
      v
npm moves it to temporary .catdesk-* directory
      |
      v
Windows still has catdesk.exe open/locked
      |
      v
new package installs successfully
      |
      v
npm cleanup cannot unlink old executable -> EPERM warning
```

The temporary directory was no longer present when checked later, and the installed package was healthy.

Recommended update procedure on Windows:

```text
1. Completely exit CatDesk.
2. Ensure no catdesk.exe process remains.
3. Run: npm update -g catdesk
4. Start CatDesk again after npm finishes.
```

This warning is separate from the lightweight MCP experiment, but it is included here because it was discovered during the same investigation and may be relevant to future release/update UX work.

---

# 33. Final conclusion

The experiment is not merely a cosmetic redesign.

It changes CatDesk from:

```text
coding tools + embedded ChatGPT dashboard
```

into:

```text
lean coding-tool MCP server + local supervision UI
```

The strongest ideas worth preserving are:

1. remove the automatic embedded CatDesk widget from ordinary tool calls;
2. keep UI-only state local;
3. keep MCP results small and bounded;
4. paginate reads/searches instead of flooding context;
5. make async output strictly incremental;
6. preserve complete command output locally instead of returning or discarding it all at once;
7. show real shell activity in CatDesk's own terminal;
8. preserve a clear separation between model-facing information and human-facing observability.

The experimental branch has passed its complete test suite, but it is based on an older mainline state. Future work should therefore **port the design onto a fresh branch from the latest main rather than merge the experimental branch directly**.
