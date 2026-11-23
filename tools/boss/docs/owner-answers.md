# Owner answers: the owner decides, the engine acts

A question on the board is a decision. It is never a to-do for the person answering it. When an answer means the board should change, the engine makes that change in the same step that records the answer, or it refuses the answer and says why.

This page covers the two mechanisms that hold that rule for questions asked by a progression run, and the reconciliation that removes the most common reason one was asked.

## Binding a change to an answer

A progression run that needs the owner ends with `boss propose done --outcome blocked --question …`. It can bind a board change to any answer:

```sh
boss propose done --outcome blocked --summary "A cancelled build-fix blocks the conflict work" \
  --question "Skip that build-fix so the conflicts can be resolved?" --answer-type yes-no --criterion 2 \
  --explanation 'Context:' '…' 'Yes:' '…' 'No:' '…' 'Recommendation:' '…' \
  --on-answer 'yes=archive-revision <id>' --on-answer 'no=start <id>'
```

`<answer>` is `yes` or `no`, or the option label of a multiple-choice question, verbatim. The changes are:

| Change                                              | What the engine does when the answer is picked                                                                                                                |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `archive-revision <id>`                             | Closes a revision of the item's own PR chain, withdraws its not-yet-started runs, releases whatever waited on it, and retires the CI attempt that created it. |
| `start <id>`                                        | Starts the row, as an explicit start by the owner.                                                                                                            |
| `undepend <dependent-id> <prerequisite-id>`         | Removes that dependency and unblocks the dependent if nothing else holds it.                                                                                  |
| Option label `Park this item` / `Archive this item` | Parks or archives the item the question is about. No binding needed.                                                                                          |

The change is made with the owner's authority. The agent's own `archive-revision` needs proof that a revision is dead (its parent PR green, or the revision already delivered). The owner's answer is the decision that it is not wanted, so no proof is asked for.

### Checked when the question is asked

The declaration is refused, and the run can ask again properly, when:

- a bound change names a row outside the run's neighbourhood (the item, its revisions, its direct prerequisites and dependents);
- a bound change cannot be made, or would change nothing (the revision is already closed, the dependency does not exist);
- the question cites criterion 1 or 2, which means it asks for an action, and none of its answers is one the engine carries out. No answer to such a question could change anything, and the next run would be refused exactly as the first was;
- the wording hands the change to the owner (see below);
- the run is not a progression run. Only a progression run has a neighbourhood to bound a change by.

### Applied when the owner answers

The change is made in the transaction that records the answer, and only when the owner answers. A coordinator or progression answerer records no bound change: the answer is refused with a typed failure, nothing is written, and the question stays open. Three things can happen:

- **It is made.** The item's description gains a line saying what the engine did, so the next progression run neither repeats it nor asks for it again. The engine log has `operator answer: made the board change bound to the chosen answer`.
- **Nothing was left to change** (the revision was closed in the meantime). The answer is recorded and the description says so.
- **It cannot be made** (a worker got onto the revision after the question was asked; the engine's start path refuses). The answer is refused with the reason, nothing is recorded, and the question stays open for the other answer or a dismissal. The card shows the reason.

An answer that binds no change behaves as before: it is recorded on the item's description and the next progression run acts on it with its own verbs.

## What a question may not say

A progression question is refused when its question text, an option label, or the part of the explanation that says what an answer does is worded so that the owner would make the change. For a yes/no question that is everything but `Context:` and `Details:`. For a multiple-choice question `Context:` is kept, because that is where each option's outcome is described:

- "by hand", "manually", "yourself";
- "you" or "please" governing a board verb: "you archive", "please delete", "you will need to start";
- a sentence that opens with a board verb and says where to do it: "Delete the cancelled row on the board";
- a command to run: `bossctl …`, `boss task …`.

An option label in the first person ("I will delete the cancelled row now") is refused for every kind of run, at payload validation: a label names what happens when it is picked.

A board verb on its own is fine. "Archive the cancelled build-fix" is what the engine does when `archive-revision` is bound to that answer.

The same two functions (`owner_handoff_wording`, `owner_voiced_label` in `boss_protocol`) are run over every option set `engine_option_sets()` returns. Producers read their labels from that list, so a new engine-defined option is checked by the same test.

### What is not a bound change

`--on-answer` is the three board mutations above, plus the item-local Park / Archive labels and the built-in create-prerequisite and link-prerequisite questions. Changing priority, merging a pull request, stopping a live worker, and answering someone else's question stay human-only. Binding any of those would be a separate capability (the critical-priority gate, GitHub merge intents, live worker teardown). A progression run that needs one of them ends with `--summary` rather than a question the owner would have to carry out.

## Moot CI-fix attempts

A CI-fix revision is created by a `ci_remediations` attempt, and the two are meant to end together. When the revision never gets a worker (its run was cancelled, it was left with automatic start off), the attempt stays `pending` with nothing to finish it, and:

- every later revision on the PR waits behind the dead one;
- for a merge-queue failure, the PR's stored CI state reads `fail` although every check on its head passes;
- the board keeps the attempt's "ci failing (n/m)" badge.

On every open probe with green head CI, whatever the mergeability, the merge poller retires such an attempt, archives its revision and releases what waited on it. That includes an item held on `awaiting_operator_answer` with an open question and an active CI-fix: the poller probes it only for this retire, leaves the question's hold in place, and does not admit the row to automatic remediation or merge.

A merge-queue failure is not a required check on the PR head. Head CI is green by construction for those, so a ready or autostart queue-side fix stays pending until one of the three reasons below. Closing it on green head CI alone would drop a real prerequisite. The next writer that can make a queue-side fix moot is a runnable revision of the same PR chain, not a parked dependent or a dependent on another PR.

The reason is recorded as the attempt's `failure_reason`:

| `failure_reason`                      | When                                                                                                                                                                                                                           |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `moot_head_ci_green`                  | The failure was on the PR head, and the head is green now.                                                                                                                                                                     |
| `moot_conflict_preempts_queue_fix`    | The failure was on a merge-queue build, and the PR now conflicts with its base. It cannot re-enter the queue until the conflict is resolved, which moves the head.                                                             |
| `moot_stopped_ahead_of_queued_writer` | The failure was on a merge-queue build, the fix was stopped (no run queued, automatic start off), and another writer on the PR is queued behind it. That writer moves the head, which supersedes a merge-queue failure anyway. |
| `moot_revision_closed`                | The revision was already archived or deleted. Only the attempt is retired.                                                                                                                                                     |
| `revision_archived_by_owner`          | The owner's answer closed the revision (see above).                                                                                                                                                                            |

Left alone, always:

- a revision a worker is on (`claimed`, `running`, `waiting_human`);
- a revision that has delivered (`in_review`, or its implementation completed);
- a merge-queue fix on a PR that still merges cleanly, unless it is both stopped and in another writer's way. Head CI is green by construction for a merge-queue failure, and a fix that is queued to run is doing its job;
- a parked item.

The retire emits `CiFailureCleared`, and the same pass writes the head's real CI state. An owner answer that archives the revision publishes the same event after commit, so the card's "ci failing" badge is not left on a stale pending row. To see it happen, read `ci_watch: retired a moot CI-fix attempt` in the engine log, or `boss engine ci list --work-item <id>`.

## Where the code is

- Bound changes, wording refusal, and the answer hook: `tools/boss/engine/work-db/src/answer_actions.rs`, called from `apply_run_done` and from `answer_operator_question_from`.
- Wire types, the `--on-answer` spelling and the wording functions: `tools/boss/protocol/src/types/answer_actions.rs`.
- What the agent is told: `tools/boss/engine/progression-brief/src/catalogue.rs`.
- The reconciliation: `tools/boss/engine/work-db/src/moot_ci_fix.rs`, called from `ci_watch::retire_moot_pending_ci_fixes` in the merge poller's `retire_moot_revisions`.
