# Just-in-time merge-conflict resolution

By default the engine resolves a merge conflict on an in-review PR as soon as the merge poller sees it. Most of that work is wasted: 53% of conflict states are a repeat conflict on the same PR, and 58% of resolutions were redone or never landed (`docs/investigations/merge-conflict-sources-and-remedies.md`, Finding 2 and idea E4). With deferral on, a conflict on a PR nobody has asked to merge is **recorded and left alone**, and resolved when a merge is requested or a person asks.

Ships off. Turning it on is an operator decision: see [Turning it on](#turning-it-on).

## What a deferred conflict looks like

| Where                        | What you see                                                                                                                                                                                                                      |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Review card                  | The red `conflicts` chip, as for any conflicting PR. When nothing but the merge is left, the status line reads "Waiting — conflicted; the conflict will be resolved when a merge is requested" (wait reason `conflict_deferred`). |
| Work item                    | Still `in_review`, not blocked. `pr_mergeable_state` stays `conflicting`. No conflict revision exists.                                                                                                                            |
| `boss engine conflicts list` | One row with status `deferred` and `deferral_arm = deferred`. `--status deferred` lists every PR being held.                                                                                                                      |
| Stuck report                 | `awaiting_merge:conflict_deferred`, or `review_without_live_work:conflict_deferred` when the review itself has stalled. The bucket is the one the item would be in anyway.                                                        |
| Progression                  | A `deferred` row is not an in-flight attempt, so it never hides a stuck item from the progression sweep. The brief lists it under "Conflict resolutions".                                                                         |

A deferred PR is never reported clean, and a deferred conflict is never forgotten: every later merge-poller pass re-reads the PR and re-asks whether the conflict should still wait.

## When a conflict waits

All of these must hold when the poller observes the PR conflicting:

1. The PR is in the deferring arm (see [the experiment](#the-ab-experiment)).
2. No merge has been requested for it.
3. Its work item is `in_review` with no resolution in flight or owed: no `pending`/`running` attempt, no live conflict revision, no active `merge_conflict` signal. A row that is blocked, or that a person moved, keeps the watcher's existing handling.
4. This conflict has not already been released. A conflict is deferred at most once.

The product-level and per-PR opt-outs, a parked row, and an active auto-rebase attempt are checked first, as before. They leave the PR alone entirely.

## When it is resolved

| Trigger                                      | How                                                                                                                                                        | `deferral_end_reason` |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- |
| A merge is requested                         | Merge When Ready in Boss, or auto-merge / the merge queue on GitHub. If GitHub refuses the request because of the conflict, the request still releases it. | `merge_requested`     |
| A person asks                                | `boss engine conflicts retry <id>` on the `deferred` row.                                                                                                  | `human_request`       |
| Deferral is switched off, or the arm changes | The next pass releases every held conflict.                                                                                                                | `deferral_disabled`   |
| A resolution started some other way          | For example a recovery path minted a conflict revision.                                                                                                    | `resolution_started`  |

Releasing only closes the deferral record. The same merge-poller pass then runs the unchanged resolution path, so a released conflict goes through the escalation ladder, the worker fallback, the churn guard and the push gate exactly like one that was never deferred. There is no shorter path for a PR that is about to merge.

A deferred conflict can also end without any resolution:

| What happened                                                                                 | `deferral_end_reason` |
| --------------------------------------------------------------------------------------------- | --------------------- |
| GitHub reports the PR mergeable again (the author rebased, or a review-feedback revision did) | `conflict_cleared`    |
| The PR merged                                                                                 | `pr_merged`           |
| The PR was closed unmerged                                                                    | `pr_closed`           |

### Why "a merge was requested" is the readiness signal

Deferral is meant to wait until the PR is otherwise ready to merge. The engine reads that as: someone has asked for the merge. It is the one fact the engine holds that says a person decided this PR should land, because the engine never merges a PR on its own. It is read from four places, any one of which is enough: GitHub's merge-queue entry, GitHub's armed auto-merge, an active `github_merge_intents` row, and an active `trunk_merge_intents` row.

Review and CI state were the alternative and were rejected. A PR is usually reviewed and green within minutes of opening and can then sit for weeks, conflicting again on every nearby merge. Keying on review and CI would resolve exactly those conflicts, which are the ones deferral exists to skip.

The cost is the one the investigation names: merging a deferred PR waits for one resolution and one CI run after the merge request. On a `trunk_queue` product the standing merge intent resubmits the PR once the conflict clears. With GitHub-native merging the armed auto-merge does the same; if GitHub refused to arm it, request the merge again once the card stops showing the conflict.

While a conflict is deferred, CI remediation for the PR waits too, as it does today for any conflicting PR (a conflict pre-empts CI). That makes a PR that is both conflicted and red wait on a merge request: nothing fixes its CI until someone asks to merge it. If you want it fixed, request the merge (the conflict is then resolved and CI remediation resumes) or run `conflicts retry` on the card; do not wait for green first.

## PRs that never become ready

A deferred, conflicted PR that nobody asks to merge stays `deferred`. It costs no resolution runs and raises no question for the repo owner. What happens to a long-stalled review item is decided by the same stuck-work handling as before; deferral only makes sure that handling sees the truth. In particular a `deferred` row is not counted as a fix in flight, so it cannot hide a stalled item.

## Turning it on

Two flags in `tools/boss/engine/feature-flags/src/lib.rs`, both in the `conflict` section of Debug → Feature Flags:

| Flag                                    | Default | Effect                                                                                                       |
| --------------------------------------- | ------- | ------------------------------------------------------------------------------------------------------------ |
| `conflict_resolution_deferral`          | off     | Master switch. Off: every conflict is resolved on detection, and anything held is released on the next pass. |
| `conflict_resolution_deferral_ab_split` | on      | Only odd-numbered PRs are deferred. Has no effect while the master switch is off.                            |

Flags are read on every detection, so a change takes effect on the next merge-poller pass with no restart.

### The A/B experiment

With both flags on, PRs split by number parity: **odd** numbers are the treatment arm (deferred), **even** numbers and PRs whose number cannot be read are the control arm (resolved on detection). Turn the split off to defer every PR.

Every `conflict_resolutions` row written while deferral is on records its arm in `deferral_arm` (`deferred` or `control`), so the comparison does not depend on the flags staying put. The three measures the investigation asks for (experiment 5) come from:

- **Resolutions per merged PR:** rows with status `succeeded`, grouped by `deferral_arm`, over merged PRs. Deferral records (`deferred`, or `superseded` with a `deferral_end_reason`) are not resolutions.
- **Time from merge request to merge:** `github_merge_intents.created_at` (or `trunk_merge_intents.created_at`) to the task's `completed_at`, by arm.
- **How often a deferred conflict needed a human:** deferral records whose `deferral_end_reason` is `human_request`, against all deferral records. `merge_requested` is the designed path and is not a human rescue.

## Where the code is

- Decision (when to hold, when to release): `tools/boss/engine/core/src/conflict_deferral.rs`, called at the front of `conflict_watch::on_conflict_detected_with`.
- Ledger rows and their invariants: `tools/boss/engine/work-db/src/conflict_deferral.rs`.
- Arm assignment: `ConflictDeferralArm` in `tools/boss/engine/feature-flags/src/lib.rs`.
- Card wait reason: `conflict_deferred` in `tools/boss/engine/storage-values/src/dispatch_wait.rs`, named in `tools/boss/engine/work-db/src/unscheduled_wait.rs`.
