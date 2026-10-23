# Cube leases are a cache hint: Boss owns workspace occupancy, and the engine stops retaining, reaping and heartbeating leases

- **Status:** proposed (design only; no code in this PR)
- **Date:** 2026-10-14
- **Provenance:** design run for the project "Remove unnecessary cube lease machinery from the engine". Implements the investigation in [`tools/boss/docs/investigations/cube-lease-purpose-and-boss-simplification.md`](https://github.com/spinyfin/mono/blob/main/tools/boss/docs/investigations/cube-lease-purpose-and-boss-simplification.md) ([mono#3168](https://github.com/spinyfin/mono/pull/3168), merged). The investigation is not redone here; this doc confirms or corrects its end state against current `main`, settles the two open choices, and lists the task breakdown.
- **Source revision for citations:** `714516342c8822a55d07a153cf082001be88ad1e` (current `main@origin`). Every file cited below is byte-identical between the investigation's revision and this one; the only commits in between are doc-only or unrelated. `core/` means `tools/boss/engine/core/src/`; `cube/` means `tools/cube/src/`; `proto/` means `tools/boss/protocol/src/`.
- **Incidents this doc reasons about:** [mono#1546](https://github.com/spinyfin/mono/pull/1546) (phantom-free pool starvation), [mono#1710](https://github.com/spinyfin/mono/pull/1710) (waiting_human zombie), [mono#1747](https://github.com/spinyfin/mono/pull/1747) (heartbeat auto-reap), [mono#1763](https://github.com/spinyfin/mono/pull/1763) (remote lease reconcile), [mono#2134](https://github.com/spinyfin/mono/pull/2134) (orphaned ladder rung lease), [mono#2219](https://github.com/spinyfin/mono/pull/2219) / [mono#2418](https://github.com/spinyfin/mono/pull/2418) (prompt reclamation), [mono#2500](https://github.com/spinyfin/mono/pull/2500) (disk fill from renewing dead rows), commit `bc5558bb12` (2026-05-07 duplicate dispatch), the 2026-05-12 "`@` re-pointed mid-flight" chore, the 2026-07-03 and 2026-07-18 `anaplian` / `flunge-agent-035` incidents, and the 388 s teardown stall.

A cube lease buys a worker a workspace whose Bazel output base is already warm. It does not preserve work and it does not prove a worker is alive. This design removes every engine mechanism whose only justification was one of those two beliefs, and puts the one property that was actually load-bearing, "two Boss agents never share a working copy", where it belongs: in Boss, checked before cube is asked for anything.

## TL;DR

The investigation's end state holds at current `main` with two corrections and three sharpenings (see "Confirmation against main"). The ordering gap it names is real and is the critical path: today the engine leases first and checks occupancy second, after cube has already run `jj new main@origin` in the workspace, and its only memory of a refusal is an in-process map. The first task therefore introduces a durable, host-keyed occupancy inventory consulted before allocation and fed to cube's `--exclude`.

On the open heartbeat choice, this doc recommends **keeping one minimal live-owner refresh** as the final, small PR. The evidence is in "Live-worker heartbeat: the two options"; the decision is also surfaced as a question in the sibling attentions manifest so the owner can choose the other option without reopening the plan. Every other heartbeat population, the breaker, auto-reap, lease-derived startup verdicts, waiting-state retention, terminal-remote refresh, offline-host lease I/O, the ladder lease trio, `HeartbeatGuard`, the duplicate allocation attempt, synchronous release tails, and the inert recovery preferences are removed outright.

Eleven PR-sized tasks, all in scope, with the heartbeat decision last.

## Goals

- Boss records durable, host-keyed workspace occupancy and checks it **before** asking cube for a workspace; every workspace Boss still occupies on that host and repo is passed to cube's `--exclude`.
- Leases are clean scratch. Dispatch, startup respawn and recovery all go through fresh allocation and restore from the engine's `boss-recovery/` and publication bookmarks; nothing revives an old `CubeWorkspaceLease`.
- At most one optional, bounded, best-effort refresh for live owners, routed to the owning host's cube, with no verdicts, retries, breaker or reap attached to it.
- When a run ends: stop the worker, tear down the driver, then make one asynchronous, bounded release attempt off the critical path. Cube's TTL covers anything missed.
- No lease retention for terminal or waiting-review/merge rows; no lease-derived liveness or death verdicts; no reap or breaker machinery; no ladder lease registry, reap or short-TTL heartbeat; no terminal-remote heartbeat; no offline-host lease I/O; no inert `prefer` / `allow_dirty` recovery preferences.
- Keep: `--release-on-setup-failure`, the per-PR chain single-writer guard, tmux adoption and local quarantine, pane-then-driver-then-cube teardown ordering, conflict-ladder and review scratch operations, and the post-allocation occupancy guard. Lease columns stay as provenance, never as obligations. No destructive schema migration.

## Non-goals

- **Changing cube.** Every deliverable lands in `tools/boss/`. Cube-side observations are recorded in "Cube-side notes" and nowhere else.
- **Dropping or rewriting lease and preference columns.** `cube_lease_id`, `cube_workspace_id`, `workspace_path`, `preferred_workspace_id`, `prefer_is_soft` and `allow_dirty` stay in the schema and in `WorkExecution`. This design stops producing and acting on the preference fields; it does not migrate them away.
- **Cross-host transfer of unpushed work.** Recovery still requires the same shared jj store ([core/coordinator/blocked_workspace.rs:41–47](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/blocked_workspace.rs#L41-L47)). Lease retention never substituted for this and its absence is not introduced here.
- **The bookmark reference-integrity wedges** (baseline after rebase, diverged pointers, missing baseline) and the read-RPC admission timeouts. Separate investigations.
- **Weakening any process-supervision sweep.** `dead_pid_sweep`, `dead_pane_sweep`, `stale_worker_sweep`, `lost_workspace_sweep`, `spawn_ack_sweep`, `terminal_work_sweep`, `remote_lease_reconcile`'s pid probing and `host_reconcile`'s offline drain all keep their worker-state transitions. Only their cube release tails change.
- **Fencing non-Boss cube callers.** A human running `cube workspace lease` on the same host is outside the single-occupancy guarantee this design makes, exactly as it is today.

## Confirmation against main

The investigation's end state is confirmed, with the following corrections and sharpenings. Each is a finding, not a restatement.

**Correction 1: the recovery preferences are not fully inert.** The investigation calls `preferred_workspace_id` / `allow_dirty` "stale policy remnants that allocation ignores". Allocation does ignore them: ordinary dispatch hard-codes `prefer = None, allow_dirty = false` ([core/coordinator/execution.rs:2092–2093](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/execution.rs#L2092-L2093)). But `preferred_workspace_id` has a second consumer: pool slot selection. `claim_worker` passes it to `select_claim_index` and the spill policy prefers an idle slot whose `last_workspace_id` matches ([core/coordinator.rs:1231–1239](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator.rs#L1231-L1239), [core/dispatch_spillover.rs:86–108](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/dispatch_spillover.rs#L86-L108), [core/coordinator/scheduler.rs:407–421](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/scheduler.rs#L407-L421)). That affinity only ever pays off if the execution lands in the preferred cube workspace, which allocation no longer attempts, so the slot affinity is dead logic with a live input. The preference-removal task deletes both the producers and this consumer.

**Correction 2: the ladder lease registry has a UI consumer.** `background_work.rs` uses `ladder_lease_registry::snapshot()` as half of its eligibility test for showing an in-flight mechanical rung in the toolbar badge ([core/background_work.rs:14–20](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/background_work.rs#L14-L20)). Deleting the registry outright would silently drop that signal. The ladder task replaces the lease-keyed registry with a rung-in-flight registry keyed by attempt id that carries no cube obligation, and points `background_work` at it.

**Sharpening 1: the occupancy gap is wider than ordering.** The guard runs after `lease_workspace_with_fallback` returns ([core/coordinator/execution.rs:1245](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/execution.rs#L1245) then [1296–1384](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/execution.rs#L1296-L1384)), by which point cube has reset the workspace with `jj new <main>@<remote>` ([cube/app/reset.rs:205–244](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/reset.rs#L205-L244)). Three further weaknesses: the refusal memory is an in-process `Mutex<HashMap>` keyed per execution ([core/coordinator.rs:2284](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator.rs#L2284)), so it is empty after a restart and never shared across executions; the guard only sees occupants with a live tracked pid in the in-memory registry ([core/coordinator.rs:1819–1832](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator.rs#L1819-L1832)), which is also empty after a restart; and a refusal hands the workspace straight back with `release_workspace` ([core/coordinator/execution.rs:1360](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/execution.rs#L1360)), which can reset it a second time under the live occupant. The occupancy inventory below closes all four.

**Sharpening 2: `waiting_human` is a live-worker state and keeps its workspace.** `ExecutionStatus::is_live()` is `Running | WaitingHuman` ([proto/types/execution.rs:221–223](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/protocol/src/types/execution.rs#L221-L223)); a worker parked on a human question still has its pane. The retention this design removes is only for `WaitingReview` and `WaitingMerge`, whose worker has opened its PR and exited by design ([core/runner/mod.rs:80–81, 126–131](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/runner/mod.rs#L80-L131)). The investigation's phrase "waiting states" must be read with that boundary.

**Sharpening 3: what "never lost work" means, by dimension.** When a lease expires under a live worker and cube later acts on the workspace, three things can happen: a non-Boss caller leases it and cube resets it; cube's GC "aged-unhealthy recycler" salvages and resets a free dirty workspace past `max_age_secs`; or, once clean, the trim pass removes it ([cube/app/gc.rs:448–452, 499–505, 736–744](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/gc.rs#L448-L505)). In every case the snapshotted, bookmark-reachable history in the shared store survives (and the recycler writes a salvage record before resetting). What does not survive is unsnapshotted edits since the worker's last `jj status`, which the worker contract requires it to checkpoint ([core/execution_bookmark_recovery.rs:14–39](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/execution_bookmark_recovery.rs#L14-L39)). The owner's frame, "a slowdown and possible confusion, never lost work", holds on the dimension of snapshotted history; it does not hold on the dimension of unsnapshotted bytes in a working copy, and this design does not claim it does. The occupancy inventory makes the Boss-caused reuse impossible; the heartbeat option below is about the non-Boss and GC cases only.

**Silence noted.** Of the nineteen inventory rows, eleven have no recorded originating incident ("not established" in the investigation). The code comments for those rows assert a purpose but cite nothing. This design treats that as "the decision to add the mechanism was never argued", not as "the argument was lost", and removes them on the strength of the frame alone.

## Alternatives considered

**1. Keep the machinery; fix only the two visible bugs (the terminal-remote SSH loop and the guard ordering).** Rejected. The SSH loop to `anaplian` is one instance of a structural defect: the heartbeat's terminal-remote population has no host-enabled predicate and never records failures, so nothing retires it ([core/cube_lease_heartbeat.rs:895–937](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/cube_lease_heartbeat.rs#L895-L937)). The same structure (a lease is an obligation; an obligation must be retried until cube confirms) also produced the 1,609 "orphaned" lease ids the UI still maps, the `HeldForRemoteWorker` retention in `force_release`, and the disk-fill from renewing dead rows (mono#2500). Fixing instances leaves the generator in place, and the next agent to touch this code will reason from the surviving comments.

**2. Make cube the liveness oracle (pass pids to cube; have cube refuse to expire a lease with a live holder; have cube fence the directory).** Rejected. It requires cube changes, which are out of scope. More importantly, the engine already decided cube is not a sufficient oracle: tmux adoption bypasses the cube probe entirely and local quarantine overrides its `Dead` verdicts with `Unknown` ([core/app/server.rs:1398–1447](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/app/server.rs#L1398-L1447)), and cube's own docs assign liveness to callers ([tools/cube/docs/main.md:580–583](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/docs/main.md#L580-L583)). A rejection on these grounds does not disqualify the existing adoption and quarantine paths; it is what they already practice.

**3. Rely on TTL alone and keep only the post-allocation `occupying_live_worker` check.** Rejected. This is the current design minus heartbeats, and it keeps the ordering gap: cube resets the workspace before Boss looks ([cube/app/reset.rs:205–244](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/reset.rs#L205-L244)). The frame's "Boss must enforce it itself" is a requirement on the first mutation, not on the spawn, so a check that runs after the reset satisfies the letter and breaks the property.

**4. Express occupancy as a predicate over existing columns instead of a new table.** Considered seriously, because the investigation prefers no migration "if existing durable run identity can represent the reservation". It cannot, cleanly: `mark_execution_orphaned` deliberately keeps lease and workspace columns on terminal rows as provenance ([core/work/executions_runs.rs:490–497](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/work/executions_runs.rs#L490-L497)), so "row carries a workspace id" cannot mean "occupied" without re-deriving death for every orphaned row on every allocation. The brief also asks that lease columns be provenance, not obligations; using them as the occupancy source would make them obligations again. A small additive table with an explicit release stamp is the honest representation.

**5. Shorten the TTL instead of heartbeating.** Rejected. It changes a cube-visible default (out of scope), makes expiry common rather than rare, and only moves the exposure window; it does not remove the need for Boss-side occupancy.

## Chosen approach

### Invariant, stated at the load-bearing level

**No two Boss-dispatched processes may mutate the same working copy, on any host, at any point from allocation to proven exit.** The container that usually carries this is a cube lease; the property is about the working copy and the processes, so it is enforced by Boss on `(host_id, workspace_id)` and is independent of lease validity, lease columns, and cube's expiry sweep.

### Occupancy inventory (ownership before allocation)

A new additive table, `workspace_occupancy(host_id, repo_id, workspace_id, execution_id, kind, acquired_at, released_at)`, with a forward migration and a unique index on `(host_id, workspace_id) WHERE released_at IS NULL`. `kind` is `worker` for dispatched executions. In-process scratch operations (conflict-ladder rungs, review scratch, handler scratch) do **not** register: their lifetime is one async call, a crash ends the only process that could write there, and cube will not hand out a leased workspace, so the TTL is the correct cleanup for them.

Allocation becomes, under a per-host async mutex in the coordinator:

1. Read every unreleased `workspace_id` for `(host_id, repo_id)`.
2. Call cube with all of them in `--exclude` (cube skips excluded ids at candidate selection: [cube/app/workspace.rs:579–594](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/workspace.rs#L579-L594); the CLI accepts repeats: [cube/src/cli.rs:116–121](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/cli.rs#L116-L121)). Cube provisions a new workspace when none is free, so exclusions never starve allocation.
3. Insert the occupancy row for the returned workspace, before spawn.
4. Run the existing `occupying_live_worker` check as defense in depth. If it trips (an inventory gap), do **not** call `release_workspace`: a release can reset the live occupant. Record the occupancy under the occupant's execution and fail the start as today; cube's TTL reclaims the stray lease.

Release points are the ones that already exist, not new ones: `clear_execution_workspace` is the single DB chokepoint every terminal path reaches ([core/work/exec_tail.rs:141–169](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/work/exec_tail.rs#L141-L169)), so it stamps `released_at`. For a remote worker whose exit is unproven, the occupancy stays open until `remote_lease_reconcile`'s pid probe proves death; for an offline or removed host, `host_reconcile`'s drain releases the occupancy without any cube I/O.

At startup, after tmux adoption and local quarantine, open occupancies whose execution is terminal and whose worker is proven gone are released; everything else stays open. An open occupancy only ever excludes one workspace, which costs nothing because cube provisions on demand, so a conservative inventory is safe.

### Clean-scratch allocation and recovery

`lease_workspace_with_fallback` keeps one attempt, with exclusions from the inventory, and the `--release-on-setup-failure` flag ([core/cube_commands.rs:235–246](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/cube_commands.rs#L235-L246)). The second, now-identical attempt and the `(prefer, allow_dirty)` plumbing through `CubeClient::lease_workspace` and both host adapters are deleted. Recovery stays what it already is: `recover_execution_bookmark` restores the predecessor's `boss-recovery/` reference into the fresh workspace and never reads the old one ([core/coordinator/blocked_workspace.rs:21–47](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/blocked_workspace.rs#L21-L47)).

Startup respawn of a `running` execution whose pane was never issued ([core/startup_pane_reconcile.rs:321–328](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/startup_pane_reconcile.rs#L321-L328)) stops reconstructing a `CubeWorkspaceLease` from the row ([core/coordinator/run.rs:1108–1219](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator/run.rs#L1108-L1219)). An `Absent` verdict means no worker process ever existed in that workspace, so the engine releases the recorded lease once (bounded), releases the occupancy, and requeues the execution through ordinary transient recovery, which allocates fresh and restores from refs. `Present` and `Undetermined` behave exactly as today.

### Startup reconciliation without cube verdicts

`run_reconcile::probe_in_flight_runs` derives `Live` / `Dead` / `Unknown` from cube lease state ([core/run_reconcile.rs:1–61](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/run_reconcile.rs#L1-L61)) and `reheartbeat_live_runs` renews whatever it called `Live` ([core/app/server.rs:1455–1474](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/app/server.rs#L1455-L1474)). Both go. The skip set fed to `reconcile_active_dispatch` ([core/app/server.rs:1545–1547](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/app/server.rs#L1545-L1547)) is built from process evidence only: tmux-adopted executions, quarantine-protected executions, local rows with a durable pid that probes alive or inconclusive, remote rows whose pid probe is alive or inconclusive, and rows with no evidence at all. Only a positive process-death verdict (local `ESRCH`, remote `kill -0` false, tmux identity confirmed dead) lets a row be orphaned and redispatched. This is the rule the quarantine override already imposes on top of the cube probe; the design makes it the only rule.

### Teardown and release

The order stays pane, then driver, then cube ([core/completion/teardown.rs:156–227](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/completion/teardown.rs#L156-L227)). The cube step moves off the critical path: after driver teardown completes, the handler spawns one detached, bounded release attempt whose bound covers adapter resolution as well as the cube call (today's 120 s `CUBE_RELEASE_TIMEOUT` is the starting value; it becomes a `cube_op_budget` constant). The attempt runs only when the worker is stopped or proven gone; when exit is unproven (a cancelled remote worker, a mid-spawn cancel) no release is attempted and cube's TTL reclaims the lease. In every case the actionable lease handle is cleared once; nothing retries, and no sweep re-selects a row because its lease column is still set.

`force_release` keeps its two ordering guards (`remote_worker_proven_gone`, `NoLiveWorker` for mid-spawn) as the conditions for **calling** cube, because a release resets the directory. It stops using them to retain lease columns, keep heartbeats alive, or hold the row for a retrying reconciler ([core/completion/release.rs:26–88](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/completion/release.rs#L26-L88)).

All twenty-odd release tails in sweeps, coordinator failure paths and handler helpers (enumerated in the task entries) call the same one-shot helper. `host_reconcile`'s offline drain stops calling cube at all ([core/host_reconcile.rs:285–298](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/host_reconcile.rs#L285-L298)).

### Waiting-review and waiting-merge rows

`RunWaitState::release_workspace()` returns `true` for `WaitingReview` and `WaitingMerge` ([core/runner/mod.rs:126–131](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/runner/mod.rs#L126-L131)). A later revision allocates fresh and positions on the bound PR's base from refs, which is already how `recover_execution_bookmark` works. `WaitingHuman` and `WorkerPaneAlive` keep their workspace because their worker is alive. The sweeps' `is_live()` gates stay as they are; their comments change from "the workspace is retained for a follow-up" to "no worker is expected here".

### Live-worker heartbeat: the two options

**What the refresh would be.** After the removals in this plan, what is left of `cube_lease_heartbeat.rs` (2,969 lines today, tests from line 1,507) is: snapshot the live-worker registry; for each slot with a live pid, non-terminal activity, a non-terminal execution, a recorded lease id and an enabled host, route to the owning cube through the existing `HostRoutedCubes` seam ([core/cube_lease_heartbeat.rs:216–300](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/cube_lease_heartbeat.rs#L216-L300)) and make one bounded `cube workspace heartbeat` call every 300 s. No DB scan, no breaker, no verdict, no reap, no event beyond the existing timeline stage. Roughly 150 lines plus tests.

**Evidence on how long real workers run** (operator-supplied aggregates from the investigation; not re-queried here): 7,524 executions with start and finish stamps in 60 days; 74 over six hours, 45 over twelve, 13 over twenty-four, maximum 47.2 h. The long rows are mostly cancelled reviews near 36 h, one completed review at 37.1 h, and one cancelled revision at 47.2 h. Row-level activity is unknown, so these cannot be classified as busy, idle or stranded. What the code establishes is that nothing caps a worker: the escalation-pending hold is unbounded, and `stale_worker_sweep` reaps only an idle `Rich`-driver worker past two hours of no checkpoint, never a busy one ([core/stale_worker_sweep.rs:481–499](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/stale_worker_sweep.rs#L481-L499)). A live worker past 24 h is therefore rare (about 1 in 600 executions crosses it) but permitted.

**What happens at expiry once ownership-before-allocation exists.** Cube marks the workspace free lazily, on its next lease call from any caller ([cube/app/workspace.rs:240–260](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/workspace.rs#L240-L260)). Boss's inventory still lists the workspace as occupied, so every Boss allocation excludes it and no second Boss worker ever lands there. The worker is unaffected until a non-Boss caller leases the workspace or cube's GC recycles it (see Sharpening 3). If that happens, the worker's `@` moves or its files vanish, its run fails, and Boss redispatches from refs with a cold Bazel base.

**Option A, keep the minimal refresh (recommended).** Cost: ~150 lines, one bounded cube call per live owner per five minutes, and the module continues to exist as a place future agents might re-grow policy. Benefit: closes the non-Boss-caller and GC exposures for the live owners Boss knows about, removes the dependence on an unproven 24 h bound, and keeps remote affinity through the existing routing.

**Option B, drop heartbeats entirely.** Cost: the exposures above for roughly one execution in six hundred, bounded to unsnapshotted edits since the last checkpoint and a cold rebuild. Benefit: no heartbeat code at all, and a cleaner statement of the frame ("the lease is a hint; Boss never maintains it").

**Recommendation: Option A.** The deciding fact is that the refresh's cost is almost entirely the cost of the file existing, while the exposure it closes is on real workers (not rows) that Boss can enumerate precisely. The frame's acceptance of "slowdown and confusion" is honest, but it is not a reason to decline a 150-line mechanism that avoids the slowdown. The plan is structured so that either answer is the final, small PR: by then the module is already stripped to the refresh, and the last task either renames and documents it or deletes it.

### Attention and event policy

Keep: `remote_worker_died`, `cube_workspace_occupied` start failures, reference-integrity failures. Delete with their policy: the heartbeat auto-reap event and stage, the breaker's failure-streak events, the `prefer_workspace_id` / `allow_dirty` / `fallback_from_prefer` fields on `CubeWorkspaceLease*` dispatch events, and the `lease_reheartbeat` startup mark. A failed refresh or release is a `debug`-level diagnostic, never an attention item.

### Documentation that changes with the premise

In the same PRs that change the code: the `cube_lease_heartbeat.rs` module doc and the server's stale "~30 min" comment ([core/app/server.rs:1700–1716](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/app/server.rs#L1700-L1716)); `post-crash-recovery.md`'s "orphaned rows keep their lease for resume" and "startup probe asks cube" sections ([tools/boss/docs/post-crash-recovery.md:36–75](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/docs/post-crash-recovery.md#L36-L75)); and `worker-liveness-contract.md`'s note that `NoLiveWorker` exists to avoid releasing leases ([tools/boss/docs/worker-liveness-contract.md:135](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/docs/worker-liveness-contract.md#L135)). Tests that pin the removed policy are deleted in the same PR as the policy; tests that pin the retained property (occupancy refusal, pane-before-driver order, no reap without process proof) are kept and extended.

## Cube-side notes

Observations only. No cube change is part of this project.

- Cube's TTL comment says the long default exists to protect dead-but-unreaped holders for same-workspace resume and relies on Boss auto-reap ([cube/app/workspace.rs:52–74](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/workspace.rs#L52-L74)). After this project that rationale is stale; the 24 h default remains a reasonable performance choice on its own.
- Cube's README and `docs/main.md` describe Boss as the heartbeat and release owner ([tools/cube/docs/main.md:580–583](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/docs/main.md#L580-L583)). If Option B is chosen, that text becomes inaccurate.
- `--prefer --allow-dirty` remains a valid route for other cube callers; Boss simply stops using it.
- Lease-time GC can run synchronously inside `cube workspace lease` ([cube/app/gc.rs:330–350](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/cube/src/app/gc.rs#L330-L350)). The engine's 90 s lease budget already accounts for this; the occupancy inventory adds no cube work.
- Whether cube's lease-time quarantine-reclaim scan can touch an `--exclude`d workspace was not established from the Boss side. The first task's tests assert the argv, not cube's internals; if the scan does touch excluded ids, that is a cube finding to file separately.

## Risks / open questions

- **Heartbeat decision.** Recommended Option A; the owner may choose Option B. Surfaced in the attentions manifest. Either way the final task is small.
- **Non-Boss callers and cube GC** can still act on a TTL-expired workspace with a live Boss worker (Option B) or on a worker Boss has lost track of (both options). Snapshotted history survives; unsnapshotted edits since the last `jj status` do not. This is accepted under the owner's frame and stated here so nobody later reads "never lost work" as covering the working copy.
- **Inventory leaks.** A crash between cube returning a lease and the occupancy insert orphans the lease to TTL with no row (acceptable). A crash after the insert and before spawn leaves an open row whose execution has no process; the startup pass releases it on positive death evidence and otherwise leaves one workspace excluded. Open rows on removed hosts are released by the host drain. There is no path that excludes a workspace forever on a live host without an unresolved execution behind it.
- **Two engine installs sharing one cube pool.** The inventory is engine-local. `ladder_lease_reap` already assumed this case exists for rung leases. Nothing in this design makes it worse than today, and nothing fixes it; it is out of scope.
- **Blocking runners lose `HeartbeatGuard`.** The guard only ever covered in-process runners that block until the run completes; production pane workers were never covered by it ([core/coordinator.rs:392–499](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/coordinator.rs#L392-L499)). The remaining consumers are test fakes.
- **Size of the preference-removal task.** `preferred_workspace_id` is a `WorkExecution` field referenced in some fifty files as a struct literal or builder call. The task removes producers, the slot-affinity consumer and the tests that pin them, not the field; it may still be near the line cap and should be reviewed for that.
- **The thirteen executions over 24 h** cannot be classified from aggregates. If the owner wants row-level evidence before choosing between the heartbeat options, that is a separate read-only query against production, which this design does not propose.

## Proposed implementation task breakdown

Breakdown size: 11 entries (11 in-scope, 0 deferred) — the change introduces one new durable unit (occupancy inventory plus migration), must shrink a 2,969-line heartbeat module in three separately reviewable cuts (terminal-remote population, DB fallback plus waiting retention, breaker plus auto-reap), replaces the lease-derived startup path, deletes the allocation-API preference plumbing with the per-run guard, removes the three-module ladder trio against a UI consumer, consolidates release tails in two file-disjoint sweeps (completion and dispatch, then sweeps and helpers), deletes the preference producers, and ends with the small heartbeat decision; each of those is a seam with its own tests and callers.

Parallelism at each depth is noted per entry. Three entries co-edit `cube_lease_heartbeat.rs` and are serialized; everything else overlaps only incidentally.

### Occupancy inventory and ownership before allocation

Scope: add the `workspace_occupancy` table (forward, additive migration; `schema_init` snapshot updated), a `workspace_occupancy` module in `work/` with `open_for_host_repo`, `acquire`, `release`, and `release_proven_gone_at_startup`; a per-host allocation mutex in the coordinator; `lease_workspace_with_fallback` reduced to one attempt that passes every open occupancy for `(host, repo)` as `--exclude` and registers the returned workspace before spawn; the post-allocation `occupying_live_worker` check retained, with its refusal path no longer calling `release_workspace` and no longer writing the in-process `refused_workspaces` map, which is deleted; `clear_execution_workspace` stamps `released_at`; startup releases open rows only on positive process-death evidence. Incident: mono#1546 and the 2026-05-07 duplicate dispatch (`bc5558bb12`), both of which were a second Boss worker landing in an occupied workspace. Why it does not recur: the second worker is excluded at allocation, before cube can reset anything, and the exclusion survives restarts because it is durable.

Deliverables:

- Migration and `work/workspace_occupancy.rs` with unit tests for acquire, release, unique-open-per-workspace, host keying, and startup release on proven death only.
- `coordinator/execution.rs`: single allocation attempt; `--exclude` carries every open occupancy; occupancy inserted before spawn; refusal does not release; `refused_workspaces` and its `config.rs` initializer deleted.
- `clear_execution_workspace` releases occupancy in the same transaction.
- `coordinator_tests/unit.rs::occupying_live_worker_blocks_only_a_live_tracked_occupant` extended; new dispatch tests for: unrelated executions sharing a candidate, concurrent pre-spawn claims on one host, remote host names, restart with an open row, and "cube argv never omits an occupied id".
- `cube_commands.rs` argv tests updated for the exclusion list.

Effort: large. Dependencies: none. Scope: in-scope

### Remove the terminal-remote heartbeat and lease retry obligations

Scope: delete the terminal-remote population from `run_one_pass` ([core/cube_lease_heartbeat.rs:895–937](https://github.com/spinyfin/mono/blob/714516342c8822a55d07a153cf082001be88ad1e/tools/boss/engine/core/src/cube_lease_heartbeat.rs#L895-L937)) and the `retained_remote_tests`; drop the `cube_lease_id IS NOT NULL AND remote_pid IS NOT NULL` arm from `terminal_remote_cleanup_runs` so the query selects on persona state only; in `release_terminal_remote_resources`, attempt the cube release once on positive death and clear the actionable handle regardless of the outcome (no "will retry"); in `force_release`, stop retaining lease columns for `HeldForRemoteWorker` (the occupancy row from the previous entry is what keeps the workspace excluded until the pid probe proves death). Incident: the 2026-07-03 `anaplian` loop (six orphaned executions, 1,559 failures, warnings every 5.5 minutes). Why it does not recur: terminal rows are never refresh targets, there is no lease-conditioned candidate set, and no retry obligation exists to re-select a disabled host.

Deliverables:

- Heartbeat module no longer queries `terminal_remote_cleanup_runs`; the query's lease predicate is removed; `work/run_rows.rs` tests updated.
- `remote_lease_reconcile::release_terminal_remote_resources` makes one attempt and clears the handle; its tests in `remote_lease_reconcile/tests.rs` (including the retained-resource cases) replaced with: a disabled or unresolvable host with terminal rows produces zero cube SSH attempts across repeated passes while the occupancy row stays open.
- `force_release` returns `HeldForRemoteWorker` without retaining the lease columns; `completion/tests` updated.

Effort: medium. Dependencies: Occupancy inventory and ownership before allocation. Scope: in-scope

### Remove the DB-fallback heartbeat and waiting-review/merge retention

Scope: delete `db_fallback_death_evidence`, the DB-fallback scan in `run_one_pass_impl`, and `HeartbeatSweepOutcome::db_fallback_heartbeated`; make `RunWaitState::release_workspace()` true for `WaitingReview` and `WaitingMerge` so the run-completion path tears down the driver and releases; delete the `preserve_execution_workspace_preference` calls in `merge_poller/sweep.rs` and `pr_flow.rs` that exist only to re-target a retained checkout (the helper itself goes with the preference producers later). Incident: mono#2500 (disk fill from renewing dead rows) and the waiting-state retention rows with no recorded origin. Why it does not recur: nothing renews a row that is not a live registry owner, so a dead row cannot be kept alive; a revision after review allocates fresh and restores from refs, which is already the only path allocation supports.

Deliverables:

- `cube_lease_heartbeat.rs` DB-fallback code and its tests (the fallback and retention cases) deleted; replacement tests assert "no registry owner, no refresh" for `waiting_review`, `waiting_merge`, `queued` and terminal rows.
- `runner/mod.rs` predicate change with a `coordinator/run.rs` test that a `WaitingReview` outcome runs driver teardown then release.
- A revision-after-review test in `coordinator_tests/recovery.rs` showing fresh allocation plus bookmark restore with no workspace preference.
- Sweep comments that cite retention for follow-up revisions rewritten to "no worker expected".

Effort: medium. Dependencies: Remove the terminal-remote heartbeat and lease retry obligations (same file; forward-port its deletions). Scope: in-scope

### Remove the heartbeat breaker and auto-reap; consolidate the one refresh

Scope: delete `HeartbeatFailureBreaker`, `AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`, `record_heartbeat_result`, `auto_reap_dead_lease`, `reap_and_release_lease`, the `confirm_execution_dead` call and the auto-reap dispatch stage and attention; keep `run_one_pass` as the registry-driven refresh with one whole-operation bound covering `ExecutionCubes` resolution and the cube call, skipping executions whose bound host is disabled; keep the behavioural intent of `hung_heartbeat_does_not_block_other_slots`; rewrite the module doc and the server's "~30 min" comment. Incident: mono#1747 (lease errors as death evidence) and the second half of the 2026-07-03 `anaplian` incident. Why it does not recur: cube errors are affinity diagnostics; execution state is only ever changed by the process supervisors, which this entry does not touch; a disabled host is skipped by predicate rather than retired by failure count.

Deliverables:

- Breaker, reap, and verdict code deleted from `cube_lease_heartbeat.rs`; the auto-reap policy tests replaced with "cube outage does not terminalize", "dead-owner slot is skipped", "disabled host is skipped", remote routing, and timeout coverage.
- `run_reconcile::confirm_execution_dead` has no remaining caller and is deleted here.
- Module doc and `app/server.rs` heartbeat comment rewritten to the cache-affinity frame.

Effort: medium. Dependencies: Remove the DB-fallback heartbeat and waiting-review/merge retention (same file). Scope: in-scope

### Startup reconciliation from process evidence; respawn through fresh allocation

Scope: delete `run_reconcile::probe_in_flight_runs`, `classify` and the `CubeClient` workspace-list dependency; delete `reheartbeat_live_runs` and the `lease_reheartbeat` startup mark; build the `reconcile_active_dispatch` skip set and `reconcile_unspawned_running_panes` verdicts from tmux adoption, local quarantine, durable-pid probes and remote pid probes only; replace `resume_pane_spawn_for_running_execution` so an `Absent` pane releases the recorded lease once, releases the occupancy, and requeues through ordinary transient recovery; update `post-crash-recovery.md`. Incident: the 2026-05-07 duplicate dispatch (`bc5558bb12`) and the startup-before-spawn gap. Why it does not recur: the skip set treats absence of process evidence as "do not redispatch", which is stricter than the cube probe's `Unknown`-as-`Live` rule, and the occupancy inventory excludes the old workspace until its row is released; the respawn gap is still repaired, through the same allocation path every other dispatch uses.

Deliverables:

- `run_reconcile.rs` reduced to the verdict type and process-derived report, or deleted if the report shape is folded into `readoption.rs`; `app/server.rs` startup wiring updated.
- `coordinator/run.rs::resume_pane_spawn_for_running_execution` deleted; `startup_pane_reconcile` `Absent` arm routes to transient recovery.
- `coordinator_tests/recovery.rs` startup tests and `startup_pane_reconcile.rs` tests adapted: old workspace missing, old lease expired, same bookmark restored elsewhere, uncertain old spawn never duplicated.
- `post-crash-recovery.md` sections on orphaned-row lease retention and the startup cube probe rewritten.

Effort: large. Dependencies: Occupancy inventory and ownership before allocation; Remove the heartbeat breaker and auto-reap (for `reheartbeat_live_runs`, same file). Parallel with the next two entries. Scope: in-scope

### Remove HeartbeatGuard and the prefer/allow-dirty allocation API

Scope: delete `HeartbeatGuard` and `LEASE_HEARTBEAT_INTERVAL` from `coordinator.rs` and its pool tests; remove `prefer_workspace_id` and `allow_dirty` from `CubeClient::lease_workspace`, `cube_commands::lease_workspace`, both host adapters, and the scratch callers (`conflict_ladder`, `speculative_conflict`, `handler_helpers`); delete the `prefer_workspace_id`, `allow_dirty` and `fallback_from_prefer` fields from the lease dispatch events; delete the `--allow-dirty` reclaim-report parsing in `cube_commands.rs`. Incident: the 2026-05-12 "`@` re-pointed mid-flight" chore. Why it does not recur: the registry-driven refresh covers pane workers, which the guard never did, and Boss-side occupancy makes a re-lease of an occupied workspace impossible regardless of TTL.

Deliverables:

- `HeartbeatGuard` and its tests gone; `run_execution` no longer spawns a per-run task.
- Allocation API is `lease_workspace(repo_id, task, exclude_workspace_ids)` everywhere; argv tests assert no `--prefer` / `--allow-dirty` can be emitted.
- Dispatch event payloads and their tests updated.

Effort: medium. Dependencies: Remove the heartbeat breaker and auto-reap; Occupancy inventory and ownership before allocation. Parallel with the startup entry and the ladder entry. Scope: in-scope

### Remove ladder lease upkeep

Scope: delete `ladder_lease_heartbeat.rs`, `ladder_lease_reap.rs`, their server wiring and the two shutdown drains; replace `ladder_lease_registry.rs` with a rung-in-flight registry keyed by attempt id (no lease id, no `release_all_on_shutdown`) that `conflict_ladder` registers into and `background_work` reads for badge eligibility; drop the install-id suffix on rung task labels since the reap was its only consumer. Incident: mono#2134 (the 2026-07-18 `flunge-agent-035` rung lease orphaned 31 s after acquisition across a restart). Why it is acceptable: a rung lease orphaned by a crash holds one workspace's cache until TTL; the rebase it was doing is reconstructed from refs on the next attempt; cube provisions on demand so nothing starves; no worker process survives the crash to be endangered.

Deliverables:

- Three modules deleted or replaced as above; `app/server.rs` no longer runs a startup reap, a ladder heartbeat, or a shutdown drain.
- `background_work.rs` eligibility uses the attempt-id registry; its tests updated.
- Conflict-ladder functional tests unchanged; shutdown tests assert shutdown no longer waits on cube.

Effort: medium. Dependencies: none (incidental `app/server.rs` overlap with the startup entry; later one forward-ports). Scope: in-scope

### One-shot bounded release in completion and dispatch

Scope: introduce a single `release_workspace_once` helper (bounded over adapter resolution plus cube, detached from the caller, debug-level on failure); use it from `finish_worker_teardown` after driver teardown, `force_release`, the `coordinator/run.rs` completion and cancelled-during-spawn tails, the `coordinator/execution.rs` failure tails, `scheduler.rs::schedule_execution_error`, and `coordinator/config.rs`; remove `CUBE_RELEASE_TIMEOUT` in favour of a `cube_op_budget` constant. Incident: the 388 s terminal/live-pane stall. Why it does not recur: the stall was fixed by ordering, which is kept; moving the cube call off the completion path removes the remaining way cube latency can delay terminal status or slot handling.

Deliverables:

- Helper with tests: bound covers adapter resolution; a stalled cube never delays the caller; no attempt when the worker is unproven.
- Completion teardown tests preserve pane-then-driver order and assert the cube step is detached; mid-spawn and remote unknown-owner tests remain.
- All listed coordinator release tails use the helper; synchronous release assertions replaced with queued one-shot assertions.

Effort: medium. Dependencies: Occupancy inventory and ownership before allocation; Remove the terminal-remote heartbeat and lease retry obligations. Scope: in-scope

### One-shot release in sweeps and helper tails; no offline-host lease I/O

Scope: route the release tails in `dead_pid_sweep`, `dead_pane_sweep`, `stale_worker_sweep`, `spawn_ack_sweep`, `terminal_work_sweep`, `lost_workspace_sweep`, `remote_lease_reconcile` (live reap), `app/review.rs`, `app/handler_helpers.rs`, `speculative_conflict.rs` and `conflict_ladder.rs` through the helper; delete the cube call from `host_reconcile`'s offline drain and `execution_liveness::force_release_lease_best_effort`; keep every worker-state transition, persona and slot release, backup and task recovery in those sweeps. Incidents: mono#1710 (waiting_human zombie), mono#1763 (dead remote workers blocking chains), the never-attached-pane gap. Why they do not recur: those incidents were cured by process supervision, which is untouched; only the cube bookkeeping attached to each cure changes.

Deliverables:

- Every listed tail calls the helper or, for offline hosts, nothing; `force_release_lease_best_effort` deleted.
- Existing supervisor tests still verify task recovery, persona and slot release and driver cleanup; synchronous force-release assertions replaced.
- `host_reconcile` tests assert zero cube calls for an offline host.

Effort: medium. Dependencies: One-shot bounded release in completion and dispatch. Parallel with the preference entry. Scope: in-scope

### Remove inert recovery preferences and slot affinity

Scope: stop producing `preferred_workspace_id`, `prefer_is_soft` and `allow_dirty` in `transient_recovery.rs`, `work/executions_runs.rs` (dead-worker resume), `work/dispatch.rs` (orphaned-predecessor inheritance), `pr_flow.rs` (operator-answer redispatch) and `exec_tail::preserve_execution_workspace_preference`; delete the slot-affinity input (`claim_worker` / `claim_worker_force` / `select_claim_index` / `select_spill_claim_index` preferred-workspace parameters and `release_worker`'s `last_workspace_id`); delete the tests that assert these preferences; rewrite the `transient_recovery.rs` module doc. Columns and `WorkExecution` fields stay. Incident: none recorded; the policy predates bookmark recovery. Why nothing regresses: allocation has ignored these fields since bookmark recovery landed, and `recovery_uses_shared_store_when_cube_recovered_nothing` already proves restoration without them.

Deliverables:

- No production code path writes a non-null preference; `work/dispatch.rs` and `work/executions_runs.rs` inserts pass `NULL` / `false`.
- Pool claim API takes no workspace preference; `dispatch_spillover` affinity branch and its tests deleted.
- `transient_recovery.rs` tests asserting `preferred_workspace_id` / `allow_dirty` deleted; bookmark restore tests kept.

Effort: medium. Dependencies: Startup reconciliation from process evidence; Remove HeartbeatGuard and the prefer/allow-dirty allocation API. Scope: in-scope

### Final heartbeat decision: keep the minimal refresh or delete it

Scope: apply the owner's answer to the question in the attentions manifest. Option A: rename `cube_lease_heartbeat.rs` to reflect its only remaining job (an affinity refresh for live owners), confirm the disabled-host predicate and whole-operation bound, and update `worker-liveness-contract.md`. Option B: delete the module, its `HostRoutedCubes` seam if nothing else uses it, its server wiring and tests, and update the same doc. Incident: mono#1546 for Option A's continued existence; none for Option B. Why Option B does not reintroduce mono#1546: the starvation came from the post-allocation guard refusing phantom-free workspaces in a loop; with ownership before allocation the engine excludes them and cube provisions fresh ones, so the pool cannot wedge.

Deliverables:

- Exactly one of the two shapes above, with `worker-liveness-contract.md` updated to match.
- No remaining reference to lease state as liveness evidence anywhere in `tools/boss/docs/` or module docs.

Effort: small. Dependencies: Remove the heartbeat breaker and auto-reap; Startup reconciliation from process evidence; Remove HeartbeatGuard and the prefer/allow-dirty allocation API. Scope: in-scope
