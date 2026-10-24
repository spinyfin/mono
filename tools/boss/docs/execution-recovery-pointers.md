# Execution recovery pointers

The engine keeps three bookmarks per execution in the shared jj store of the repo:

- `boss-base/<execution id>`: the baseline, the commit the lease was positioned on before the worker edited anything.
- `boss-recovery/<execution id>`: the recovery head the worker advances as it works.
- `boss/<execution id>`: the publication head, the branch a worker pushes.

The `execution_bookmarks` row for the execution records the shared store path, the host, and the last checkpointed baseline commit (`base_commit`). A missing bookmark can be rebuilt from that commit or a verified evolution successor that is an ancestor of every preserved head.

Recovery resumes a later execution from `boss-base..<head>` of its predecessor. The integrity check requires a single line of history: every head must descend from the baseline, and when both heads exist one must descend from the other. A pointer that fails the check is never resumed from as-is.

## What each pointer is for, and how long it is needed

Every bookmark in the shared store costs every later operation: jj writes a full view of all bookmarks per operation, in every workspace of the repo, and nothing in jj ever removes an operation. With one execution's three pointers kept forever, the dev Mac's mono store held 13,279 bookmarks (4,458 `boss-base/`, 4,456 `boss-recovery/`, 4,346 `boss/`, 7 `pr/`), its views were 2.7 MB each, and its op store reached 95 GiB over 137k operations. The pointers are only needed while the execution's work might still have to be recovered, so the question is what each one carries and who reads it.

Every consumer, by pointer (paths are repo-relative):

**`boss-base/<exec>`**

- Written by: `create_from` (`tools/boss/engine/recovery/src/execution_bookmark.rs:581`); moved by reconciliation and mechanical-rebase checkpoints through `set_bookmarks` (`tools/boss/engine/recovery/src/execution_bookmark/reconcile.rs:398`, called at `:479`, `:507`, `:603`) and by `restore_pointers` (`tools/boss/engine/recovery/src/execution_bookmark/snapshot.rs:21`).
- Read by: `resolve_base` (`execution_bookmark.rs:475`; callers `execution_bookmark.rs:496`, `reconcile.rs:430`, `prune.rs:197`), which is the floor of `diff` (`execution_bookmark.rs:700`) and `unpublished_diff` (`execution_bookmark.rs:781`) and of the lineage check; the redispatch unpublished revsets (`tools/boss/engine/recovery/src/execution_bookmark/redispatch.rs:116`, `:127`); the rearm migration's error parsing (`tools/boss/engine/core/src/work/execution_recovery_rearm.rs:25`); pointer snapshots (`snapshot.rs:8`); prune (`prune.rs:186`).
- What is actually needed: a baseline **commit id** that follows rewrites. The bookmark gives that for free (jj moves bookmarks with rebases) and keeps the commit visible; the record's `base_commit` column plus jj's evolution log give the same answer, and `resolve_base` and `heal_baseline` (`execution_bookmark.rs:724`) fall back to them whenever the bookmark is gone. Needed only until the work is proven published.

**`boss-recovery/<exec>`**

- Written by: `create_from` on `@` (`execution_bookmark.rs:581`); the worker advances it, as the dispatch prompt instructs (`tools/boss/engine/core/src/runner/prompt.rs:1578`, `:1761`).
- Read by: `resolve_heads` (`execution_bookmark.rs:441`), `restore` (`:745`), `restore_for_retry` (`:751`), `release_heads` (`:684`), `diff` (`:700`); the coordinator's blocked-workspace recovery (`tools/boss/engine/core/src/coordinator/blocked_workspace.rs:45`, diff at `:83`); the abandoned-bookmark sweep (`tools/boss/engine/core/src/abandoned_execution_bookmarks.rs:17`); prune (`prune.rs:188`).
- What is actually needed: the tip of the worker's **local** work, including work that was never pushed. This is the one pointer nothing else can reconstruct. Needed until the work is proven published, then never.

**`boss/<exec>`**

- Written by: `create_from` on `@` (`execution_bookmark.rs:581`); the worker advances it and pushes it (`tools/boss/engine/core/src/runner/prompt.rs:465`, `cube pr create --branch boss/<exec>`); cube may move it to the remote head on a bound-PR checkout.
- Read by: `resolve_heads` (`execution_bookmark.rs:441`, the second head); PR binding by head ref (`tools/boss/engine/core/src/pr_binding_repair_sweep.rs:285`, `:288`; `tools/boss/engine/core/src/design_detector.rs:40`); the worker branch prefix (`tools/boss/engine/core/src/completion.rs:453`, `:497`); cube's bookmark sweep, which forgets it once it is reachable from `main` (`tools/cube/src/app/gc.rs:112`, `:157`); its remote-tracking twin `boss/<exec>@origin` is a publication voucher for prune (`prune.rs:58`, `tools/boss/engine/core/src/execution_bookmark_prune.rs:121`).
- What is actually needed: a **git branch name** to push, which jj can only push from a local bookmark, plus "what was pushed last". The branch must exist while the worker may push again; afterwards GitHub (`headRefName`), the merge poller's `tasks.pr_head_sha`, and the executions' `pr_head_before`/`pr_head_after` columns all record the pushed head without the bookmark.

**`pr/<n>`**

- Written by: cube's bound-PR checkout (`cube workspace goto`) when positioning on a PR (`blocked_workspace.rs:182`, `:284`).
- Read by: reconciliation rung 1, the PR head (`reconcile.rs:469`), and the positioned workspace itself; forgotten by cube's sweep once no workspace is positioned on it.
- What is actually needed: the PR head for the duration of one positioning. Already bounded (7 local bookmarks on the dev Mac); what lingered were 656 `refs/heads/pr/<n>` git refs that an older jj left behind after the bookmark was forgotten, which the store upkeep below removes.

**The minimal set.** During an execution's life the three pointers encode three different things, and two of them legitimately diverge: the worker pushes `boss/` (which cube may then move to the remote head) while `boss-recovery/` keeps tracking local unpushed work, and the baseline is the one commit both must descend from. What is not needed is any of them **after** the PR that carried the work is merged or closed, and that is where all of the growth was: the count was 3 × executions ever run, and should be 3 × live executions plus the unproven records kept for a human. The lifecycle below makes it so, which removes the growth term; coalescing changes only the constant.

Two coalescings are sound but are not implemented here, because each is its own reviewable change across the recovery and engine crates:

- **`boss-base/` as a recorded commit, not a bookmark.** `resolve_base` already treats the bookmark as a convenience over `base_commit` and jj's evolution log, and `heal_baseline` already rebuilds it. Stopping `create_from` from creating it saves one of the two creation operations per execution and a third of the live bookmarks. The reconciliation note texts and the rearm migration parse the bookmark's name out of error strings, and the tests across both crates assume it exists, so this is a deliberate change, not a deletion.
- **`boss/` owned by the worker's push, not by dispatch.** The engine creates `boss/<exec>` at dispatch only so the worker finds it; the worker's own `jj bookmark set boss/<exec> -r @` before `cube pr create` would create it just the same, cube's sweep already forgets it once merged, and `Heads.publication` is already optional in recovery. With that, the engine owns exactly one bookmark per execution (`boss-recovery/`), `boss/` exists only between first push and merge, and `pr/<n>` only during a positioning.

## Lifecycle: how the pointers are retired

Creation is two operations at dispatch (`boss-base/` on the durable baseline, then both heads on `@`). From then on no engine path moves a pointer that already points where it is being set: `set_bookmarks` and `restore_pointers` skip unchanged bookmarks, and an empty `bookmark forget` runs nothing, because an operation that changes no bookmark still writes a full view.

**The prune sweep** (`engine/core/src/execution_bookmark_prune.rs`) runs in the abandoned-branch sweep loop, every five minutes, over at most 100 live local records of terminal executions per pass (`list_prune_candidates`). A record whose chain root is still open is stamped `pr_open` and looked at again in six hours. For the rest it asks the recovery crate to prove the work published (`recovery/src/execution_bookmark/prune.rs`), with these vouchers in order:

1. `unpublished_diff` empty: every non-empty preserved change is already reachable from a remote bookmark.
2. The PR head the merge poller last observed (`tasks.pr_head_sha`), which for a merged PR is the head that merged.
3. The PR heads the chain's executions recorded around their own pushes (`pr_head_before` / `pr_head_after`), newest first.
4. The pushed branch's remote-tracking bookmark `boss/<exec>@origin`, then the trunk's.
5. Only when all of those fail and the PR has a URL: one `gh pr view` for `headRefOid` (which survives the branch's deletion), at most twenty per pass.

A voucher proves the work the same way reconciliation does ([`containment`]): each head is the voucher or one of its ancestors, or every non-empty preserved change has a content-equivalent variant (same change id, empty bookkeeping-filtered interdiff) in the voucher's history. Nothing is ever inferred from a name, and a voucher the store cannot resolve is skipped, never trusted.

Outcomes, recorded in `execution_bookmarks.prune_outcome` / `prune_checked_at` (schema 51):

- `published`: all three bookmarks are forgotten in one operation and the record is retired with the voucher in `retired_reason`. The commits stay in the store; only the references go.
- `already_gone`: both heads were already gone; a surviving baseline is forgotten and the record retired.
- `unproven`: every head no voucher carries is **kept**, whichever head that is (the recovery and publication heads can diverge, so a voucher that carries one says nothing about the other; when no voucher resolved, both stay). The baseline is forgotten because the record's `base_commit` rebuilds it, and so is any head a voucher did carry. An `execution_bookmark_prune_unproven` attention item names the kept heads, the changes nothing vouches for and the exact `jj bookmark forget` that retires them once someone has looked; the record is re-examined daily. Because only proven heads are ever forgotten, the second pass after the GitHub head lookup sees every head that still needs proof.
- `error`: logged, reported through the recovery-failure signal, re-examined daily. One unreadable store never stops the rest of the batch.

**Store upkeep** is cube's, not the engine's: `cube repo gc <repo>` abandons operations older than three days and collects what nothing references, but only at an instant with no `jj` process on the host (a busy store skips the pass until the next day), run once a day per repo by cube's periodic pool-GC pass (see `tools/cube/docs/main.md`, "Shared-store upkeep"). It is generic jj maintenance and knows nothing about these namespaces.

## How the pointers get damaged

| Failure                                 | Typed kind        | How it arises                                                                                                                                                                                                                                                                                                                                     |
| --------------------------------------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Head does not descend from the baseline | `NotDescended`    | The change under the baseline was rewritten in one operation (a rebase onto newer main, an amend) while another operation re-parented the head or resurrected the old variant. Concurrent workspaces on one store merge their operations and leave the change divergent; the bookmark stays on the variant the head's lineage no longer includes. |
| The two heads are on different lines    | `Diverged`        | The worker advanced `boss/…` to push but not `boss-recovery/…`, or cube's bound-PR checkout re-pointed the PR branch at the remote head after a local amend.                                                                                                                                                                                      |
| The baseline bookmark is gone           | `MissingBaseline` | The baseline was created on the empty commit `cube workspace goto` leaves under the titled change, and the worker's `jj abandon` of that commit deleted the bookmarks on it (jj deletes bookmarks on abandoned commits). Cube also forgets conflicted bookmarks at lease time.                                                                    |

The baseline is created on the unique topologically nearest ancestor below `@` that is not both empty and undescribed, so the positioning artifact can no longer carry it, and the recorded `base_commit` rebuilds a forgotten bookmark whenever the heads still descend from it.

## Reconciliation

When the integrity check fails with one of the three kinds above, dispatch repairs the pointers from something the engine recorded itself, in this order, and never from a name or a guess:

1. **The PR head.** If the item has a recorded open PR, cube's bound-PR checkout writes `pr/<n>`. Every head must be contained in it: either an ancestor of the PR head, or every non-empty preserved commit outside the PR history has a commit with the same change id and an empty bookkeeping-filtered `jj interdiff` inside it. Upstream commits already on the remote main branch are never preserved work. Recovery snapshots all preserved pointers before checkout and restores them before validation. On success all three bookmarks and the recorded baseline commit move to the PR head, and every implementation kind restores against that PR.
2. **The recorded baseline commit.** If only the bookmark is missing and the heads still descend from `base_commit`, the bookmark is recreated there.
3. **The other head.** Without a PR, two diverged heads of one execution may vouch for each other: if one already carries every change of the other (by change id and equivalent patch), the stale one moves to it.

Anything the engine cannot vouch for is a `NotContained` failure that names each missing change and the exact `jj bookmark set` that would adopt the vouching commit after the content is confirmed present. Nothing moves.

## What happens to the item

- Stranded baseline or diverged heads that nothing vouches for: the item blocks with the precise reason (`execution_recovery_failed`, autostart off). This is a real pointer problem.
- Missing baseline that nothing vouches for: a deliberate fresh start. The record is retired (`retired_at`), so it is no longer a recovery source and no later dispatch retries the dead pointer; an `execution_bookmark_recovery_retired` attention item names the superseded commits, which stay in the store; and the execution proceeds with normal positioning. A self-retry also releases its head bookmarks so it can create fresh pointers under its own names.
- Reconciled pointers: recovery continues as usual and the worker prompt gains an `## EXECUTION POINTER RECONCILIATION` section listing what moved and why.

Items wedged before schema 43 by any of the three legacy blocker texts are re-armed once by the schema 43 data migration (`execution_recovery_rearm`), the same way schema 39 re-armed the false not-descended failure.

Engine-driven mechanical and speculative rebases snapshot the baseline and both heads of every live local record in the PR-owning chain (the root item and each of its revisions, which share the PR branch in the shared store) before cube positioning. The snapshot is best-effort per record: a record whose store cannot be read is logged and left out, so one damaged historical record cannot disable rung 1 or silence the speculative prediction metrics. The snapshot stays alive across the whole mechanical operation, including rung 0's resolver edits and `cube workspace push`, and is restored on every outcome. Speculative probes never push, so their temporary lineage is simply discarded. When the operation pushed a PR head, each record whose preserved work that head already carries (by ancestry, or by change id with an empty interdiff) has its baseline, recovery head and publication head moved onto the pushed head together and its `base_commit` persisted; a record with unpublished work keeps its saved identities as one consistent set, so no operation ever moves only one pointer of a record. Mechanical checkpoints are best-effort per record and can never skip the deletion tripwire or prevent retirement after a clean push. Recovery restaging checkpoints the baseline before fetching, preserves the pointers across the fetch, and gives the new execution its restaged baseline. A rewrite followed by loss of the baseline bookmark can still recover from jj evolution history; a matching change id without a recorded predecessor relationship is insufficient.

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

Responsible paths: the same unprotected `cube workspace rebase`, then a worker publishing from a stale pre-rewrite workspace. The speculative no-push probe uses the same `cube workspace rebase` command and rewrites the shared store in the same way, which is why it is now snapshotted and restored too.
