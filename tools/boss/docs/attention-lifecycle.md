# Failure-signal lifecycle: what raises a signal, and what lowers it

Boss shows an operator two kinds of failure signal on a work item:

- **attention items** (`work_attention_items` rows with `status = 'open'`), and
- the kanban card's red **"Failed to start — …" banner**, which is rendered by `WorkDispatchFailureBanner` (`tools/boss/app-macos/Sources/WorkBoardBanners.swift`) from `tasks.dispatch_failed_reason` / `dispatch_failed_error`, read at `tools/boss/app-macos/Sources/WorkBoardCard.swift`.

Both are engine-owned state. The app renders them and does not interpret them: it has no notion of whether a signal is still live, and should not grow one.

## The rule

**A signal comes down when, and only when, there is positive evidence that the condition it describes is over.** Not on a timer, not on a lane change, not because an operator got tired of looking at it.

Every attention kind declares which shape of evidence applies to it, in `tools/boss/engine/core/src/attention_lifecycle.rs`. The four shapes are:

| `ClearedBy`                    | Evidence                                                                                                               | Applied by             |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `WorkResumed`                  | A run for the same work item started at or after the signal was raised                                                 | the generic reconciler |
| `ExecutionKindCompleted(kind)` | An execution of that kind for the same work item reached `completed` at or after the signal was raised                 | the generic reconciler |
| `ProducerReconciles`           | The kind's own producer re-reads its condition every pass and resolves the item itself                                 | that producer          |
| `HumanDecision`                | None is sufficient — the signal records something a later success does not undo, or is a gate a human is meant to open | a human                |

`ProducerReconciles` and `HumanDecision` are declarations that no generic rule applies, **and why**. They are not blanks. Each entry in the table carries a `rationale` string, and a unit test refuses an entry whose rationale is too short to be a real one — the point is that a future reader can tell a deliberate "human only" from a forgotten one.

## Where the boundary is

Every automatic rule requires the evidence to **postdate the signal**. A run that started before an attention was filed proves nothing about it.

That is what keeps a genuinely-broken item loud. If dispatch keeps failing, nothing ever starts, no evidence ever accrues, and the banner and the attentions stay exactly where they are — indefinitely, with no expiry. The goal is that a card reflects the item's _current_ state, not that failures become invisible.

Nothing is deleted, either. Resolution stamps `status = 'resolved'` and `resolved_at`; the row stays queryable, and the underlying failure remains in the dispatch-event log and the execution's own history.

## Where it runs

- **Inline, on the producing path.** The fastest clear, and the one an operator actually sees: `finalize_pr_review_pass` resolves the dead-review attention as it completes, `dispatch_claimed_execution` resolves the stall attention on slot claim, `start_execution_run_on_host` clears the dispatch-failure columns in the same transaction that flips the execution to `running`.
- **`attention_reconcile_sweep`, every 5 minutes.** The backstop, driven by the same declarative table (`WorkDb::reconcile_stale_attention_signals`). It catches what the inline paths structurally cannot: a condition that ended while a previous engine process was running, and any future code path that starts work without knowing it should clear something.

Both apply the same rule, so the sweep is never doing anything the inline path would have disagreed with.

## Adding a new attention kind

Add an entry to `ATTENTION_LIFECYCLES` at the same time you add the kind. `ClearedBy::HumanDecision` is a perfectly good answer — an _undeclared_ kind is not. Filing an unregistered kind emits a `tracing::warn!` from `warn_if_lifecycle_undeclared`, and `every_attention_kind_constant_in_the_crate_is_registered` fails if a constant exists with no entry.

This is the actual defect being guarded against. Raising a signal always went through shared, well-tested plumbing; lowering one was left to each producer to remember. So kinds added later simply never got a resolution path, and could be raised but never lowered — which is how a work item ends up in the Merging lane with a bound PR, four completed revisions, and seven still-`open` attentions describing failures a later successful pass had already superseded.

## Debugging a signal that will not clear

1. Find the kind: `SELECT id, kind, status, created_at, resolved_at FROM work_attention_items WHERE work_item_id = '<id>' OR execution_id IN (SELECT id FROM work_executions WHERE work_item_id = '<id>')`.
2. Look the kind up in `ATTENTION_LIFECYCLES`. If it is `HumanDecision` or `ProducerReconciles`, the reconciler is deliberately not touching it — read the entry's `rationale` for why.
3. If it is automatic, check the evidence. For `WorkResumed`, the item needs a `work_runs` row whose `started_at` is at or after the attention's `created_at`. For `ExecutionKindCompleted`, an execution of that kind with `status = 'completed'` and a `finished_at` at or after it. No such row means the condition genuinely has not been superseded, and the signal is correct to still be showing.

All timestamp columns in this schema are epoch-seconds-as-text, so `CAST(… AS INTEGER)` comparisons are exact rather than lexicographic.
