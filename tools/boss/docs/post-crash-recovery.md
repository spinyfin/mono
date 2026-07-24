# Post-crash recovery: orphaned executions

When the Boss macOS app is force-quit (or crashes) while a worker is
mid-task, the libghostty pane that hosted the worker dies along with
the app. On the next relaunch the engine restores the `work_executions`
row from sqlite but has no live worker to reattach it to — the row is
**orphaned**.

This doc describes how the engine detects orphans, how to recover from
them manually, and what state survives the cycle.

## What "orphaned" means

`orphaned` is a terminal status on the `work_executions` table,
alongside `completed` / `failed` / `cancelled` / `abandoned`. It
specifically denotes: _a worker was spawned for this execution, then
the engine lost the ability to verify it was still alive (typically
because the libghostty pane died across an engine restart)._

Compared to the other terminal statuses:

- `completed` / `failed`: the worker finished its turn and the
  completion handler stamped the verdict.
- `cancelled`: a human invoked `bossctl work cancel <execution>` or
  `bossctl agents stop <agent>`.
- `abandoned`: the row never produced a `work_runs` entry (the engine
  crashed mid-dispatch, before the worker spawned).
- `orphaned`: the row had a live worker once; the engine no longer
  has signal that it does.

**Critical difference from `cancelled`:** the cube workspace lease on
an orphaned row is intentionally **not** released. The workspace
filesystem typically still holds in-flight commits the next worker
should resume against. The engine clears `cube_lease_id` /
`cube_workspace_id` / `workspace_path` only when the human explicitly
cancels, or when cube's lease TTL elapses and cube force-releases it.

## How orphans get detected

### Automatic detection at engine startup

`tools/boss/engine/src/app.rs::serve` runs a probe at startup over
every non-terminal `work_executions` row that carries a recorded
`cube_lease_id`. The probe (`crate::run_reconcile::probe_in_flight_runs`)
asks cube whether the lease is still bound to the same workspace and
not yet expired; the verdict is one of:

- `Live` — cube confirms the lease. The engine leaves the row alone.
- `Dead` — cube says the workspace is free, the lease id has changed,
  or the lease has logically expired (TTL passed). The engine marks
  the execution `orphaned` immediately and inherits the workspace_id
  into the next ready row's `preferred_workspace_id` so the redispatch
  resumes against the same branch.
- `Unknown` — the probe couldn't decide (cube call failed, workspace
  not in the snapshot, sparse persisted state). The row is left
  alone; a loud `tracing::warn!` is emitted so the operator can
  resolve manually.

After the reaper passes, `reconcile_active_dispatch` runs as before
and creates fresh `ready` rows for work items whose Doing-column
status no longer matches a live execution.

### Manual escape hatch: `bossctl agents reap <run-id>`

The automatic probe is bounded by cube's lease TTL (30 minutes by
default — see `tools/cube/src/app.rs::DEFAULT_LEASE_TTL_SECS`). If the
app crash was recent, cube still reports the lease as `leased` and the
probe verdict is `Live`, even though the worker pane is gone.

For that gap, the coordinator (Boss-only) can reap manually:

```
bossctl agents reap exec_18ad6336fedcb190_12
```

This bypasses the probe and transitions the execution straight to
`orphaned`. The same workspace-preservation rules apply: cube columns
are left intact so a re-dispatch can pick the same branch back up.

`agents reap` requires `RpcTier::BossOnly`. Worker panes cannot
invoke it (they shouldn't be reaping each other).

## What happens after reap

Once the predecessor is `orphaned`:

1. The work item's kanban status is unchanged. If it was `active`
   (Doing), it stays there — the dispatcher will pick it back up.
2. `bossctl work start <work-item>` or the auto-dispatcher creates a
   new `work_executions` row in `ready`. The new row's
   `preferred_workspace_id` defaults to the orphan's
   `cube_workspace_id`, so cube will re-lease the same workspace
   when one is free.
3. The fresh worker spawns into that workspace. Inside the lease,
   `jj git fetch && jj edit <bookmark>` brings it back to the branch
   the orphan was working on; from there it can push and open / update
   the PR as if no crash had happened.

## Automatic workspace patch backup

Preserving the lease keeps the in-flight work _reachable_ only as long as
the workspace's dirty working copy survives — it is still lost the moment
that workspace is re-leased and reset, or if cube cannot reclaim it. As a
precaution, every path that transitions a live execution to `orphaned`
now also snapshots the workspace's uncommitted work to a durable patch
**at death-detection time**, independent of whether the workspace can
later be reclaimed.

The capture is implemented in
`tools/boss/engine/src/recovery_backup.rs` and invoked from all three
at-death hooks:

- `dead_pid_sweep` — worker PID probe reports the process is gone.
- `stale_worker_sweep` — worker is wedged past the staleness threshold.
- the startup reaper in `app.rs` — cube probe verdict `Dead` across an
  engine/UI restart.

Each hook runs `jj diff --git` against the dead worker's leased workspace
(equivalent to `jj diff --git -R <ws>`) and, if the working copy is
dirty, writes the git-format patch to:

```
$HOME/Library/Application Support/Boss/recovery/<exec-id>.patch
```

(overridable via the `BOSS_RECOVERY_DIR` env var). The path is recorded
in two places so a human or a resuming worker can find it: the
`[engine-reconcile]` audit line appended to the work item's description
(`Uncommitted work backed up to <path>.`), and the `recovery_patch`
field of the `dead_pid_reconcile` / `stale_worker_reconcile` dispatch
event.

The capture is **best-effort and non-fatal**: a missing workspace path,
an unavailable `jj`, or an empty diff is logged and swallowed, and the
reap proceeds regardless.

Boss's own bookkeeping (`.boss/`, notably the `events-pending.jsonl`
hook spool) is filtered out of the capture. Patch size is not a signal
of value: three of the four patches taken at 14:42 PDT on 2026-07-23
were 203 KB / 197 KB / 38 KB of nothing but that spool, and only an
11 KB patch held real code.

**Scope:** the patch captures _uncommitted_ working-copy changes only.
Local commits the worker already `jj describe`d into ancestors of `@`
are not captured (capturing `trunk..@` is a possible future
enhancement), and branches already pushed to origin are already durable
there.

## Automatic recovery: cube first, patch second

The engine replays these patches itself — you do not normally need to
apply one by hand. `tools/boss/engine/core/src/recovery_apply.rs` is the
read side, driven from `coordinator.rs::reconcile_workspace_recovery`
on every resume dispatch:

1. **Cube first.** The resume leases `--prefer <workspace> --allow-dirty`,
   which reclaims the dead worker's own workspace _without_ resetting it.
   Cube reports `dirty_verified` on the lease payload: `true` means the
   working copy still held work existing on no remote, i.e. the work was
   recovered in place, with its jj operation log intact. Nothing is
   replayed — applying the patch on top would duplicate or conflict with
   work that is already there.
2. **Patch second.** Only when cube could _not_ recover — the lease
   failed, or it succeeded with `dirty_verified: false` because the tree
   had already been reset — is the patch applied (`git apply --3way`)
   into whatever workspace the resuming worker actually got.

The outcome is recorded three ways: a `workspace_recovery` dispatch
event (`source` = `cube_in_place` | `patch`, with restored file and line
counts); a `.boss/recovery-report.json` marker in the workspace, which
the worker's `## STARTUP RECOVERY` prompt block reads so the worker
knows it is resuming and what it is resuming _from_; and, on success, a
rename of the patch to `<exec-id>.patch.applied` so a later restart does
not replay it over the work it already restored.

**A failed apply is loud.** It emits `workspace_recovery` with
`outcome=error`, logs at ERROR, leaves the patch un-consumed for manual
salvage, and writes a marker whose prompt block tells the worker
explicitly not to assume anything was recovered. Recovery code that
fails quietly is worse than none: the worker rebuilds from scratch while
believing it is resuming.

To replay a patch by hand (e.g. one left behind by a failed apply):

```
git apply --3way "$HOME/Library/Application Support/Boss/recovery/<exec-id>.patch"
```

**Retention.** Patches are load bearing now, so GC must not race
recovery. Applied patches are renamed to `*.patch.applied` at recovery
time and are the safe thing to age out; an un-consumed `*.patch` is
either awaiting a resume that has not happened yet or the residue of a
failed apply, and is the one thing a human may still need. Boothby's
recovery-patch GC (catalogue action #17) is scoped accordingly — see
`docs/designs/boothby.md`.

## Recovery cheat-sheet

```
# Inspect: is the execution still considered live?
bossctl agents list                 # in-memory live workers (empty on relaunch)
boss chore show <work-item-id>      # kanban + latest execution status

# Force the orphan reap if the engine missed it:
bossctl agents reap <run-id>

# Re-dispatch a fresh worker:
bossctl work start <work-item-id>   # picks up the orphan's workspace_id

# If the workspace itself is stuck (rare — cube usually self-heals
# via TTL after 30 min), see tools/cube/docs/remaining-work.md for
# `cube workspace force-release`.
```

## Why we don't release the lease

The default would be to release the cube lease whenever an execution
goes terminal — that's what `cancel_execution` does. The reaper path
deliberately doesn't, because:

- The workspace's filesystem usually has uncommitted work, partial
  branches, or open PRs the next worker should pick up. Releasing the
  lease lets cube hand the workspace to someone else (or auto-clean
  it), which makes the in-flight state harder to recover.
- Cube's lease TTL provides a safety net: orphaned leases that no
  worker is heartbeating expire on their own within 30 minutes.
- A human who _does_ want a clean slate can invoke `bossctl agents
stop <run-id>` first, then `agents reap` — the stop path is the
  documented way to release the lease deliberately.
