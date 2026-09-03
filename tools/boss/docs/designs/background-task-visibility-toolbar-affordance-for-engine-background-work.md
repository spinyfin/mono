# Background task visibility: only long-lived, headless engine work enters the toolbar badge

- **Date:** 2026-08-12 (design); revised 2026-09-02 against the shipped implementation
- **Status:** Implemented — v1 shipped; both deferred entries remain deferred
- **Shipped as:** [mono#2745](https://github.com/spinyfin/mono/pull/2745) startup planner recovery, [mono#2776](https://github.com/spinyfin/mono/pull/2776) engine/protocol snapshot, [mono#2789](https://github.com/spinyfin/mono/pull/2789) CLI, [mono#2792](https://github.com/spinyfin/mono/pull/2792) unified attempt list in macOS, [mono#2799](https://github.com/spinyfin/mono/pull/2799) macOS polling, [mono#2827](https://github.com/spinyfin/mono/pull/2827) planner-indicator reconciliation, [mono#2829](https://github.com/spinyfin/mono/pull/2829) toolbar badge and popover
- **Provenance:** Project design for “Background task visibility: toolbar affordance for engine background work”
- **Related designs:** [Auto-populate project tasks on design PR merge](auto-populate-project-tasks-on-design-pr-merge.md), [Merge-conflict reduction and fast resolution](merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md), [Work execution](work-execution.md)
- **Related in-flight work:** “Surface parked/blocked work items in the kanban with an error indicator and a drag-to-retry gesture”

The load-bearing decision is that this badge is not a census of every async engine activity. It contains only finite operations that are long-lived and absent from any global live surface; worker executions stay in Agents, while a contextual source view such as the planner hourglass must share the badge’s operation identity instead of becoming a competing status.

## Verdict

The engine owns a global snapshot with two v1 sources: project-planner runs older than 15 seconds, and conflict attempts carrying the durable `mechanical_rung_in_flight` execution marker. The macOS app polls it every five seconds through the existing unified engine-attempt RPC and renders one toolbar button adjacent to—not merged with—the app updater. That is what shipped.

The planner’s stranded-`running` startup reaper landed first, as required. Two things about that prerequisite are weaker as built than as written, and both are recorded below: recovery runs _after_ the live-frontend-listener refusal check rather than first, because a startup about to refuse itself must not mutate a database a live engine still owns; and a database error during recovery is logged while startup continues, rather than refusing to serve.

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

As built, the two were made unable to disagree by giving the global snapshot sole ownership of running state, for both indicators. That closed the disagreement and also changed what the project card shows during the badge’s 15-second anti-flicker window; the consequence is stated in “Polling and planner reconciliation” rather than left implicit here.

Planner timing gives a defensible anti-flicker boundary but not a measured median. Each LLM call has a 180-second timeout, two transport attempts, and an outer two-pass schema-validation loop, so the bounded LLM portion can reach roughly twelve minutes. The repository contains only a 30–90 second typical-runtime estimate, not measurements; almost all of that time is the LLM because materialization is one short SQLite transaction.

Design-doc fetching can make up to three `gh api` calls with two 500 ms backoffs. Its command runner has no timeout, so “normally under five seconds” is not a hard upper bound; nevertheless, it remains request-scoped cache work with no durable operation identity. A wedged fetch deserves a timeout fix, not a new badge source.

### The mechanical rung marker is the conflict source of truth

`conflict_resolutions.mechanical_rung_in_flight` is `1` for the engine-direct rebase, `0` for deterministic residual resolvers, and `NULL` otherwise. The attempt stamps its cube lease/workspace with the marker and clears all three unconditionally when the rung concludes. The marker is durable so startup recovery can identify a rung killed by restart. In contrast, `status IN ('pending', 'running')` also covers attempts waiting for retry or worker escalation and says nothing about whether a mechanical rung is executing.

The existing `ListConflictResolutions` request accepts global `status` filters, so `pending` plus `running` is a zero-cost open-attempt baseline using the same rows Activity already consumes. It is not the badge count: an open attempt may be waiting for remediation, cooling down for retry, or about to dispatch a visible worker. Calling that population “work happening now” would give the badge the wrong meaning.

The queue does not supply the missing distinction. `ConflictRemediationQueue` writes `SlotState::InFlight` before awaiting one of its two permits, so that state conflates queued and executing jobs; it retains a separate `CompletedAt` only for cooldown. The slot map is also trapped inside the merge-poller task. Queue depth—for example, “three conflicts waiting”—has no RPC, metric, or durable record, and even exposing the current map would not produce it. Supporting queue depth would require new waiting-versus-running state around permit acquisition plus a read surface, which v1 does not need.

`mechanical_rung_in_flight` is the only positive signal whose meaning is “a mechanical rung is executing.” It is stamped with the rung plus lease/workspace immediately after lease acquisition, changes from `1` to `0` if residual conflicts enter deterministic resolution, and is cleared unconditionally when the mechanical sequence concludes. V1 uses that field as the primary inclusion and phase source. The process-wide `ladder_lease_registry::snapshot()` is already callable from the handler, but it means only “this process holds the lease”; it is a negative safety check against a failed marker-clear write, not the semantic upgrade from open attempt to executing rung.

This source is the strongest measured justification for the affordance. Mechanical runs have been observed at 1.5–8.75 minutes, concurrency is capped at two, and one earlier inline implementation produced 32 minutes with no merge-poller trace across seven consecutive ladder runs, delaying merge detection by 33 minutes. A conflict can now wait roughly one run per two entries ahead of it; that backlog explains why the open-attempt count can exceed the working count, while the badge appears only after an entry obtains a workspace.

Queue wait is deliberately not counted in v1 because it is not observable separately today. Activity can still show the broader open-attempt baseline, so the two counts are allowed to differ on the explicitly named dimension of active mechanical execution.

The deterministic-resolver crate does not start another task or maintain an activity registry: its resolver registry visits conflicted files sequentially and logs each result. Rung 0 is therefore a phase of the enclosing remediation entry, not a separately counted operation. The ladder lease heartbeat likewise belongs to that same entry and must not be rendered separately.

JSON already carried the mechanical marker while both human renderers dropped it: the CLI conflict table/detail omitted it, and `WorkConflictResolution` had no `mechanicalRungInFlight` field. Both gaps are closed — the CLI table gained a `MECH RUNG` column and the detail view prints the field, and the Swift model decodes it — while the toolbar renders the engine-authored background projection rather than either of them. Both mechanical rungs are absent on a stock install because `conflict_ladder_mechanical_rebase` defaults off; `speculative_conflict_prediction` also defaults off and is not a badge source. With the ladder flag off, this source contributes zero entries and the toolbar button simply remains hidden.

The duration evidence contains an in-tree contradiction. `ladder_lease_heartbeat.rs` says a mechanical run is normally seconds or well under a minute, while the remediation module records observed full ladder runs of 1.5–8.75 minutes and the seven-run, 32-minute blackout. The measured range is the authoritative calibration and the heartbeat prose was stale. Its constants remain safe without a behavior change: the 600-second TTL exceeds the measured 525-second maximum, and the 120-second heartbeat refreshes a live lease several times within that TTL. The comments were corrected in the same engine change that added the read surface; the TTL and interval were not touched.

### The initial CI candidate fails the inclusion test

CI remediation is not headless once it becomes long-running. A fix attempt spawns an engine-triggered revision, while an infrastructure retrigger creates an `ExecutionKind::CiRemediation` execution; both are visible in Agents. The only headless interval is before dispatch, but `ci_watch` is handler code invoked from `merge_poller::sweep_one`, not a background loop, and `ci_remediations.status = 'pending'` does not prove that a handler is currently executing. That short transition has neither a positive live marker nor evidence that it survives the 15-second threshold, so it is excluded rather than inferred from a durable pending row.

The parent card also carries CI failure state, and Activity already lists the durable remediation attempt. `ci_remediations` and `ci_inflight_observations` are already RPC-exposed; a new badge projection would duplicate those surfaces before dispatch and the Agents pane after dispatch.

`ci_inflight_observations.alert_level_emitted` is useful phase data for a PR that has remained in flight, but it records observed CI state rather than a finite engine operation. `build_wait` is a Stop-message classifier, not a sweep, and `BuildWaitTracker` is an in-memory timer owned by the completion handler; neither has durable state of its own. The tracker describes a visible worker execution, retains `waited_secs` against a 45-minute horizon, and logs both values, but its execution-status publication carries only `reason = "worker_build_wait_pending"`; no percentage is published. These are precedents for engine-owned phase reporting, not toolbar sources.

### Poller activity is service health, not finite work

The merge poller is one process-lifetime `tokio` task with a 60-second full-sweep cadence. `TrunkQueueProbe` is a state machine driven from that task’s `select!`, not a second poller loop, and `ci_watch` has no spawn or interval of its own. Counting pollers would therefore pin the badge at exactly one while the engine is healthy and reveal no finite operation to the user.

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
- `envelope_watch` is a millisecond-cheap 60-second sweep over `work_executions` plus live worker state. For trivial/small/medium/large executions it derives the exact inputs for a continuous `elapsed_secs / envelope_secs` ratio (600/900/1800/3600-second envelopes) and `over_by_secs`; `Max` deliberately has no envelope. That is high-quality engine state, but every subject is already an Agents-pane execution, so putting the same executions in this badge would violate the first exclusion. If surfaced, the ratio belongs on the existing Agents live model. Its `envelope_overrun` attention row must not be reused as transport for engine state.
- `build_wait` and `BuildWaitTracker` do not spawn a sweep loop. They classify a visible worker’s Stop narration and retain only an in-memory first-seen timestamp, reset on engine restart.
- There is no GitHub PR-comment poller to include. The only `issues/{number}/comments` request is the Trunk bot probe; the PR-review crate parses and renders review material but does not run a comment-polling service.
- Comment-intent `NULL` means no classification result, not proof that classification code is executing. Absence is never a live marker.
- Short request handlers, cache fills, attachment serving, GitHub authentication, and design-doc fetches do not satisfy the duration and operation-identity tests.
- Automation triage, PR-review workers, answer agents, CI remediation, build waits, and conflict worker rungs are already execution-backed.

No repository document records an earlier global inclusion policy; the existing surfaces grew feature by feature. That silence is a finding: v1 establishes the policy above rather than pretending an unwritten policy already existed.

### Coordination with parked/blocked work-item state

The related parked/blocked effort is moving a dispatch halt onto typed task fields so the kanban can render it directly. That representation is intentionally scoped to a work item and is not reusable as a global, finite-operation list.

The shared rule is reusable: attention items are for decisions about work or engine actions that need human sign-off, not for engine/execution mechanics the engine can describe itself. The background-work read model therefore writes no attention items — running state is described, not escalated.

The startup planner reaper does raise one attention per recovered run, and that is the same rule applied rather than an exception to it. A planner run cut short by a restart leaves a decision no engine state can describe: re-run the populate, or not. The distinction is between mechanics (never an attention) and a residue that needs a human (always one).

## Alternatives considered

### Show every currently running async activity

Rejected because it fails both stable meaning and existing practice. Resident services would keep the badge permanently nonzero, while fast request tasks would flicker; worker-backed activity would duplicate the Agents tab. The checkable counterexample is the engine’s pollers and schedulers: they are intentionally long-lived and healthy precisely when they never finish.

### Count open conflict attempts or plumb the queue

Neither option matches v1’s “mechanical rung executing” semantic. The existing global `ListConflictResolutions(status: [pending, running])` request cheaply returns open attempts, but those rows include retry states and paths about to become visible workers. Plumbing `ConflictRemediationQueue.slots` would not fix the ambiguity because `InFlight` is claimed before permit acquisition and conflates queued with running. The existing mechanical-rung marker already expresses the required state exactly; queue depth would need a new state model and is out of scope.

### Let the app union source tables and apply its own threshold

Rejected because inclusion would become client policy. The app would need to know feature flags, distinguish conflict rungs from revision workers, infer stale planner rows, and keep threshold logic aligned with engine changes. This also creates the third read path the current Activity hand-union and unused `ListEngineAttempts` verb warn against.

The rejection held where it mattered — the badge’s membership is entirely engine-authored, and the Swift client counts even a kind it does not recognise rather than applying a filter of its own. It did not eliminate the second read path it also argued against: the legacy per-table lists survive for the kanban card badges, described under “Engine-owned read model”.

### Publish a new start event and make the badge event-only

Not chosen for v1. A start event improves latency, but events are not a restart snapshot and the planner has no start publication today; the app would still need a query on connection or after dropped events. A five-second local poll over a tiny indexed set is sufficient for a planner intentionally delayed by 15 seconds and for mechanical rungs measured in minutes, and it exposes the smallest correctness surface.

### Store background jobs as attention items

Rejected because no human decision clears ordinary planner or mechanical-rung execution. It would conflict with the related dispatch-halt ruling and burden attention lifecycle code with transient engine state.

This project does write one attention, so the rejection has to be checkable against its own practice rather than resting on the label. The startup reaper raises a `followup` for each planner run a restart interrupted. That is not a background job stored as an attention: the run is over, not transient, and it leaves a decision — re-run the populate or not — that clears only when a human makes it. The rejected shape is the opposite one, where the attention is the _live_ representation and completion clears it with no human involved.

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
4. It has crossed its source-specific anti-flicker gate: a planner row is at least 15 seconds old, while a conflict item has entered a mechanical rung and carries its exact durable marker.
5. Its marker has a defined normal-completion clear and restart recovery path.

The planner’s 15-second threshold is an engine constant. It sits below the unmeasured 30-second low end of the planner estimate while suppressing common short cases; the precise number is a policy choice, not a claim of measured planner latency. The conflict source needs no new timestamp or duration gate: acquiring a real workspace plus stamping a mechanical rung is its structural admission gate, and the observed rung range is 1.5–8.75 minutes. Polling still suppresses attempts that acquire and release between five-second snapshots.

No separate frequency debounce is needed for the selected population. At most one live planner row exists per project, conflict remediation deduplicates per PR, and semaphore-wait/cooldown entries lack a live lease and are excluded. The planner threshold removes its common fast cases, while the five-second polling cadence bounds both appearance latency and a just-completed item’s remaining display time. The engine does not retain a completed item merely to satisfy a minimum dwell, because “running” must remain truthful.

### Source projection and crash behavior

| Source                   | Positive live evidence                                                                         | Phase shown                                                                       | Normal clear                                        | Engine death / restart                                                                                                                                                                                                                                           |
| ------------------------ | ---------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Project planner          | `planner_runs.outcome = 'running'` and `created_at` is at least 15 seconds old                 | `Planning <project>`; elapsed time from `created_at`                              | Every normal result updates the outcome             | Startup transaction marks every inherited `running` row `planner_failed` with an explicit restart summary, after the live-frontend refusal check and before the socket binds or the populator is installed; each recovered run also raises a follow-up attention |
| Mechanical conflict rung | `mechanical_rung_in_flight` is `1` or `0`, with its stamped lease/workspace still process-live | `Rebasing <work item>` for rung 1; `Applying deterministic resolution` for rung 0 | Row clears its marker; live registry unregisters it | Startup conflict reconciliation clears the durable marker before serving; ladder lease reap releases the workspace                                                                                                                                               |

The planner reaper needs no heartbeat or stale-age guess. Planner tasks are process-local and cannot survive an engine restart, so every `running` row inherited at startup is definitively stranded. The sweep runs in one transaction, leaves `staged`, `applied`, and terminal failures untouched, and frees the project’s unique live-run slot for a retry.

Its placement in startup is narrower than this design first stated. The sweep does not run first: it runs after the tmux preflight and after the live-frontend-listener refusal check, because a startup that is about to refuse itself must not mutate a database a live engine still owns. It does run before the socket binds and before the populator is installed, which is the property that actually matters — the ordering constraint is “no client can observe an inherited `running` row”, not “first in `serve`”.

Recovery failure is not fatal. A database error during the sweep is logged and startup continues, so on that one path a stale `running` row older than 15 seconds can reach the badge. That is a deliberate availability trade-off — an engine that cannot write its own database has larger problems than a wrong badge — but it means “no count may appear in chrome before recovery is authoritative” holds on every path except this one, and the doc should not claim otherwise.

Conflict remediation duration cannot be measured from `conflict_resolutions.created_at`, because an attempt may wait before obtaining a workspace, and the mechanical marker has no timestamp. V1 does not fabricate elapsed time or add tracking to fill that gap. It reuses the global `pending`/`running` conflict query as a candidate set and treats non-`NULL` `mechanical_rung_in_flight` as the positive inclusion and phase source. The stamped lease/workspace is also checked against `ladder_lease_registry::snapshot()`, but only as a current-process veto for a marker that failed to clear; lease membership by itself never creates an item.

The stamp currently precedes live registration, and normal cleanup clears the stamp before unregistering the lease. Requiring the marker plus a non-stale lease suppresses those boundary windows: registration makes the stamped rung visible, successful clearing removes it first, and unregistering removes it even if marker cleanup fails. On restart the empty registry prevents the inherited marker from reaching the snapshot while startup conflict reconciliation clears the durable attempt before serving and ladder lease reap recovers the workspace.

### Engine-owned read model

Add a typed `BackgroundWorkItem` protocol projection with only fields the minimal popover needs:

- stable `id` namespaced by source;
- typed `kind` (`project_planner` or `conflict_remediation`);
- `source_id` for reconciliation with the source model;
- `product_id`, plus optional `project_id` or `work_item_id` context;
- engine-authored `title` and `phase`;
- optional `started_at`; planners provide it, while mechanical rungs omit it because neither existing source records lease-acquisition time.

The response contains only source-qualified items in a stable engine order: known starts oldest first, then items without a start ordered by `source_id`. Ordering compares `started_at` as a string, which is correct because the engine writes fixed-width epoch seconds.

There is no `visible_count` field. This design originally specified one, with the invariant “`visible_count` must equal the number of items”. Implementation dropped it, and that is the better shape: the load-bearing property is that the badge equals the snapshot, and a separate integer is a container that can drift from the thing it counts. `background_work.len()` carries the property structurally, so it cannot be violated rather than merely being asserted by a test. What the app must still honour is unchanged — use the length verbatim, and never filter or re-count by kind.

The engine reuses the query behind `ListConflictResolutions(status: [pending, running])` for candidates, then includes only rows with a mechanical marker and a non-stale stamped lease/workspace. Open, queued, cooldown, and worker-bound attempts therefore remain outside the badge. A snapshot that fails to build degrades rather than failing the response: the handler logs and returns an empty `background_work` list, so a transient auxiliary error cannot blank Activity’s primary attempts list. The cost is that a build failure is indistinguishable on the wire from “nothing is running” — acceptable while the next poll is five seconds away, and recorded as a risk below.

Extend `ListEngineAttempts` rather than adding `ListBackgroundWork`. The request gains an `include_background_work` flag defaulting false, and `EngineAttemptsList` gains a backward-compatible `background_work` snapshot defaulting empty. `limit = 0` is the background-only polling form, so the app does not refetch history every five seconds.

This is also the point to retire the macOS hand-union of `ListConflictResolutions` plus `ListCiRemediations` for Activity. The app consumes the unified attempt list for its rows and uses the existing kind-specific `Get…` verbs only when a selected detail pane needs fields absent from `EngineAttemptListEntry`. The badge is therefore a live view on the existing engine-attempt read path, not a third representation.

Half of that retirement did not happen, and this section should not read as if it had. Activity’s own refresh does use the unified verb, but the two legacy list requests were never removed: `ChatViewModel` still sends `list_conflict_resolutions` and `list_ci_remediations` on every conflict and CI lifecycle push, now _alongside_ `refreshEngineAttempts()`. The reason is a consumer this design never identified — the kanban card badges (`activeConflictResolution(for:)`, `activeCiRemediation(for:)`) read the source-specific arrays, not Activity’s list. So the plan asked for a removal the app could not perform, and the result is two live list representations of the same rows plus roughly doubled request volume on those events. Moving the card badges onto the unified entry, or onto a narrower per-work-item read, is follow-up work; until it lands, the “one list representation” property asserted here is not true.

The CLI’s existing `boss engine attempts list` consumer must tolerate the additive response and gains a `--background` read-only form. That is a diagnostic view of the same snapshot, not another query implementation.

### Polling and planner reconciliation

The macOS model requests `ListEngineAttempts(limit: 0, include_background_work: true)` immediately after connection and every five seconds while connected. It owns a cancellable polling task and published snapshot in the same architectural shape as `UpdateModel`, without copying the updater’s launch jitter, six-hour cadence, or `UserDefaults` persistence. It cancels the timer and clears the snapshot on disconnect, preventing stale chrome when no engine can attest that work is still running.

Activity’s manual refresh uses the same verb with its normal history limit and also accepts the background snapshot. Normal completion events and work invalidations may trigger an immediate refresh, but correctness does not depend on an event arriving.

The poller as built is more defensive than a plain timer, and each guard answers a specific way the two payloads on one response can race. Every request carries an envelope `request_id`; the snapshot and the Activity list are gated by _independent_ generations, so a slow `limit = 0` poll cannot overwrite a newer snapshot while a late `limit = 200` history refresh that lost the snapshot race can still update Activity with a payload that is perfectly valid for its own gate. Replies with no request id are ignored, event-triggered refreshes are skipped while a background-only request is already in flight so an invalidation burst cannot stack RPCs, and pending entries that can no longer win either gate — or whose request came back `work_error` — are pruned instead of being held until disconnect.

For planner items, `source_id` is the `planner_runs.id`. The global snapshot is authoritative only for the dimension it represents — current running state — so the two affordances share identity and running outcome without any claim that their full data models are equivalent. `PlannerRunAffordance` takes its hourglass and label from that live identity; the project-scoped query stays authoritative for staged, applied, and failed history and for full planner detail. When a live item is clicked before the per-project row is cached, the app refreshes that project and opens the existing planner popover once the matching row arrives, tracking the specific run it is waiting for so that an unrelated later arrival cannot pop the popover open unprompted.

Making one dimension authoritative has a consequence this design did not state, and the implementation had to decide it unaided: a cached `planner_runs` row whose outcome is `running` does _not_ light the hourglass on its own — only the snapshot does. That is what makes the two indicators structurally unable to disagree, and it is the honest reading of “authoritative for current running state”. It also means the project card shows no hourglass during a run’s first 15 seconds, because the engine’s anti-flicker gate is deliberately withholding the item. The affordance falls back to the previous terminal run’s history icon in that window so the popover’s Release/Undo entry point stays reachable; on a project’s first-ever run there is nothing to fall back to and the affordance is simply absent until the gate opens.

The 15-second constant was chosen as a _toolbar_ anti-flicker gate and was never evaluated as a project-card gate — the card is a contextual per-project view where a two-second run appearing and vanishing is informative rather than noisy. Whether the card should keep an immediate running source of its own is an open decision, not a settled one, and it is recorded as such below rather than presented as a design choice this document made.

### Minimal toolbar affordance

Place `BackgroundWorkToolbarButton` in the primary-action group adjacent to `UpdateBadgeToolbarButton`, between Notifications and Update. Do not merge their icons or popovers.

When the snapshot is empty, render no button. This is the correct feature-flag-disabled state and avoids an empty popover whose only content would explain that nothing is enabled. When it is non-empty, render a compact engine-work glyph (`gearshape.2`) with a capped numeric badge (`99+`) and an accessibility label such as “2 background operations running.” The spoken label reports the true count, not the capped text. The badge itself is the shared `ToolbarCountBadge` view that the notifications bell already uses, so the cap rule lives in one place instead of being reimplemented next to an existing copy of itself.

Clicking opens one read-only popover. Each row contains only:

- the engine-authored title;
- project or work-item context when present;
- phase, plus elapsed time only when the engine supplies an actual operation start.

There are no buttons, navigation, history, success rows, failure rows, charts, or controls. If the final item completes while the popover is open, dismiss the popover when the next snapshot makes the toolbar button disappear rather than showing an empty state. The dismiss reset must live outside the button’s own conditional presence: a reset attached to the button would never run on the one transition that matters, because that transition removes the button from the view tree.

`started_at` is a wire string and the engine writes epoch seconds. The renderer parses an all-digit value as epoch seconds only inside a plausible range — not before 2001, not more than a day ahead — and otherwise falls through to ISO-8601, so a compact date or a millisecond timestamp that happens to be all digits is omitted rather than rendered as a nonsense duration. Conflict items still carry no start and still show no elapsed time.

### Validation contract, and what it actually caught

The implementation is complete only when the genuine boundaries are exercised. Each item below states the boundary and whether the shipped work reached it.

- **Startup planner recovery.** A test seeds a real `running` planner row, starts the engine’s actual `serve` path, and proves the row is terminal, its project re-claimable, and its follow-up attention visible by the time the socket is usable. Done in [mono#2745](https://github.com/spinyfin/mono/pull/2745).

- **Engine RPC eligibility matrix.** Planner rows on both sides of 15 seconds; conflict rows with no marker, a marker with no live lease, a mismatched lease/workspace pair, a worker-bound attempt whose marker already cleared, rung-0 versus named rung-1 phase text, a missing task row, and the ordering invariant. A socket integration test drives a real engine and proves opt-out stays empty, opt-in returns both sources with engine-authored fields, and `limit = 0` empties `attempts` while leaving `background_work` populated. Done in [mono#2776](https://github.com/spinyfin/mono/pull/2776).

- **Conflict orphan across a real restart.** The boundary is that a durable mechanical marker inherited from a dead process cannot enter the visible response, because the process-local lease registry starts empty and startup reconciliation clears the row. **Not reached.** The veto is covered only by unit tests with an injected lease set, and an injected set is built from the same beliefs as the code it checks — it can confirm the comparison is written correctly, but it cannot find an ordering bug between startup reconciliation, lease reap, and the first served request. The planner half of this property has a genuine startup test; the conflict half has none.

- **CLI and Swift decoding.** Human table and detail output render the mechanical rung while JSON stays compatible; Swift decodes the marker and the unified response, replaces snapshots atomically, and clears on disconnect. Done in [mono#2789](https://github.com/spinyfin/mono/pull/2789), [mono#2792](https://github.com/spinyfin/mono/pull/2792), and [mono#2799](https://github.com/spinyfin/mono/pull/2799) — with one exception. The planned assertion that Activity no longer sends the two legacy list requests was not written, because it is not true; the requests survive for the card badges.

- **Toolbar states.** Zero, one, multiple, and completion-while-open, driven through the real `applyBackgroundWorkSnapshot` path rather than a fixture. Done in [mono#2829](https://github.com/spinyfin/mono/pull/2829). The required capture of the real isolated Boss UI is **not done**: the `--capture-to` instance started and reached the capture path, but `NSApp.windows` stayed empty. Offscreen `NSHostingView` renders of the production `BackgroundWorkPopover` stand in — better than a hand-built preview, since they host the real view — but they exercise the view in isolation, not the assembled toolbar, so they cannot catch a placement or toolbar-composition problem. That was the whole point of asking for the capture.

## Risks / open questions

- **The project-card hourglass no longer lights during a planner run’s first 15 seconds.** An engine constant chosen as a toolbar anti-flicker gate now governs a per-project indicator it was never evaluated for. Whether the card should keep an immediate running source of its own, or whether the gate is right for both surfaces, is undecided — this is an unmade decision that shipped as behaviour, not a choice this document made.

- **Two live list representations remain in the app.** Activity uses the unified verb, but the kanban card badges still drive `list_conflict_resolutions` and `list_ci_remediations`, and both fire on the same lifecycle pushes. Until the card badges move, the single-representation property this design asserts is false and request volume on those events is roughly doubled.

- **Startup recovery is not unconditional.** A database error during the planner sweep is logged while startup continues, so a stale `running` row can reach the badge on that path alone. Everything else about the prerequisite holds.

- **A snapshot build failure is indistinguishable from an idle engine.** The handler degrades to an empty list so a transient error cannot blank Activity. If that error is persistent rather than transient, the badge silently and permanently reports nothing running, and only the engine log says otherwise.

- **The planner’s 15-second threshold is still calibrated from bounds and an estimate, not production measurements.** It remains one engine constant (`PLANNER_BACKGROUND_MIN_AGE_SECS`), easy to change. If planner duration data shows many valid runs finishing below it, lower it and update the contract tests in the same change rather than leaving a test pinning the superseded number.

- **The mechanical marker is primary; the lease snapshot is only a stale-clear guard.** The marker owns the “executing rung” meaning and the phase text; the process-wide snapshot can only confirm that the stamped lease is still held here. Neither supplies rung age, so the popover omits conflict elapsed time rather than relabelling attempt age. Queue depth remains unknown because `SlotState::InFlight` conflates permit wait and execution.

- **Marker writes are best-effort.** A failed stamp omits a real rung, which is safer than inventing liveness; a failed clear is removed by the live-lease veto after unregister. Startup reconciliation remains the durable backstop after a process death — see the untested boundary above.

- **Polling is intentionally simple but not free.** The query is local and tiny, and five seconds is slower than the eligibility threshold. If the source count grows substantially, the replacement should be snapshot-on-connect plus invalidation events, not faster polling or app-side caches.

- **A hung design-doc fetch remains possible because `gh_output` has no timeout.** Still a real reliability gap, still not the finite operation this badge represents. Fix it at the command-timeout boundary in separate reliability work rather than broadening this feature.

## Implementation breakdown, as built

Nine entries were planned: seven in scope, two deferred. All seven shipped, in the planned order and with the planned dependencies, so the ordering argument held — no count reached chrome before the planner reaper was deployed. The two deferred entries remain deferred and unscheduled. Each entry below records what landed and where it diverged; the durable reasoning behind each lives in the sections above.

### Reap stranded planner runs during engine startup

Shipped in [mono#2745](https://github.com/spinyfin/mono/pull/2745). A transactional `WorkDb::recover_running_planner_runs_on_engine_restart` marks every inherited `planner_runs.outcome = 'running'` row `planner_failed` with an engine-restart summary and returns the recovered rows.

Diverged in three ways, each argued above: the sweep runs after the live-frontend refusal check rather than first; a database error logs and lets startup continue; and each recovered run raises a `followup` attention, reusing the populator’s design-task group key when the run has a `design_task_id` so the restart outcome lands with that design’s other outcomes.

### Extend the unified engine-attempt RPC with live background work

Shipped in [mono#2776](https://github.com/spinyfin/mono/pull/2776) as `engine/core/src/background_work.rs`, the `BackgroundWorkItem` / `BackgroundWorkKind` protocol types, and the additive `include_background_work` request flag with the `background_work` response field.

Contract changes against the plan: `visible_count` was dropped in favour of the array length; a failed snapshot degrades to an empty list rather than failing the response; conflict phase text resolves the work item’s name from `tasks.name` with an id fallback instead of rendering the raw id; and `snapshot_with_leases()` sits beside `snapshot()` so unit tests inject the lease set rather than mutating the process-wide registry. `ConflictRemediationQueue.slots` was left untouched, as required. The stale duration prose in `ladder_lease_heartbeat.rs` was corrected to the measured range without changing the TTL or the heartbeat interval.

### Expose the unified snapshot through the existing engine-attempt CLI

Shipped in [mono#2789](https://github.com/spinyfin/mono/pull/2789). The conflict table gained a `MECH RUNG` column, the detail view prints the field, and `boss engine attempts list --background` renders the engine’s snapshot as `Background work (N)` plus engine-authored rows. The CLI queries no source table.

Two additions beyond the plan, both about not lying to a caller: `--json` emits the `background_work` key only when `--background` is passed, so an empty array can never be mistaken for a live empty snapshot by a caller that never opted in; and the human table prints `source_id` rather than the namespaced `<kind>:<source_id>` id, so the cell can be pasted straight into `boss engine conflicts show`.

### Adopt the unified engine-attempt list in macOS

Shipped in [mono#2792](https://github.com/spinyfin/mono/pull/2792). Activity’s Engine rows come from `ListEngineAttempts`, detail loads on selection and reports a terminal fetch failure instead of leaving a spinner, and `WorkConflictResolution` decodes `mechanicalRungInFlight`.

Two divergences. The Swift `BackgroundWorkKind` is an open enum with an `unknown(String)` case rather than the closed pair the protocol defines, so an item from a source a newer engine adds still counts toward the badge. That is the right call and follows directly from this design’s rule that inclusion is never a client decision: silently dropping an unrecognised kind would make the app lower a count the engine authored. Separately, the planned removal of the legacy per-table list requests did not happen — see “Engine-owned read model”.

### Poll and own the global background snapshot in the macOS model

Shipped in [mono#2799](https://github.com/spinyfin/mono/pull/2799): connection-scoped five-second poll of the `limit = 0` form, atomic snapshot replacement, cancel-and-clear on disconnect.

Beyond the plan, the model gained request-id correlation, independent snapshot and Activity generations, refresh coalescing, and pending-request pruning. These are not gold-plating; each closes a race the design’s one-sentence description hid. Without independent generations, a history refresh that lost the snapshot race would discard a still-valid Activity payload, and a slow poll could overwrite a newer snapshot.

### Reconcile the existing planner indicator with global live state

Shipped in [mono#2827](https://github.com/spinyfin/mono/pull/2827) via `PlannerRunAffordancePresentation.liveRunningRunID` and `PlannerPopoverWaitState`. Completion arriving through either path turns the hourglass off, and the invariant that two planner indicators cannot claim conflicting running identities is covered by test.

The unplanned consequence — no hourglass during a run’s first 15 seconds, with a history-icon fallback so the popover stays reachable — is described under “Polling and planner reconciliation” and is carried as an open decision, not a settled one.

### Add the adjacent toolbar badge and minimal reveal popover

Shipped in [mono#2829](https://github.com/spinyfin/mono/pull/2829): the conditional primary-action button between Notifications and Update, the capped badge, accessibility text, and the read-only popover, with no actions, history, empty state, or dashboard elements.

The design required isolated-app capture evidence for this entry and did not get it; the substitute and its limits are recorded in the validation section.

### Surface execution envelope ratio in Agents

Still deferred and unscheduled. The reasoning is unchanged: every ratio belongs to a worker execution already visible in Agents, so it must extend the Agents live-state projection rather than the toolbar snapshot, and `envelope_overrun` attention rows must not be reused as transport for engine state.

### Design a separate global merge-queue summary if operators request it

Still deferred and unscheduled. Merge-queue position is already on the task card and is not running engine work. Reopen only on a new operator request, and only as its own design with its own subject.
