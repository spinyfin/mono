# Merge intent: a merge request that outlives its attempt

When someone asks Boss to merge a work item, the engine starts a merge attempt: a place in GitHub's merge queue, an armed auto-merge, or a Trunk queue entry. That attempt can end without merging, usually because CI failed in the queue or the PR conflicts with its base. The engine then fixes the cause with a CI-fix or conflict-resolution revision.

The request itself is recorded separately, as a **merge intent** on the work item, and a failed attempt does not clear it. It records which PR head the person approved, it stands while the fix is made, and it ends on a specific event with a reason the card shows.

**What this does today, and what it does not.** The intent is recorded, kept through failed attempts, ended on the events below, and shown in the app and CLI. When the item is mergeable again the engine ends the intent and leaves the card in Review with a sentence saying what changed since the approved head. It does **not** merge again on its own: there is no code path from a standing intent to a merge request. See [Not yet automatic](#not-yet-automatic).

## How it relates to the other merge records

| Record                                            | What it is                                                                   | Lifetime                                                                                           |
| ------------------------------------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `merge_intents`                                   | A person's request to merge this item's PR, and the head they approved.      | Until the PR merges or one of the events under [What ends an intent](#what-ends-an-intent).        |
| `github_merge_intents`                            | One `gh pr merge --auto --squash` that GitHub accepted for one exact head.   | Retired when that head leaves the queue, the head moves, or the PR merges or closes.               |
| `trunk_merge_intents`                             | One Trunk queue submission, resubmitted by the poller after an eviction fix. | Until Trunk merges or cancels it, or the CI budget is exhausted.                                   |
| `tasks.merge_queue_state = 'resolving_conflicts'` | A merge that hit a conflict the engine is resolving mechanically.            | Until the rebase lands and the merge is re-requested, or the resolution needs a revision or fails. |

The last three each track one submission to a merge mechanism. `merge_intents` is the request behind them. On a GitHub-native product it is the only record still standing once a submission has left the queue; a Trunk submission stays `active` through an eviction and its fix on its own.

## When an intent is recorded

Every accepted Merge When Ready records one, replacing any intent already standing for the item (asking again is a fresh approval of the PR as it is now):

- **GitHub-native merge accepted.** The approved head is the one `gh pr merge --match-head-commit` was pinned to.
- **GitHub refused the request because of a deferred conflict.** The request still stands and releases the conflict. The approved head is the one the merge poller last stored for the PR.
- **Trunk submission accepted.** The approved head is the one the merge poller last stored for the PR.

A request GitHub or Trunk refused for any other reason records nothing: the person saw the error. If the approved head cannot be read, the intent is recorded without one, and the item is later treated as changed (see below).

## What ends an intent

| Event                                                                                                                                        | Status                     | Reason                     | Stays on the card |
| -------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------- | -------------------------- | ----------------- |
| The merge poller observes the PR merged                                                                                                      | `merged`                   | `merged`                   | no                |
| The item finishes any other way (PR closed unmerged, marked done, archived)                                                                  | `closed`                   | `task_finished`            | no                |
| The item is deleted                                                                                                                          | `closed`                   | `task_deleted`             | no                |
| The card is moved out of review                                                                                                              | `withdrawn`                | `moved_out_of_review`      | no                |
| The item is bound to a different PR                                                                                                          | `withdrawn`                | `pr_rebound`               | no                |
| A merge is requested again                                                                                                                   | `superseded`               | `new_request`              | no                |
| The item is parked, directly or through its project                                                                                          | `invalidated`              | `parked`                   | yes               |
| A reviewer requests changes                                                                                                                  | `invalidated`              | `changes_requested`        | yes               |
| A revision is created that is not a CI fix or conflict resolution                                                                            | `invalidated`              | `non_remediation_revision` | yes               |
| The item becomes blocked for a reason other than a merge conflict or CI failure                                                              | `invalidated`              | `blocked`                  | yes               |
| The PR leaves GitHub's merge queue for a reason other than failed checks or a merge conflict                                                 | `lapsed`                   | `removed_from_merge_queue` | yes               |
| Failed checks, and a CI fix will not be minted (maintenance off, budget spent or zero, suppressed, or the same-head episode already handled) | `needs_review`             | `no_decision`              | yes               |
| The item is mergeable again (see [the next section](#when-the-item-is-mergeable-again))                                                      | `needs_review` or `lapsed` | see below                  | yes               |

Notes on the rows that are easy to get wrong:

- **Automatic fixes never end it.** `blocked: ci_failure`, `blocked: ci_failure_exhausted` and `blocked: merge_conflict`, and revisions created by CI remediation (`created_via` starting `ci-fix:`) or conflict resolution (`merge-conflict:`), are the engine fixing what ended the attempt. Every other revision source ends the intent: automated review findings, review-guide and document comments, a person's request, a progression run.
- **Removal from the queue by hand is a withdrawal.** The dequeue reason comes from GitHub's timeline. Failed checks and a merge conflict are what the engine fixes, so the intent stands through them — unless a CI fix will not be minted, in which case it ends as `needs_review` rather than wait forever. Any other reason, including one the engine does not recognise, ends it, so that a later fix cannot act on a request someone took back.
- **Un-parking does not revive it.** A parked item gets no upkeep, so what was approved may be stale by the time it returns.
- **The rules are enforced where the card is written.** Finishing, deletion, a move out of review, a PR rebind, parking, requested changes, a non-remediation revision and an unrelated block are SQLite triggers on `tasks` (`merge_intents_follow_task`, `merge_intents_end_on_revision`), so every writer of those columns is covered without knowing about intents. An already-ended sentence is taken off the card when a new revision is inserted in the first statement of `merge_intents_end_on_revision` or the PR head moves past the one the ending named (`merge_intents_clear_ended_on_head_move`). The module documentation in `tools/boss/engine/work-db/src/merge_intent.rs` is the reference.

An intent that "stays on the card" is shown until the card leaves review, is bound to another PR, a merge is requested again, a new revision is inserted, or the PR head moves past the head the ending compared.

## When the item is mergeable again

On every clean probe of an open PR that is not parked, the merge poller's host settles the item's standing intent (`tools/boss/engine/core/src/merge_intent.rs`). It acts only when all of these hold:

1. The card is in Review's **Ready** section: in review, not blocked, no revision queued or running, no AI review pending or with unaddressed findings, required CI green, mergeable. This is the engine's existing definition of "could be merged right now" (`classify_review_section`), not a second one.
2. No submission is live: no active `github_merge_intents` row and no active `trunk_merge_intents` row. A live submission owns what happens next.
3. No CI remediation or conflict resolution attempt is pending or running.
4. No fix is owed. If the PR left the merge queue on failed checks, the intent waits until a CI remediation attempt exists for that merge-queue episode (the same identity as `ci_remediation_exists_for_merge_queue_episode`), including an older terminal attempt for the same head. If no attempt will be minted — automatic PR maintenance is off, the CI-fix budget is spent or zero, a person suppressed CI remediation, or the episode is already handled — the intent ends immediately as `needs_review` instead of standing. As a backstop, an owed fix older than ten minutes with no attempt is treated as no decision and the card returns to Review.

Then it compares the approved head with the head the poller last stored, and ends the intent:

| What the engine holds                                        | Status         | Reason                        | The card says                                                                               |
| ------------------------------------------------------------ | -------------- | ----------------------------- | ------------------------------------------------------------------------------------------- |
| Head changed; CI fixes and/or conflict resolutions ran since | `needs_review` | `no_decision`                 | "Review before merging again: an automatic CI fix changed the PR (aaaaaaa → bbbbbbb)."      |
| Head changed; no automatic fix ran since                     | `needs_review` | `changed_outside_remediation` | "Review before merging again: the PR changed without an automatic fix (aaaaaaa → bbbbbbb)." |
| Either head unknown                                          | `needs_review` | `no_decision`                 | "Review before merging again: the merge ended and the PR could not be confirmed unchanged." |
| Head unchanged                                               | `lapsed`       | `attempt_ended`               | "Merge again to retry: the merge at aaaaaaa did not complete and the PR is unchanged."      |

Each sentence leads with what to do and why, because a card shows about two lines of it; the popover and `boss task show` carry it whole.

`no_decision` means no safe automatic merge decision is available: automatic fixes changed the PR, a required fix was declined or never started, or a head is unknown. The detail identifies which case needs a person to decide. An unknown head is never treated as unchanged.

A standing intent also counts as "a merge has been requested" for [just-in-time conflict resolution](conflict-resolution-deferral.md): a conflict that appears after a failed attempt is resolved for the merge that was asked for, instead of waiting to be asked again.

## Where it shows

- **Card.** Outside the Merging section, a request that ended shows the engine's sentence under the title, ahead of an earlier "Merge paused" label. A failure to disarm GitHub auto-merge outranks both the ended sentence and live status, because the PR can still merge. Otherwise, a live worker or dispatch status (including a queued revision's "Next up") outranks that sentence. A request that still stands shows "Merge requested" when nothing more specific is showing. A card in Merging shows the merge's own progress.
- **Card popover.** A "Merge request" row: the sentence, the two short heads when a decision compared them, and the count of automatic re-merges when there are any.
- **`boss task show` / `boss chore show`.** `Merge request: <status> (requested <time>)`, the approved head, the head when decided, the reason code and the sentence. `--json` carries the same record as `merge_intent`.
- **Engine log.** `merge intent: item is mergeable again; request ended and card left in review`, with `status`, `reason` and `decided_head_sha`. The board refresh it triggers carries the reason `merge_intent_settled`.
- **Database.** `merge_intents` keeps every intent, including superseded ones: `status`, `reason`, `detail`, `approved_head_sha`, `decided_head_sha`, `automatic_merges`, `created_at`, `ended_at`.

## Not yet automatic

Dequeue replay protection and CI-fix ownership assume the engine host's UTC clock is synchronized with GitHub. Requests and remediation rows use the host clock; timeline events use GitHub's clock. A host clock ahead of GitHub can reject a fresh departure as historical, and a host clock behind GitHub can misattribute a newly minted fix. These comparisons require synchronized clocks.

These are limits of the current behaviour, stated so that nobody reads more into a standing intent than it does.

- **No new automatic re-merge decision.** When no existing submission or automatic path below owns the merge, a completed CI-fix or conflict revision returns to Review with an explanation. A merge-queue-rebounce worker can already re-enqueue the fixed head itself, so returning to Review is not universal. The decision in `core/src/merge_intent.rs` has no outcome that merges. A change that adds one must keep "no decision" a reason to return to Review: a judgement that is missing, fails or is unsure is not approval. `automatic_merges` is always 0 until then.
- **Four existing automatic paths do not consult the intent.** A Trunk-queue product resubmits the PR after an eviction fix. A PR whose GitHub auto-merge stayed armed through a red PR-branch CI run merges when the fix turns it green. A merge that hit a conflict the engine could rebase mechanically is re-requested once the rebase lands (`resolving_conflicts`). A merge-queue-rebounce CI-fix worker re-enqueues its verified fixed head using pinned GitHub auto-merge or `enqueuePullRequest` (`execution-runner/src/runner/prompt.rs`, `compose_queue_ci_fragment`); `queue_ci_evidence` verifies the receipt. All four predate merge intents, merge without any judgement of what the fix changed, and carry on even if the intent has since ended (a reviewer requesting changes mid-rebase does not stop the re-request). While one is live an intent that still stands simply stays `active`. Gating existing automatic paths under the standing intent and its verdict must include this worker requeue, including refusing a missing or ambiguous verdict.
- **No dedicated withdraw control.** A person withdraws a request by moving the card out of review or removing the PR from GitHub's merge queue.
- **A push from outside the engine is caught only at the comparison.** The intent does not end the moment someone pushes to the PR. It ends as `needs_review` when the item is next mergeable, with `changed_outside_remediation` if no automatic fix ran in between. A push that lands alongside an automatic fix is reported as that fix's change.
