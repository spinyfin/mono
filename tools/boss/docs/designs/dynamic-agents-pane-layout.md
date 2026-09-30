# Dynamic Agents pane layout: identity is the execution, position is never identity

- **Date:** 2026-09-30
- **Status:** design proposal
- **Project:** Dynamic Agents pane layout
- **Provenance:** project-design execution; no implementation code
- **Verified against:** `main` at `2e351399284f` (2026-09-30), including merged mono#3010; source checkpoints below pin this revision
- **Baseline:** [Tmux-only local worker panes](./make-tmux-the-only-pane-hosting-mode.md); remaining sequencing constraints: **Delete app-mediated worker input and narrow hosting status** and **Enforce and verify the tmux-only local-pane invariant**
- **Direction notes:** [Fleet scaling, the slot model, and team semantics](./fleet-scaling-dynamic-panes-and-team-semantics.md)
- **Related contract:** [Worker liveness](../worker-liveness-contract.md)

Worker identity in the Agents view is the **execution id**. Persona is an **engine-allocated durable lease**. Page, cell, and visual order are app-local presentation state that never reaches the wire. Capacity, pagination, filtering, and ordering follow that ownership split.

## TL;DR

Replace the four pool tabs with one grid of engine-reported local workers, including a status card for a worker without an attached viewer. Compute per-page capacity from the window size against a minimum legible pane of 70 columns by 24 rows at the fixed 10pt worker font, capped at 16 panes per page. Pages appear only when occupied cells exceed capacity. Project and type filters narrow membership before cells are assigned and reset on app launch.

The engine allocates a unique persona from the 40-name roster, durably records it on `work_runs`, and restores it on tmux adoption. Exhaustion produces a unique `Ensign N` rather than refusing dispatch. Engine metadata supplies agent type, project, host, and execution start time. Viewer detach/focus RPCs and the app collection use run id. Slots remain engine capacity handles and bare-integer CLI addresses; backend pools and the admission-only concurrency cap do not change.

## Goals

- Show only engine-reported local workers in one Agents view, with a grid that adapts to the active set and available space.
- Replace Bridge Crew, Lower Decks, Automations, and Reviewers tabs with pagination only when needed.
- Badge every pane or status card as Coding, Design, Review, Automation, or Answer; render unrecognised values as a loud Unknown.
- Keep unique Star Trek personas and support filtering by project and type.
- Keep pagination and filtering independent of execution: hidden workers continue running, streaming, and accepting CLI commands.
- Keep workers visible even when their viewer cannot attach.

## Non-goals

- Changing `MAX_WORKER_POOL_SIZE` (16), `MAX_AUTOMATION_POOL_SIZE` (8), `MAX_REVIEW_POOL_SIZE`/`DEFAULT_REVIEW_POOL_SIZE` (16), slot ranges 1–16 / 17–24 / 25–40, or the `is_main` admission gate in `coordinator/scheduler.rs`. `MAX_CONCURRENT_INTERACTIVE_WORKERS` remains the default 8; the runtime cap and explicit-launch semantics stay unchanged.
- Re-keying the engine's `LiveWorkerStateRegistry`, `WorkerPool` claims, or `WorkerRegistry` away from slot id.
- Rendering remote SSH workers locally. Their detached lifecycle is unchanged; persona uniqueness includes them, local view membership does not.
- Growing the 40-name roster or adding portrait assets. Keep the existing eight TNG portraits.
- Adding a pane-capacity preference, shrinking fonts to fit, persisting filters, or introducing a status filter.
- Adding CLI verbs. `bossctl agents focus` reveals an agent; `bossctl reveal` retains its kanban meaning.

## Current state and findings

Source references below name files under `tools/boss/` and describe the verified baseline, unless explicitly marked as proposed.

### Slot is identity in three layers at once

`LiveWorkerStateRegistry` is slot-keyed, and protocol `name_for_slot` makes slot 1 Riker and slot 25 Seven. The app mirrors the 40-name roster in `WorkerNames.swift`. `WorkersWorkspaceModel` pre-allocates 16 + 8 + 16 `WorkerSlot` values by default; review count follows `EnginePoolConfig`. Attach, detach, and focus still route by slot. `WorkersDetailView` renders four permanently mounted grids and toggles opacity.

The fleet-scaling notes already separate UI real estate, capacity, and worker identity, proposing a crew member assigned for a mission and returned to the roster on close. Slot-derived persona is a consequence of the fixed grid; the roster's modulo wrap is only a defensive fallback.

### Surface and frontend focus identities are already run-based

`TerminalPaneSession.id` is `"run-<runId>"`, and `WorkerSlotView` pins SwiftUI identity to it so recycling a slot creates a distinct session. `FrontendRequest::FocusWorkerPane { run_id }` reaches `app/panes.rs`, and `pane_ops.rs::focus_worker_pane(run_id)` translates it to the app-facing `FocusWorkerPaneInput { slot_id }`.

The CLI resolves live references by run id, numeric slot id, then case-insensitive crew name; its durable/hosted roster fallback uses the same identity forms. Slot- and roster-name-shaped misses cannot fall through to work-item selection. Preserve those rules when introducing overflow names.

### Metadata exists in part

`LiveWorkerState` carries `kind`, attributed `pool`, work-item binding, and `held`; it has no general agent type, project, explicit host id, or execution start time. Attributed pool can be automation even when a worker spills into an interactive slot.

`protocol/src/types/execution.rs` now has twelve kinds: `answer_agent`, `automation_triage`, `chore_implementation`, `ci_remediation`, `conflict_resolution`, `investigation_implementation`, `pr_review`, `pr_review_guide`, `product_design`, `project_design`, `revision_implementation`, and `task_implementation`. Project attribution is absent for unfiled work and must not be guessed from pool.

### Tmux-only hosting and progress recovery are the baseline

- Mono#2993 requires `TmuxWorkerHost` for local dispatch in `runner/pane_spawn.rs` and `StartWorkerInput`. Mono#2995 removed `TmuxHostingPools`, the hosting setting, dispatch hosting stamps, and the rollout badge.
- Mono#2996 deleted `SpawnWorkerPane` and `ReleaseWorkerPane` from `EngineToAppRequest`. Every local surface uses `WorkersWorkspaceModel.swift`'s `tmux attach-session` command. The app owns viewer attachment and detachment, not the worker lifecycle.
- Mono#2862 landed: `work/run_rows.rs::TMUX_RUN_ADOPTABLE_PREDICATE` tests durable local tmux identity and execution status, not the short-lived spawn row's `r.status`. Same-run `register_readoption` preserves live state and holds.
- Semantic progress checkpoints also landed (mono#2871). `engine/core/src/live_worker_state.rs::seed_semantic_progress` restores driver-originated progress without treating shell survival as proof of activity. The Agents pane draws spawning neutrally; the Doing-card live-state path renders spawning as unknown. The persisted-status fallback still exists and is not a membership source for this design.
- Mono#3010 landed. Its startup/death/spawn-ack/husk cleanup makes the recovery slot lookup durable and guards current ownership before acting. It leaves two app process-evidence consumers: `retire_pane` Guard 3 (`hosted_pane_run_for_slot`) and `list_hosted_pane_statuses` in `app/pane_ops.rs`. They still depend on the app's slot-to-run report.

### Viewer presence is not worker liveness

`spawn_flow.rs` retains a successfully started tmux worker when viewer attachment fails; `viewer_error_does_not_fail_the_worker` tests this. An attached surface is therefore insufficient as the membership source.

A retained tmux session is also insufficient proof of liveness. `tmux_session_options.rs` already sets `remain-on-exit=on`. Mono#3023's `tmux_adoption/dead_pane.rs` probes `#{pane_dead}`, records terminal evidence, and performs token-verified cleanup instead of adopting a dead pane.

### Scrollback is bounded

`tmux_session_options.rs` sets `history-limit=2000`. Ghostty retains its own scrollback while its surface stays mounted. The existing pool grids keep all attached surfaces mounted across tab changes; this design preserves that behaviour. Driver transcripts are the durable record.

### Source checkpoints for the tmux-only baseline

These links pin the verified main revision, independently of this design branch's older code base. The proposed membership projection and run-keyed app model below are implementation work, not claims that those changes already exist.

| Boundary                                                                                          | Verified source                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Local dispatch requires tmux; a failed viewer attach leaves the worker running                    | [`PaneSpawnRunner`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/runner/pane_spawn.rs#L1061) and [`start_worker`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/spawn_flow.rs#L933)                                                                                                                                                                             |
| App surfaces attach to tmux; detach is non-destructive; Focus and Detach still address slots      | [`WorkersWorkspaceModel`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift#L456) and [viewer protocol](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/protocol/src/engine_app.rs#L106)                                                                                                                                                         |
| Adoption ignores spawn-row status; same-run adoption retains state; semantic progress is restored | [adoptability predicate](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/work/run_rows.rs#L12) and [`register_readoption` / `seed_semantic_progress`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/live_worker_state.rs#L611)                                                                                                                                     |
| Retained dead panes require engine reconciliation and token-verified cleanup                      | [session options](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/tmux_session_options.rs#L12) and [dead-pane reconciliation](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/tmux_adoption/dead_pane.rs#L106)                                                                                                                                                       |
| App slot-to-run reports still inform retirement and hosted-status classification after mono#3010  | [`retire_pane` Guard 3](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/app/pane_ops.rs#L649) and [`list_hosted_pane_statuses`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/app/pane_ops.rs#L739)                                                                                                                                                                |
| Creation-ordered adoption differs from viewer reattachment order                                  | [`list_adoptable_tmux_runs`](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/work/run_rows.rs#L1127), [slot-sorted snapshot](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/live_worker_state.rs#L889), and [viewer reattachment](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/app/readoption.rs#L899) |
| Local pools total 40 slots; review max/default is 16; the roster has 40 names                     | [pool geometry](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/engine/core/src/coordinator.rs#L179) and [roster](https://github.com/spinyfin/mono/blob/2e351399284f3ba5fe0e0c200476a9ce302a6dc8/tools/boss/protocol/src/worker_names.rs#L46)                                                                                                                                                                                                  |

## Alternatives considered

### Keep fixed slot grids and hide idle cells

Rejected because grouping remains pool-shaped, capacity cannot follow the window, and slot remains the app collection key. A single reviewer should not inherit a fourth-page location from slot 25.

### Allocate personas in the app

Rejected because the CLI, coordinator, local viewers, remote workers, and headless engine all need one unique name for the same execution. Only the engine sees every live worker and can own that lease.

### Assign page and cell in the engine

Rejected because layout depends on app geometry. No page or cell belongs in an engine identity or CLI reference.

### Reflow on every arrival and exit

Rejected while the view is on screen because it moves panes under a reader's cursor. Immediate reflow remains appropriate while the view is hidden.

### Detach the viewer for off-page panes

Detaching is non-destructive by the `DetachWorkerPaneInput` contract. Rejected as the pagination policy because reattachment creates a fresh surface, loses Ghostty-side scrollback, and can display a blank pane during tmux redraw. Tmux history does not restore viewer scrollback. Keeping successful attachments mounted follows current practice and bounds normal local surfaces at 40, independent of which page is selected.

### Refuse dispatch when the roster is exhausted

Rejected: a label cannot veto the 41st concurrent named worker. Allocate a unique overflow name instead.

### Shrink the font to fit

Rejected because the constraint is legibility. Capacity changes; the fixed 10pt worker font does not.

## Chosen approach

### Identity

| Concept                    | Owner  | Value                                                                    | Uses                                                                         |
| -------------------------- | ------ | ------------------------------------------------------------------------ | ---------------------------------------------------------------------------- |
| Worker identity            | Engine | Execution id, equal to `LiveWorkerState.run_id` today                    | Viewer detach/focus key, CLI run reference, SwiftUI identity                 |
| Process-container identity | Engine | `work_runs.tmux_server_label` + `tmux_session_name` + `tmux_spawn_token` | Adoption, teardown, input; never view identity                               |
| Capacity handle            | Engine | Slot id: 1–40 local, 200+ remote                                         | Pool claims, engine registries, bare-integer CLI address, diagnostic tooltip |
| Persona                    | Engine | Durable lease unique across live workers                                 | Pane/card header, kanban, `LiveWorkerState.name`, crew-name CLI address      |
| Cell, page, visual order   | App    | Ephemeral per app session                                                | Presentation only                                                            |

The legacy `LiveWorkerState.shell_pid` field is not process-container identity and the view must not read it; engine recovery still maintains pid evidence internally. The view must also avoid `LiveWorkerState.tmux_hosted` and its `Models+WorkerActivity.swift` mirror: the remaining hosting-status cleanup changes their shape.

**No engine state or wire type introduced by this project carries a page, cell, or visual position, and no CLI reference resolves through one.** A bare integer remains a slot id.

### Persona allocation

- Allocate the lowest-index free roster name at spawn registration, alongside the pool/kind stamps, across local and remote workers.
- Hold the lease until engine slot release, including terminal workers awaiting cleanup. Uniqueness is over concurrent leases, not history.
- Persist a nullable `work_runs.persona` in the spawn-record transaction. On tmux adoption, restore existing leases before assigning missing personas to old rows, in `created_at, id` order. Same-run adoption preserves the lease alongside the already-preserved live state.
- Retain the `" (Remote)"` display qualifier as a host marker; remote workers draw from the same persona pool.
- When all 40 names are held, allocate `Ensign N` with the lowest free `N`, warn, and increment `persona_roster_exhausted`. Never refuse dispatch. With at most 40 local slots, overflow requires remote workers too.
- Resolve `HostedPaneStatus.crew_name` and `bossctl agents list --all` from the durable persona by run id. `HostedPaneEntry` remains an app viewer report, not a persona allocator, and keeps reporting `slot_id` until the invariant task retires the app process oracle.
- Migrate engine/UI consumers off slot-derived names, then remove `name_for_slot` and the duplicate Swift roster. CLI matching and miss handling must recognise overflow names without treating them as work-item selectors.

The app renders the engine name and maps the existing portrait assets by name. Persona restore extends tmux adoption only; no app-originated spawn-ack or husk restoration path is added.

### Badge type

The engine stamps `agent_type` with an exhaustive match over `ExecutionKind`, so new variants require a deliberate mapping.

| Agent type | Execution kinds                                                                                                                                   |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Review     | `pr_review`, `pr_review_guide`                                                                                                                    |
| Automation | `automation_triage`, and automation-sourced rows using the existing attribution precedence                                                        |
| Design     | `project_design`, `product_design`                                                                                                                |
| Coding     | `task_implementation`, `chore_implementation`, `revision_implementation`, `investigation_implementation`, `ci_remediation`, `conflict_resolution` |
| Answer     | `answer_agent`                                                                                                                                    |

Review guides are review-related work, even if a particular execution has no local pane. The mapping does not manufacture membership. As in `attributed_pool_label`, `pr_review` and `pr_review_guide` take precedence over automation source; the remaining automation-sourced kinds receive Automation.

Missing or unrecognised wire values render an **Unknown** warning badge with the raw value, participate in the Unknown filter, and are never dropped.

### Project, host, and ordering metadata

Stamp `project_id` and `project_name` from the dispatched work item; absent project means Unfiled. The project multi-select lists projects with live members plus Unfiled.

The same metadata projection supplies `host_id` from the durable run and `started_at` from the execution, at spawn and on adoption. Locality must be explicit rather than inferred from slot ranges or a hosting-mode boolean. These fields supply membership and ordering, not layout.

### What "only running agents" means

Membership comes from the engine-pushed `LiveWorkerState` snapshot, keyed in the app by `run_id`, restricted to `host_id == "local"`. It begins when the engine publishes a registered run and ends when its authoritative snapshot releases that run. Remote SSH workers are excluded. A disconnected feed retains the last snapshot with an unavailable/stale indication; disconnection is not an empty snapshot. Preserve the existing rejection of duplicate run/slot ids before replacing that snapshot. Only an accepted authoritative snapshot can remove members; malformed or unavailable input must not clear the grid.

Viewer ownership is separate: a surface's lifetime is bounded only by accepted `AttachWorkerPane` and matching `DetachWorkerPane`. Attach failure, surface loss, or detach while a run remains in engine membership leaves its cell visible with persona, type, activity, and **"Viewer not attached"** in place of the terminal. It consumes capacity, participates in filters and waiting counts, and can be focused as a card. A later attach fills the same cell. Reuse the existing engine reattachment path; the app does not spawn workers or probe their liveness.

An attach that arrives before the live snapshot may be retained in the viewer map but cannot invent running membership. Conversely, a delayed detach after engine release cannot resurrect it. Continue reporting attached viewers honestly through `ListHostedPanes` until they are detached.

- `spawning`, `working`, `idle`, `waiting_for_input`, and `errored` remain members. Spawning gets a neutral "Starting" pill.
- Engine terminal state (`terminated`, or an engine terminal execution update while cleanup is pending) produces **"Exited"** and a dim header until engine release. Surface death never sets Exited. Dead panes can linger under `remain-on-exit`; the interval is controlled by engine dead-pane reconciliation and token-verified cleanup, not an app reaper or UI timer. Unreadable probes can delay cleanup, so this is not a fixed retention guarantee.
- Waiting workers are not reordered. Their orange pill, page-selector dot, and needs-input count include cards without viewers.

On engine release, only the epoch placeholder remains. Engine recovery owns gaps or contradictions in its live registry; the app must not reconstruct membership from terminal content, DB "running" fallbacks, app spawn acknowledgements, or CLI polling.

### Capacity computation

Use the terminal cell size reported by libghostty at the fixed 10pt font and the available pane area. Expose that measurement to the layout model; the current `setCellSize` callback only writes a status string.

| Constant             | Initial value | Rationale                                                                                                 |
| -------------------- | ------------- | --------------------------------------------------------------------------------------------------------- |
| `MIN_COLS`           | 70            | Starting estimate from the operator's existing four-column laptop layout; validate legibility in captures |
| `MIN_ROWS`           | 24            | Space for the composer and useful output; validate against real content                                   |
| `HEADER_PT`          | measured      | Measure the two-line rendered header                                                                      |
| `MAX_PANES_PER_PAGE` | 16            | Bound the number of simultaneous headers and surfaces resized per page; excess workers get another page   |

For terminal cell size `(cw, ch)`, pane area `(W, H)`, and grid gap:

```text
minPaneW = MIN_COLS * cw
minPaneH = MIN_ROWS * ch + HEADER_PT
cols     = max(1, floor((W + gap) / (minPaneW + gap)))
rows     = max(1, floor((H + gap) / (minPaneH + gap)))
capacity = min(cols * rows, MAX_PANES_PER_PAGE)
```

A 6pt by 13pt cell gives an estimated 4-by-2, eight-pane maximized laptop layout; large-display capacity depends on logical resolution and hits the 16-pane cap when the formula permits more. These are estimates, not fresh measurements. Keep constants in one `PaneCapacityPolicy` and tune them through the capture task.

For `k <= capacity` occupied cells, choose `(c, r)` with `c * r >= k`, `c <= cols`, and `r <= rows`, maximizing `min(paneW / minPaneW, paneH / minPaneH)`, then fewer empty cells, then more columns. This yields a full-size single pane, side-by-side pairs, and compact larger grids. A window below the minimum still renders one pane; the policy cannot promise minimum dimensions in less space.

### Ordering, cells, and layout epochs

Sort by `(execution started_at, execution id)`, oldest first. Tmux adoption reads `list_adoptable_tmux_runs` in `work_runs.created_at, id` order and restores personas there. Viewer recovery re-sends `AttachWorkerPane`, but its current loop walks a live-state snapshot; the app must sort explicitly rather than infer order from RPC arrival.

While Agents is on screen:

1. A cell stays put and its page grid stays the same shape for a **layout epoch**.
2. Arrivals fill the lowest free cell; if none fits the current grid, open the next page.
3. Engine release leaves a dim placeholder ("Riker finished"), not a collapsing grid.
4. A **layout boundary** removes holes and refits: entering Agents, page change, filter change, resize end, or Tidy. Highlight Tidy when it would change the layout.

While Agents is hidden, changes apply immediately. A persona on a finished placeholder is historical text, not an active lease or addressable worker.

### Pagination selectors and resize

Pages are fixed windows of `capacity` cells. Show selectors when an occupied cell is at or beyond the first page; remove them when release or tidy leaves all occupied cells on page one. Clamp the selected page to the highest occupied page so the operator moves only when the selected page empties. Each selector shows count and a waiting-worker dot.

Ghostty geometry sync is already capped at 30 Hz in `GhosttyTerminalView.swift`. Resize surfaces during a drag; recompute capacity only at drag end, or after 300 ms of geometric quiet for other geometry changes, with a 16pt dead band around row/column thresholds. Preserve the focused pane as anchor, or the first occupied cell on the selected page if none is focused. After refit, select its new page.

### Visibility never affects execution

Keep exactly one mounted surface for each successfully attached local viewer until engine detach, regardless of page or filter. Off-screen viewers keep their page geometry at zero opacity with hit-testing disabled; page selection never recreates a surface. Output remains in Ghostty scrollback, bounded tmux history, and the driver transcript. A worker without a viewer has tmux history and transcript output and stays visible as a card.

Filtering does not detach, pause, or throttle workers. CLI send, interrupt, stop, status, and probe continue using engine execution/slot resolution and durable tmux identity.

### Filtering semantics

- Apply project and type filters to engine local membership, including cards without viewers, before cell assignment.
- Project and type are multi-selects; include Unfiled and Unknown respectively.
- Changing filters ends the epoch; always show the hidden count when a filter is active.
- Reset to All on app launch so restart recovery opens with the full picture.
- Waiting is surfaced by counts and dots, not a separate status filter.

### Focus as the reveal path

`bossctl agents focus <ref>` and the Doing-card agent icon use run-keyed `FocusWorkerPaneInput`. Switch to Agents, clear a hiding filter with a transient banner, select the page, and outline the cell. Focus the terminal if attached, otherwise focus the "Viewer not attached" card. No new CLI verb or worker launch is needed.

### Viewer RPCs keyed by run id

Add `run_id` only to `DetachWorkerPaneInput` and `FocusWorkerPaneInput`; attach already carries it. Re-key `WorkersWorkspaceModel` by run id, retaining slot as capacity/diagnostic metadata. A delayed detach or focus for a prior run must never target the new occupant of its old slot.

Do not modify `SendToPaneInput` or `InterruptWorkerPaneInput`: **Delete app-mediated worker input and narrow hosting status** deletes them. Follow that task when editing shared protocol enums.

Before changing `SlotBusy` or dropping slot-indexed compatibility, **Enforce and verify the tmux-only local-pane invariant** must retire `retire_pane` Guard 3 and `list_hosted_pane_statuses`'s app process oracle. Until then, `ListHostedPanes`/`HostedPaneEntry` must report both `slot_id` and `run_id`, and `SlotBusy` keeps its slot-occupancy meaning. After that prerequisite, make same-run attach idempotent: preserve the existing surface and return success, with no surviving slot-desync caller interpreting a changed error contract. Retain slot in viewer reports for diagnostics; resolve personas in the engine by run id.

### Header, empty state, and pool information

Each header shows the engine persona and portrait, summary or task title, type badge, activity pill, live-status toggle, and existing subtitle. The tooltip carries run id, slot id, pool, and project.

The view header has filters, conditional page selectors, Tidy, hidden and needs-input counts, and a pool strip such as "Interactive 5/16 · Automation 2/8 · Review 1/16". Use engine-pushed pool sizes; label counts as attributed workload from `LiveWorkerState.pool`, since automation spill means those counts are not physical slot occupancy. The empty state is "No agents running" plus an idle flavour line from the existing portrait crew.

### Dependency on the tmux-only project

Tmux deletion work has mostly landed. This project forward-ports onto merged mono#2862, #2993, #2995, #2996, and #3010. It adds no dual-mode renderer, hosting badge, or app-owned worker lifecycle.

1. **Delete app-mediated worker input and narrow hosting status** precedes **Key viewer pane RPCs by run id** (shared protocol enums) and **Replace the pool tabs with the dynamic Agents view** (shared `Models+WorkerActivity.swift` and `PlannerAffordances.swift`).
2. **Enforce and verify the tmux-only local-pane invariant** follows that deletion and precedes both RPC re-keying and the view rewrite. Its acceptance must retire the remaining `ListHostedPanes` process oracle. This order preserves slot safety while the engine still needs it and avoids rewriting the same contracts twice.

Persona, metadata, and the pure layout model can progress independently of this sequence. Coordinate persona's `pane_ops.rs` name projection with the invariant sweep if both edit it. Retain the slot report until that sweep actually removes its safety consumers; do not infer completion merely from mono#3010.

## Risks / open questions

- Capacity constants need real-display measurements; estimates do not establish legibility.
- Epochs can leave holes for a long time; highlighted Tidy and refitting at boundaries make that state explicit.
- `Ensign N` remains a proposed overflow naming choice.
- Exited retention follows engine reconciliation; unreadable probes can delay it.
- Membership follows engine bookkeeping, including recovery delays. Viewer errors must remain visible without pretending the app can establish process liveness.
- Filters reset on restart by design; changing that choice requires updating restart tests as well as the model.

## Proposed implementation task breakdown

Ten in-scope capabilities. Each entry targets no more than three major deliverables and a PR under roughly 1,500 changed lines / 25 files. Estimates below include tests; implementation should preserve these capability boundaries instead of recombining the view, filters, kanban migration, and operator documentation. The two tmux prerequisites above are existing external tasks, not new rows.

### Allocate personas in the engine as durable unique leases

Scope: Implement the durable persona column and allocator, restore leases through tmux adoption, and migrate engine/CLI name consumers (including hosted-status projection) off slot-derived naming. Tests cover local/remote uniqueness, overflow, restoration of older rows, release reuse, and CLI resolution. Keep app slot reporting intact; removing duplicate Swift naming belongs to the UI migration below.

Effort hint: `large`. Estimated size: 900–1,400 lines / 12–20 files.

Dependencies: none; mono#2862 and the progress checkpoint are already baseline.

Scope: in-scope

Parallelism: Can run beside the layout model and viewer RPC task. Coordinate `pane_ops.rs` overlap with the external invariant sweep; metadata follows persona because both edit live-state projection.

### Stamp agent type, project, and membership metadata on live worker state

Scope: Add the exhaustive type mapping and project attribution, plus explicit host/start-time fields used by membership and ordering; populate at spawn and adoption and expose useful metadata in CLI list/status. Tests cover all twelve kinds, attribution precedence, Unfiled, local/remote distinction, and restart preservation.

Effort hint: `medium`. Estimated size: 500–900 lines / 8–15 files.

Dependencies: Allocate personas in the engine as durable unique leases

Scope: in-scope

Parallelism: Can run beside the pure layout model and viewer RPC task; consumes the hosting-status cleanup's final shape if it has landed without reintroducing `tmux_hosted`.

### Key viewer pane RPCs by run id

Scope: Add run id to Detach and Focus, update engine dispatch and app handlers, and re-key the app viewer map. Preserve temporary slot-array projections for the old grid. Test same-run attach, delayed detach after slot reuse, focus, and truthful viewer reporting. Resolve duplicate-attach/error semantics only after the external invariant sweep has removed slot-based safety consumers. Do not touch deleted send/interrupt RPCs.

Effort hint: `medium`. Estimated size: 700–1,200 lines / 10–20 files.

Dependencies: **Delete app-mediated worker input and narrow hosting status**; **Enforce and verify the tmux-only local-pane invariant** (external tmux tasks)

Scope: in-scope

Parallelism: Can run beside persona and layout work; the view rewrite follows because both edit `WorkersWorkspaceModel.swift`.

### Build the pane capacity and layout model

Scope: Add pure Swift `PaneCapacityPolicy` and `PaneLayoutModel`: grid selection, stable cells, epochs, pagination, filter application, hidden/waiting counts, resize dead band, and anchor selection. Unit-test geometry, holes, boundaries, cards without viewers, filter changes, and stable run identity. No UI.

Effort hint: `medium`. Estimated size: 700–1,100 lines / 4–8 files.

Dependencies: none

Scope: in-scope

Parallelism: Independent of engine/protocol changes; new model and test files only.

### Replace the pool tabs with the dynamic Agents view

Scope: Connect engine membership and the separate viewer map to a uniform paginated grid; render persona/type/activity headers and viewer-missing cards; integrate measured cell size, resize boundaries, Tidy, page/waiting selectors, pool strip, and empty state. Keep all attached surfaces mounted across pages, delete slot-array projections, and test snapshot/attach/detach races, rejected snapshots, feed reconnection, and terminal-state rendering. Filters initially remain All; filter controls and kanban migration are separate capabilities.

Effort hint: `large`. Estimated size: 1,000–1,400 lines / 10–18 files.

Dependencies: Stamp agent type, project, and membership metadata on live worker state; Key viewer pane RPCs by run id; Build the pane capacity and layout model; **Delete app-mediated worker input and narrow hosting status**; **Enforce and verify the tmux-only local-pane invariant**

Scope: in-scope

Parallelism: Can run beside backend invariant tests and kanban persona migration. Shared `Models+WorkerActivity.swift` and `PlannerAffordances.swift` edits must build on the external input/status deletion.

### Add project and type filtering to Agents

Scope: Add the two filter controls and visible hidden count over the existing layout model, with Unfiled/Unknown and launch-reset semantics. Test membership, waiting indicators, and preservation of mounted viewers under filtering. Add a short operator document covering membership, missing viewers, filters, pagination, and epochs.

Effort hint: `medium`. Estimated size: 400–800 lines / 5–10 files.

Dependencies: Replace the pool tabs with the dynamic Agents view

Scope: in-scope

Parallelism: Can run beside kanban persona migration and capacity validation; it does not change engine identity or viewer RPCs.

### Render kanban agent names from the engine persona

Scope: Migrate Doing-card names and portrait lookup to the engine persona, preserving the neutral/unknown spawning treatment. Test persona stability across slot reuse and missing metadata; remove slot-name helpers and the duplicate Swift roster after their last pane/card consumer migrates. Idle flavour can use the existing portrait crew without recreating a slot roster.

Effort hint: `small`. Estimated size: 250–600 lines / 4–10 files.

Dependencies: Allocate personas in the engine as durable unique leases

Scope: in-scope

Parallelism: Card work can run beside the view rewrite; coordinate final shared-helper deletion after both consumers migrate. No dependency on filters, pagination controls, or operator documentation.

### Make focus bring an agent into view

Scope: Handle run-keyed focus from CLI and Doing-card clicks: switch mode, clear hiding filters with a banner, select page, focus terminal or viewer-missing card, and outline it. Test hidden, off-page, absent-viewer, and non-Agents-mode cases, plus equivalent run/slot/persona CLI references.

Effort hint: `small`. Estimated size: 250–500 lines / 4–8 files.

Dependencies: Add project and type filtering to Agents

Scope: in-scope

Parallelism: Can run beside capacity validation and backend invariant tests.

### Pin backend pool geometry and the admission-only cap

Scope: Confirm or add tests pinning interactive 16, automation 8, review max/default 16, slots 1–16 / 17–24 / 25–40, and the default interactive cap 8. Pin `is_main`-only admission with review dispatch still possible at the cap, and CLI bare integers resolving as slots. Protocol tests confirm no page/cell address is introduced by the new viewer messages.

Effort hint: `small`. Estimated size: 200–500 lines / 3–8 files.

Dependencies: Allocate personas in the engine as durable unique leases; Stamp agent type, project, and membership metadata on live worker state; Key viewer pane RPCs by run id

Scope: in-scope

Parallelism: Engine/CLI tests can run beside the view and filter work.

### Validate capacity constants on real displays

Scope: Use an isolated capture instance at one, four, eight, and capacity agents on laptop and large-display geometries, including a viewer-missing card. Attach captures for operator inspection and record measured cell size, capacity, and legibility in a dated report. Tune policy constants with matching model tests if needed; state any display that could not actually be measured.

Effort hint: `small`. Estimated size: 100–300 lines / 2–5 files, excluding uncommitted capture artifacts.

Dependencies: Replace the pool tabs with the dynamic Agents view

Scope: in-scope

Parallelism: Can run beside filters, kanban migration, and focus work; this validates the chosen capacity policy.
