# Post-crash recovery: orphaned executions

Local workers live in engine-owned tmux sessions, so a Boss macOS app
force-quit or crash does **not** kill them: the app is only a viewer that
re-attaches on relaunch. A row becomes **orphaned** when the engine loses
the worker itself — the tmux session is gone or absent, its identity token
no longer matches, or an engine restart cannot re-adopt it — and the
`work_executions` row restored from sqlite has no live worker to reattach
to.

This doc describes how the engine detects orphans, how to recover from
them manually, and what state survives the cycle.

## What "orphaned" means

`orphaned` is a terminal status on the `work_executions` table,
alongside `completed` / `failed` / `cancelled` / `abandoned`. It
specifically denotes: _a worker was spawned for this execution, then
the engine lost the ability to verify it was still alive (typically
because the tmux session died, its token mismatched, or the engine could
not re-adopt it across a restart)._

Compared to the other terminal statuses:

- `completed` / `failed`: the worker finished its turn and the
  completion handler stamped the verdict.
- `cancelled`: a human invoked `bossctl executions cancel <execution>`
  (never-started rows only), `bossctl work cancel <execution>` (any
  non-terminal row), or `bossctl agents stop <agent>` (live worker).
  Operator cancellation is distinct from the engine losing a run.
- `abandoned`: the row never produced a `work_runs` entry (the engine
  crashed mid-dispatch, before the worker spawned).
- `orphaned`: the row had a live worker once; the engine no longer
  has signal that it does.

**Lease handling:** resume reads preserved work from the
`preserved/<execution>` bookmark in the shared jj store, not from the
originating workspace. After an orphan is recorded, the engine releases the cube lease
once the pane is confirmed gone and no process has its cwd inside the
workspace. Teardown preservation of the current workspace commit is best
effort and runs first; a failure is reported through a
`preserved_work_needs_inspection` item and never holds the lease. Inconclusive
probes keep the lease held; cube's 24-hour TTL does not prove work was preserved.
A release attempt atomically claims
`cube_lease_id` before calling cube; a genuine failure restores the claim.
Successful release, including an already-absent lease, clears the remaining
workspace metadata. The leftover sweep checks at most 100 recent local
executions per pass and does not infer teardown from terminal status alone.

**A successor waits for that release.** A finished run's unpublished work is
preserved as its lease is released, so while the lease is still recorded
dispatch cannot tell "no work to restore" from "work not yet preserved". A
`ready` execution whose recovery predecessor (the latest earlier execution of
the same work item and kind that ran) is terminal but still records a cube
lease is therefore held with the dispatch wait reason
`awaiting_predecessor_preservation`; it never starts from trunk in the
meantime. This applies to the drain and to `bossctl agents launch` alike, and
to a lookup that fails, including after inline liveness reconciliation. The
hold ends when the predecessor's lease is released (the cleared lease columns
are what dispatch reads). The leftover sweep also retries leases older than
cube's TTL. What keeps a
lease recorded is a pane or process still in the workspace, occupancy that
cannot be confirmed, or a cube failure; the leftover sweep keeps retrying and
the successor stays held behind it rather than abandoning the work. A
preservation failure does not hold the lease: the release goes ahead and a
`preserved_work_needs_inspection` attention item on the predecessor carries the
error and the lease id, so a successor that then starts without that work does
so with the loss on record.

Transient recovery confirms pane teardown, preserves the stalled run's work,
and releases its lease through the same inferred-death release the dead-pid,
stale-worker and spawn-ack sweeps use (for a remote run, through the owning
host's adapter) before inserting a successor. Unconfirmed teardown leaves the
execution live for reconciliation and retry. Once teardown is confirmed, one
transaction orphans the stalled run and inserts its successor (reusing an
existing successor if a concurrent reconciler created one), whether or not the
release completed. If the release was kept or refused, the orphaned
predecessor keeps its lease and dispatch holds the successor until the
responsible reconciler finishes preservation and release: the leftover-lease
sweep for a local run, and the remote lease reconcile for a remote run with a
recorded worker pid. A remote run with no recorded pid is beyond that
reconciler, so an incomplete release raises a `preserved_work_needs_inspection`
attention item on the predecessor rather than holding silently. If the run was
cancelled or otherwise settled while its work was being preserved, no successor
is created. The successor carries no workspace preference and leases any free
workspace; the work reaches it through the preserved tip.

## How orphans get detected

### Automatic detection at engine startup

`tools/boss/engine/src/app.rs::serve` runs a probe at startup over
every non-terminal `work_executions` row that carries a recorded
`cube_lease_id`. The probe (`crate::run_reconcile::probe_in_flight_runs`)
asks cube whether the lease is still bound to the same workspace and
not yet expired; the verdict is one of:

- `Live` — cube confirms the lease. The engine re-heartbeats it so the
  workspace survives the restart gap. If the row is `running` and no
  pane was ever registered (tmux adoption missed it: no durable tmux
  identity matched), startup recovery re-issues the pane spawn against
  the already-leased workspace instead of waiting for the 300 s
  never-attached reaper. Presence is decided from the durable tmux
  identity (`EnginePaneOracle`), not from the app. A recorded tmux
  identity with no adopted or live worker is `Absent` and permits the
  respawn. A missing or unreadable identity is `Undetermined`: the
  engine logs a loud `startup_pane_respawn` error and deliberately does
  not respawn. Those undetermined rows are saved and retried when an app
  session registers (`retry_startup_pane_reconcile`); a headless engine
  never retries them. The app is only the retry trigger there, never a
  presence oracle.
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

The automatic probe is bounded by cube's lease TTL (24 hours by
default — see `tools/cube/src/app/workspace.rs::DEFAULT_LEASE_TTL_SECS`,
and the engine's matching `LEASE_TTL_SECS` in
`tools/boss/engine/core/src/cube_lease_heartbeat.rs`). If the worker's
tmux session died recently, cube still reports the lease as `leased` and
the probe verdict is `Live`, even though the worker is gone.

**An app crash or relaunch alone is never a reason to reap.** The worker
keeps running in its tmux session, and `agents reap` tears that session
down (token-verified) and orphans the execution — killing a healthy,
still-working worker. Before reaping, confirm the tmux session is really
gone (`bossctl agents list` and the tmux inventory both show no live
worker for the run).

For that gap, the coordinator (Boss-only) can reap manually:

```
bossctl agents reap exec_18ad6336fedcb190_12
```

This bypasses the probe and transitions the execution straight to
`orphaned`. The same workspace-preservation rules apply: cube columns
are left intact so a re-dispatch can pick the same branch back up.

`agents reap` requires `RpcTier::BossOnly`. Worker panes cannot
invoke it (they shouldn't be reaping each other).

**Do not use `agents reap` to cancel a never-started (`ready` /
`queued`) execution.** That is not what the verb is for, and the
resulting `orphaned` status misrepresents an operator decision as
"engine lost the run." Use:

```
bossctl executions cancel <execution-id> --reason "…"
# or every never-started execution on a work item:
bossctl executions cancel --work-item <id> --reason "…"
```

`executions cancel` refuses rows that have already started and points
at `bossctl agents stop` instead.

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

## Review-guide attempts across a restart

A `pr_review_guide` execution carries a `pr_review_guide_attempts` row that
the orphan reap closes together with the execution (`failed`, reason
"execution orphaned"). Because `orphaned` is an inferred death, that closure
has to be as reversible as the status it was derived from:

- **Re-adoption reopens the attempt.** When a live worker disproves the
  inference (`execution re-adopted: inferred death disproven by a live
worker`), the same transaction puts the attempt back to `running` and the
  series back to `generating`, provided the attempt failed _after_ a worker
  ran, its comparison is still the series' selected one, nothing has been
  published for it and no other attempt is live for the series. The trace
  line names the reopened attempt in `reopened_review_guide_attempt`. A
  pre-start failure stays failed (the re-enqueue pass owns those).
- **The startup reconcile checks liveness before closing.** The pass that
  finishes attempts bound to terminal executions
  (`review_guide_attempt_reconcile`) runs after the tmux adoption pass and
  treats an `orphaned` execution as live when adoption observed its pane
  (adopted, or handed to re-adoption) or when its durable shell pid is
  alive. Such an attempt is logged `action = "kept_open"` and left for
  re-adoption; every attempt it does finish, fail or dispatch is logged with
  `attempt_id`, `execution_id`, `reason` and `action`. A failed _read_ of
  the execution leaves the attempt untouched rather than failing it. The
  periodic terminal-work sweep applies the same pid probe.
- **A guide submitted against a closed attempt stores nothing.**
  `boss propose review-guide` is refused with `not_accepted` (exit 6) before
  any `worker_proposals` row is written, so the worker's retry after
  re-adoption is a fresh submission rather than a replay of the refusal.
- **Guides older engines stored as refused are recovered on startup.**
  `review_guide_submission_recovery` finds `rejected` review-guide
  proposals whose reason was "this execution has no running review-guide
  attempt", validates the stored body against the recorded comparison at
  the execution's workspace, reopens the attempt under the fences above,
  applies the proposal against it and publishes. A body that no longer
  validates (for example because the workspace is gone) is logged and left
  untouched; regenerate that PR's guide by hand.

The restart-during-spawn shape — a guide run spawned seconds before an
engine restart, orphaned by the restarted engine's recovery probe, re-adopted
on the worker's next hook, and submitting its guide minutes later — is
covered by `restart_during_spawn_reopens_the_attempt_with_the_readopted_execution_and_publishes_the_guide`
in `tools/boss/engine/work-db/src/review_guide_jobs_tests.rs`.

## Workspace patch backup (retired)

The engine no longer exports a patch when it records a death. A worker's
unpublished work is preserved once, at teardown, under the
`preserved/<execution>` bookmark in the shared jj store (see
[execution-recovery-pointers.md](execution-recovery-pointers.md)), which is
what resume restages from. The death-detection hooks (`dead_pid_sweep`,
`dead_pane_sweep`, `lost_workspace_sweep`, `stale_worker_sweep`,
`spawn_ack_sweep` and the startup reaper) write no patch, and their
`[engine-reconcile]` audit lines and dispatch events carry no `recovery_patch`.

Patches written by earlier engines stay in the engine's `recovery/` directory
(`<exec-id>.patch`, overridable with `BOSS_RECOVERY_DIR`) and are still
replayed, and retained, by the section below.

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
bossctl agents list                 # live workers (tmux-backed; survive app relaunch)
boss chore show <work-item-id>      # kanban + latest execution status

# Force the orphan reap if the engine missed it (ONLY once the tmux
# session is confirmed gone; never just because the app crashed):
bossctl agents reap <run-id>

# Re-dispatch a fresh worker:
bossctl work start <work-item-id>   # picks up the orphan's workspace_id

# If the workspace itself is stuck (rare — cube usually self-heals
# via TTL after 24h), see tools/cube/docs/remaining-work.md for
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
  worker is heartbeating expire on their own within 24 hours.
- A human who _does_ want a clean slate can invoke `bossctl agents
stop <run-id>` first, then `agents reap` — the stop path is the
  documented way to release the lease deliberately.
