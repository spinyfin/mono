# Background task visibility: only long-lived, headless engine work enters the toolbar badge

- **Date:** 2026-08-12
- **Status:** Proposed
- **Provenance:** Project design for “Background task visibility: toolbar affordance for engine background work”
- **Related designs:** [Auto-populate project tasks on design PR merge](auto-populate-project-tasks-on-design-pr-merge.md), [Merge-conflict reduction and fast resolution](merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md), [Work execution](work-execution.md)
- **Related in-flight work:** “Surface parked/blocked work items in the kanban with an error indicator and a drag-to-retry gesture”

The load-bearing decision is that this badge is not a census of every async engine activity. It contains only finite operations that are long-lived and absent from any global live surface; worker executions stay in Agents, while a contextual source view such as the planner hourglass must share the badge’s operation identity instead of becoming a competing status.

## Verdict

Ship an engine-owned global snapshot with two v1 sources: project-planner runs older than 15 seconds and mechanical conflict rungs whose durable lease/phase marker matches the process-wide live ladder-lease snapshot. Poll every five seconds through the existing unified engine-attempt RPC, and render one toolbar button adjacent to—not merged with—the app updater.

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

| Claim                                    | Verified implementation                                                                                                      |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Execution-backed work is already visible | `protocol/src/types/execution.rs`, `engine/core/src/pane_summary.rs`, and coordinator pool routing                           |
| Planner persistence and publication      | `engine/core/src/work/planner_runs.rs`, `engine/core/src/populator.rs`, and `app-macos/Sources/PlannerAffordances.swift`     |
| Conflict activity, phase, and recovery   | `engine/core/src/ladder_lease_registry.rs`, `conflict_remediation.rs`, `work/conflict_res.rs`, and startup reconciliation    |
| CI vehicle selection                     | `engine/core/src/ci_watch.rs` and `engine/core/src/completion/remediation.rs`                                                |
| Poller topology and durable proxies      | `engine/core/src/merge_poller/schedule.rs`, `trunk_queue_poller.rs`, `github_api_usage.rs`, and `metrics/src/persistence.rs` |
| Existing engine-attempt read paths       | `protocol/src/wire.rs`, `engine/core/src/work/blocking.rs`, and `app-macos/Sources/ChatViewModel+Attentions.swift`           |
| Merge-queue card visibility              | `protocol/src/types/task.rs` and `app-macos/Sources/WorkCardBadges.swift`                                                    |
| Toolbar ownership and polling precedent  | `app-macos/Sources/ContentView.swift`, `ContentViewChrome.swift`, and `UpdateCore/UpdateModel.swift`                         |

### Existing visibility is the first exclusion

Every `work_executions` row has an `ExecutionKind`, is assigned to a worker pool, and receives a pane summary. The Agents tab is therefore the canonical live surface for answer agents, automation triage, PR review, CI-remediation retriggers, revision-backed CI fixes, and conflict-ladder worker rungs. Counting one of those executions in the toolbar would make the count mean “some visible work plus some invisible work,” which is not a stable property.

The answer agent specifically occupies a main-pool slot, holds a cube lease, and has a worker pane. Its read-only capability profile does not make it headless.

### The planner is both eligible and already partly visible

`planner_runs` records `outcome = 'running'` before the LLM work begins, but every current accessor is project- or id-scoped. The detached spawn in `populator.rs` publishes work/attention events only when it finishes; there is no global query and no start event.

The macOS app already renders the same row per project: `PlannerRunAffordance` uses an hourglass for `running`, and the popover says “Planner is running…”. Its refresh is pull-based per project and is triggered by a work invalidation. The toolbar badge must be a global projection of this same run identity, not a second planner status that can disagree with it.

Planner timing gives a defensible anti-flicker boundary but not a measured median. Each LLM call has a 180-second timeout, two transport attempts, and an outer two-pass schema-validation loop, so the bounded LLM portion can reach roughly twelve minutes. The repository contains only a 30–90 second typical-runtime estimate, not measurements; almost all of that time is the LLM because materialization is one short SQLite transaction.

Design-doc fetching can make up to three `gh api` calls with two 500 ms backoffs. Its command runner has no timeout, so “normally under five seconds” is not a hard upper bound; nevertheless, it remains request-scoped cache work with no durable operation identity. A wedged fetch deserves a timeout fix, not a new badge source.

### Conflict remediation has a free candidate query and live refinement

`conflict_resolutions.mechanical_rung_in_flight` is `1` for the engine-direct rebase, `0` for deterministic residual resolvers, and `NULL` otherwise. The attempt stamps its cube lease/workspace with the marker and clears all three unconditionally when the rung concludes. That row is durable and queryable, but `status IN ('pending', 'running')` also covers attempts waiting for retry or worker escalation; neither status nor a non-`NULL` marker proves that the current process still owns a queue task.

The existing `ListConflictResolutions` request accepts global `status` filters, so `pending` plus `running` is a zero-cost open-attempt baseline using the same rows Activity already consumes. It is not the badge count: an open attempt may be waiting on the two-permit semaphore, cooling down for retry, or about to dispatch a visible worker. Calling that population “work happening now” would give the badge the wrong meaning.

Two in-memory sources have different reachability. `ConflictRemediationQueue.slots` is trapped inside the merge-poller task and includes semaphore wait plus cooldown. In contrast, `ladder_lease_registry` is a process-wide static with a ready-made `snapshot()` of live `(lease_id, workspace_id)` pairs, callable from an RPC handler today. V1 therefore refines the open-attempt baseline by requiring a non-`NULL` mechanical rung marker whose durable lease/workspace pair is present in that snapshot. The result means “a headless mechanical rung currently holds a workspace in this process,” not “an attempt is open” and not “a remediation is waiting for capacity.”

This source is the strongest measured justification for the affordance. Mechanical runs have been observed at 1.5–8.75 minutes, concurrency is capped at two, and one earlier inline implementation produced 32 minutes with no merge-poller trace across seven consecutive ladder runs, delaying merge detection by 33 minutes. A conflict can now wait roughly one run per two entries ahead of it; that backlog explains why the open-attempt count can exceed the working count, while the badge appears only after an entry obtains a workspace.

That last queue wait is deliberately not counted in v1. It is invisible, but surfacing it would require plumbing `ConflictRemediationQueue.slots`; the existing lease snapshot is sufficient for the chosen “actually working” semantic. Activity can still show the broader open-attempt baseline, so the two counts are allowed to differ on the explicitly named dimension of current workspace activity.

The deterministic-resolver crate does not start another task or maintain an activity registry: its resolver registry visits conflicted files sequentially and logs each result. Rung 0 is therefore a phase of the enclosing remediation entry, not a separately counted operation. The ladder lease heartbeat likewise belongs to that same entry and must not be rendered separately.

Neither the current CLI conflict table nor the Swift conflict model carries the mechanical marker. Both mechanical rungs are absent on a stock install because `conflict_ladder_mechanical_rebase` defaults off; `speculative_conflict_prediction` also defaults off and is not a badge source. With the ladder flag off, this source contributes zero entries and the toolbar button simply remains hidden.

### The initial CI candidate fails the inclusion test

CI remediation is not headless once it becomes long-running. A fix attempt spawns an engine-triggered revision, while an infrastructure retrigger creates an `ExecutionKind::CiRemediation` execution; both are visible in Agents. The only headless interval is before dispatch, but `ci_watch` is handler code invoked from `merge_poller::sweep_one`, not a background loop, and `ci_remediations.status = 'pending'` does not prove that a handler is currently executing. That short transition has neither a positive live marker nor evidence that it survives the 15-second threshold, so it is excluded rather than inferred from a durable pending row.

The parent card also carries CI failure state, and Activity already lists the durable remediation attempt. `ci_remediations` and `ci_inflight_observations` are already RPC-exposed; a new badge projection would duplicate those surfaces before dispatch and the Agents pane after dispatch.

`ci_inflight_observations.alert_level_emitted` is useful phase data for a PR that has remained in flight, but it records observed CI state rather than a finite engine operation. `build_wait` is a Stop-message classifier, not a sweep, and `BuildWaitTracker` is an in-memory timer owned by the completion handler; neither has durable state of its own. The tracker describes a visible worker execution, retains `waited_secs` against a 45-minute horizon, and logs both values, but its execution-status publication carries only `reason = "worker_build_wait_pending"`; no percentage is published. These are precedents for engine-owned phase reporting, not toolbar sources.

### Poller activity is service health, not finite work

The merge poller is one process-lifetime `tokio` task with a 60-second full-sweep cadence. `TrunkQueueProbe` is a state machine driven from that task's `select!`, not a second poller loop, and `ci_watch` has no spawn or interval of its own. Counting pollers would therefore pin the badge at exactly one while the engine is healthy and reveal no finite operation to the user.

No SQLite row marks a merge, Trunk, or CI pass as in flight. `PrPollSchedule`, `TrunkQueueProbe.queues`, `ConflictRemediationQueue.slots`, and the per-pass `ProbeSnapshot` are process memory; after restart there is no pass state to recover or read out of process. Existing durable proxies rank as follows, but none is equivalent to current pass execution:

| Rank | Durable proxy                                         | Fidelity and existing surface                                                                                                                                      |
| ---- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1    | `github_api_calls`                                    | Per-call caller/API/verb/start/duration/cost, written within about two seconds and retained for 14 days; best evidence of recent poller activity, not an open pass |
| 2    | `tasks.pr_state_polled_at`                            | Per-work-item successful-check time already on the task wire and already rendered by the UI as “last checked”                                                      |
| 3    | `metrics_gauge['merge_poller.adaptive_tracked']`      | Adaptive-set size persisted on the metrics flush cadence, so it can be roughly 30 seconds stale and says nothing about a pass executing now                        |
| 4    | active `trunk_merge_intents` and task queue detail    | Durable queue membership and position; meaningful lifecycle state already projected onto task cards, but not engine work in progress                               |
| 5    | `ci_remediations` and `ci_inflight_observations` rows | Durable attempt/observation state with existing RPCs; mostly becomes a visible execution and does not identify a live `ci_watch` handler                           |

If poller diagnostics need a richer read surface later, they should expose these existing proxies in fidelity order rather than inventing an “in-flight pass” marker. That observability problem must not broaden this finite-operation badge.

### Merge-queue position is meaningful but belongs to the work item

`trunk_merge_intents` has no standalone list RPC or CLI, but the value a user cares about is already copied into `tasks.merge_queue_detail`, sent over the work-item wire model, used to order the Merging section, and rendered as a queue-position badge on the task card. Waiting in an external merge queue is PR lifecycle state, not evidence that Boss is currently executing a finite job.

Putting the same position in this popover would duplicate a more contextual surface and could keep the toolbar permanently active on a busy repository. A future request for a global merge-queue summary should be designed as such; it is not v1 background-work visibility.

### Other background code is not eligible

- Resident loops—merge polling, schedulers, heartbeats, backups, PR-review recovery, external-tracker reconciliation, metrics flushes, monitoring, and socket servers—are engine services rather than finite operations. Including them would pin the badge on for the lifetime of the engine. The Trunk observer and `ci_watch` are not additional loops, while PR-review recovery runs every 60 seconds and external-tracker reconciliation every 120 seconds; topology and cadence do not turn any of them into a finite user operation.
- `envelope_watch` is a 60-second resident sweep over `work_executions` plus live worker state. Its durable output is an attention item for an over-envelope visible worker, which is correctly a human-attention concern; counting the sweep or the worker here would duplicate existing surfaces.
- `build_wait` and `BuildWaitTracker` do not spawn a sweep loop. They classify a visible worker’s Stop narration and retain only an in-memory first-seen timestamp, reset on engine restart.
- There is no GitHub PR-comment poller to include. The only `issues/{number}/comments` request is the Trunk bot probe; the PR-review crate parses and renders review material but does not run a comment-polling service.
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

### Count open conflict attempts or plumb the queue

Neither option matches v1’s “actually working” semantic. The existing global `ListConflictResolutions(status: [pending, running])` request cheaply returns open attempts, but those rows include semaphore wait, retry states, and paths about to become visible workers. Promoting `ConflictRemediationQueue.slots` into shared state could distinguish queued work, but it adds plumbing solely to broaden the badge beyond active mechanical rungs. Intersecting the durable candidates with the already-readable ladder lease snapshot is both cheaper and more exact for v1.

### Let the app union source tables and apply its own threshold

Rejected because inclusion would become client policy. The app would need to know feature flags, distinguish conflict rungs from revision workers, infer stale planner rows, and keep threshold logic aligned with engine changes. This also creates the third read path the current Activity hand-union and unused `ListEngineAttempts` verb warn against.

### Publish a new start event and make the badge event-only

Not chosen for v1. A start event improves latency, but events are not a restart snapshot and the planner has no start publication today; the app would still need a query on connection or after dropped events. A five-second local poll over a tiny indexed set is sufficient for a planner intentionally delayed by 15 seconds and for mechanical rungs measured in minutes, and it exposes the smallest correctness surface.

### Store background jobs as attention items

Rejected because no human decision clears ordinary planner or mechanical-rung execution. It would conflict with the related dispatch-halt ruling and burden attention lifecycle code with transient engine state.

### Merge the badge with the updater button

Rejected because the updater is app-owned release state with its own download progress and actions—there is no engine-side update checker—while this snapshot is engine-owned operational state and read-only. Combining them would make one popover responsible for unrelated ownership, lifecycles, and controls. The established toolbar already supports adjacent primary actions, including Notifications and Update, so adjacency survives contact with existing practice.

The updater is still the closest chrome precedent. `UpdateModel` owns published state and a cancellable polling task, waits a 30–120 second launch jitter, checks every six hours, and conditionally renders `UpdateBadgeToolbarButton`. The background-work client should mirror that observable-model/timer/button shape, but not its ownership or persistence: the engine decides background membership, and the app must clear rather than persist the snapshot when disconnected.

### Include merge-queue position

Rejected for v1 because the card already displays the position and because an external queue wait is not a running engine operation. This does not argue that global queue visibility is unimportant; it argues that it has a different subject and should not redefine this count.

## Chosen approach

### Eligibility invariant

An item appears if and only if the engine can establish all of these properties:

1. It is a finite operation with a stable identity and positive running marker.
2. It has no live `work_executions` row and no existing global live surface.
3. The current engine process can attest that work is happening now; an open attempt or wait for bounded capacity is insufficient.
4. It has crossed its source-specific anti-flicker gate: a planner row is at least 15 seconds old, while a mechanical conflict item has both a durable rung marker and a matching live workspace lease.
5. Its marker has a defined normal-completion clear and restart recovery path.

The planner’s 15-second threshold is an engine constant. It sits below the unmeasured 30-second low end of the planner estimate while suppressing common short cases; the precise number is a policy choice, not a claim of measured planner latency. The conflict source needs no new timestamp or duration gate: acquiring a real workspace plus stamping a mechanical rung is its structural admission gate, and the observed rung range is 1.5–8.75 minutes. Polling still suppresses attempts that acquire and release between five-second snapshots.

No separate frequency debounce is needed for the selected population. At most one live planner row exists per project, conflict remediation deduplicates per PR, and semaphore-wait/cooldown entries lack a live lease and are excluded. The planner threshold removes its common fast cases, while the five-second polling cadence bounds both appearance latency and a just-completed item’s remaining display time. The engine does not retain a completed item merely to satisfy a minimum dwell, because “running” must remain truthful.

### Source projection and crash behavior

| Source                   | Positive live evidence                                                                                                                 | Phase shown                                                                       | Normal clear                                        | Engine death / restart                                                                                                                                   |
| ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Project planner          | `planner_runs.outcome = 'running'` and `created_at` is at least 15 seconds old                                                         | `Planning <project>`; elapsed time from `created_at`                              | Every normal result updates the outcome             | Startup transaction marks every inherited `running` row `planner_failed` with an explicit restart summary before clients or the populator can observe it |
| Mechanical conflict rung | Open conflict row with non-`NULL` rung/lease/workspace, and the lease/workspace pair is present in `ladder_lease_registry::snapshot()` | `Rebasing <work item>` for rung 1; `Applying deterministic resolution` for rung 0 | Row clears its marker; live registry unregisters it | Process-local snapshot is empty immediately; startup conflict reconciliation abandons the durable attempt, and ladder lease reap releases the workspace  |

The planner reaper needs no heartbeat or stale-age guess. Planner tasks are process-local and cannot survive an engine restart, so every `running` row inherited at startup is definitively stranded. The sweep must execute before socket serving, pollers, and populator installation; it must leave `staged`, `applied`, and terminal failures untouched and free the project’s unique live-run slot for a retry.

Conflict remediation duration cannot be measured from `conflict_resolutions.created_at`, because an attempt may wait before obtaining a workspace, and the live lease registry does not record acquisition time. V1 does not fabricate elapsed time or add tracking to fill that gap. It reuses the global `pending`/`running` conflict query as a candidate set, then intersects rows carrying a mechanical marker with `ladder_lease_registry::snapshot()` on both lease id and workspace id. The live snapshot is authoritative only for current-process lease ownership; the row is authoritative only for durable attempt/work-item identity and rung `0`/`1` phase.

The stamp currently precedes live registration, and normal cleanup clears the stamp before unregistering the lease. Requiring both signals suppresses those boundary windows: a durable row without a matching snapshot is never shown, and unregistering makes completion disappear even if marker cleanup fails. On restart the empty snapshot prevents a stale badge immediately, while existing conflict reconciliation and ladder lease reap still clean the durable attempt and workspace so the operation can retry safely.

### Engine-owned read model

Add a typed `BackgroundWorkItem` protocol projection with only fields the minimal popover needs:

- stable `id` namespaced by source;
- typed `kind` (`project_planner` or `conflict_remediation`);
- `source_id` for reconciliation with the source model;
- `product_id`, plus optional `project_id` or `work_item_id` context;
- engine-authored `title` and `phase`;
- optional `started_at`; planners provide it, while mechanical rungs omit it because neither existing source records lease-acquisition time.

The response contains only source-qualified items in a stable engine order: known starts oldest first, then items without a start ordered by source id. Its `visible_count` must equal the number of items; the app uses that value verbatim for the badge and must not filter or re-count by kind. The engine reuses the query behind `ListConflictResolutions(status: [pending, running])` for candidates, then excludes every row without a mechanical marker and matching live lease/workspace snapshot. Open, queued, cooldown, and worker-bound attempts therefore remain outside the badge.

Extend `ListEngineAttempts` rather than adding `ListBackgroundWork`. The request gains an `include_background_work` flag defaulting false, and `EngineAttemptsList` gains a backward-compatible `background_work` snapshot defaulting empty. `limit = 0` is the background-only polling form, so the app does not refetch history every five seconds.

This is also the point to retire the macOS hand-union of `ListConflictResolutions` plus `ListCiRemediations` for Activity. The app consumes the unified attempt list for its rows and uses the existing kind-specific `Get…` verbs only when a selected detail pane needs fields absent from `EngineAttemptListEntry`. The badge is therefore a live view on the existing engine-attempt read path, not a third representation.

The CLI’s existing `boss engine attempts list` consumer must tolerate the additive response and gains a `--background` read-only form. That is a diagnostic view of the same snapshot, not another query implementation.

### Polling and planner reconciliation

The macOS model requests `ListEngineAttempts(limit: 0, include_background_work: true)` immediately after connection and every five seconds while connected. It owns a cancellable polling task and published snapshot in the same architectural shape as `UpdateModel`, without copying the updater’s launch jitter, six-hour cadence, or `UserDefaults` persistence. It cancels the timer and clears the snapshot on disconnect, preventing stale chrome when no engine can attest that work is still running.

Activity’s manual refresh uses the same verb with its normal history limit and also accepts the background snapshot. Normal completion events and work invalidations may trigger an immediate refresh, but correctness does not depend on an event arriving.

For planner items, `source_id` is the `planner_runs.id`. The global snapshot is authoritative only for the dimension it represents—current running state. `PlannerRunAffordance` uses that live identity for its hourglass and label; its existing project-scoped query remains authoritative for staged/applied history and full planner details. When a live item is clicked before the per-project row is cached, the app refreshes that project and opens the existing planner popover after the row arrives. Thus the two affordances share identity and running outcome without claiming their full data models are equivalent.

### Minimal toolbar affordance

Place `BackgroundWorkToolbarButton` in the primary-action group adjacent to `UpdateBadgeToolbarButton`, between Notifications and Update. Do not merge their icons or popovers.

When `visible_count == 0`, render no button. This is the correct feature-flag-disabled state and avoids an empty popover whose only content would explain that nothing is enabled. When nonzero, render a compact engine-work glyph with a capped numeric badge (`99+`) and an accessibility label such as “2 background operations running.”

Clicking opens one read-only popover. Each row contains only:

- the engine-authored title;
- project or work-item context when present;
- phase, plus elapsed time only when the engine supplies an actual operation start.

There are no buttons, navigation, history, success rows, failure rows, charts, or controls. If the final item completes while the popover is open, dismiss the popover when the next snapshot makes the toolbar button disappear rather than showing an empty state.

### Validation contract

The implementation is complete only when the genuine boundaries are exercised:

- A startup test seeds a real `running` planner row, starts the engine’s actual startup sequence, and proves the row becomes terminal before the list RPC can return it.
- Engine RPC tests drive planner rows on both sides of 15 seconds, then cover open conflict rows with no marker, a marker with no live lease, mismatched lease/workspace pairs, matching rung-0/rung-1 pairs, and worker-bound attempts. They prove the count equals the returned list and that execution-backed CI/conflict rows never appear.
- A real startup test proves the process-local ladder lease snapshot is empty and a durable mechanical marker cannot enter the visible response while conflict reconciliation and lease reap clean up the orphan.
- Swift model tests decode the unified response, replace snapshots atomically, clear on disconnect, and prove Activity no longer sends the two legacy list requests.
- UI tests cover zero, one, multiple, and completion-while-open states. The toolbar PR captures the real isolated Boss UI for reviewer evidence rather than relying only on a hand-built SwiftUI preview.

## Risks / open questions

- **The planner’s 15-second threshold is calibrated from bounds and an estimate, not production measurements.** It is intentionally engine-owned and easy to change. If planner duration data shows many valid runs finish below it, lower the constant in the engine and update the contract test in the same change.
- **Migrating Activity to the unified list adds detail-on-demand behavior.** Selection must remain stable while the detail RPC returns, and a failed detail fetch must leave the common row visible with a local error rather than blanking the Activity list.
- **Conflict visibility depends on two signals with deliberately limited equivalence.** The durable row and process-wide lease snapshot are equivalent only on lease/workspace identity: the snapshot owns current-process lease liveness, while the row owns attempt context and rung phase. Neither supplies lease age, so the UI must omit conflict elapsed time rather than relabel attempt age. The intersection intentionally undercounts open attempts waiting on the semaphore; that is the chosen “actually working” semantic, not data loss.
- **Polling is intentionally simple but not free.** The query is local and tiny, and five seconds is slower than the eligibility threshold. If source count grows substantially, the replacement should be snapshot-on-connect plus invalidation events, not faster polling or app-side caches.
- **A hung design-doc fetch remains possible because `gh_output` has no timeout.** That is a real reliability gap, but it is neither durable nor the finite operation this badge represents. Fix it at the command timeout boundary in separate reliability work rather than broadening this feature.

## Proposed implementation task breakdown

Breakdown size: 8 entries (7 in-scope, 1 deferred) — the change has one planner-recovery seam, one engine/protocol contract that reuses the existing conflict query and lease snapshot, a thin CLI caller, and four ordered macOS seams needed to retire the old hand-union, poll safely, reconcile the planner indicator, and render the toolbar.

### Reap stranded planner runs during engine startup

Add a transactional `WorkDb` startup sweep that changes every inherited `planner_runs.outcome = 'running'` row to `planner_failed` with an engine-restart result summary, and invoke it before frontend serving and populator installation. Cover exact startup ordering, preservation of non-running outcomes, idempotence, and the ability to claim a new run for the same project after recovery.

Effort: medium

Dependencies: none

Scope: in-scope

Parallelism: First correctness prerequisite. The unified read-surface task is ordered after it so no count can reach clients before startup recovery is authoritative.

### Extend the unified engine-attempt RPC with live background work

Add the typed `BackgroundWorkItem` projection and extend `ListEngineAttempts`/`EngineAttemptsList` with the opt-in background snapshot. Implement the new global running-planner query and 15-second filter; reuse the global `pending`/`running` conflict query as candidates and intersect mechanical rows with `ladder_lease_registry::snapshot()` on lease plus workspace. Add rung phase mapping, an optional wire start time populated only for planners, stable ordering, the count invariant, `limit = 0` background-only behavior, and engine/socket integration tests. Do not plumb `ConflictRemediationQueue.slots`; open attempts without a matching live lease, merge-queue state, worker-backed attempts, and resident loops must stay hidden.

Effort: large

Dependencies: Reap stranded planner runs during engine startup

Scope: in-scope

Parallelism: Begins after planner recovery lands. The CLI and macOS adoption tasks may then proceed in parallel against this contract.

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

Add the conditional primary-action toolbar button beside the updater, capped count badge, accessibility text, and read-only row popover. Cover zero/one/many states, disabled mechanical feature flags, planner elapsed-time rendering, omission of fabricated conflict elapsed time, automatic dismissal after the last completion, and isolated-app screenshot evidence; add no actions, history, empty state, or dashboard elements.

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
