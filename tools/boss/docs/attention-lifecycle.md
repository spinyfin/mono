# Failure-signal lifecycle: what raises a signal, and what lowers it

Boss shows an operator two kinds of failure signal on a work item:

- **attention items** (`work_attention_items` rows with `status = 'open'`), and
- the kanban card's red **"Failed to start — …" banner**, which is rendered by `WorkDispatchFailureBanner` (`tools/boss/app-macos/Sources/WorkBoardBanners.swift`) from `tasks.dispatch_failed_reason` / `dispatch_failed_error`, read at `tools/boss/app-macos/Sources/WorkBoardCard.swift`.

Both are engine-owned state. The app renders them and does not interpret them: it has no notion of whether a signal is still live, and should not grow one.

## The rule

**A signal comes down when, and only when, there is positive evidence that the condition it describes is over.** Not on a timer, not on a lane change, not because an operator got tired of looking at it.

Every attention kind declares which shape of evidence applies to it, in `tools/boss/engine/core/src/attention_lifecycle.rs`. The five shapes are:

| `ClearedBy`                    | Evidence                                                                                                                                                    | Applied by             |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `WorkResumed`                  | A run for the same work item started at or after the signal was raised; for `pane_spawn_failed`, that later spawn run must also have status `completed`     | the generic reconciler |
| `ExecutionKindCompleted(kind)` | An execution of that kind for the same work item (other than the one the attention was filed against) reached `completed` at or after the signal was raised | the generic reconciler |
| `OwningExecutionTerminal`      | The execution the item is scoped to has reached a terminal status (cancelled, failed, abandoned, completed, or orphaned), even with no later run            | the generic reconciler |
| `ProducerReconciles`           | The kind's own producer re-reads its condition every pass and resolves the item itself                                                                      | that producer          |
| `HumanDecision`                | None is sufficient — the signal records something a later success does not undo, or is a gate a human is meant to open                                      | a human                |

`ProducerReconciles` and `HumanDecision` are declarations that no generic rule applies, **and why**. They are not blanks. Each entry in the table carries a `rationale` string, and a unit test refuses an entry whose rationale is too short to be a real one — the point is that a future reader can tell a deliberate "human only" from a forgotten one.

## Where the boundary is

Every automatic rule requires the evidence to **postdate the signal**. A run that started before an attention was filed proves nothing about it.

That is what keeps a genuinely-broken item loud. If dispatch keeps failing, nothing ever starts, no evidence ever accrues, and the banner and the attentions stay exactly where they are — indefinitely, with no expiry. The goal is that a card reflects the item's _current_ state, not that failures become invisible.

Nothing is deleted, either. Resolution stamps `status = 'resolved'` and `resolved_at`; the row stays queryable, and the underlying failure remains in the dispatch-event log and the execution's own history.

A signal can be re-raised onto an already-open row instead of getting a fresh one — the dedup contract every filer implements is one open row per (scope, kind). When that happens, `last_raised_at` is stamped forward to the re-raise time while `created_at` keeps recording when the row was first opened, so the boundary check above compares evidence against `COALESCE(last_raised_at, created_at)`, not bare `created_at`. Otherwise evidence from between the original raise and the re-raise would look postdating when it is really stale. For `ExecutionKindCompleted`, the completing execution must also not be the one the attention was filed against — an execution cannot supply the evidence that clears a signal about itself.

## Where a spawn failure surfaces

Execution-scoped `pane_spawn_failed` items appear on the owning work item through `list_attention_items_for_work_item`; review-guide executions resolve to their root task. Other execution-only signals retain their existing scope. Dispatch events also carry `details.spawn_failure` with the rejected step and cause, and the failed run retains the full error chain.

## Where it runs

- **Inline, on the producing path.** The fastest clear, and the one an operator actually sees: `finalize_pr_review_pass` resolves the dead-review attention as it completes, `dispatch_claimed_execution` resolves the stall attention on slot claim, `start_execution_run_on_host` clears the dispatch-failure columns in the same transaction that flips the execution to `running`.
- **`attention_reconcile_sweep`, every 5 minutes.** The backstop, driven by the same declarative table (`WorkDb::reconcile_stale_attention_signals`). It catches what the inline paths structurally cannot: a condition that ended while a previous engine process was running, and any future code path that starts work without knowing it should clear something.

Both apply the same rule, so the sweep is never doing anything the inline path would have disagreed with.

| Producer-owned kind | Clearing evidence                                                                                                                                                       | Sweep                                         |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| `hung_worker`       | Progress resumed or no hung worker remains for the owner; persisted warnings are reconciled after restart while in-flight executions awaiting adoption remain protected | Live-worker activity sweep, every ten seconds |

## Run-budget alarm

`run_budget_overrun` (`engine/core/src/run_budget_watch.rs`) is `HumanDecision`, mirroring `envelope_overrun`. The 60-second budget watch reads the tailed cost snapshot on `work_runs` for live runs (gated by `run_budget_alarm`, default on) and files at most one `run_budget_overrun` attention item per execution when total tokens pass 25,000,000 or rounds pass 150. It is an informational signal only and never stops the worker; a later cheap run does not un-spend the overrun, so only human resolution lowers it.

## The fleet-level spawn-failure streak alert is not an attention item

Attention items are scoped to one execution or one work item. A failure pattern that only exists _across_ executions has no row to live on, so it is raised as an engine-health issue instead.

The first such signal is `pre_start_spawn_failure_streak` (`engine/core/src/pre_start_streak.rs`). The engine counts pre-start spawn outcomes per (driver, worker kind). Two consecutive failures with no success in between raise one alert for that combination; each further failure updates the same alert with the live count and the latest full error; the combination's next successful spawn clears it. `SlotBusy` rejections are not counted. It never pauses dispatch and changes nothing about how each failure is handled.

It appears in three places, all rendered from the same engine health report:

- the app's chrome banner (first issue, red), with the latest error in the expanded body;
- `bossctl state` (a `spawn alerts:` section) and `bossctl state --json` (`spawn_failure_streaks`);
- the `engine.health` topic, pushed on raise, on every update and on resolve.

Each failed spawn gets an execution-scoped `pane_spawn_failed` item. The work-item attention listing includes it, and resolves a review-guide execution through its source comparison to the series' root task. The product work-tree attention list includes only directly associated items, so execution-scoped spawn failures do not appear as external-tracker sync issues.

State is in memory and starts empty on engine restart; a combination that is still broken re-raises after two more failures.

### Progression launch backoff

The second is `progression_launch_backoff` (`engine/core/src/progression_sweep.rs`), a warning. A progression run whose agent never started is not an attempt on its item: it starts the item's cooldown but never counts toward the two failures after which the engine asks the owner about the item. The item is retried each time its cooldown ends. While any stuck item is waiting out such a cooldown, the report says how many are waiting, when the first is retried, and the latest error.

| Raised by                                                                                                                                        | Lowered by                                                                                                        |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| A progression sweep pass that finds a stuck item whose latest progression run failed to launch and whose cooldown from that run is still running | The first pass after no such item is left: an agent started for it, its cooldown ended, or it stopped being stuck |

It is read from the database on every pass (`WorkDb::progression_launch_backoff`), so it survives an engine restart, and it is pushed on the `engine.health` topic whenever it changes. It is reported only while the `progression_agents` flag is on. It never pauses dispatch.

## Workspace-lease attention

`workspace_lease_release_failed` (every gate passed but cube's release call failed) is `ProducerReconciles`. `apply_release` (`engine/completion/src/workspace_lease_release.rs`) resolves it for the execution as soon as the lease is released or found already gone, so a later sweep that returns the workspace lowers it. It is also the durable proof the leftover-lease sweep uses to retry a row that never recorded a worker pid.

`workspace_lease_unreleased` is `ProducerReconciles`. It records a finished run's held lease, including unconfirmed worker death: process cleanup failed or left occupants, or a remote worker could not be confirmed gone. The separate `workspace_lease_occupancy_kept` item confirms that a release passed the pane gate and occupancy kept the lease; open legacy cleanup items with the three recovery cleanup titles also prove that the pane gate passed, while unconfirmed-death items never do. The first time those legacy titles are recognised they are converted into an occupancy-kept marker so a later title rewrite cannot erase retry authorization. Host-resolution and cube-registry faults file `workspace_lease_recovery_unavailable` instead of rewriting the unreleased item. A live registry entry prevents retry, and a recorded PID without confirmed death prevents cleanup of a lease cube still holds. If cube already reports the lease absent, the retry first preserves on the owning host, using the lease boundary for a reassigned workspace and live preservation only for a free workspace with an unchanged claim counter. Live preservation runs only after occupancy agrees the workspace is empty. Failing to run preservation (jj, unreachable host, missing recovery record or path) keeps the recovery record and successor held; an inspection after a completed preservation still reconciles, matching the held-lease path. Reconciliation never resets or releases another holder's workspace. Each local or remote occupancy re-check refreshes the item's body with the occupants seen on that scan (`created_at` is unchanged). `apply_release` lowers it when the lease is returned. A recovery successor waits while its predecessor holds the lease and is failed, with the latest item's text in its error, only once the problem has outlasted `PREDECESSOR_HOLD_BOUND_SECS` (30 minutes). That clock runs from the later of the item's first filing and the moment the successor first began waiting on it, so a successor created after the item was already old still gives the sweeps the full bound before it can fail, and a successor that waited and then failed keeps no wait reason. One fresh open item never fails a successor on its own: the sweeps retry, and a transient fault clears itself. The same bound applies to an open `workspace_lease_release_failed` item (occupancy already passed; cube's release call keeps failing) and to an open `workspace_lease_recovery_unavailable` item (the owning host or cube registry could not be reached). While a release-failed item is still inside the bound it also blocks the remote missing-evidence fallback, so a cube retry is not treated as silence. The bound also covers predecessors that left no item at all, measured from the later of when the predecessor ended and the successor's first wait.

`workspace_unowned_occupants_released` is `HumanDecision`. Cleanup signals only processes carrying the run's exact identity. When the only processes left in a finished run's workspace are ones no live run owns (for example a process that started before it could register), the lease is held for `UNOWNED_OCCUPANT_BOUND_SECS` (10 minutes: cleanup of owned processes converges inside one pass and sweeps retry every minute) after the run ended, then returned with this item naming the PIDs. The processes are never signalled. An old owner's orphans are cleaned only once its recorded shell is proven gone: the PID is absent, or now belongs to a process without that run's identity (a reused PID).

## Preserved-work attention

`preserved_work_not_restaged` is `ProducerReconciles`. Dispatch files it when a recorded preserved tip cannot be restaged into a new lease. A later successful restage of the selected tip resolves it; cleanup also resolves it after a restaging successor publishes a changed PR head. Starting fresh is not evidence the work was recovered. Open not-restaged or abandoned-bookmark items prevent age-based tip cleanup.

`preserved_work_archived` is `ProducerReconciles`. Dispatch files it when restore restages the selected tip and archives unpublished unselected variants under `preserved-archive/<execution>/variants/<commit>`. A later restage of that selected tip does not recover the archived work. Closed-chain cleanup resolves the item when it forgets the archives after the retention period; open inspection requests keep the archives, which remain eligible after the request is resolved even if the tip is already gone.

`preserved_work_needs_inspection` is `HumanDecision`. Teardown files it when it cannot safely preserve what the workspace holds. The triggers are: the workspace tip and the existing preserved bookmark cannot be combined; no lease boundary was confirmed for an inferred release (including a first teardown with no earlier record), so no historical tip can be chosen; no tip was journaled at the lease boundary; a lease-boundary journal walk exhausted its operation-view budget, so the historical tip is unresolved; unpublished work registered in a free workspace is absent from the boundary tip, because its owner cannot be established; the boundary change was rewritten afterwards and the rewrite is not adopted; snapshotting a free workspace hid the boundary commit, in which case neither it nor its visible successor is eligible for automatic restore and both commits are named; or the boundary commit of a reassigned workspace is still that workspace's working-copy commit (or a descendant), where a bookmark would follow the later holder's pending edits, so it is named and excluded from automatic restore. Rejected changes are retained under inspection-only `preserved-archive/<exec>/unsafe/<change>/<commit>` references; automatic restore rejects their change identities through both existing bookmarks and recorded-tip fallback, including later rewrites. Empty or fully published boundary work does not trigger inspection. For an ordinary held-lease teardown, the lease is still released and nothing is deleted. Absent-lease reconciliation keeps the recovery record when preservation cannot run at all, and still reconciles after a completed preservation that filed inspection. The same item is filed when teardown cannot run the preservation at all (no recovery record for a started execution, no workspace path, an owning host or cube registry that cannot be reached, a jj failure); its reason then carries the error and the lease id, and an ordinary held-lease teardown releases the lease anyway. The item can name both tips, a single tip, or none, and its reason carries a suffix with the registry's view of the workspace (state, lease ids, lease generation). A workspace that cube shows as leased under another lease, or as free with a changed claim counter, is never snapshotted and unrelated work is never reported; a rewrite of this execution’s own boundary change is still reported for inspection. A later restage of the old tip does not recover the new workspace tip named in the item.

## Adding a new attention kind

Add an entry to `ATTENTION_LIFECYCLES` at the same time you add the kind. `ClearedBy::HumanDecision` is a perfectly good answer — an _undeclared_ kind is not. Filing an unregistered kind emits a `tracing::warn!` from `warn_if_lifecycle_undeclared`, and `every_attention_kind_constant_in_the_crate_is_registered` fails if a constant exists with no entry.

This is the actual defect being guarded against. Raising a signal always went through shared, well-tested plumbing; lowering one was left to each producer to remember. So kinds added later simply never got a resolution path, and could be raised but never lowered — which is how a work item ends up in the Merging lane with a bound PR, four completed revisions, and seven still-`open` attentions describing failures a later successful pass had already superseded.

## Debugging a signal that will not clear

1. Find the kind: `SELECT id, kind, status, created_at, last_raised_at, resolved_at FROM work_attention_items WHERE work_item_id = '<id>' OR execution_id IN (SELECT id FROM work_executions WHERE work_item_id = '<id>')`.
2. Look the kind up in `ATTENTION_LIFECYCLES`. If it is `HumanDecision` or `ProducerReconciles`, the reconciler is deliberately not touching it — read the entry's `rationale` for why.
3. If it is automatic, check the evidence against `COALESCE(last_raised_at, created_at)`, not bare `created_at` — a re-raised row moves the reference point forward to the last re-raise, and comparing against the original `created_at` will find stale evidence that looks postdating but is not. For `WorkResumed`, the item needs a `work_runs` row whose `started_at` is at or after that value. For `pane_spawn_failed`, the later spawn run must also have status `completed`; a mere run start does not clear it. For `ExecutionKindCompleted`, an execution of that kind with `status = 'completed'` and a `finished_at` at or after it, whose `id` is not the attention's own `execution_id` — an execution never clears an attention filed against itself. No such row means the condition genuinely has not been superseded, and the signal is correct to still be showing.

All timestamp columns in this schema are epoch-seconds-as-text, so `CAST(… AS INTEGER)` comparisons are exact rather than lexicographic.
