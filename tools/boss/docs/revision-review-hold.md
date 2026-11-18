# Revision review hold

A revision that pushed and finished is not moved to Review straight away. It stays `active`, with no live run, until the automated review of what it pushed reaches a decision. This is the review hold. This page says what takes the hold, what releases it, and where to look when a revision stays `active` with nothing running.

## What the hold rests on

The hold is released by the review of one specific pull-request head. That head is recorded on the revision's latest finished `revision_implementation` run, in `work_executions.revision_stop_contributed_head`. A pre-merge review batch releases every held revision of its PR whose recorded head equals the batch's `target_sha`, at the moment the batch reaches a terminal status.

A held revision is therefore safe only while one of these is true:

- its recorded head has a pre-merge batch that is still running, or
- it carries an open `pr_review_admission_deferred` attention item, which makes the deferred-admission sweep retry admission for it on every pass.

Anything else is a revision nothing will release. The rules below exist so that no path leaves one.

## What takes the hold

Both completion paths record the head the run finished at before they take the hold.

| Path                                             | Head it records                                                                                                                                                                               |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Worker declares `delivered`                      | The direct read of the PR head, bounded by the declared-delivery network budget. When that read fails, the head GitHub reported in the answer that verified the delivery mergeable stands in. |
| Stop boundary, review batch admitted             | The `target_sha` of the batch admission returned.                                                                                                                                             |
| Stop boundary, no batch (deferred or unreadable) | The head the Stop boundary stamped, or the head recorded as `pr_head_after` when it stamped none.                                                                                             |

`work_executions.pr_head_after` is written by the same completion on both paths.

A declared delivery whose head cannot be read from either source does not complete. The run fails as a delivery the engine could not confirm: the task goes to `blocked` with `blocked_reason = worker_failed` and a `blocked_detail` that names the unreadable head, the status ledger records the basis `delivered_without_pr`, and a `run_done_audit_flagged` attention item is filed on the run. `bossctl dispatch stuck` lists the row as a worker failure with that basis. Start the revision again once the pull request can be read.

## What releases it

| Event                                                                                  | Result                                                                                               |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| The batch for the recorded head reaches a verdict                                      | Every revision held for that head moves to `in_review`.                                              |
| The batch for the recorded head fails, or is reaped as inert                           | The same. A failure is a decision; it carries no verdict.                                            |
| Admission for the revision finds the batch for the head already decided                | The revision moves to `in_review` at once. A Stop-boundary completion does not take the hold at all. |
| The review of the push is skipped (unchanged head, empty or trivial diff, pure rebase) | The revision moves to `in_review` without a verdict.                                                 |
| The review-cycle limit is reached                                                      | The revision moves to `in_review` and a `pr_review_cycle_bound` attention item is filed.             |

The third row is why the order of events does not matter. A batch for the pushed head can be admitted for another row (an explicit `bossctl review start`, or another revision's push) and decided before this revision's run ends. Its decision then finds no revision to release. `WorkDb::settle_revision_review_hold` records the head and checks the batch's status in one transaction, so a revision that comes to be held after the decision is released by the batch's state and not by the decision.

Every release in this table cascades to the revisions queued behind the released one in the same transaction, so the next revision is dispatchable immediately.

## When a revision stays `active` with nothing running

Find what its hold rests on:

```sql
SELECT t.id, t.status, e.id, e.status, e.pr_head_before, e.pr_head_after,
       e.revision_stop_contributed_head, e.finished_at
FROM tasks t
JOIN work_executions e ON e.work_item_id = t.id AND e.kind = 'revision_implementation'
WHERE t.id = '<revision id>'
ORDER BY e.created_at DESC;
```

Then the batches of the PR that owns it:

```sql
SELECT id, target_sha, generation, status, producing_work_item_id, created_at, completed_at
FROM pr_review_batches
WHERE cycle_root_id = '<id of the PR-owning task>' AND phase = 'pre_merge'
ORDER BY created_at;
```

Compare `revision_stop_contributed_head` with the batches' `target_sha`:

- A running batch at that head: the hold is correct. It ends with the batch.
- No batch at that head, and an open `pr_review_admission_deferred` item on the revision: admission is waiting on the review pool, dispatch focus or a pause. The item's title says which.
- A decided batch at that head, or no recorded head at all: this is the state the rules above rule out. The deferred-admission sweep settles it on its next pass once the row has been idle for ten minutes.

## What is logged

Each step that decides a hold logs the state it decided on, under these messages in the engine log:

| Message                                                                                                 | Logged by                              | Carries                                                                       |
| ------------------------------------------------------------------------------------------------------- | -------------------------------------- | ----------------------------------------------------------------------------- |
| ``run_done finalize: declared `delivered`, bound to a resolvable PR``                                   | declared delivery                      | completion target, `pr_head_before`, `pr_head_after`                          |
| `pr completion: execution terminalized; teardown in flight`                                             | Stop-boundary completion               | completion target, `pr_head_after`, the head the hold was pointed at          |
| `run_done post-effects: no review batch admitted`                                                       | declared delivery, after the run ended | whether admission was deferred or found the head already reviewed             |
| `revision review hold: held for this head until its review batch reaches a decision`                    | any path that settles a hold           | revision, head, batch                                                         |
| `revision review hold: the review of this head had already reached its decision; released the revision` | any path that settles a hold           | revision, head, batch and its terminal status                                 |
| `review decision left a revision of this pull request held`                                             | a batch's verdict, failure or reap     | the decided head beside the head each still-held revision awaits, and its run |
| `merge poller: a decided batch covers this head but the hold did not release` (debug)                   | deferred-admission sweep               | candidate, batch and its status                                               |

The sixth line is the one that separates an ordinary hold (the revision awaits a later head, whose batch is still to come) from a stranded one (it awaits no head, or a head no batch will review).
