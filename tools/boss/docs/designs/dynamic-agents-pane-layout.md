# Dynamic Agents pane layout: identity is the execution, position is never identity

- **Date:** 2026-09-02
- **Status:** design proposal
- **Project:** Dynamic Agents pane layout
- **Provenance:** project-design execution; no implementation code
- **Verified against:** `main` at `32f8a06c` (2026-09-02)
- **Hard dependency:** [Tmux-only local worker panes](./make-tmux-the-only-pane-hosting-mode.md) (sibling project, in design)
- **Direction notes this builds on:** [Fleet scaling, the slot model, and team semantics](./fleet-scaling-dynamic-panes-and-team-semantics.md)
- **Related contract:** [Worker liveness](../worker-liveness-contract.md)

The contested property is that a worker's identity in the new Agents view is its **execution id**, that its **persona is an engine-allocated durable lease** rather than a slot-derived label, and that page, cell, and visual order are app-local presentation state that never reaches the wire. Everything else in this design (capacity, pagination, filtering, ordering) is derived from that split, and every alternative that put position, page, or slot back into identity was rejected on it.

## TL;DR

Replace the four pool tabs with one grid that shows exactly the workers the engine currently hosts a pane for, laid out in a uniform cell grid whose per-page capacity is computed from the window size against a minimum legible pane of 70 columns by 24 rows at the fixed 10pt worker font, capped at 16 panes per page. Pages appear only when occupied cells exceed capacity. Filters by project and type narrow the set before cells are assigned and are never persisted.

Identity moves off the slot: the engine allocates each run a unique persona from the 32-name roster (durable on `work_runs`, restored on re-adoption, overflowing to `Ensign N` rather than colliding or refusing dispatch), stamps an explicit agent type and project on `LiveWorkerState`, and keys the viewer-side pane RPCs by run id. Slots remain the engine's capacity handle and the bare-integer CLI address; they stop being what the app keys a pane on. Pools, slot ranges, and the admission-only concurrency cap do not change.

## Goals

- One Agents view that shows only running agents and adapts its grid to how many are active.
- Remove the visible pools (Bridge Crew, Lower Decks, Automations, Reviewers); pagination selectors appear only when the window cannot fit every visible pane.
- Derive per-page capacity from the Boss window size, using a minimum legible pane defined in terminal columns and rows.
- Badge every pane with a general type (Coding, Design, Review, Automation, Answer) and render an unmapped kind as a loud Unknown, never omit it.
- Keep Star Trek personas, with a guarantee that one persona is on at most one pane at a time.
- Offer filtering by project and by type.
- Leave backend pools, slot ranges, and the admission-only concurrency cap exactly as they are.
- Guarantee that pagination and filtering are view-only: an agent that is not on screen keeps running, keeps streaming, and stays addressable by every CLI verb.

## Non-goals

- Changing `MAX_WORKER_POOL_SIZE` (16), `MAX_AUTOMATION_POOL_SIZE` (8), `DEFAULT_REVIEW_POOL_SIZE` (8), the slot ranges 1-16 / 17-24 / 25-32, `MAX_CONCURRENT_INTERACTIVE_WORKERS`, or the `is_main`-only admission gate in `coordinator/scheduler.rs`.
- Re-keying the engine's `LiveWorkerStateRegistry`, `WorkerPool` claims, or `WorkerRegistry` away from slot id. Slot remains the engine's live capacity handle; the slot-model rethink in the fleet-scaling notes is a separate project.
- Rendering remote SSH workers in the Agents view. They hold no local pane today and the tmux-only project keeps them on their detached lifecycle; persona uniqueness covers them, panes do not.
- Growing the persona roster or adding portrait assets. The roster stays at 32 names and the eight TNG portraits stay as they are.
- A user-facing preference for pane capacity, and automatic font shrinking to fit more panes. Capacity follows the window; legibility is not traded for count.
- Persisting filters across app restarts (a decision, argued below, not an omission).
- New CLI verbs. `bossctl agents focus` becomes the reveal path for panes; `bossctl reveal` stays a kanban verb.
- A status filter (for example "needs input"). Waiting workers are surfaced through page indicators and the pane header, not by reordering or by a new filter.
- Improving the app-hosted spawn path. The view renders both hosting modes during the overlap with the tmux-only project, but all new investment goes to the run-id/tmux identity.

## Current state and findings

### Slot is identity in three layers at once

`LiveWorkerState` is keyed by `slot_id` in `engine/core/src/live_worker_state.rs`, and `name` is computed by `boss_protocol::name_for_slot`, so slot 1 is always Riker and slot 25 is always Seven. The app mirrors the roster in `WorkerNames.swift`, and `WorkersWorkspaceModel` pre-allocates 16 + 8 + 8 `WorkerSlot` values and routes every engine RPC (`SpawnWorkerPane`, `AttachWorkerPane`, `DetachWorkerPane`, `FocusWorkerPane`, `SendToPane`, `InterruptWorkerPane`) by slot id. `WorkersDetailView` renders four permanently mounted grids and toggles opacity between them.

The fleet-scaling notes already recorded that a slot conflates UI real estate, capacity control, and worker identity, and named the intended direction: "identities form a pool; a pane spawns, a crew member is assigned for the mission, and returns to the roster when the pane closes." No decision was ever recorded that persona should be slot-derived; the roster comment describes the modulo wrap as a defensive fallback that must never be exercised. That is a coincidence of the fixed grid, not a designed invariant, and this design treats it as such.

### The pane surface is already keyed by run id

`TerminalPaneSession.id` is `"run-<runId>"` and `WorkerSlotView` pins SwiftUI identity to it precisely because a slot can be released and respawned into a new session within one update pass. The app therefore already has the right identity for the surface and the wrong identity for routing. Only the RPC keys and the grid need to move.

### The frontend focus request is already run-id keyed

`FrontendRequest::FocusWorkerPane { run_id }` reaches `app/panes.rs` with a run id, and the engine maps it to a slot only to satisfy the app-facing `FocusWorkerPaneInput { slot_id }`. The CLI resolver in `bossctl/src/agents.rs` tries run id, then numeric slot id, then case-insensitive crew name, and refuses to fall through to a work-item selector for anything slot- or name-shaped. None of this needs to change shape.

### Attribution needed for badges and filters is engine state, and mostly on the wire already

`LiveWorkerState` already carries `kind` (an `ExecutionKind` string), `pool` (the attributed pool label from `attributed_pool_label`, which reports `automation` for automation-sourced rows regardless of the slot they spilled into), `work_item_id`, and `held`. It does not carry a project or a general type. `ExecutionKind` has eleven variants at `main`: `answer_agent`, `automation_triage`, `chore_implementation`, `ci_remediation`, `conflict_resolution`, `investigation_implementation`, `pr_review`, `product_design`, `project_design`, `revision_implementation`, `task_implementation`. Tasks carry `project_id`; chores may not.

### Both hosting modes are live at `main`

The repository default is still app-hosted; the configured installation runs all three pools in tmux. Under tmux, the app's `attachWorkerPane` runs `tmux attach-session` inside a Ghostty surface and the worker process lives in the detached session regardless of the viewer. Under app hosting, the Ghostty surface owns the pty. The view must render both during the overlap.

### The two live defects have not landed

- **Re-adoption wipes live state.** `TMUX_RUN_ADOPTABLE_PREDICATE` in `work/run_rows.rs` still selects `r.status = 'active'`, and the periodic tmux sweep rebuilds `LiveWorkerState` from scratch. PR #2862 ("Fix tmux worker re-adoption state") retains live state and holds on same-run re-adoption and is **open, not merged** at the verified commit. Any persona stored only in memory would be re-derived from the slot on every sweep until it lands, so persona durability is sequenced after it.
- **Spawning renders as green.** In the Agents pane itself `WorkersDetailView.liveActivityColor` already draws `.spawning` in the neutral secondary colour. The green comes from the kanban Doing card: `AgentActivityState.init(runtime:)` maps `work_executions.status == "running"` to `.active` whenever no `LiveWorkerState` exists, which is exactly the window after a re-adoption wipe. PR #2862 also owns rendering a re-adopted spawning worker as unknown. This design builds on that rendering and does not restate the fix.

### Scrollback for an unseen pane is bounded, not unbounded

The tmux-only design makes the private server's `history-limit=2000` explicit. A Ghostty surface keeps its own scrollback for as long as it is mounted. Today every live worker's surface stays mounted whether or not its tab is selected, which is the precedent this design keeps.

## Alternatives considered

### Keep the fixed slot grids and hide idle cells

The cheapest change: keep four grids, collapse unoccupied cells, and stop showing pool names. Rejected because the pages stay pool-shaped (a reviewer in slot 25 always lands on a "fourth page" even when it is the only agent running), per-page capacity cannot follow the window, and the app still keys every pane on a slot. It satisfies the letter of "show only running agents" while keeping position as identity.

### Let the app allocate personas as a view label

Persona would be a display-only choice made by the app from the roster, with the engine's `name` ignored. Rejected because uniqueness across concurrently live workers is a cross-process invariant that the CLI (`bossctl agents focus Riker`), the coordinator (which refers to workers by name from `LiveWorkerState.name`), and any future remote viewer all consume, and only the engine sees every live worker including remote ones and those with no app attached. Today both sides derive the same name from the slot and it works only because the slot is unique per live worker; once the name is decoupled from the slot there must be one allocator, and it has to be the process that owns liveness.

### Let the engine assign page and cell

The engine would compute a stable layout and push `page`/`cell` on `LiveWorkerState`. Rejected because layout depends on window geometry only the app knows, and because putting a page number on the wire is precisely what turns position into an addressable identity. The ownership constraint is explicit: layout geometry stays out of the engine.

### Reflow immediately on every arrival and exit

Simplest layout rule: recompute the optimal grid whenever the visible set changes. Rejected for the reason the operator gave: it moves panes under a reader's cursor. The chosen approach keeps this behaviour only when the Agents view is not on screen, where nobody is reading.

### Detach the viewer for off-page panes

Save resources by detaching the Ghostty surface from any pane not on the current page and re-attaching on page-in. Rejected: under app hosting there is no detach that does not kill the worker; under tmux it works but costs a fresh surface, loses the Ghostty-side scrollback, and shows a blank pane while tmux redraws. The established practice (four grids always mounted) already bounds resource cost by the number of live workers, at most 32 local, and this design keeps it.

### Refuse dispatch when the roster is exhausted

Guarantee uniqueness by never spawning a 33rd named worker. Rejected outright: a label must never affect execution. Overflow gets a generic unique name instead.

### Shrink the font to fit more panes

Rejected because the operator's constraint is legibility. The worker font stays at the fixed 10pt in the launch spec, and capacity is what gives.

## Chosen approach

### Identity

| Concept                    | Owner  | Value                                                                                 | Where it appears                                                                                                         |
| -------------------------- | ------ | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| Worker identity            | Engine | Execution id (`exec_…`), equal to `LiveWorkerState.run_id` today                      | Every RPC key, every CLI verb, the pane's SwiftUI identity                                                               |
| Process-container identity | Engine | `work_runs.tmux_session_name` + `tmux_spawn_token` (tmux) or `shell_pid` (app-hosted) | Adoption, teardown, input delivery; never shown as identity in the view                                                  |
| Capacity handle            | Engine | Slot id (1-32 local, 200+ remote)                                                     | `WorkerPool` claims, `LiveWorkerStateRegistry` key, `bossctl agents list` column, bare-integer CLI address, pane tooltip |
| Persona                    | Engine | Durable lease from the roster, unique across live workers                             | Pane header, kanban card, `LiveWorkerState.name`, crew-name CLI address                                                  |
| Cell, page, visual order   | App    | Ephemeral, per app session                                                            | Nowhere on the wire, never accepted by any CLI verb                                                                      |

The invariant at the load-bearing level: **no engine state and no wire type carries a page number, a cell index, or a visual position, and no CLI reference form resolves through one.** A bare integer in a worker reference is a slot id today and stays a slot id. That holds not because the app declines to send a page number, but because there is no field to send it in.

### Persona allocation

Persona becomes an engine-side lease with these rules:

- Allocated at spawn registration (the same point that stamps `pool` and `kind`), as the lowest-index roster name not held by any live worker, local or remote. Filling from Riker first keeps the familiar "bridge crew" feel when few agents run.
- Held from allocation until the engine releases the worker's slot, so a finished-but-unreaped worker keeps its name until the reaper runs. Uniqueness is over live workers, not over history.
- Durable: a new nullable `work_runs.persona` column is written in the same transaction as the spawn record, and tmux adoption restores it. This is why PR #2862 must land first: until same-run re-adoption preserves state, any in-memory lease would be lost on the next sweep.
- Remote workers keep the `" (Remote)"` display qualifier; their persona is drawn from the same pool, so the qualifier is a host marker rather than a collision guard.
- **Overflow policy:** when all 32 names are held, the engine allocates `Ensign N` with the lowest free `N`. It logs at warn level and increments a `persona_roster_exhausted` counter. Dispatch is never refused and no two live workers ever share a name. With 32 local slots this can only happen once remote workers are also live.
- `name_for_slot` and the Swift roster are deleted. `HostedPaneEntry`/`bossctl agents list --all` report the durable persona. The CLI resolver keeps its order (run id, slot id, crew name) and matches names against the live `name` field, so `Ensign 3` resolves like `Riker`.

The engine owns this because uniqueness is a liveness invariant; the app renders the string it is given and maps the eight portrait names to portraits by name rather than by slot.

### Badge type

The engine stamps `agent_type` on `LiveWorkerState` from an exhaustive match, so adding an `ExecutionKind` variant is a compile error until the mapping is extended:

| Agent type | Execution kinds                                                                                                                                   |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Review     | `pr_review`                                                                                                                                       |
| Automation | `automation_triage`, and any kind whose row is automation-sourced (the same precedence `attributed_pool_label` already uses)                      |
| Design     | `project_design`, `product_design`                                                                                                                |
| Coding     | `task_implementation`, `chore_implementation`, `revision_implementation`, `investigation_implementation`, `ci_remediation`, `conflict_resolution` |
| Answer     | `answer_agent`                                                                                                                                    |

The app renders the string it receives. Anything outside that closed set, or a missing field from an older engine, renders as an **Unknown** badge in the warning colour with the raw value beside it ("Unknown: `foo_bar`"), appears under the "Unknown" option of the type filter, and is never dropped. Both layers are loud: the engine cannot ship an unmapped kind, and the app cannot hide a value it does not recognise.

### Project attribution

The engine stamps `project_id` and `project_name` on `LiveWorkerState` at spawn from the work item. A chore or other row without a project stamps neither. The project filter is a multi-select over the projects that currently have a live worker plus an explicit "Unfiled" entry. Selecting only specific projects hides unfiled agents, and the filter bar always shows the hidden count ("3 hidden by filters"), so nothing is invisible without a visible reason.

### What "only running agents" means

A pane is shown for an execution exactly while the engine hosts a pane for it: from the app's acceptance of `SpawnWorkerPane`/`AttachWorkerPane` until the matching `ReleaseWorkerPane`/`DetachWorkerPane`. The app never infers membership from activity, from pane content, or from CLI polling; the engine's release is the only thing that removes a pane, which keeps the [liveness contract](../worker-liveness-contract.md) intact.

Consequences, by state:

- `spawning`, `working`, `idle`, `waiting_for_input`, `errored`: shown. `spawning` renders as a neutral "Starting" pill, never green, building on PR #2862.
- `terminated` and finished-but-unreaped: shown with an "Exited" pill and dimmed header until the engine releases the slot. The scrollback stays readable for exactly as long as the engine keeps the worker on its books, which is bounded by the existing reaper cadence and, under tmux, by the `remain-on-exit` retention the tmux-only design adds. Hiding it earlier would make the app disagree with `bossctl agents list`.
- `waiting_for_input`: not reordered. It is surfaced by the orange pill it already has, by a dot on the page selector for every page that contains a waiting worker, and by a "needs input" count in the header. Reordering would break ordering stability for the sake of a signal the header already carries.

### Capacity computation

Capacity is computed by the app from the size of the pane area and the terminal cell size libghostty reports for the fixed 10pt worker font. It is never guessed from pixels alone.

| Constant             | Value    | Reason                                                                                                                                                                                                                                           |
| -------------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `MIN_COLS`           | 70       | Today's four-column laptop layout gives roughly 71 columns per pane, which the operator has been using daily and calls a reasonable limit. Nothing narrower has been validated as legible.                                                       |
| `MIN_ROWS`           | 24       | Claude Code's composer box plus one screenful of tool output; today's laptop panes give about 36 rows, and three rows of panes at 24 is the next step down.                                                                                      |
| `HEADER_PT`          | measured | The two-line pane header, taken from the rendered view rather than hard-coded.                                                                                                                                                                   |
| `MAX_PANES_PER_PAGE` | 16       | A four-by-four grid is the most headers a glance can triage; local concurrency above 16 was measured as negative-sum in the saturation experiment, so a second page is the right home for it; and it bounds the surfaces re-laid out per resize. |

With cell size `(cw, ch)` and pane area `(W, H)`:

```text
minPaneW = MIN_COLS * cw
minPaneH = MIN_ROWS * ch + HEADER_PT
cols     = max(1, floor((W + gap) / (minPaneW + gap)))
rows     = max(1, floor((H + gap) / (minPaneH + gap)))
capacity = min(cols * rows, MAX_PANES_PER_PAGE)
```

Estimated at a 6pt by 13pt cell, this yields 8 on a 16-inch laptop maximized (4 by 2), which matches the operator's laptop figure, and 12 to 18 on large displays depending on their logical resolution, which is where the cap applies. The operator's "12 or 14" is therefore a consequence of the formula plus the cap, not an input to it, and the constants live in one `PaneCapacityPolicy` value so the validation task can tune them from real captures.

For a page with `k` occupied cells (`k <= capacity`), the grid is chosen among all `(c, r)` with `c * r >= k` and `c <= cols`, `r <= rows`, by maximizing the balanced scale `min(paneW / minPaneW, paneH / minPaneH)`, then fewer empty cells, then more columns. This gives one full-size pane for one agent, side-by-side halves for two, two-by-two for four, and three-by-two for five or six on the laptop.

### Ordering, cells, and layout epochs

The stable ordering key is `(execution started_at, execution id)`: oldest first, which is also the order the engine re-attaches panes after an app restart (`list_adoptable_tmux_runs` orders by `created_at`).

Cells are assigned by the app and are never on the wire. The rules while the Agents view is on screen:

1. A pane's cell never changes and the page grid never changes shape. This is a **layout epoch**.
2. A new agent takes the lowest free cell across all pages. If none is free within the current grid, it opens the next page and the page selector appears.
3. When the engine releases a pane, its cell becomes a dim placeholder ("Riker finished") rather than collapsing. Nothing else moves.
4. The epoch ends, and the layout is recomputed against the ordering key with holes removed and the grid re-fitted, at a **layout boundary**: entering the Agents view, changing page, changing a filter, the end of a window resize, or pressing the small Tidy control in the header. The control is highlighted whenever a tidy would change something, so a stale layout is visible rather than silent.

When the Agents view is not on screen, every change applies immediately, so switching to it always shows a freshly tidied layout. This is the immediate-reflow alternative, kept only where no one is reading.

Because cells are view-local and reset at every boundary, they cannot become identity: the same agent may sit in a different cell after a tidy, and no verb, log line, or wire field ever refers to a cell.

### Pagination selectors

Pages are fixed windows of `capacity` cells over the cell index space. The selector is visible exactly when some occupied cell index is at or beyond `capacity`; it disappears when a tidy or a release brings every occupied cell within the first page. The current page index is clamped to the highest page that still has an occupied cell, so the operator is moved only when the page they were reading has nothing left on it. Each selector shows the page's count and a dot if it contains a waiting worker.

### Live resize

Panes resize continuously during a drag exactly as today (Ghostty geometry sync is already capped at 30 Hz). Capacity is recomputed only at the end of a live resize, or after 300 ms of geometric quiet for non-drag changes such as full screen or a split-view collapse, and with a 16pt dead band around each column and row threshold so divider jitter cannot flip a boundary. Because recomputation is itself a layout boundary, a resize re-fits the grid; the anchor pane, defined as the pane holding keyboard focus or else the first occupied cell on the current page, decides which page is shown afterwards, so the pane the operator was reading stays on screen. Thrashing is impossible mid-drag because nothing recomputes mid-drag; oscillation across a boundary can only be caused by the operator resizing across it repeatedly, which is their own action.

### Visibility never affects execution

Every live worker has exactly one mounted Ghostty surface for the lifetime of its pane, whether it is on the current page, on another page, or filtered out. Off-screen panes are rendered in their own page's grid geometry at zero opacity with hit-testing disabled, which is the mechanism the four pool grids use today, so switching pages never resizes a terminal and never tears down a surface. Output is retained in the mounted surface's scrollback and, under tmux hosting, in the session's bounded 2,000-line history as well; the driver transcript remains the complete record either way. No pane is throttled, detached, or paused for being invisible, and `bossctl agents send`, `interrupt`, `stop`, `status`, and `probe` reach it by run id or slot id exactly as they do today.

### Filtering semantics

- Filters are applied to the engine's set of hosted panes before cells are assigned, so page count follows the filtered set.
- Two filters: project (multi-select including "Unfiled") and type (multi-select over Coding, Design, Review, Automation, Answer, Unknown). Changing either is a layout boundary.
- The hidden count is always shown in the filter bar when any filter is active.
- Filters reset to "All" on app launch. After a restart the operator's first need is the full picture, and a persisted filter combined with pagination would hide a running agent at exactly the moment they are checking whether restart recovery worked. That is the reason, and it is a decision a reviewer may overturn.

### Focus as the reveal path

`bossctl agents focus <ref>` and clicking a Doing card's agent icon both end at `FocusWorkerPaneInput`, which gains `run_id`. The app switches to the Agents mode if needed, clears any filter that hides the pane and shows a transient banner saying so, selects the pane's page, makes its surface first responder, and briefly outlines the pane. No new verb is needed and `bossctl reveal` keeps its kanban meaning.

### Viewer RPCs keyed by run id

`DetachWorkerPaneInput`, `FocusWorkerPaneInput`, `SendToPaneInput`, and `InterruptWorkerPaneInput` gain `run_id`, and the app keys its pane collection on it; `slot_id` stays in the payload for display and diagnostics. `AttachWorkerPaneInput` and `HostedPaneEntry` already carry both. The app no longer holds slot ranges, so `EnginePoolConfig` is used only for the pool occupancy strip. `SlotBusy` keeps its wire name but the app raises it only when a pane for the same run id is already hosted; the engine's existing slot-desync repair paths are untouched.

### Header, empty state, and the retained pool information

Each pane header reads `[portrait] <Persona> is <gerund>` or `<Persona>: <task title>`, then the type badge, the activity pill, and the live-status eye toggle, with the subtitle line unchanged. The tooltip carries run id, slot id, pool, project, and hosting mode. The view header carries the filter bar, the page selector when needed, the Tidy control, the hidden and needs-input counts, and a pool occupancy strip ("Interactive 5/16 · Automation 2/8 · Review 1/8") built from `LiveWorkerState.pool` and the pool sizes the engine already pushes at session registration, so the fact that pools still exist stays visible after the tabs go. The legacy-hosting badge moves into this header until the tmux-only project removes it. The empty state is one line, "No agents running", plus a single idle flavour line from a random off-duty crew member.

### Dependency on the tmux-only project

This project does not depend on tmux-only hosting landing. It depends on PR #2862 and on both hosting modes continuing to exist during the overlap. Where the two projects touch the same code, the order is:

- **PR #2862 lands before** the persona task here. Persona durability is built on the preserved re-adoption path.
- **Tmux Phase 1 ("Persist the semantic worker-progress checkpoint")** and the persona column both add `work_runs` columns. They may land in either order; the later one forward-ports the migration list.
- **This project's identity and view tasks land before tmux Phase 4** ("Delete app-owned worker lifecycle RPCs", "Delete app-mediated worker input and narrow hosting status", "Remove the hosting setting and rollout-only surfaces"). Those deletions edit `engine_app.rs`, `WorkersWorkspaceModel.swift`, `app/panes.rs`, and `Models+WorkerActivity.swift`, and they are gated on a seven-day soak that this project should not wait on. The deletion tasks forward-port the run-id keying and delete the `SpawnWorkerPane` arm from a collection that is already keyed by run id.
- Under tmux hosting the durable `tmux_session_name`/`tmux_spawn_token` pair remains the process-container identity the engine reasons with. The view never resolves it; it is a better anchor than a slot for the engine, and the execution id is the right anchor for the app.

## Risks / open questions

- **The capacity constants are derived from today's laptop layout, not measured on the large display.** The validation task captures both displays and tunes `PaneCapacityPolicy`; the risk is that 70 by 24 is too small on a high-density display and the cap, not the formula, ends up doing all the work.
- **Layout epochs can leave a page stale for hours.** Mitigated by the highlighted Tidy control and by immediate reflow whenever the view is not on screen; if operators find the placeholder cells annoying, a shorter automatic tidy on quiescence could be added later without touching identity.
- **Two migrations touch `work_runs` in parallel projects.** Incidental overlap; whichever lands second forward-ports.
- **The app-hosted path is still the repository default.** The view supports it, but the identity work targets run id and tmux; an operator on the app-hosted default sees the same view with worse restart behaviour, which is the tmux-only project's problem to remove.
- **`Ensign N` is a naming choice.** It is deliberately generic and unmistakably an overflow; a reviewer may prefer another rank or growing the roster.
- **Finished-but-unreaped panes stay visible until release.** This keeps the view consistent with `bossctl agents list` but means "only running agents" includes an exited worker for up to a reaper interval.
- **Filters do not persist.** Argued above; overturning it is a one-line change in the view model.

## Proposed implementation task breakdown

Breakdown size: 8 entries (8 in-scope, 0 deferred) — the change has three engine/protocol seams (persona lease, type and project stamps, run-id-keyed viewer RPCs), one pure layout model, one view rewrite with a small focus follow-on, one invariant-pinning test sweep, and one capture-based validation of the capacity constants.

### Allocate personas in the engine as durable unique leases

Scope: Add a nullable `work_runs.persona` column; allocate the lowest free roster name (or `Ensign N` on exhaustion, with a warn log and a `persona_roster_exhausted` counter) at spawn registration in the same transaction as the spawn record; hold it until slot release; restore it on tmux adoption; stamp it into `LiveWorkerState.name`; delete `name_for_slot` and derive `HostedPaneEntry`/`bossctl agents list --all` crew names from the durable column; keep the `" (Remote)"` qualifier as a host marker; make the CLI name tier match any live `name`. Tests cover uniqueness across local and remote, overflow naming, restart restoration, and release reuse. Starts only after PR #2862 has merged and must forward-port it.

Effort hint: `large`

Dependencies: none within this list (external prerequisite: PR #2862)

Scope: in-scope

Parallelism: May run in parallel with **Key viewer pane RPCs by run id** and **Build the pane capacity and layout model**; their production file sets are distinct. **Stamp agent type and project on live worker state** must follow it because both edit `live_worker_state.rs` in protocol and engine and the `bossctl agents list` renderer.

### Stamp agent type and project on live worker state

Scope: Add `agent_type`, `project_id`, and `project_name` to `LiveWorkerState`, computed by the engine at spawn registration: type from an exhaustive match over `ExecutionKind` with the automation-source precedence `attributed_pool_label` already uses, project from the dispatched work item (absent for unfiled rows). Render both in `bossctl agents list`/`status`. Tests pin the full kind-to-type table and the unfiled case.

Effort hint: `medium`

Dependencies: Allocate personas in the engine as durable unique leases

Scope: in-scope

Parallelism: May run in parallel with **Key viewer pane RPCs by run id** and **Build the pane capacity and layout model**.

### Key viewer pane RPCs by run id

Scope: Add `run_id` to `DetachWorkerPaneInput`, `FocusWorkerPaneInput`, `SendToPaneInput`, and `InterruptWorkerPaneInput` in `boss-protocol`, populate it in the engine's `app/panes.rs`, `pane_delivery.rs`, `pane_ops.rs`, and `probe_interrupt.rs` handlers, and re-key `WorkersWorkspaceModel` on run id with `slot_id` retained as a display attribute. Keep the existing slot-array projections so the current `WorkersDetailView` keeps rendering until it is replaced. Narrow the app's `SlotBusy` to "a pane for this run id already exists". Protocol round-trip tests and app model tests cover attach, detach, focus, send, and interrupt by run id.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with **Allocate personas in the engine as durable unique leases** and **Build the pane capacity and layout model**. **Replace the pool tabs with the dynamic Agents view** must follow it because both substantially edit `WorkersWorkspaceModel.swift`.

### Build the pane capacity and layout model

Scope: Add a pure Swift `PaneCapacityPolicy` (the constants and the cols/rows/capacity formula from cell size and pane area, with the dead band) and `PaneLayoutModel` (stable ordering key, cell assignment, layout epochs and boundaries, hole placeholders, grid selection by balanced scale, pagination windows, selector visibility rule, page clamping, filter application before assignment, hidden counts, and anchor-pane page selection after a capacity change). No UI. Unit tests cover the laptop and large-display estimates, epoch stability under arrivals and exits, boundary re-fit, filter changes, and that no output of the model is ever an identity.

Effort hint: `medium`

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with the three engine/protocol entries above; it touches only new files.

### Replace the pool tabs with the dynamic Agents view

Scope: Rewrite `WorkersDetailView` to render the layout model: one uniform grid per page, off-page and filtered panes mounted at zero opacity in their own page geometry, page selector with counts and waiting dots, project and type filter bar with hidden count, Tidy control, pool occupancy strip, needs-input count, relocated legacy-hosting badge, and the one-line empty state. Pane headers show the wire persona, portrait by name, the type badge with the loud Unknown fallback, the neutral "Starting" pill, and the "Exited" state for released-pending panes. Delete `WorkerNames.swift`, `TrekCharacter.forSlot`, and the slot-array projections; the kanban card reads the wire name. Recompute capacity at resize end with the debounce. Document the visibility predicate, filter semantics, and epoch rules in a short operator doc alongside the code.

Effort hint: `large`

Dependencies: Allocate personas in the engine as durable unique leases; Stamp agent type and project on live worker state; Key viewer pane RPCs by run id; Build the pane capacity and layout model

Scope: in-scope

Parallelism: May run in parallel with **Pin backend pool geometry and the admission-only cap**; the latter touches only engine and CLI tests.

### Make focus bring a pane into view

Scope: On `FocusWorkerPaneInput` by run id and on Doing-card agent-icon click, switch to the Agents mode, clear any filter hiding the pane with a transient banner, select its page, make the surface first responder, and outline the pane briefly. Tests cover the filtered-out, other-page, and not-in-Agents-mode cases and that `bossctl agents focus` by run id, slot id, and persona all land on the same pane.

Effort hint: `small`

Dependencies: Replace the pool tabs with the dynamic Agents view

Scope: in-scope

Parallelism: May run in parallel with **Validate capacity constants on real displays**.

### Pin backend pool geometry and the admission-only cap

Scope: Add or confirm engine tests asserting `MAX_WORKER_POOL_SIZE`, `MAX_AUTOMATION_POOL_SIZE`, `DEFAULT_REVIEW_POOL_SIZE`, the 1-16 / 17-24 / 25-32 slot mapping in `slot_id_from_worker_id`, and that the interactive concurrency cap in `coordinator/scheduler.rs` gates only `is_main` rows while review dispatch proceeds at the cap; add CLI tests that a bare integer reference resolves to a slot id and that no `boss-protocol` type carries a page or cell field. Record the verified values in the test names so a later change to any of them is a deliberate edit.

Effort hint: `small`

Dependencies: Allocate personas in the engine as durable unique leases; Stamp agent type and project on live worker state; Key viewer pane RPCs by run id

Scope: in-scope

Parallelism: May run in parallel with **Replace the pool tabs with the dynamic Agents view**.

### Validate capacity constants on real displays

Scope: This is a validation of the chosen constants, not a comparison between layouts. Using the isolated capture instance, render the new view at one, four, eight, and capacity panes on the laptop display and on the large primary display, attach the captures to the work item, record measured cell size, computed capacity, and legibility observations in a dated repository report, and tune `MIN_COLS`, `MIN_ROWS`, or `MAX_PANES_PER_PAGE` in `PaneCapacityPolicy` if the captures show an illegible or wasteful result. A constant change updates the model tests in the same PR.

Effort hint: `small`

Dependencies: Replace the pool tabs with the dynamic Agents view

Scope: in-scope

Parallelism: May run in parallel with **Make focus bring a pane into view**.
