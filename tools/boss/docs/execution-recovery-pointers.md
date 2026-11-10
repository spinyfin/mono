# Execution recovery pointers

Dispatch creates no bookmarks. It records the baseline commit on the `execution_bookmarks` row, and the rest of the lifecycle is:

- The worker creates the publication branch `boss/<execution id>` (`jj bookmark set boss/<execution id> -r @`) immediately before `cube pr create`. The worker does not advance any recovery bookmark.
- At teardown, when the workspace holds unpublished work, the engine writes one `preserved/<execution id>` bookmark at its tip and records that tip on the execution row. Nothing is written when there is nothing to preserve. The preserved tip is the source for later edits. Preservation happens as the cube lease is released, so dispatch holds a successor (wait reason `awaiting_predecessor_preservation`) while its recovery predecessor still records a lease; see [post-crash recovery](post-crash-recovery.md).
- The engine forgets `preserved/<execution id>` (and, for a closed item, its inspection archives) once it has no further use: when a later execution that restaged the work has published, or when the item's chain has been closed for 14 days. See [Cleanup of preserved references](#cleanup-of-preserved-references).
- The engine creates and reads no `boss-base/`, `boss-recovery/` or `boss/` bookmark anywhere.

Names an earlier engine left in the store, which nothing reads, moves or removes (a one-shot store cleanup retires them):

- `boss-base/<execution id>`: leftover baseline bookmark from earlier engines. The floor is the row's `base_commit`.
- `boss-recovery/<execution id>`: leftover recovery head from earlier engines; dispatch no longer writes it, and the engine no longer reads it.
- `boss/<execution id>`: the publication head the worker creates before opening a PR, then pushes.

The `execution_bookmarks` row for the execution records the shared store path, the host, and the baseline commit id (`base_commit`) recorded at dispatch from `work_executions.pr_head_before` (bound PR) or the commit the workspace was positioned on (normally trunk, or an open task PR's head when recovery positioned there), captured as the working copy's parent right after positioning, or returned by the engine's own fresh positioning, and carried to pointer creation so a trunk that advances in between cannot move the floor. `trunk()` resolved at pointer creation is only the last resort when no positioned commit could be captured; the positioned commit is captured after every positioning (a fresh change's parent, or the PR head / target commit a goto landed on), even when the pre-pointer snapshot recorded no `pr_head_before`; a failed capture is retried as a pre-start failure rather than recording that later floor. Recovery reads that commit id as the floor of the execution's work; it never reads the `boss-base/` bookmark. A baseline commit that was rewritten or abandoned still exists in the store and still works as a floor; a row with no usable `base_commit` uses the bound PR's `pr_head_before`, then trunk.

Recovery is an optimisation. If pointer integrity checks cannot establish safe recovery, the engine archives the preserved commits and starts fresh. Transport failures retry through the ordinary positioning-failure path without retiring the recovery source. Redoing work is preferable to leaving an item unable to progress, and a recovery problem never keeps a workspace leased (see Lease release and teardown preservation below).

Recovery resumes a later execution from the unpublished range above the recorded floor (`::head ~ ::floor`). Lineage between the pointers and the floor is not an integrity invariant: two heads on different lines are both resumed. Genuine integrity failures (conflicted or missing heads, a foreign store) archive and start fresh.

## What each pointer is for, and how long it is needed

Every bookmark in the shared store costs every later operation: jj writes a full view of all bookmarks per operation, in every workspace of the repo, and nothing in jj ever removes an operation. With one execution's three pointers kept forever, the dev Mac's mono store held 13,279 bookmarks (4,458 `boss-base/`, 4,456 `boss-recovery/`, 4,346 `boss/`, 7 `pr/`), its views were 2.7 MB each, and its op store reached 95 GiB over 137k operations. The pointers are only needed while the execution's work might still have to be recovered, so the question is what each one carries and who reads it.

Every consumer, by pointer (paths are repo-relative):

**`boss-base/<exec>`**

- Written by: leftover from earlier engines only. Nothing creates or moves it: dispatch `create_from` does not create it, and the engine no longer snapshots, restores or adopts pointers around rebases. Nothing checkpoints or heals it.
- Read by: nothing. The rearm migration only parses the text of an old blocker that names it (`work-db/src/execution_recovery_rearm.rs`).
- What is actually needed: a baseline **commit id**, which the row records. `create_from` takes its floor from `inherited_base` (the restore report, or the positioned commit dispatch captured as `@-`), then the predecessor's recorded `base_commit` or `pr_head_before` (whichever still exists in the store), then `trunk()`, and it never walks `@`'s ancestry. The positioned commit is normally trunk, or an open task PR's head when recovery positioned there.

**`boss-recovery/<exec>`**

- Written by: leftover from earlier engines only. Dispatch `create_from` writes no recovery bookmark, and the worker does not advance one. Teardown preservation (`preserved/<exec>`) is the source for later edits.
- Read by: nothing. The abandoned-work sweep (`core/src/abandoned_execution_bookmarks.rs`) and the coordinator's blocked-workspace recovery read the recorded `preserved/<exec>` tip only.
- What is actually needed: the tip of unpublished work, which teardown records on the execution row and bookmarks once.

**`boss/<exec>`**

- Written by: the worker, immediately before `cube pr create --branch boss/<exec>` (the dispatch prompt builder in `tools/boss/engine/execution-runner/src/runner/prompt.rs`); cube may move it to the remote head on a bound-PR checkout. Dispatch `create_from` does not create it.
- Read by: PR binding by head ref (`core/src/pr_binding_repair_sweep.rs`, `core/src/design_detector.rs`); the worker branch prefix (`completion/src/lib.rs`); cube's bookmark sweep, which forgets it once it is reachable from `main` (`tools/cube/src/app/gc.rs`). The engine reads it only as a GitHub head ref, never as a jj bookmark.
- What is actually needed: a **git branch name** to push, which jj can only push from a local bookmark, plus "what was pushed last". The branch must exist while the worker may push again; afterwards GitHub (`headRefName`), the merge poller's `tasks.pr_head_sha`, and the executions' `pr_head_before`/`pr_head_after` columns all record the pushed head without the bookmark.

**`pr/<n>`**

- Written by: nothing. Cube no longer writes, reads or sweeps `pr/<n>`: `cube workspace goto` positions on the PR head without creating an alias, and the bookmark sweep only handles execution bookmarks.
- Read by: nothing. The engine never read `pr/<n>`. Legacy aliases created by earlier cube versions remain until the dependent one-shot archival cleanup removes them, and a jj rebase may still carry a legacy alias along with the commits it names.
- What is actually needed: nothing. The PR head is recorded by GitHub and by the engine's `pr_head_*` columns. `cube repo gc` still removes orphaned `refs/heads/pr/<n>` git refs left behind by earlier jj versions (`tools/cube/src/app/store_gc.rs`).

**The minimal set.** During an execution's life the remaining names encode different things, and two of them legitimately diverge: the worker pushes `boss/` (which cube may then move to the remote head) while a teardown `preserved/` tip keeps tracking local unpushed work, and the baseline is the recorded commit id. What is not needed is any of them **after** the PR that carried the work is merged or closed, and that is where all of the growth was: the count was 3 × executions ever run. Empty initialization writes no bookmarks; no recovery head is written at dispatch; `boss/` exists between the worker's first push and merge; `pr/<n>` is no longer maintained at all.

## Cleanup of preserved references

Dispatch records the starting commit and performs no jj store operation. The worker creates the publication bookmark immediately before opening a PR. Teardown writes `preserved/<exec>` only when unpublished work exists. From then on an empty `bookmark forget` runs nothing, because an operation that changes no bookmark still writes a full view.

**The cleanup sweep** (`engine/core/src/preserved_cleanup_sweep.rs`) runs in the abandoned-branch sweep loop, every five minutes. It asks the work database for two kinds of execution whose `preserved/<exec>` tip is finished with (`list_superseded_preserved` and `list_closed_chain_preserved`, in `work-db/src/preserved_cleanup.rs`), at most 50 of each per pass. Nothing is proven about commits; both rules rest on facts the engine already records:

1. **Superseded.** A later execution restaged the tip (`execution_bookmarks.recovered_from`, directly or through a chain of up to 16 restages) and has published (`work_executions.pr_head_after` is nonempty and differs from a recorded, nonempty `pr_head_before`; a missing baseline is not proof of publication). The predecessor's `preserved/<exec>` is forgotten. `preserved-archive/<exec>/…` references are not: they hold variants that were _not_ restaged, and an attention item asked a human to look at them.
2. **Closed chain.** The item's chain root has been `done` or `archived` for 14 days (`CLOSED_CHAIN_RETENTION_SECS`, measured from the root's `completed_at` / `archived_at`). Open `preserved_work_not_restaged` or `abandoned_execution_bookmark` items prevent this cleanup. `preserved/<exec>` and `preserved-archive/<exec>/…` go together, unless a `preserved_work_needs_inspection` item for the execution is still open: that item names work teardown set aside for a human, so only the tip is forgotten. Archive cleanup remains pending independently of the tip (`execution_bookmarks.archive_cleanup_pending`, set when a `preserved_work_archived` or `preserved_work_needs_inspection` item is filed, and backfilled by its migration only for executions with a tip or such an item) and resumes after the inspection item is resolved.

Each execution's references are forgotten in **one** jj operation (`forget_preserved_references` in `recovery/src/execution_bookmark/preserved_cleanup.rs`: one `bookmark list`, then one `bookmark forget exact:…` for every name found; no operation is written when none exist). Afterwards the execution's recorded tip is cleared, so dispatch never tries to restage a bookmark that is gone, and superseded cleanup resolves `abandoned_execution_bookmark` and `preserved_work_not_restaged`. Closed-chain cleanup resolves `preserved_work_archived` when the archives are forgotten.

A failure (host unreachable, jj failed) keeps the references and the recorded tip, is logged, and is not retried for an hour, with backed-off ids excluded before the batch limit. Each list is also walked with a cursor (in id order) that moves past every attempted record and wraps only after a short batch, so a long failing prefix cannot keep a healthy later record from being reached once its backoff expires. A local repository that no longer exists holds nothing to forget, and the tip is cleared. The commits themselves stay in the store until `cube repo gc`; only the references go.

**Store upkeep** is cube's, not the engine's: `cube repo gc <repo>` abandons operations older than three days and collects what nothing references, but only at an instant with no `jj` process on the host (a busy store skips the pass until the next day), run once a day per repo by cube's periodic pool-GC pass (see `tools/cube/docs/main.md`, "Shared-store upkeep"). It is generic jj maintenance and knows nothing about these namespaces.

## Which executions inherit a predecessor

`recovery_predecessor` (`tools/boss/engine/work-db/src/execution_bookmarks.rs`) picks the item's latest terminal execution of the same kind that started or has a record. Read-only review kinds (`pr_review`, `pr_review_guide`; `ExecutionKind::never_writes_pr_branch`) never get one. A reviewer never writes to the PR branch, so an earlier reviewer of the same item has no unpublished work for it to inherit, and inheriting that record could only pin the review to the record's host, reposition it on the head the earlier reviewer looked at instead of the PR's current head, or fail it before start whenever that record's pointers are damaged. This prevents the failure pattern observed in mono#3201, where rewritten PR history left later reviews blocked in pre-start recovery with the head unreviewed. Reviews skip recovery lineage inspection, including on self-retry, so damaged predecessor or self-retry pointers cannot block review startup.

## How the pointers get damaged

Lineage between the pointers and the baseline is not an invariant. jj rewrites commits and merges concurrent operations as a matter of course, so a head that no longer descends from the baseline, two heads on different lines, and a missing or moved baseline bookmark are ordinary history and produce no verdict. The `NotDescended`, `Diverged` and `MissingBaseline` kinds were removed (see [the audit](investigations/execution-recovery-jj-semantics-audit-2026-10-30.md), failure catalogue F2, F5 and F6).

- **Range of work.** Unpublished work is the preserved tip's unpublished non-empty commits (`(::tip ~ ::remote_bookmarks()) ~ empty()`). A baseline rewritten by a pushed rebase is covered by the PR head.
- **Two heads on different lines** are both resumed: restaging duplicates the unpublished commits of both. Whichever head descends from the other wins when they share a line.
- **Genuine integrity failures** remain: a conflicted or ambiguous execution bookmark, a pointer in the wrong store (`Other`). They use the archive-and-start-fresh path below; transport errors still retry.

## Reconciliation

There is no reconciliation. The conflict ladder and the speculative pass do not snapshot, restore or adopt execution pointers around a rebase, and the host protocol no longer carries the pointer-reconcile, PR-positioning-with-snapshot or restore-rebased operations (a peer that still sends one is rejected as retired). Instead of repairing the shared store after an engine rebase, the engine does not rebase while a worker is live on the PR's chain; see below.

## What happens to the item

Every pointer-integrity verdict (`Other`, `Quarantined`) uses the same archive-and-start-fresh path. Transport failures such as unavailable SSH, GitHub or cube commands propagate to the ordinary retry path without archiving. For integrity failures, dispatch does the following:

1. Resolve **all** targets of the baseline, recovery and publication bookmarks, including every side of a conflicted bookmark and any surviving recorded baseline commit.
2. Write durable local bookmarks named `preserved-archive/<execution id>/<commit id>` for targets with unpublished non-empty work beyond the baseline and published history. The `preserved/<execution id>` name is reserved for the teardown-time tip of unpublished work, and git cannot hold both `preserved/<exec>` and `preserved/<exec>/<commit>` as refs. Every archive must succeed before any original pointer is released. Separate commit suffixes retain both diverged heads and repeated attempts. These bookmarks are never published; closed-chain cleanup forgets them with the tip.
3. Retire the source record. Release baseline and recovery names; retain predecessor publication bookmarks and their remote tracking, releasing the publication name only when the same execution must reuse it. Use bookmark deletion, not forgetting, when releasing names. Log the cause, archive names and commit identities. Add a resolved informational note to the owning work item (the root task for review guides), displayed in the work-card details; it requires no acknowledgement and never disables autostart. Stale current-attempt archives are logged and disclosed before retrying adoption, even when the predecessor remains live.
4. Discard stale restore reports and use normal fresh positioning: the current PR head for PR work, main for work without a PR, or the captured immutable commit for a review guide or post-merge review. Record the new execution's starting commit there without creating any recovery bookmark. Resumes follow the same sequence before reusing their own pointer names.

Optional task PRs must still be open before fresh positioning; merged, closed, or unavailable task PRs fall back to trunk. Explicitly bound PRs and immutable review targets retain their positioning rules at initialization and startup resume too. Host selection prefers the host that already holds restorable work when that host is eligible, and does not wait if it is not: another eligible host is chosen and the source adapter archives the work before a fresh start. If that adapter or its transport is unavailable, dispatch retries and retains the source record and pointers. Every host requires successful archiving before source retirement.

A failed archive write is a storage/host bug, not a containment decision. It leaves every original pointer intact and reports the concrete failure; it never creates an `execution_recovery_failed` blocked item or a chain-blocking card. Successful fallback retires the bad source, so later scheduler passes do not refire its failed check.

Migration `20261102_2105_recovery_fresh_start` re-enables existing items parked specifically for `execution_recovery_failed`, including containment failures. Human decisions, other blocked reasons and deleted items remain unchanged. Legacy recovery-chain cards are resolved.

Archive only heads containing unpublished, non-empty work beyond the baseline, trunk and remote history. Empty runs and already published commits need no permanent archive.

## Lease release and teardown preservation

When a run ends, the engine returns the cube workspace lease after one best-effort preservation step (`apply_release` in [completion/src/workspace_lease_release.rs](../engine/completion/src/workspace_lease_release.rs)). The pane and occupancy verdicts come first, because preserving snapshots the working copy and must not race a worker that may still be running jj there; only if they allow the release does the engine preserve (writing `preserved/<execution id>` when unpublished work exists) and then release.

Preservation never decides whether the lease is released. If it fails (the recovery record or workspace path is missing, the owning host or cube registry cannot be reached, jj fails), the engine logs it, files one `preserved_work_needs_inspection` attention item on the execution that names the error and the lease, and attempts to release the lease anyway. Local release uses `--force-reset` only after successful preservation; after failure it uses plain release, so cube's unpushed-work guard keeps or salvages the dirty tree. Remote and registry-only force-release do not run cube's salvage and do not reset the workspace tree. The worker's last working-copy commit remains in the shared jj store, findable with `jj op log`, but without a preserved bookmark it is not protected from garbage collection. A later successful retry refreshes the teardown verdict with the preserved tip and lease outcome, resolving an item that has no independent inspection evidence. An execution that never started and was never positioned (no recovery record) holds only what cube placed in the workspace and skips preservation silently. Only an unconfirmed pane or occupancy verdict (a live pane, a process in the workspace, an occupancy scan that could not complete) keeps a lease.

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

Pointer initialization is a second boundary: `create_from` checks that an inherited baseline belongs to the same store and writes no bookmark. Leftover baseline, recovery or publication names from a legacy initialization are ignored. A crash between positioning and persisting the execution row leaves no pointer to clean up. Immutable review targets remain immutable during this fallback.

The low-level checks are `restore_preserved` and `preserved_work_unpublished` in [execution_bookmark.rs](../engine/recovery/src/execution_bookmark.rs). Their failures are handled by the common coordinator fallback rather than a blocking-card policy. Host placement honours explicit constraints; the source adapter archives work on its owning host if fresh dispatch uses another host.

Regression tests in [recovery_fresh_start.rs](../engine/core/src/coordinator_tests/recovery_fresh_start.rs) cover each entry point, including review startup without recovery lineage, unchanged predecessor records and pointers, three scheduler passes without refiring or a second lease, review guides retaining their captured target after the PR advances, and implementation resumes. Read-only review kinds intentionally do not archive or create retirement notes: reviewers do not publish implementation work, and inspecting their recovery lineage would reintroduce stale-head positioning and pre-start failures prevented by mono#3320. Cleanup is covered by the `preserved_cleanup_tests` modules in the recovery crate and in `work-db`, and by the `preserved_cleanup_sweep` module of `coordinator_tests/execution_bookmarks.rs`.

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
