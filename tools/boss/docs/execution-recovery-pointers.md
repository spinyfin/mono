# Execution recovery pointers

The engine keeps three bookmarks per execution in the shared jj store of the repo:

- `boss-base/<execution id>`: the baseline, the commit the lease was positioned on before the worker edited anything.
- `boss-recovery/<execution id>`: the recovery head the worker advances as it works.
- `boss/<execution id>`: the publication head, the branch a worker pushes.

The `execution_bookmarks` row for the execution records the shared store path, the host, and the last checkpointed baseline commit (`base_commit`). A missing bookmark can be rebuilt from that commit or a verified evolution successor that is an ancestor of every preserved head.

Recovery resumes a later execution from `boss-base..<head>` of its predecessor. The integrity check requires a single line of history: every head must descend from the baseline, and when both heads exist one must descend from the other. A pointer that fails the check is never resumed from as-is.

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
