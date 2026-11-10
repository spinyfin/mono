# Dispatch wait reasons

Every work item that is waiting rather than running carries a named reason. The board card, `bossctl dispatch stuck` and `bossctl dispatch diagnose` all show the answer of one engine function, so they cannot disagree, and none of them can show a wait the engine has not named.

## Where the answer comes from

`engine/work-db/src/queued_wait.rs` classifies a wait. It is the only place that decides why an item is waiting:

- `for_item` is the item-level entry point every surface calls. It classifies a pending latest execution with `for_execution`, and otherwise the item itself with `for_unscheduled`.
- `for_execution` classifies a pending execution (`ready`, `waiting_dependency`, `waiting_review`, `waiting_merge`, or the legacy `queued`).
- `for_unscheduled` classifies an item with nothing pending: a card the board queues (`todo` with automatic start on), an `active` item with no run pending or live, or an `in_review` item.

The result travels on `TaskRuntime` as `dispatch_wait_reason` (a wait code, or the scheduler's own sentence for one hold) and `dispatch_wait_label` (the phrase to show). The vocabulary and its phrases live in `engine/storage-values/src/dispatch_wait.rs`.

| Surface                     | What it reads                                                                                                                                                    |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Board card                  | `dispatch_wait_label`, rendered as given. The app holds no code-to-text table.                                                                                   |
| `bossctl dispatch stuck`    | The same classifier (`for_item`), shown as `waiting=<reason>` on every row. The `S<n>` classification and bucket are the report's own taxonomy and are separate. |
| `bossctl dispatch diagnose` | The same `TaskRuntime`, printed as a `waiting:` line and as `wait` in `--json`.                                                                                  |

The app formats two things itself, because they need the local clock: a retry countdown (`dispatch_retry_at`) and a dispatch deadline (`dispatch_not_before`). Both values come from the engine.

## The reasons

A pending execution:

| Reason                              | Meaning                                                                                            |
| ----------------------------------- | -------------------------------------------------------------------------------------------------- |
| `awaiting_dispatch`                 | `ready`, and no scheduler pass has held it. The next pass claims it or records what stops it.      |
| `not_before`                        | `ready`, with a dispatch deadline still ahead (retry backoff, merge-order stagger).                |
| `dispatch_paused`                   | The global dispatch pause holds it.                                                                |
| `automation_paused`                 | The automation pause holds it.                                                                     |
| `focus_mode`                        | It is outside the dispatch focus set.                                                              |
| `pool_exhausted`                    | Its worker pool has no free slot.                                                                  |
| `dispatch_preflight`                | Startup preflight (boot recovery, the capability probe, or a failed runtime check) holds dispatch. |
| `progression_unavailable`           | The engine cannot start progression agents yet.                                                    |
| `awaiting_predecessor_preservation` | The run it recovers from has not released its workspace, so that run's work is not preserved yet.  |
| a sentence                          | The interactive cap, a workspace lease, or a chain hold naming the blocking task and PR.           |
| `waiting_dependency`                | An unmet `blocks` prerequisite gates it. The card names the prerequisite.                          |
| `project_order`                     | It is a later task of a project whose earlier task has not been started. The card names that task. |
| `autostart_off`                     | Its task has automatic start off, so the queued execution waits for an explicit start.             |

A card queued on the board with nothing pending. These are the reconciler's own refusals to mint an execution:

| Reason               | Meaning                                                                   |
| -------------------- | ------------------------------------------------------------------------- |
| `human_driven`       | No agent ever starts for it.                                              |
| `deferred`           | It is future scope until someone approves it.                             |
| `parked`             | It, its project or its parent is parked.                                  |
| `run_ended`          | Its last run ended. The engine never starts a replacement on its own.     |
| `awaiting_parent_pr` | It is a revision and its parent task has no pull request to push to yet.  |
| `pr_open`            | Its pull request is open, so review and merge handling own the next step. |
| `repo_unresolved`    | No repository resolves for it.                                            |

An `active` item with no run pending or live. The orphan sweep's own candidate predicate names the ordinary state, and the holds it leaves alone are named in turn:

| Reason                | Meaning                                                                       |
| --------------------- | ----------------------------------------------------------------------------- |
| `human_driven`        | No agent ever starts for it.                                                  |
| `parked`              | It, its project or its parent is parked.                                      |
| `dispatch_failed`     | A recorded dispatch failure, such as the churn guard, holds it.               |
| `recovery_attention`  | Worker recovery gave up and raised an attention item for a person.            |
| `awaiting_redispatch` | Its worker ended. The orphan sweep starts a replacement.                      |
| `awaiting_review`     | Its pull request is with review and merge handling, which owns the next step. |

Review and merge waits use `review_reservation` only when the shared admission query identifies the item, `review_batch` while a matching pre-merge batch is live, and `awaiting_merge` when merge handling owns the next step. A clean verdict at the observed head yields `review_complete`; an unfinished findings revision yields `revision_pending`. A cycle-limit attention or an outstanding human review decision yields `human_review`. Human-driven and parked holds take precedence. Review cards receive the same runtime projection as Doing cards. Other unexplained review states remain `unclassified`.

Before promising `awaiting_redispatch`, the classifier applies admission's shared pause exemptions and deliberate-park predicate. `dispatch_paused` names a pause that holds the item; `deliberate_park` means its worker stopped for a person. Critical main-pool work and reviews retain admission's operator-pause exemptions.

Board projection triggers invalidate review-batch and pause changes. Attention writes already invalidate the projection; parking writes update the task/project rows already journaled. Wait annotation is recomputed even on runtime-cache hits.

## When the engine cannot name a wait

`unclassified` is not a reason. It marks a wait that none of the conditions above explains, which is a defect in the engine's state model.

- The card reads "Unexpected state" with the engine's phrase. It never reads "unknown", and the app never substitutes a reason of its own.
- The scheduler heartbeat (`engine/dispatch/src/coordinator/unexplained_wait.rs`) checks every beat, over pending executions, queued cards and idle `active` items. Once a wait has stayed unexplained for 60 s it logs an error naming the item, the execution and their states, and files a `dispatch_wait_unclassified` attention item. Nothing is promoted or repaired: a repair would hide the defect.
- A `ready` row that passes neither claim nor hold for 60 s is reclassified from `awaiting_dispatch` to `unclassified`, so its card stops saying a dispatch is imminent.
- The attention item resolves by itself once the item is running, gone, or waiting for a reason the engine can name.

A hold recorded under a code that has no phrase is also reported as `unclassified`, so a new hold cannot reach a card as a bare identifier.

## Adding a hold

1. Add the code and its phrase to `DISPATCH_WAIT_REASON_CODES` in `dispatch_wait.rs`.
2. Produce it in the scheduler (`record_dispatch_wait_reason`) or in `queued_wait.rs`.
3. Add the state to `every_waiting_state_yields_a_named_reason` in `engine/work-db/src/tests/queued_wait_tests.rs`. That test fails while a code in the vocabulary has no state that produces it.

No client change is needed: the phrase reaches every surface through `dispatch_wait_label`.

## A move to Doing stays queued

Moving a card to Doing mints a `ready` execution and reports `did_dispatch: true`. The same request then reconciles the product. The reconciler holds every project task but the first unfinished one in `waiting_dependency` (the project's ordinal chain), and applying that to the row the move had just minted would leave it where no scheduler pass looks, with no worker and no reason.

The chain decides which task the engine starts next, so it no longer applies to a task that is already `active`. A declared `blocks` prerequisite still gates an active task.
