# Dashboard Workspace Focus and Filtered Observability Plan

## Goal
Refactor the MoonDesk Ratatui dashboard so the existing upper status area becomes a compact three-column dashboard on wide terminals:

- STATUS on the left
- inline, scrollable WORKSPACES selector in the middle
- CLIPPYMOON on the right

The existing Logs and Shell Commands lower area must retain essentially the same vertical allocation. Narrow terminals must keep a safe fallback layout.

## Constraints
- Work only on `feat/dashboard-workspace-focus`; never modify `main` directly.
- Keep changes primarily in `src/main.rs` and `src/state.rs`.
- Do not redesign MCP routing, command execution, workspace isolation, browser detection/devtools behavior, or native vision.
- Preserve `[w] Workspaces` as the full workspace-management screen.
- Use stable `WorkspaceId` values for dashboard filtering; do not bind filter state to mutable list indexes.
- Do not change usage accounting, only its presentation.
- Do not install/update dependencies unless genuinely required. If dependency download is required and fails, ask the user to enable the VPN rather than changing versions/lockfiles.

## Phase 1 — Stable dashboard workspace/focus state
1. Introduce a dashboard focus enum covering Workspaces, Logs, and Shell Commands.
2. Introduce an active workspace filter represented as All Workspaces or a stable `WorkspaceId`.
3. Snapshot registered workspace metadata needed by the dashboard (id, name/root display, connected state) so rendering never needs to mutate shared app state.
4. Reconcile the selected filter on every snapshot. If a selected workspace has been removed, fall back safely to All Workspaces.
5. Track inline workspace-pane selection/scroll separately from the full `[w] Workspaces` manager.

## Phase 2 — Runtime sequence IDs and non-destructive Clear View
1. Add monotonic runtime sequence IDs to `LogEntry` and `CommandActivity`.
2. Add monotonic counters to `AppState`; assign IDs only when a log/command activity is created.
3. Preserve command activity sequence IDs when an existing activity is updated.
4. Keep clear-view state entirely in TUI state.
5. Store separate log and command visibility cutoffs for:
   - All Workspaces
   - each individual workspace
6. `[c] Clear View` records the highest visible sequence for the active filter; it must not remove or mutate backing histories, command jobs, archives, persisted records, or MCP state.
7. Entries created after the cutoff appear normally even as bounded histories evict older rows.

## Phase 3 — Filtered observability model
1. Build filtered log and command views before selection, pagination, scrolling, hit-map generation, or rendering.
2. All Workspaces:
   - include every workspace log
   - include global/system logs (`workspace_id == None`)
   - include every command activity
3. Specific workspace:
   - include only that workspace's logs
   - exclude global/system logs
   - include only that workspace's command activities
4. Apply the active clear cutoff as part of the same pre-pagination filtering step.
5. Ensure filtered indexes are used consistently for selected/expanded state and mouse hit maps; never mix filtered indexes with raw backing-vector indexes.
6. On filter change, reset/clamp selection and scroll, collapse expanded rows, and restore sensible follow-tail behavior.

## Phase 4 — Three-column top dashboard
1. Reuse the current top-panel height wherever possible.
2. On wide terminals, split the top content horizontally into:
   - compact STATUS
   - WORKSPACES
   - CLIPPYMOON
3. Render inline workspace rows with a compact connected/idle indicator and selected/focused styling.
4. Add a visible range/scroll indicator when the list exceeds the available rows.
5. On insufficient width, hide/reflow the inline workspace pane and keep the existing dashboard usable rather than squeezing lower panels.

## Phase 5 — Compact status presentation
1. Shorten usage lines to values such as `↓93.5K ↑461K Σ554K ƒ767 $5.11`; do not change accounting.
2. In Computer mode, remove the five permanently-empty browser rows from dashboard presentation.
3. In Browser/Both mode, replace them with one compact Browser/CDP summary row.
4. Preserve the DevTools RUNNING/STOPPED/N/A row and all underlying browser/devtools/native-vision behavior.

## Phase 6 — Workspace interactions and focus cycle
1. Tab cycles Workspaces -> Logs -> Shell Commands -> Workspaces when all panes are available.
2. BackTab cycles in reverse.
3. If the inline workspace pane or Shell Commands pane is hidden by terminal size, skip unavailable panes safely.
4. With Workspaces focused:
   - Up/Down move selection
   - PageUp/PageDown move by the visible page
   - Home selects All Workspaces
   - End selects the last workspace
   - mouse wheel scrolls/moves selection
   - mouse click selects the clicked workspace
   - Enter/Space can activate/confirm the current row if selection and active filter are separated
5. Preserve current Logs/Shell Commands keyboard and mouse behavior.
6. Keep `[w] Workspaces` manager unchanged for add/browse/rename/reveal/copy/rotate/remove.
7. Keep the bottom Keys line compact while showing focus and `[c] Clear View`.

## Phase 7 — Tests
Add/update focused unit/TUI tests covering at minimum:
- wide three-column layout
- narrow fallback
- All Workspaces filtering
- specific workspace filtering
- global logs visible only in All Workspaces
- workspace focus/scroll behavior
- mouse hit maps use filtered indexes
- filtered expand/collapse indexes
- tail-follow reset after filter change
- Clear View hides old rows without mutating backing history
- new rows appear after clear
- independent workspace/all clear cutoffs
- selected workspace removal falls back safely
- Computer mode omits browser clutter
- Browser/Both mode compact browser summary
- secret URL reveal/copy behavior remains intact
- `[w] Workspaces` manager remains intact
- native vision tests remain green

## Phase 8 — Full regression, review, PR
Only after implementation is complete:
1. Inspect CI workflow(s) to confirm expected gates.
2. Run `cargo fmt --check`.
3. Run `cargo clippy --all-targets --all-features -- -D warnings`.
4. Run `cargo test --all-targets --all-features`.
5. Run applicable repository npm/package tests only if they are part of normal CI and dependencies are already installed.
6. Run the CI build/release compile path if not already covered.
7. Run `git diff --check`.
8. Fix every regression and rerun affected/full gates as appropriate.
9. Review the complete diff for accidental routing/browser/vision/workspace-isolation changes.
10. Run `git log --oneline -n 5` and commit in the repository's recent style.
11. Push explicitly with branch name `feat/dashboard-workspace-focus`.
12. Create a PR against `main` with scope/title around "Dashboard workspace focus and filtered observability".
13. Do not merge without explicit user instruction.
