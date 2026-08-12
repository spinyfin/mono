# Background task visibility: only long-lived, headless engine work enters the toolbar badge

- **Date:** 2026-08-12
- **Status:** Proposed
- **Provenance:** Project design for “Background task visibility: toolbar affordance for engine background work”
- **Related designs:** [Auto-populate project tasks on design PR merge](auto-populate-project-tasks-on-design-pr-merge.md), [Merge-conflict reduction and fast resolution](merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md), [Work execution](work-execution.md)
- **Related in-flight work:** “Surface parked/blocked work items in the kanban with an error indicator and a drag-to-retry gesture”

The load-bearing decision is that this badge is not a census of every async engine activity. It contains only finite operations that are both long-lived and otherwise absent from the UI; anything represented by a worker execution or an existing work-item surface remains there and is not counted again.

## Verdict

Ship an engine-owned global snapshot with two v1 sources: project-planner runs and active conflict-ladder mechanical rungs. Hide each operation until it has run for 15 seconds, poll that snapshot every five seconds through the existing unified engine-attempt RPC, and render it in a new toolbar button adjacent to—not merged with—the app updater.

The planner’s stranded-`running` startup reaper must land first. Until that prerequisite is deployed, no background-work count may appear in chrome.

## Goals

- Make genuinely invisible, long-running engine work discoverable without opening logs.
- Keep the app affordance to one conditional toolbar button, a numeric badge, and a read-only popover listing what is running.
- Make badge membership, timing, phase text, and count an engine decision; the app renders the snapshot without reconstructing source state.
- Prevent fast operations from flashing in and out while still surfacing the planner well before its upper-bound runtime.
- Ensure a crash or restart cannot leave a permanent false-positive badge.
- Reuse the app’s existing engine-attempt and planner representations instead of introducing another parallel read path.

## Non-goals

- A dashboard, history view, activity timeline, job inspector, or control surface.
- Cancel, retry, pause, reveal, or other actions from the popover.
- Moving worker-backed executions out of the Agents tab or counting them in both places.
- Replacing the existing project-level planner affordance, CI card badges, merge-queue card position, Activity “Engine” history, or updater UI.
- Surfacing resident engine services such as pollers, schedulers, heartbeats, backups, event writers, or metrics flushers merely because they run in background tasks.
- Treating engine state as a human-attention item.
- Creating status for short request-scoped work solely so it can appear in this badge.

## Verification method and findings

This study chose between candidate source populations and between polling and push publication. It did not assume the proposed population and then merely test whether it could be implemented.

The audit started from durable source rows, every production `tokio::spawn`/loop in `tools/boss/engine/core`, the `ExecutionKind` set, worker-pool routing, existing protocol verbs, the Activity and kanban views, and toolbar composition. The important findings are below.

| Claim                                    | Verified implementation                                                                                                                      |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Execution-backed work is already visible | `protocol/src/types/execution.rs`, `engine/core/src/pane_summary.rs`, and coordinator pool routing                                           |
| Planner persistence and publication      | `engine/core/src/work/planner_runs.rs`, `engine/core/src/populator.rs`, and `app-macos/Sources/PlannerAffordances.swift`                     |
| Mechanical rung marker and recovery      | `engine/core/src/work/conflict_res.rs`, `engine/core/src/ladder_lease_registry.rs`, and the startup reconciliation in `work/conflict_res.rs` |
| CI vehicle selection                     | `engine/core/src/ci_watch.rs` and `engine/core/src/completion/remediation.rs`                                                                |
| Existing engine-attempt read paths       | `protocol/src/wire.rs`, `engine/core/src/work/blocking.rs`, and `app-macos/Sources/ChatViewModel+Attentions.swift`                           |
| Merge-queue card visibility              | `protocol/src/types/task.rs` and `app-macos/Sources/WorkCardBadges.swift`                                                                    |
| Toolbar ownership                        | `app-macos/Sources/ContentView.swift`, `ContentViewChrome.swift`, and `UpdateCore/UpdateModel.swift`                                         |

### Existing visibility is the first exclusion

Every `work_executions` row has an `ExecutionKind`, is assigned to a worker pool, and receives a pane summary. The Agents tab is therefore the canonical live surface for answer agents, automation triage, PR review, CI-remediation retriggers, revision-backed CI fixes, and conflict-ladder worker rungs. Counting one of those executions in the toolbar would make the count mean “some visible work plus some invisible work,” which is not a stable property.

The answer agent specifically occupies a main-pool slot, holds a cube lease, and has a worker pane. Its read-only capability profile does not make it headless.

### The planner is both eligible and already partly visible

`planner_runs` records `outcome = 'running'` before the LLM work begins, but every current accessor is project- or id-scoped. The detached spawn in `populator.rs` publishes work/attention events only when it finishes; there is no global query and no start event.

The macOS app already renders the same row per project: `PlannerRunAffordance` uses an hourglass for `running`, and the popover says “Planner is running…”. Its refresh is pull-based per project and is triggered by a work invalidation. The toolbar badge must be a global projection of this same run identity, not a second planner status that can disagree with it.

Planner timing gives a defensible anti-flicker boundary but not a measured median. Each LLM call has a 180-second timeout, two transport attempts, and an outer two-pass schema-validation loop, so the bounded LLM portion can reach roughly twelve minutes. The repository contains only a 30–90 second typical-runtime estimate, not measurements; almost all of that time is the LLM because materialization is one short SQLite transaction.

Design-doc fetching can make up to three `gh api` calls with two 500 ms backoffs. Its command runner has no timeout, so “normally under five seconds” is not a hard upper bound; nevertheless, it remains request-scoped cache work with no durable operation identity. A wedged fetch deserves a timeout fix, not a new badge source.

### The mechanical conflict rungs have an exact live marker

`conflict_resolutions.mechanical_rung_in_flight` is `1` for the engine-direct rebase, `0` for deterministic residual resolvers, and `NULL` otherwise. The attempt stamps its cube lease/workspace with the marker and clears all three unconditionally when the rung concludes. Worker rungs create revision executions and are excluded.

The in-memory ladder lease registry is not another operation. It heartbeats and releases the workspace used by the same rung-0/1 attempt, so rendering it separately would double-count one rebase. The live projection should use the durable conflict row as identity and require the corresponding lease to remain present in the in-process registry; that conjunction prevents a failed marker-clear write from claiming work is still running.

Neither the current CLI conflict table nor the Swift conflict model carries the mechanical marker. Both mechanical rungs are absent on a stock install because `conflict_ladder_mechanical_rebase` defaults off; `speculative_conflict_prediction` also defaults off and is not a badge source. Rung 0 is live when the ladder itself is enabled, but it can only follow rung 1’s residual conflicts. With the feature flag off, this source contributes zero rows and the toolbar button simply remains hidden.

### The initial CI candidate fails the inclusion test

CI remediation is not headless in the current code. A fix attempt spawns an engine-triggered revision, while an infrastructure retrigger creates an `ExecutionKind::CiRemediation` execution; both are visible in Agents. The parent card also carries CI failure state, and Activity already lists the durable remediation attempt.

`ci_inflight_observations.alert_level_emitted` is useful phase data for a PR that has remained in flight, but it records observed CI state rather than a finite engine operation. `BuildWaitTracker` likewise describes a visible worker execution, computes progress against its 45-minute horizon, and already publishes `reason = "worker_build_wait_pending"`. Both are precedents for engine-owned phase reporting, not toolbar sources.

### Merge-queue position is meaningful but belongs to the work item

`trunk_merge_intents` has no standalone list RPC, but the value a user cares about is already copied into `tasks.merge_queue_detail`, sent over the work-item wire model, used to order the Merging section, and rendered as a queue-position badge on the task card. Waiting in an external merge queue is PR lifecycle state, not evidence that Boss is currently executing a finite job.

Putting the same position in this popover would duplicate a more contextual surface and could keep the toolbar permanently active on a busy repository. A future request for a global merge-queue summary should be designed as such; it is not v1 background-work visibility.

### Other background code is not eligible

- Resident loops—merge and Trunk pollers, schedulers, heartbeats, backups, external-tracker reconciliation, metrics flushes, monitoring, and socket servers—are engine services rather than finite operations. Including them would pin the badge on for the lifetime of the engine.
- Comment-intent `NULL` means no classification result, not proof that classification code is executing. Absence is never a live marker.
- Short request handlers, cache fills, attachment serving, GitHub authentication, and design-doc fetches do not satisfy the duration and operation-identity tests.
- Automation triage, PR-review workers, answer agents, CI remediation, build waits, and conflict worker rungs are already execution-backed.

No repository document records an earlier global inclusion policy; the existing surfaces grew feature by feature. That silence is a finding: v1 establishes the policy above rather than pretending an unwritten policy already existed.

### Coordination with parked/blocked work-item state

The related parked/blocked effort is moving a dispatch halt onto typed task fields so the kanban can render it directly. That representation is intentionally scoped to a work item and is not reusable as a global, finite-operation list.

The shared rule is reusable: attention items are for decisions about work or engine actions that need human sign-off, not for engine/execution mechanics the engine can describe itself. This design therefore uses a typed engine read model and does not write attention items.

## Alternatives considered

### Show every currently running async activity

Rejected because it fails both stable meaning and existing practice. Resident services would keep the badge permanently nonzero, while fast request tasks would flicker; worker-backed activity would duplicate the Agents tab. The checkable counterexample is the engine’s pollers and schedulers: they are intentionally long-lived and healthy precisely when they never finish.

### Let the app union source tables and apply its own threshold

Rejected because inclusion would become client policy. The app would need to know feature flags, distinguish conflict rungs from revision workers, infer stale planner rows, and keep threshold logic aligned with engine changes. This also creates the third read path the current Activity hand-union and unused `ListEngineAttempts` verb warn against.

### Publish a new start event and make the badge event-only

Not chosen for v1. A start event improves latency, but events are not a restart snapshot and the planner has no start publication today; the app would still need a query on connection or after dropped events. A five-second local poll over a tiny indexed set is sufficient for a feature intentionally delayed by 15 seconds, and it exposes the smallest correctness surface.

### Store background jobs as attention items

Rejected because no human decision clears ordinary planner or mechanical-rung execution. It would conflict with the related dispatch-halt ruling and burden attention lifecycle code with transient engine state.

### Merge the badge with the updater button

Rejected because the updater is app-owned release state with its own download progress and actions—there is no engine-side update checker—while this snapshot is engine-owned operational state and read-only. Combining them would make one popover responsible for unrelated ownership, lifecycles, and controls. The established toolbar already supports adjacent primary actions, including Notifications and Update, so adjacency survives contact with existing practice.

### Include merge-queue position

Rejected for v1 because the card already displays the position and because an external queue wait is not a running engine operation. This does not argue that global queue visibility is unimportant; it argues that it has a different subject and should not redefine this count.

## Chosen approach

### Eligibility invariant

An item appears if and only if the engine can establish all of these properties:

1. It is a finite operation with a stable identity and positive running marker.
2. It has no live `work_executions` row and no existing global live surface.
3. It is still executing in the current engine process.
4. Its actual operation start—not an enclosing attempt’s creation—was at least 15 seconds ago.
5. Its marker has a defined normal-completion clear and restart recovery path.

The 15-second threshold is an engine constant. It sits below the unmeasured 30-second low end of the planner estimate while suppressing short request work; the precise number is a policy choice, not a claim of measured planner latency. Existing `created_at`/`updated_at` data and the new rung-start timestamp let a later change be evidence-driven without moving policy into Swift.

No separate frequency debounce is needed for the selected population. At most one live planner row exists per project, and conflict remediation deduplicates/cools down per PR. The threshold removes common fast cases, while the five-second polling cadence bounds both appearance latency and a just-completed item’s remaining display time. The engine does not retain a completed item merely to satisfy a minimum dwell, because “running” must remain truthful.

### Source projection and crash behavior

| Source                   | Positive live evidence                                                              | Phase shown                                                                       | Normal clear                                                 | Engine death / restart                                                                                                                                         |
| ------------------------ | ----------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | ------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Project planner          | `planner_runs.outcome = 'running'`                                                  | `Planning <project>`; elapsed time from `created_at`                              | Every normal result updates the outcome                      | Startup transaction marks every inherited `running` row `planner_failed` with an explicit restart summary before clients or the populator can observe it       |
| Mechanical conflict rung | Non-`NULL` `mechanical_rung_in_flight` plus matching live lease-registry membership | `Rebasing <work item>` for rung 1; `Applying deterministic resolution` for rung 0 | Rung cleanup clears marker, start time, lease, and workspace | Existing startup conflict-ladder reconciliation abandons the headless attempt and clears the marker; ladder lease startup reap releases the orphaned workspace |

The planner reaper needs no heartbeat or stale-age guess. Planner tasks are process-local and cannot survive an engine restart, so every `running` row inherited at startup is definitively stranded. The sweep must execute before socket serving, pollers, and populator installation; it must leave `staged`, `applied`, and terminal failures untouched and free the project’s unique live-run slot for a retry.

Mechanical rung duration cannot be measured from `conflict_resolutions.created_at`, because the attempt may wait before obtaining a workspace. Add `mechanical_rung_started_at`, set it on the first in-flight stamp, preserve it when the phase moves from rung 1 to rung 0, and clear it everywhere the marker clears. Preserving the operation start prevents the item from disappearing for another threshold window during a phase transition; the existing startup reconciler remains authoritative for orphan cleanup.

### Engine-owned read model

Add a typed `BackgroundWorkItem` protocol projection with only fields the minimal popover needs:

- stable `id` namespaced by source;
- typed `kind` (`project_planner` or `conflict_mechanical_rung`);
- `source_id` for reconciliation with the source model;
- `product_id`, plus optional `project_id` or `work_item_id` context;
- engine-authored `title` and `phase`;
- `started_at`.

The response contains only currently live, threshold-qualified items, ordered oldest first for stable display. Its `visible_count` must equal the number of items; the app uses that value verbatim for the badge and must not filter or re-count by kind.

Extend `ListEngineAttempts` rather than adding `ListBackgroundWork`. The request gains an `include_background_work` flag defaulting false, and `EngineAttemptsList` gains a backward-compatible `background_work` snapshot defaulting empty. `limit = 0` is the background-only polling form, so the app does not refetch history every five seconds.

This is also the point to retire the macOS hand-union of `ListConflictResolutions` plus `ListCiRemediations` for Activity. The app consumes the unified attempt list for its rows and uses the existing kind-specific `Get…` verbs only when a selected detail pane needs fields absent from `EngineAttemptListEntry`. The badge is therefore a live view on the existing engine-attempt read path, not a third representation.

The CLI’s existing `boss engine attempts list` consumer must tolerate the additive response and gains a `--background` read-only form. That is a diagnostic view of the same snapshot, not another query implementation.

### Polling and planner reconciliation

The macOS model requests `ListEngineAttempts(limit: 0, include_background_work: true)` immediately after connection and every five seconds while connected. It cancels the timer and clears the snapshot on disconnect, preventing stale chrome when no engine can attest that work is still running.

Activity’s manual refresh uses the same verb with its normal history limit and also accepts the background snapshot. Normal completion events and work invalidations may trigger an immediate refresh, but correctness does not depend on an event arriving.

For planner items, `source_id` is the `planner_runs.id`. The global snapshot is authoritative only for the dimension it represents—current running state. `PlannerRunAffordance` uses that live identity for its hourglass and label; its existing project-scoped query remains authoritative for staged/applied history and full planner details. When a live item is clicked before the per-project row is cached, the app refreshes that project and opens the existing planner popover after the row arrives. Thus the two affordances share identity and running outcome without claiming their full data models are equivalent.

### Minimal toolbar affordance

Place `BackgroundWorkToolbarButton` in the primary-action group adjacent to `UpdateBadgeToolbarButton`, between Notifications and Update. Do not merge their icons or popovers.

When `visible_count == 0`, render no button. This is the correct feature-flag-disabled state and avoids an empty popover whose only content would explain that nothing is enabled. When nonzero, render a compact engine-work glyph with a capped numeric badge (`99+`) and an accessibility label such as “2 background operations running.”

Clicking opens one read-only popover. Each row contains only:

- the engine-authored title;
- project or work-item context when present;
- phase and elapsed time.

There are no buttons, navigation, history, success rows, failure rows, charts, or controls. If the final item completes while the popover is open, dismiss the popover when the next snapshot makes the toolbar button disappear rather than showing an empty state.

### Validation contract

The implementation is complete only when the genuine boundaries are exercised:

- A startup test seeds a real `running` planner row, starts the engine’s actual startup sequence, and proves the row becomes terminal before the list RPC can return it.
- Engine RPC tests drive planner and mechanical source rows on both sides of 15 seconds, confirm execution-backed CI/conflict rows never appear, and prove the count equals the returned list.
- A restart test proves a mechanical marker and lease cannot survive into a visible snapshot.
- Swift model tests decode the unified response, replace snapshots atomically, clear on disconnect, and prove Activity no longer sends the two legacy list requests.
- UI tests cover zero, one, multiple, and completion-while-open states. The toolbar PR captures the real isolated Boss UI for reviewer evidence rather than relying only on a hand-built SwiftUI preview.

## Risks / open questions

- **The 15-second threshold is calibrated from bounds and an estimate, not production measurements.** It is intentionally engine-owned and easy to change. If planner duration data shows many valid runs finish below it, lower the constant in the engine and update the contract test in the same change.
- **Migrating Activity to the unified list adds detail-on-demand behavior.** Selection must remain stable while the detail RPC returns, and a failed detail fetch must leave the common row visible with a local error rather than blanking the Activity list.
- **Mechanical visibility depends on two signals.** The durable row supplies identity and recovery; live registry membership supplies current-process truth. Tests must cover the small stamp/register and unregister/clear boundaries so neither transient ordering becomes a sticky false positive.
- **Polling is intentionally simple but not free.** The query is local and tiny, and five seconds is slower than the eligibility threshold. If source count grows substantially, the replacement should be snapshot-on-connect plus invalidation events, not faster polling or app-side caches.
- **A hung design-doc fetch remains possible because `gh_output` has no timeout.** That is a real reliability gap, but it is neither durable nor the finite operation this badge represents. Fix it at the command timeout boundary in separate reliability work rather than broadening this feature.

## Proposed implementation task breakdown

Breakdown size: 9 entries (8 in-scope, 1 deferred) — the change has two durability/source seams, one engine/protocol read contract with a thin CLI caller, and four ordered macOS seams needed to retire the old hand-union, poll safely, reconcile the planner indicator, and render the toolbar.

### Reap stranded planner runs during engine startup

Add a transactional `WorkDb` startup sweep that changes every inherited `planner_runs.outcome = 'running'` row to `planner_failed` with an engine-restart result summary, and invoke it before frontend serving and populator installation. Cover exact startup ordering, preservation of non-running outcomes, idempotence, and the ability to claim a new run for the same project after recovery.

Effort: medium

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with “Record exact mechanical-rung start time”; both may touch startup wiring, so if they edit `app/server.rs`, the later PR must forward-port the earlier startup ordering preservingly.

### Record exact mechanical-rung start time

Add and migrate `conflict_resolutions.mechanical_rung_started_at`; stamp it at actual rung entry, preserve it on the rung-1-to-rung-0 phase transition, and clear it on every marker-clear, terminal, and startup-reconcile path. Extend lifecycle and migration tests so the duration threshold never relies on the older enclosing conflict-attempt timestamp or resets between phases.

Effort: medium

Dependencies: none

Scope: in-scope

Parallelism: May run in parallel with “Reap stranded planner runs during engine startup” subject to the startup-wiring overlap noted there.

### Extend the unified engine-attempt RPC with live background work

Add the typed `BackgroundWorkItem` projection and extend `ListEngineAttempts`/`EngineAttemptsList` with the opt-in background snapshot. Implement the global running-planner query, the 15-second engine filter, mechanical row plus live-registry conjunction, stable ordering, count invariant, `limit = 0` background-only behavior, and engine/socket integration tests. This task must not expose CI remediation, merge queue state, worker-backed attempts, or resident loops.

Effort: large

Dependencies: Reap stranded planner runs during engine startup; Record exact mechanical-rung start time

Scope: in-scope

Parallelism: Begins only after both source-correctness prerequisites land.

### Expose the unified snapshot through the existing engine-attempt CLI

Update `boss engine attempts list` for the additive response and add a read-only `--background` rendering of the same engine-provided count/items. Do not query source tables from the CLI, and keep history output unchanged when the flag is absent.

Effort: small

Dependencies: Extend the unified engine-attempt RPC with live background work

Scope: in-scope

Parallelism: May run in parallel with “Adopt the unified engine-attempt list in macOS”; their files do not overlap.

### Adopt the unified engine-attempt list in macOS

Add Swift models/parsing for `EngineAttemptListEntry` and `BackgroundWorkItem`, switch Activity’s Engine filter from the two-list hand-union to `ListEngineAttempts`, and fetch kind-specific detail only on selection. Remove the old combined refresh calls so there is one list representation, while preserving current Activity row ordering, filters, and detail behavior.

Effort: large

Dependencies: Extend the unified engine-attempt RPC with live background work

Scope: in-scope

Parallelism: May run in parallel with the CLI task. It must land before polling so the app never temporarily carries both the legacy hand-union and a third live representation.

### Poll and own the global background snapshot in the macOS model

Add the connection-scoped five-second poll using the unified verb’s `limit = 0` form, atomically replace the engine-authored snapshot, accept event-triggered early refreshes, and cancel/clear on disconnect. Test timer lifetime, reconnect, out-of-order responses, count/list consistency, and completion without adding source-specific inclusion logic.

Effort: medium

Dependencies: Adopt the unified engine-attempt list in macOS

Scope: in-scope

Parallelism: Ordered after the unified-list adoption because both substantially edit the engine client and event-handling surfaces; forward-port the Activity migration rather than reintroducing legacy requests.

### Reconcile the existing planner indicator with global live state

Make the project-card hourglass consume the global planner item’s run identity for current running state while retaining the project-scoped planner query for staged/applied history and full popover detail. Add tests for a run discovered globally before project refresh, completion arriving through either path, and the invariant that two planner indicators cannot show conflicting running outcomes.

Effort: medium

Dependencies: Poll and own the global background snapshot in the macOS model

Scope: in-scope

Parallelism: Ordered after polling because it consumes that snapshot and is likely to overlap the same view-model state.

### Add the adjacent toolbar badge and minimal reveal popover

Add the conditional primary-action toolbar button beside the updater, capped count badge, accessibility text, and read-only row popover. Cover zero/one/many states, disabled mechanical feature flags, elapsed-time rendering, automatic dismissal after the last completion, and isolated-app screenshot evidence; add no actions, history, empty state, or dashboard elements.

Effort: medium

Dependencies: Reconcile the existing planner indicator with global live state

Scope: in-scope

Parallelism: Final v1 task; it depends on all state and reconciliation work so chrome cannot ship before the planner reaper.

### Design a separate global merge-queue summary if operators request it

If card-level queue position later proves insufficient, write a focused follow-up design comparing a global queue affordance with the existing Merging section and task-card badge. Keep that work outside the background-operation count unless it establishes a new finite engine operation rather than merely aggregating external queue state.

Effort: small

Dependencies: none

Scope: deferred (future / not a v1 blocker) — merge-queue position is already visible on task cards and is not running engine work

Parallelism: Independent of v1 and should not be scheduled from this project unless a new operator request reopens the decision.
