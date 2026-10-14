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

Items wedged before schema 41 by any of the three legacy blocker texts are re-armed once by the schema 41 data migration (`execution_recovery_rearm`), the same way schema 39 re-armed the false not-descended failure.

Engine-driven mechanical and speculative rebases checkpoint baseline bookmarks and database provenance before reporting success. Recovery restaging checkpoints the baseline before fetching, preserves the pointers across the fetch, and gives the new execution its restaged baseline. A rewrite followed by loss of the baseline bookmark can still recover from jj evolution history; a matching change id without a recorded predecessor relationship is insufficient.
