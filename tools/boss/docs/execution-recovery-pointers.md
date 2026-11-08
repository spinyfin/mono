# Execution recovery pointers

Dispatch creates no bookmarks. It records the baseline commit on the `execution_bookmarks` row, and the rest of the lifecycle is:

- The worker creates the publication branch `boss/<execution id>` (`jj bookmark set boss/<execution id> -r @`) immediately before `cube pr create`. The worker does not advance any recovery bookmark.
- At teardown, when the workspace holds unpublished work, the engine writes one `preserved/<execution id>` bookmark at its tip and records that tip on the execution row. Nothing is written when there is nothing to preserve. The preserved tip is the source for later edits.
- A transitional `boss-recovery/<execution id>` head exists only when a positioned lease already carries unpublished commits (work restored from a preserved tip). A `boss-base/<execution id>` bookmark exists only for legacy executions that were in flight before this layout; recovery never reads it.

Legacy names still found in the store:

- `boss-base/<execution id>`: leftover baseline bookmark from earlier engines. The floor is the row's `base_commit`.
- `boss-recovery/<execution id>`: recovery head, transitional as described above.
- `boss/<execution id>`: the publication head the worker creates before opening a PR, then pushes.

The `execution_bookmarks` row for the execution records the shared store path, the host, and the baseline commit id (`base_commit`) recorded at dispatch from `work_executions.pr_head_before` (bound PR) or the commit the workspace was positioned on (normally trunk, or an open task PR's head when recovery positioned there), captured as the working copy's parent right after positioning, or returned by the engine's own fresh positioning, and carried to pointer creation so a trunk that advances in between cannot move the floor. `trunk()` resolved at pointer creation is only the last resort when no positioned commit could be captured; the positioned commit is captured after every positioning (a fresh change's parent, or the PR head / target commit a goto landed on), even when the pre-pointer snapshot recorded no `pr_head_before`; a failed capture is retried as a pre-start failure rather than recording that later floor. Recovery reads that commit id as the floor of the execution's work; it never reads the `boss-base/` bookmark. A baseline commit that was rewritten or abandoned still exists in the store and still works as a floor; a row with no usable `base_commit` uses the bound PR's `pr_head_before`, then trunk.

Recovery is an optimisation. If pointer integrity checks cannot establish safe recovery, the engine archives the preserved commits and starts fresh. Transport failures retry through the ordinary positioning-failure path without retiring the recovery source. Redoing work is preferable to leaving an item unable to progress.

Recovery resumes a later execution from the unpublished range above the recorded floor (`::head ~ ::floor`). Lineage between the pointers and the floor is not an integrity invariant: two heads on different lines are both resumed. Genuine integrity failures (conflicted or missing heads, a foreign store) archive and start fresh.

## What each pointer is for, and how long it is needed

Every bookmark in the shared store costs every later operation: jj writes a full view of all bookmarks per operation, in every workspace of the repo, and nothing in jj ever removes an operation. With one execution's three pointers kept forever, the dev Mac's mono store held 13,279 bookmarks (4,458 `boss-base/`, 4,456 `boss-recovery/`, 4,346 `boss/`, 7 `pr/`), its views were 2.7 MB each, and its op store reached 95 GiB over 137k operations. The pointers are only needed while the execution's work might still have to be recovered, so the question is what each one carries and who reads it.

Every consumer, by pointer (paths are repo-relative):

**`boss-base/<exec>`**

- Written by: leftover from earlier engines only. Nothing creates or moves it: dispatch `create_from` does not create it, and the engine no longer snapshots, restores or adopts pointers around rebases. Nothing checkpoints or heals it.
- Read by (the bookmark itself): prune (`prune.rs`), and the rearm migration's error-string parsing (`work-db/src/execution_recovery_rearm.rs`).
- Consumers of the recorded commit id, not the bookmark: `resolve_base` (called from `execution_bookmark.rs` and `prune.rs`) reads the row's `base_commit`, then `pr_head_before`, then `trunk()`; it is the floor of `diff` and `unpublished_diff`.
- What is actually needed: a baseline **commit id**, which the row records. `resolve_base` returns it (falling back to `pr_head_before`, then `trunk()`, when the row has none), and nothing repairs, heals or re-checkpoints it: `heal_baseline`, `checkpoint_baseline` and `evolved_baseline` are gone, and the ancestry walk that `durable_baseline` performed is gone with them: `create_from` takes its floor from `inherited_base` (the restore report, or the positioned commit dispatch captured as `@-`), then the predecessor's floor, then `trunk()`, and it never walks `@`'s ancestry. The positioned commit is normally trunk, or an open task PR's head when recovery positioned there.

**`boss-recovery/<exec>`**

- Written by: `create_from` on `@` only when the positioned lease already carries unpublished commits. The worker does not advance it. Teardown preservation is the source for later edits.
- Read by: `resolve_pointers`, `restore`, `restore_for_retry`, `release_heads` and `diff` (all in `execution_bookmark.rs`); the coordinator's blocked-workspace recovery (`create_or_start_fresh` and `recover_from_record` in `coordinator/blocked_workspace.rs`); the abandoned-bookmark sweep (`core/src/abandoned_execution_bookmarks.rs`); prune (`prune.rs`).
- What is actually needed: the tip of unpublished work that was already on the lease at dispatch, including work that was never pushed. Needed until the work is proven published or teardown has preserved it, then never.

**`boss/<exec>`**

- Written by: the worker, immediately before `cube pr create --branch boss/<exec>` (the dispatch prompt builder in `tools/boss/engine/execution-runner/src/runner/prompt.rs`); cube may move it to the remote head on a bound-PR checkout. Dispatch `create_from` does not create it.
- Read by: `resolve_pointers` (the second head); PR binding by head ref (`core/src/pr_binding_repair_sweep.rs`, `core/src/design_detector.rs`); the worker branch prefix (`completion/src/lib.rs`); cube's bookmark sweep, which forgets it once it is reachable from `main` (`tools/cube/src/app/gc.rs`); its remote-tracking twin `boss/<exec>@origin` is a publication voucher for prune (`prune.rs`, `core/src/execution_bookmark_prune.rs`).
- What is actually needed: a **git branch name** to push, which jj can only push from a local bookmark, plus "what was pushed last". The branch must exist while the worker may push again; afterwards GitHub (`headRefName`), the merge poller's `tasks.pr_head_sha`, and the executions' `pr_head_before`/`pr_head_after` columns all record the pushed head without the bookmark.

**`pr/<n>`**

- Written by: cube's bound-PR checkout (`cube workspace goto`) when positioning on a PR (`recover_from_record` in `blocked_workspace.rs`).
- Read by: reconciliation rung 1, the PR head (`reconcile.rs`), and the positioned workspace itself; forgotten by cube's sweep once no workspace is positioned on it.
- What is actually needed: the PR head for the duration of one positioning. Already bounded (7 local bookmarks on the dev Mac); what lingered were 656 `refs/heads/pr/<n>` git refs that an older jj left behind after the bookmark was forgotten, which the store upkeep below removes.

**The minimal set.** During an execution's life the remaining names encode different things, and two of them legitimately diverge: the worker pushes `boss/` (which cube may then move to the remote head) while a transitional `boss-recovery/` or a teardown `preserved/` tip keeps tracking local unpushed work, and the baseline is the recorded commit id. What is not needed is any of them **after** the PR that carried the work is merged or closed, and that is where all of the growth was: the count was 3 × executions ever run. Empty initialization writes no bookmarks; a recovery head exists only for unpublished work already on the lease; `boss/` exists between the worker's first push and merge; `pr/<n>` only during a positioning.

## Lifecycle: how the pointers are retired

Empty initialization records the starting commit and performs no jj store operation. Restored unpublished work creates one transitional recovery head on `@`. The worker creates the publication bookmark immediately before opening a PR. From then on no engine path moves a pointer that already points where it is being set: `set_bookmarks` and `restore_pointers` skip unchanged bookmarks, and an empty `bookmark forget` runs nothing, because an operation that changes no bookmark still writes a full view.

**The prune sweep** (`engine/core/src/execution_bookmark_prune.rs`) runs in the abandoned-branch sweep loop, every five minutes, over at most 100 live local records of terminal executions per pass (`list_prune_candidates`). A record whose chain root is still open is stamped `pr_open` and looked at again in six hours. For the rest it asks the recovery crate to prove the work published (`recovery/src/execution_bookmark/prune.rs`), with these vouchers in order:

1. `unpublished_diff` empty: every non-empty preserved change is already reachable from a remote bookmark.
2. The PR head the merge poller last observed (`tasks.pr_head_sha`), which for a merged PR is the head that merged.
3. The PR heads the chain's executions recorded around their own pushes (`pr_head_before` / `pr_head_after`), newest first.
4. The pushed branch's remote-tracking bookmark `boss/<exec>@origin`, then the trunk's.
5. Only when all of those fail and the PR has a URL: one `gh pr view` for `headRefOid` (which survives the branch's deletion), at most twenty per pass.

A voucher proves the work the same way reconciliation does ([`containment`]): each head is the voucher or one of its ancestors, or every non-empty preserved change has a content-equivalent variant (same change id, empty bookkeeping-filtered interdiff) in the voucher's history. Nothing is ever inferred from a name, and a voucher the store cannot resolve is skipped, never trusted.

Outcomes, recorded in `execution_bookmarks.prune_outcome` / `prune_checked_at` (schema 60):

- `published`: all three bookmarks are forgotten in one operation and the record is retired with the voucher in `retired_reason`. The commits stay in the store; only the references go.
- `already_gone`: both heads were already gone; a surviving baseline is forgotten and the record retired.
- `unproven`: every head no voucher carries is **kept**, whichever head that is (the recovery and publication heads can diverge, so a voucher that carries one says nothing about the other; when no voucher resolved, both stay). The baseline is forgotten because the record's `base_commit` rebuilds it, and so is any head a voucher did carry. An `execution_bookmark_prune_unproven` attention item names the kept heads, the changes nothing vouches for and the exact `jj bookmark forget` that retires them once someone has looked; the record is re-examined daily. Because only proven heads are ever forgotten, the second pass after the GitHub head lookup sees every head that still needs proof.
- `error`: logged, reported through the recovery-failure signal, re-examined daily. One unreadable store never stops the rest of the batch.

**Store upkeep** is cube's, not the engine's: `cube repo gc <repo>` abandons operations older than three days and collects what nothing references, but only at an instant with no `jj` process on the host (a busy store skips the pass until the next day), run once a day per repo by cube's periodic pool-GC pass (see `tools/cube/docs/main.md`, "Shared-store upkeep"). It is generic jj maintenance and knows nothing about these namespaces.

## Which executions inherit a predecessor

`recovery_predecessor` (`tools/boss/engine/work-db/src/execution_bookmarks.rs`) picks the item's latest terminal execution of the same kind that started or has a record. Read-only review kinds (`pr_review`, `pr_review_guide`; `ExecutionKind::is_read_only_review`) never get one. A reviewer never writes to the PR branch, so an earlier reviewer of the same item has no unpublished work for it to inherit, and inheriting that record could only pin the review to the record's host, reposition it on the head the earlier reviewer looked at instead of the PR's current head, or fail it before start whenever that record's pointers are damaged. This prevents the failure pattern observed in mono#3201, where rewritten PR history left later reviews blocked in pre-start recovery with the head unreviewed. Reviews skip recovery lineage inspection, including on self-retry, so damaged predecessor or self-retry pointers cannot block review startup.

## How the pointers get damaged

Lineage between the pointers and the baseline is not an invariant. jj rewrites commits and merges concurrent operations as a matter of course, so a head that no longer descends from the baseline, two heads on different lines, and a missing or moved baseline bookmark are ordinary history and produce no verdict. The `NotDescended`, `Diverged` and `MissingBaseline` kinds were removed (see [the audit](investigations/execution-recovery-jj-semantics-audit-2026-10-30.md), failure catalogue F2, F5 and F6).

- **Range of work.** Unpublished work is `::head ~ ::baseline` (by commit id) minus `::remote_bookmarks()` and empty commits. A baseline rewritten by a pushed rebase is covered by the PR head. `diff` starts from the fork point of the head with the floor or trunk, so the patch for a rewritten baseline holds the PR line and the work above it but not the upstream changes the rebase pulled in.
- **Two heads on different lines** are both resumed: restaging duplicates the unpublished commits of both, and `restore_for_retry` creates one change with both heads as parents. Whichever head descends from the other wins when they share a line.
- **Genuine integrity failures** remain: a conflicted or ambiguous execution bookmark, a pointer in the wrong store (`Other`). They use the archive-and-start-fresh path below; transport errors still retry.

## Reconciliation

There is no reconciliation. The conflict ladder and the speculative pass do not snapshot, restore or adopt execution pointers around a rebase, and the host protocol no longer carries the pointer-reconcile, PR-positioning-with-snapshot or restore-rebased operations (a peer that still sends one is rejected as retired). Instead of repairing the shared store after an engine rebase, the engine does not rebase while a worker is live on the PR's chain; see below.

## What happens to the item

Every pointer-integrity verdict (Other, NotContained and PrHeadUnavailable) that cannot be reconciled uses the same archive-and-start-fresh path. Transport failures such as unavailable SSH, GitHub or cube commands propagate to the ordinary retry path without archiving. For integrity failures, dispatch does the following:

1. Resolve **all** targets of the baseline, recovery and publication bookmarks, including every side of a conflicted bookmark and any surviving recorded baseline commit.
2. Write durable local bookmarks named `preserved-archive/<execution id>/<commit id>` for targets with unpublished non-empty work beyond the baseline and published history. The `preserved/<execution id>` name is reserved for the teardown-time tip of unpublished work, and git cannot hold both `preserved/<exec>` and `preserved/<exec>/<commit>` as refs. Every archive must succeed before any original pointer is released. Separate commit suffixes retain both diverged heads and repeated attempts. These bookmarks are never published or included in execution-pointer pruning.
3. Retire the source record. Release baseline and recovery names; retain predecessor publication bookmarks and their remote tracking, releasing the publication name only when the same execution must reuse it. Use bookmark deletion, not forgetting, when releasing names. Log the cause, archive names and commit identities. Add a resolved informational note to the owning work item (the root task for review guides), displayed in the work-card details; it requires no acknowledgement and never disables autostart. Stale current-attempt archives are logged and disclosed before retrying adoption, even when the predecessor remains live.
4. Discard stale restore reports and use normal fresh positioning: the current PR head for PR work, main for work without a PR, or the captured immutable commit for a review guide or post-merge review. Record the new execution's starting commit there, creating a recovery head only when unpublished work is already on the lease. Resumes follow the same sequence before reusing their own pointer names.

Optional task PRs must still be open before fresh positioning; merged, closed, or unavailable task PRs fall back to trunk. Explicitly bound PRs and immutable review targets retain their positioning rules at initialization and startup resume too. Host selection prefers the host that already holds restorable work when that host is eligible, and does not wait if it is not: another eligible host is chosen and the source adapter archives the work before a fresh start. If that adapter or its transport is unavailable, dispatch retries and retains the source record and pointers. Every host requires successful archiving before source retirement.

A failed archive write is a storage/host bug, not a containment decision. It leaves every original pointer intact and reports the concrete failure; it never creates an `execution_recovery_failed` blocked item or a chain-blocking card. Successful fallback retires the bad source, so later scheduler passes do not refire its failed check.

Migration `20261102_2105_recovery_fresh_start` re-enables existing items parked specifically for `execution_recovery_failed`, including containment failures. Human decisions, other blocked reasons and deleted items remain unchanged. Legacy recovery-chain cards are resolved.

Archive only heads containing unpublished, non-empty work beyond the baseline, trunk and remote history. Empty runs and already published commits need no permanent archive.

### Startup entry points and checks

Implementation execution kinds enter `ExecutionCoordinator::recover_execution_bookmark` in [coordinator/blocked_workspace.rs](../engine/dispatch/src/coordinator/blocked_workspace.rs), called before worker startup by [coordinator/execution.rs](../engine/dispatch/src/coordinator/execution.rs).

| Entry point                                                   | Recovery checks and fresh-start coverage                                                                                                                                                     |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Task and chore implementation                                 | Predecessor diff, PR preparation and restaging; fallback uses main or the task's current PR.                                                                                                 |
| Revision implementation, including review-findings revisions  | The same checks against the revision chain's bound PR; an uncontained predecessor is archived.                                                                                               |
| Conflict resolution and CI remediation                        | The shared pre-start check and direct restore path; failure archives and positions on the bound PR.                                                                                          |
| Automated PR review                                           | Skips recovery lineage inspection, including self-retry; starts at the current PR head.                                                                                                      |
| Review guide                                                  | Skips recovery lineage inspection; positions on its required immutable review target.                                                                                                        |
| Startup pane resume (resume_pane_spawn_for_running_execution) | Integrity failures archive and retire the record before fresh positioning and spawning; transport failures retry.                                                                            |
| Host selection                                                | Prefer the recovery source host when it is eligible; if it cannot admit work, select another eligible host and archive through the source adapter. No wait or backpressure on a full source. |
| Resume / same-execution pre-start retry                       | Its own record is inspected first; fallback archives all old targets before releasing names and creating new pointers.                                                                       |

Pointer initialization is a second boundary: `create_from` checks that an inherited baseline belongs to the same store and, when it must create a recovery head, refuses an existing recovery-head name. Leftover baseline or publication names from a partial legacy initialization are ignored. `create_or_start_fresh` in the coordinator archives and releases partially initialized current-run pointers, retries adoption of a valid predecessor, and only then archives the source before positioning fresh and creating the new record. This also handles a crash between writing refs and persisting their database row. Immutable review targets remain immutable during this fallback.

The low-level checks are `resolve_pointers`, `resume_heads` and `restore_for_retry` in [execution_bookmark.rs](../engine/recovery/src/execution_bookmark.rs). Their failures are handled by the common coordinator fallback rather than a blocking-card policy. Host placement honours explicit constraints; the source adapter archives work on its owning host if fresh dispatch uses another host.

Regression tests in [recovery_fresh_start.rs](../engine/core/src/coordinator_tests/recovery_fresh_start.rs) cover each entry point, including review startup without recovery lineage, unchanged predecessor records and pointers, three scheduler passes without refiring or a second lease, review guides retaining their captured target after the PR advances, and implementation resumes. Read-only review kinds intentionally do not archive or create retirement notes: reviewers do not publish implementation work, and inspecting their recovery lineage would reintroduce stale-head positioning and pre-start failures prevented by mono#3320. [archive_tests.rs](../engine/recovery/src/execution_bookmark/archive_tests.rs) covers conflicted targets, archive-write failures, repeated archives and pruning.

- Any pre-start failure, recovery included, is a `pre_start_failure` record in the execution's dispatch stream (`dispatch-events/executions/<id>/dispatch.jsonl`) at the moment it is classified: the error, the attempt number, and whether the engine retries after a backoff or fails the execution for good.
- A review batch in which no member started (across all roles and attempts, including the supervisor) files its `pr_review_quorum_failed` attention as "reviewers never started", with each role's last pre-start error, and says when it has spent the last automatic generation for the head (`MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS`). The card's review badge then shows "AI review failed to start" instead of "Not reviewed: latest commit" until a review is started explicitly or a new head is pushed, so an unreviewed head that nothing will review again is never silent.

Engine-driven rebases (the conflict ladder's rung 1, rung 0's push, and the speculative no-push probe) rewrite the PR branch in the shared jj store. They are deferred while any execution of the PR's chain (the root item and each revision, which share the PR branch) is `claimed`, `running` or `waiting_human` and is not a review: `WorkDb::live_writers_in_chain` answers that, the ladder returns `LadderOutcome::DeferredForLiveWriter` before it touches cube, and the speculative pass skips the candidate without marking it checked. The attempt stays pending and is retried on a later tick. A failed lookup also defers. Nothing is snapshotted or restored.

## Incident evidence

Read from the shared store's `jj op log` (each operation's `args` tag) and `jj evolog`. The op log records the command line but not the calling process, so "who" below is the command's shape, not a proven caller.

**`exec_18da4153d84bc200_22a` (`NotDescended`).** Case 1 (the work was rebased onto a newer main) is confirmed, and it is the first half of the cause.

1. The baseline `boss-base/…22a` was created on the PR head `819023e5` (parent `bece8152`, main at that time).
2. Eight minutes later, operation `1e799ef90c` ran `jj rebase -b boss/exec_18da3d67b1c1dec0_a -d main@origin --ignore-immutable`, the command `cube workspace rebase` runs (`tools/cube/src/app/workspace_ops.rs`). It was issued for the PR's earlier execution branch, not this one, and it rewrote `819023e5` and its descendants to `8e2a0fa1` (parent `ab635574`, newer main). jj moved the baseline bookmark with the rewrite, so it was consistent after this step.
3. The heads were then built on the rewritten lineage, but operation `d234704a82` (`jj rebase -r @ -d 819023e5… -d ab635574`) re-parented the head onto the superseded variant `819023e5` by commit id, and a second `cube workspace rebase` (`c6b21fdc05`) rebased that variant again, producing the final head `6c94926b` (parents `867e2bff`, `ab635574`). `8e2a0fa1` is not an ancestor of it while `boss-base/…22a` still sits on `8e2a0fa1`; change `mwswktqxypvu` is divergent across `819023e5`, `8e2a0fa1` and `867e2bff`.

Responsible paths: an unprotected `cube workspace rebase` rewriting a baseline other executions depend on (case 1, the engine-side gap this change closes), plus a head re-parented by commit id onto the pre-rewrite variant.

**`exec_18dcff786c983aa8_13` (`MissingBaseline` / lost heads, then a stale head).** Case 1 is again confirmed.

1. At 13:50 the baseline `boss-base/…13` was created on the PR head `5cc5f089` and the heads on its empty child `38749622`.
2. At 13:52:04 operation `e704a2d3e1` ran `jj rebase -b boss/exec_18dcf8a862a6de40_459 -d main@origin --ignore-immutable` (again the `cube workspace rebase` command, for an earlier execution's branch). It rewrote `5cc5f089` to `29fe1059` and `38749622` to `77cd8ce1`; bookmarks followed, so the pointers were still consistent. The same minute shows the command repeated for about ten unrelated `boss/…` branches, the pattern of a sweep over in-review PRs.
3. At 14:18:15 both head bookmarks were `jj bookmark forget`-ed (consistent with cube's lease-time forgetting of conflicted bookmarks; the op log records only the command), and at 14:18:45 a worker ran `jj bookmark set … -r @ --allow-backwards` from a workspace whose `@` was still a child of the pre-rewrite `5cc5f089` (`51a11aec`). The heads therefore returned on the superseded lineage while the baseline bookmark sat on `29fe1059`.

Responsible paths: the same unprotected `cube workspace rebase`, then a worker publishing from a stale pre-rewrite workspace. The speculative no-push probe uses the same `cube workspace rebase` command and rewrites the shared store in the same way, which is why it is now deferred while a worker is live on the chain, as the other engine rebases are.
