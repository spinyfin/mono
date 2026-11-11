# Recovering a store with missing predecessor commits

A successful cached `jj log` does not prove that a shared store can rebuild its
index. jj 0.26 walks legacy predecessor commits during a full rebuild, but can
read a newer index that omits them. Collection using that inherited index can
remove the predecessor objects while ordinary commands continue working.

Cube builds a private index before abandoning operations, rebuilds it before
collection, collects using that index, and rebuilds it afterward. An error fails
the pass and records a failed `repo.store_gc` audit event. The preflight leaves
the live index and operation head intact if objects are already missing. This
applies to both daily collection and collection after legacy bookmark cleanup.

## Determine whether recovery is needed

Neither an immediate-collection success row nor a reduced operation-store size
proves integrity. A store collected using only jj 0.26's own complete index is
not necessarily damaged: the regression control preserves its evolution history.
A store that inherited a newer index can be damaged even if it still loads.
Cube's fresh-index verification detects this before another rewrite or collection.
Do not delete the live index to investigate; it may be the only reason the store
still opens. Preserve the original error, jj versions, and audit records.

An operation reparented onto root is not itself evidence of missing objects.
Read its ancestry with `jj --ignore-working-copy --at-operation <operation-id>
op log --no-graph`; a retained operation followed by the all-zero root confirms
the rewrite. To distinguish a slow rebuild from missing predecessors, rebuild
an independent copy as described below. Include the complete operation store,
not just its latest view: rebuild cost depends on retained views and commits,
not only the number of operations.

For a pass that refuses multiple operation heads, do not immediately retry the
engine's post-cleanup collection. A second head may still name abandoned ancestry;
merging it against the empty root can restore deleted bookmarks or move workspace
heads. Preserve the error and inspect the heads before repair. After the store
has a verified single head, cube's regular upkeep can try again with fresh-index
checks. Cleanup remains complete when its collection fails; it does not need to
be replayed. Inspect `repo.store_gc` and `repo.store_gc.index_warmup` together to
see both a warm-up timeout and a subsequent refusal.

## Recover without deleting predecessor records

The tested forward recovery is a coordinated transition to jj 0.32.0, whose
[index retention change](https://github.com/jj-vcs/jj/commit/ea8aa1e17ca845fa28fbbe5867527f2e2963f860)
does not require those legacy predecessor objects. This restores rebuildability
for the newer clients; it cannot recreate already-deleted evolution history.
It is a manual procedure, not an automatic fallback during collection.

1. Stop writers and collectors for the affected shared store, including workers
   and the engine's cleanup. Back up the complete store and working-copy state
   while quiescent. Keep the backup until validation and normal use succeed.
2. Test on an independent copy of the store and its Git object database first.
   The copy must not point back to the live `.jj/repo`, Git directory, or Git
   alternates. Preserve predecessor metadata, operation heads, and indexes.
3. Use jj 0.32.0 for every client sharing the store, including cube's `jj` on
   `PATH`, engine processes, and worker shells (after upgrading, declare it with
   `cube repo declare-oldest-jj <repo> 0.32` so the collector is not refused as too
   new; the declaration is stored with the repo in cube's database and audited as
   `repo.oldest_jj_declared`, so it applies to every collector however it is started). Verify `jj --version` in each
   launch environment. Do not mix upgraded collectors with older readers.
4. On the independent copy, run
   `jj --ignore-working-copy --repository <copy> debug reindex`.
   Compare visible commit IDs, bookmarks, and working-copy
   revisions against the readable original. Check files containing unpublished
   work. A missing Git ancestor, tree, blob, or view is a different corruption:
   stop and restore the required objects instead of proceeding.
5. With all clients upgraded and the live store still quiescent, run the same
   fresh reindex against the live store. Check its primary and secondary
   workspaces before resuming writers. Resume collection using the fixed cube;
   its post-collection verification must succeed before upkeep reports success.

The regression test reproduces a 0.26 missing-predecessor failure, switches both
readers and the collector to 0.32, rebuilds, collects twice, and checks fresh
rebuilds, identical visible commit IDs, file content, and secondary-workspace
loads. This procedure requires upgrading every shared client; merely
upgrading one shell or copying a newer cached index is insufficient.

If older clients must remain, restore the exact missing objects from a known
good backup or another complete object store, including their required trees and
blobs. Verify object identities and a fresh index on an independent copy before
restoring them to the live store. Predecessor metadata and cached indexes do not
contain enough information to recreate arbitrary lost Git objects. Repointing
an operation head at an old cached index only restores temporary readability;
it does not repair the missing objects.

## Verification record: mono and appoint stores, 2026-11-10

Taken on the host that runs the stores, after the pass that abandoned and
collected `mono` (audit row `2026-11-11T00:01:08Z`, 20955 -> 7258 operations).
The stores were cloned with `cp -cR` (APFS copy-on-write) into a scratch
directory and every command ran against the clone, never the live store.
`mono` is 12 GB and was being written while the clone ran, so its first clone
was missing a freshly written `store/extra` segment and `debug reindex` failed
with `Failed to load table segment`. That failure was an artifact of cloning a
live store, not damage: after copying the missing `store/extra` and `op_store`
files from the live store (`cp -cRn`), the clone reindexed.

Commands and results:

- Whether retained history was re-parented onto the root operation. For each
  repo, `jj --ignore-working-copy --at-operation <oldest retained op> op log
--no-graph -T 'id.short(12) ++ " " ++ time.start()'` lists exactly the oldest
  retained operation and then the all-zero root, so the oldest retained
  operation's only ancestor is the root: `mono` `d8e5065c299a` (2026-11-07
  18:36 -06:00), 7325 operations in total, and `appoint` `ded45bdbdfbf`
  (2026-11-07 13:08 -06:00), 10 operations. This is what `jj op abandon ..<cut>`
  does by design (descendants of the abandoned range are re-parented onto the
  root), not evidence of lost retained history.
- Whether the stores can still be rebuilt. `jj --ignore-working-copy debug
reindex` on a consistent clone of `appoint` printed `Finished indexing 3663
commits.` and on `mono` printed `Finished indexing 66479 commits.`, both exit 0. Neither store is missing predecessor objects today.
- The cube audit log (`~/.local/share/cube/audit`) has no `repo.store_gc` row
  with `outcome: failed` for `mono` or `appoint` through 2026-11-11.

Not determined: whether the 2026-11-10 `mono` collection failure that prompted
this work was this predecessor-pruning bug, a precursor of it, or unrelated.
The cube audit log holds no failure row for it, so there is no error text to
classify, and a store that reindexes cleanly today cannot show which it was.
