# Worker-liveness convergence in the Boss engine — a design review

- **Date:** 2026-07-29
- **Question:** is the accumulation of worker-liveness machinery in the engine evidence of a fundamental design problem, or appropriate incremental convergence?
- **Deliverable:** this document. No behavioural code changes; no change to any open PR.
- **Read at:** `main` = `ddd01898f7b5231f1b3b33785a95880fbaecf930`; PR #2503 head = `39fb66d92c19e0eb5a0b89207aea61de90a6bba3` (branch `boss/exec_18c6b3aaffd58198_b8`, +3074/−110 across 27 files, two commits).
- **Related:** [#2503](https://github.com/spinyfin/mono/pull/2503) (open, under review), [#2500](https://github.com/spinyfin/mono/pull/2500) (merged), [#2502](https://github.com/spinyfin/mono/pull/2502) (open), [#2519](https://github.com/spinyfin/mono/pull/2519) (open), [#2521](https://github.com/spinyfin/mono/pull/2521) (merged).
- **Citation convention:** `path:line` without a qualifier means the file as it stands on `main`. `path:line` marked _(PR head)_ means the file as it stands on #2503's head commit, whose line numbers differ.

## Verdict

**No, this is not the failure mode the question fears.** The eleven-plus mechanisms are overwhelmingly _not_ redundant workarounds layered on each other; they answer genuinely different questions from genuinely different evidence, and the codebase has a repeated, documented habit of consolidating them the moment two of them start answering the _same_ question. PR #2503 is a root-cause fix, not another layer: it identifies a real invariant violation (every liveness decision read derived in-memory bookkeeping) and supplies the missing durable second opinion plus the missing second verb.

**But there are two specific structural gaps, and they are the reason each fix keeps leaving a hole for the next one to close.**

1. **Convergence is event-triggered, not state-reconciled.** #2503 states an invariant — "a live worker for an execution the engine believes is dead must be re-adopted or reaped" — and then enforces it at exactly two detector sites, one of which (`orphan_sweep`) has three early exits and a candidate query that all fire hardest during precisely the incident the PR was written for. No sweep in the engine ever scans for the _state_ "terminal execution + live durable pid". This is the same shape as every prior gap in the chain.

2. **"Terminal execution still holding resources" has no owner.** Not one query in `work/` selects a terminal execution that still records a `cube_lease_id`. Every reclaim path filters `status NOT IN (terminal)`. The only bound on a stranded lease is cube's 24-hour TTL. Live cube state at the time of writing: **78 of 101 workspaces leased**, with 63 of those 78 (81%) belonging to task labels that appear more than once — up to nine concurrent leases for one task.

Neither gap needs a redesign. Both are closed by finishing work the codebase has already started, and both closures _delete_ code rather than adding a twelfth mechanism. Details in §4.

**Confidence:** high on the two gaps (both are code-level and directly cited), high on "not a fundamental design problem", medium on the ranked severity of the sub-findings. Falsifiers are in §3.

## Method, and what I could not check

Everything below was re-derived from source at the commits named above; where the scoping brief for this investigation was wrong, §7 says so.

Two things I could not establish:

- **The Boss DB.** Worker sessions are barred from `~/Library/Application Support/Boss/` and from `bossctl`, so I could not cross-reference lease ages against `work_executions` history, and could not reproduce the `bossctl workspace summary` reading of eight workspaces with `execution=-`. What I did instead: read cube's own state directly (`cube workspace list --json`, read-only), and establish the _code-level mechanisms_ that can produce that class. §5 gives the exact query that would settle it.
- **A live engine.** No engine was run. Every claim here is static analysis plus the cube-side observation.

PR #2503 has **no inline review comments and no issue comments on GitHub** — I checked both endpoints. The nine findings the brief refers to live in the Boss product, which I cannot read. §7 reports what I could determine about them from the diff itself.

## 1. What PR #2503 actually does

### 1.1 The diagnosis is correct and is the important part

The PR's root-cause claim is that every liveness decision read _derived_ bookkeeping — `LiveWorkerStateRegistry` or the `WorkerPool` claim table — both of which `release_worker_pane` clears unconditionally on every terminal path. That is verifiable and it is exactly right: on `main`, `app.rs:1388` takes the slot mapping, and by `app.rs:1492` it has dropped the pool claim, the live-state entry and the live-status task, whatever happened in between. A wrong terminalization therefore erases the _only_ evidence that would contradict it, and every downstream consumer then agrees with the wrong belief.

This diagnosis is not a restatement of "we need another check". It is the identification of a systematic bias: the engine had no liveness input that survived its own teardown. That is a genuine root cause.

### 1.2 The invariant

Stated in `tools/boss/docs/worker-liveness-contract.md` _(PR head, new file)_:

> A live worker for an execution the engine believes is dead must either be **re-adopted** or **reaped**, promptly and observably.

The policy is a pure function, `worker_readoption.rs:155` _(PR head)_:

| Terminal status                                | Other live execution on the row? | Verdict                                   |
| ---------------------------------------------- | -------------------------------- | ----------------------------------------- |
| `orphaned` / `abandoned` (inferred)            | no                               | **Re-adopt**                              |
| `orphaned` / `abandoned` (inferred)            | yes                              | **Reap** (`superseded_by_live_execution`) |
| `cancelled` / `completed` / `failed` (decided) | either                           | **Reap** (`terminal_by_decision`)         |

The asymmetry argument — re-adoption rewrites a record and fails recoverably, reaping destroys work irreversibly — is sound, and the inference/decision split is the right axis to cut on. This is the strongest part of the design.

The storage layer restates the anti-duplication half independently (`work/executions_runs.rs:262` _(PR head)_, which refuses to re-adopt into a row that already has a `running`/`waiting_human` sibling). Defence in depth at the right layer.

### 1.3 Is the invariant enforceable where it is placed? Partly.

The invariant is a statement about a _state_. It is enforced at two _events_.

**Trigger A — a hook arrives for a terminal execution** (`app/worker_events.rs:700` _(PR head)_). Strong, and correctly argued: a hook is produced by the worker's own process and cannot be forged by stale bookkeeping. Covers any worker that is alive _and talking_.

**Trigger B — the orphan sweep's re-dispatch guard** (`orphan_sweep.rs:341` _(PR head)_, converging at `:385`). This is meant to cover the worker that is alive but _quiet_ — parked in a long foreground build. It does not cover that case reliably, for four reasons that are all visible in the same function:

1. **Pool-full early return.** `orphan_sweep.rs:165` _(PR head)_ returns from the whole pass when `has_idle_worker()` is false. During a duplicate-dispatch storm the pool is, by construction, full of the duplicates. No convergence happens at all.
2. **The churn guard runs first.** `orphan_sweep.rs:205` _(PR head)_ `continue`s the item when it has accumulated `ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD` terminal executions in the window. That is precisely the signature of the 2026-07-28 incident, and it fires _before_ the durable-process guard is ever reached. The row is parked with an attention item and its live worker stays untracked indefinitely.
3. **The candidate query.** `list_orphan_active_candidates` only yields items with `tasks.status='active'`, no `ready` execution, and `updated_at` older than `ORPHAN_MIN_AGE_SECS`. A stranded live worker on a row that has been demoted to `todo` (which `force_stop_execution` does) or moved to `in_review` is never a candidate, so never converged.
4. **The trust window is anchored differently from what the PR says.** This one is worth its own subsection.

### 1.4 The re-dispatch guard's trust window does not mean what the PR body says

The PR body's "A pid is a durable number, not a durable handle" section states, covering both bounded entry points:

> The window anchors on `COALESCE(finished_at, created_at)`, so it bounds how long ago the engine last had first-hand knowledge of the process rather than how long the run has been going — a six-hour worker terminalized a minute ago is exactly what the reap path is for.

That is true of the reap path. `work/executions_runs.rs:2229` _(PR head)_ does anchor on `CAST(COALESCE(finished_at, created_at) AS INTEGER)`.

It is **not** true of the re-dispatch guard. `latest_local_worker_process_for_work_item` — the query behind `probe_work_item_worker`, the guard that `REDISPATCH_PID_TRUST_SECS` is literally named for — anchors on `CAST(r.created_at AS INTEGER)` at `work/executions_runs.rs:2286` _(PR head)_. `work_runs.created_at` is epoch seconds stamped at run insert (`work/executions_runs.rs:865`, `work/audit_misc.rs:76`), i.e. roughly worker start time.

**Consequence:** the anti-duplicate-dispatch guard protects only workers whose run row was created within the last hour. A worker that has been running for two hours and is wrongly terminalized right now yields `None` from the guard, and the sweep redispatches straight over it. The class of long-running workers is not hypothetical — `cube_lease_heartbeat.rs:15-17` exists specifically because "large chores, multi-bazel builds, reviews" outlive a lease TTL, and the 2026-07-14 and 2026-07-26 husk incidents both involved long-lived panes.

The 2026-07-28 incident is covered (those runs were terminalized seconds after start), so this is not a regression — but the PR's own framing claims a general invariant, and the guard delivers a one-hour-old-workers-only version of it. This is the single most load-bearing discrepancy between the PR's description and its code.

### 1.5 The convergence latch is narrower than "serialized per run"

`converging_terminal_runs` (`app.rs:657` region _(PR head)_, taken at `app/readoption.rs:82`) is a per-process `HashSet<String>`. It serializes convergence against _other convergence_ for the same run. It does not serialize convergence against `spawn_ack_sweep`, `dead_pane_sweep`, `stale_worker_sweep`, `husk_pane_sweep` or `terminal_work_sweep`, all of which run on their own 60-second loops and all of which can write the same row concurrently. The claim in the PR body — "Convergence is serialized per run" — is accurate as stated but reads as a stronger guarantee than it is; the interesting race is convergence-vs-sweep, not convergence-vs-convergence.

Two smaller robustness points on the latch: it is released by an explicit call (`app/readoption.rs:53` _(PR head)_), not by a guard object, so a panic or a task cancellation inside `converge_terminal_execution_inner` leaks the latch and makes that run permanently unconvergeable for the engine's lifetime. An RAII guard would remove the class.

### 1.6 The convergence reap does not release the cube lease

`reap_contradicting_worker` calls `self.release_worker_pane(run_id)` directly (`app/readoption.rs:265` _(PR head)_). `release_worker_pane` never touches the lease — that is the caller's job, and the caller that does it is `force_release` (`completion/release.rs:25`), which gates the cube release on the `PaneReleaseOutcome` and claims lease ownership atomically via `clear_execution_workspace` (`work/exec_tail.rs:141`) before calling out.

So a convergence reap tears down the pane and the process tree and leaves `cube_lease_id` set on a terminal row — which, per §2.4, is the one state no reclaim path in the engine can see. This is a new instance of an existing leak class, not a new class, but it is a new instance and it ships in a PR whose subject is losing track of workers.

### 1.7 What #2503 gets unambiguously right

Worth stating plainly, because the rest of this section is critical:

- **It unifies rather than duplicates.** `durable_liveness.rs` _(PR head)_ is a new module, but the same PR migrates `dead_pane_sweep`'s hand-rolled copy onto it (`dead_pane_sweep.rs:236` _(PR head)_), and because `cube_lease_heartbeat`'s DB-fallback gate already routes through `dead_pane_sweep::shell_pid_death_evidence` (`dead_pane_sweep.rs:194`), that consumer is unified transitively. That is net-negative duplication.
- **`Unknown` is never `Gone`.** `durable_liveness.rs:99-103` _(PR head)_, tested at `:409`. This is the correct and non-obvious call; treating a mid-spawn worker's absent pid as death is how live-but-slow spawns get reaped.
- **`EPERM` is alive.** `durable_liveness.rs:169` _(PR head)_.
- **The bounded/unbounded split is a real safety boundary,** correctly reasoned (`durable_liveness.rs:37-47` _(PR head)_): callers that signal a process must not act on a pid the table can no longer vouch for, and callers whose failure direction is "decline to act" can use the unbounded form.
- **`bossctl agents stop` on an untracked worker now works** (`bossctl/src/agents.rs` _(PR head)_, engine side at `app.rs:1572` _(PR head)_). Being unable to reap a running worker was its own defect and this fixes it, narrowly (only `exec_…` selectors get the fallback, so a typo'd crew name still fails loudly).
- **The tests are unusually honest.** `bounded_probe_refuses_a_row_outside_the_trust_window` asserts the _unbounded_ probe is deliberately unaffected; `release_worker_pane_refuses_to_reap_from_a_pid_outside_the_trust_window` spawns a real process in its own group so dropping the bound fails by killing a bystander; the concurrency test takes the latch directly rather than racing futures, with a comment explaining exactly why a race would let the test pass with the latch deleted.

## 2. One authoritative account of liveness

### 2.1 The state vector

A worker's liveness is not one bit. It is a tuple of nine partially-independent variables, each with a different owner, a different lifetime, and a different failure direction:

| #   | Variable                                                          | Where it lives | Survives engine restart? | Cleared by `release_worker_pane`? |
| --- | ----------------------------------------------------------------- | -------------- | ------------------------ | --------------------------------- |
| 1   | `work_executions.status`                                          | DB             | yes                      | no                                |
| 2   | `work_runs.shell_pid`                                             | DB             | yes                      | no                                |
| 3   | `work_runs.host_id`                                               | DB             | yes                      | no                                |
| 4   | `work_executions.cube_lease_id`                                   | DB             | yes                      | no                                |
| 5   | `workspace_path` exists on disk                                   | filesystem     | yes                      | no                                |
| 6   | `LiveWorkerStateRegistry` entry (incl. activity, `last_event_at`) | memory         | **no**                   | **yes**                           |
| 7   | `WorkerPool` claim                                                | memory         | **no**                   | **yes**                           |
| 8   | `WorkerRegistry` run→slot map                                     | memory         | **no**                   | **yes**                           |
| 9   | App-hosted pane for the slot                                      | macOS app      | no                       | yes                               |

The 2026-07-28 incident is one cell of this product: (1) terminal, (2) alive, (6)(7)(8) empty, (9) present. Every prior incident in the chain is a different cell. **That is the honest characterisation of why the mechanism count is high** — not that eleven checks were bolted on for one question, but that the question has a nine-dimensional state space and the engine discovers the cells one incident at a time.

### 2.2 The mechanisms, and the question each actually answers

Thirteen liveness-relevant loops are wired in `app/server.rs` (of 29 `spawn_loop` calls total), plus several non-loop checkpoints. The brief's list of eleven is close; here is the corrected and extended enumeration, each with the _distinct_ question it answers:

| Mechanism                                          | Question it answers                                                   | Evidence read                 | Failure direction                     |
| -------------------------------------------------- | --------------------------------------------------------------------- | ----------------------------- | ------------------------------------- |
| `LiveWorkerStateRegistry` (`live_worker_state.rs`) | "what does the engine currently believe about slot N?"                | memory (6)                    | erased by restart _and_ by teardown   |
| `WorkerPool` claims (`coordinator.rs`)             | "is this execution occupying a dispatch slot?"                        | memory (7)                    | same                                  |
| `WorkerRegistry` (`worker_registry.rs`)            | "which slot hosts run R?"                                             | memory (8)                    | same                                  |
| `execution_liveness::classify_pane_liveness:112`   | "did a pane ever attach?"                                             | (2)+`started_at`              | conservative: `LiveOrIndeterminate`   |
| `dead_pane_sweep::shell_pid_death_evidence:194`    | "is the recorded pid gone (ESRCH)?"                                   | (2)                           | conservative: only ESRCH reaps        |
| `dead_pid_sweep::probe_pid:1137`                   | raw `kill(pid,0)`                                                     | OS                            | shared primitive                      |
| `worker_process_exit.rs`                           | "is a _vanished_ process a death **for this driver**?"                | driver semantics              | `codex exec` exits per turn by design |
| `durable_liveness.rs` _(PR head)_                  | "is the pid recorded for execution E alive, from durable state only?" | (2)                           | `Unknown` ≠ `Gone`                    |
| `lost_workspace_sweep.rs`                          | "has the workspace directory vanished?"                               | (5)                           | positive evidence only                |
| `spawn_ack_sweep.rs`                               | "did this spawn produce _any_ evidence of a process?"                 | (6) with `shell_pid==0`       | grace 60 s from `started_at`          |
| `stale_worker_sweep.rs`                            | "is a live process making progress?"                                  | (6) activity + tool-in-flight | only touches `working` slots          |
| `terminal_work_sweep.rs`                           | "is a live pane bound to work that is already over?"                  | (1)+work-item status          | two-pass confirmation                 |
| `husk_pane_sweep.rs`                               | "does the app host a pane the engine has forgotten?"                  | (9) vs (6)                    | `MAX_RETIREMENTS_PER_PASS:135` = 3    |
| `pool_claim_sweep.rs`                              | "is a pool claim outliving its execution?"                            | (7) vs (1)                    | leaves claims backed by live panes    |
| `cube_lease_heartbeat.rs` DB-fallback gate         | "should I keep this lease alive?"                                     | (2)+(1)+(3)                   | only _stops_ beating on proof         |
| `run_reconcile::confirm_execution_dead:236`        | "does cube still show this lease held?"                               | cube                          | `Unknown` ⇒ treated as live           |
| `remote_lease_reconcile.rs`                        | the remote-host analogue of `lost_workspace_sweep`                    | remote cube                   | remote only                           |
| `host_reconcile.rs`                                | "is this execution stranded on a disabled host?"                      | host registry                 | drains proactively                    |
| `ladder_lease_reap.rs`                             | "did a prior engine crash leave a conflict-ladder lease?"             | cube + (2)                    | startup only                          |
| `coordinator::occupying_live_worker:1653`          | "is cube handing me a workspace a live worker is in?"                 | (6)+OS                        | **registry-based — see §2.3**         |
| `app/engine_meta.rs:45-49`                         | "does cube's lease view match the engine's?"                          | cube + (4)                    | **reports only — see §2.4**           |

Plus, on the PR head, `app/pane_ops.rs:595` `durable_live_process_evidence` — a _second_ husk-sparing predicate, distinct from `husk_pane_sweep::live_process_evidence:169`.

Read that table as a whole and the "eleven overlapping checks" framing does not survive. Four of them are shared primitives that other mechanisms call rather than reimplement. The rest read genuinely different evidence: three read memory, five read the DB, one reads the filesystem, three read cube, one reads the app, one reads driver semantics. The overlap that _does_ exist is small and has been actively reduced over time.

### 2.3 Divergence scenarios — where two mechanisms disagree

These are the concrete cells, not abstractions.

**D1 — The lease-time occupancy guard still reads the erased registry.**
`coordinator/execution.rs:844` calls `occupying_live_worker` with `live_worker_states.snapshot()`. This is the last line of defence against two workers interleaving edits in one working copy, and its comment says so ("an interleaved working copy silently corrupts two workers' edits"). It has exactly the root-cause bug #2503 was written to fix: for a wrongly-terminalized worker the registry entry is gone, so the guard sees no occupant and grants the lease. #2503 does not touch it. After #2503, a _same-work-item_ duplicate is blocked by the re-dispatch guard, but a _different_ work item leasing the still-occupied workspace is not.

**D2 — Two husk classifiers with different sufficient conditions for "spare".**
`husk_pane_sweep::live_process_evidence:169` requires **pid alive AND hook corroboration** (either an unbalanced `PreToolUse` or a hook within `HUSK_LIVENESS_CORROBORATION_SECS`), and its doc explains why pid-alone is insufficient: a genuine husk's shell pid stays alive after `claude` exits inside it. #2503's `app/pane_ops.rs:595` _(PR head)_ requires **pid alive AND status terminal-by-inference**, on the reasoning that there is no live-state entry to read hooks from.

But "pid alive + `orphaned`" is exactly the state a genuine husk occupies when `spawn_ack_sweep` was the terminalizer. So the new predicate spares panes the old one would (correctly) retire, and then the convergence path re-adopts them. §2.3/D3 is what happens next.

**D3 — Re-adoption of a pid-less run collides with `spawn_ack_sweep`.**
`readopt_live_worker` (`app/readoption.rs:119` _(PR head)_) registers the restored slot via `register_spawn_with_capabilities`, which builds a `new_spawning_with_routing` state — activity `Spawning`, `last_event_at` `None`, and `shell_pid` = `probe_execution_worker(...).alive_pid().unwrap_or(0)`. It also does not reset `started_at` (`readopt_inferred_terminal_execution` clears only `finished_at`, `work/executions_runs.rs:262` _(PR head)_).

`spawn_ack_sweep` reaps any slot that is `Spawning`, with `shell_pid == 0`, with `last_event_at == None`, whose execution is non-terminal and older than `SPAWN_ACK_GRACE_SECS` (60 s) from `started_at` — `spawn_ack_sweep.rs:195-243`, cadence 60 s (`app/server.rs:1233`).

A run terminalized _by_ `spawn_ack_sweep` has, by that sweep's own selection criteria, **no recorded pid**. If such a worker later hooks (the lost-ack case), re-adoption produces exactly the candidate shape above, and the ack sweep re-reaps it within one pass unless a second hook lands first to set `last_event_at`. That is a re-adopt/re-reap oscillation at 60-second cadence, and it is the _most likely_ shape for the hook trigger to encounter, because the ack timeout is the terminalizer the PR names as its primary suspect.

The PR's flagship test drives the _with-pid_ variant (`a_worker_whose_spawn_ack_was_lost_is_readopted_once_it_hooks` records a durable pid before the ack-timeout reap). The pid-less variant is untested. I did not execute this; it is derived from the code and should be confirmed with a test before landing.

**D4 — Re-adoption paints `Spawning` for a worker mid-turn.**
Same registration path. For the hook trigger this self-corrects on the next hook. For the re-dispatch-guard trigger — whose entire purpose is the worker that emits no hooks — it does not: the slot stays `Spawning`, and `mark_stalled_spawns` will promote it to `WaitingForInput` if the driver is awaiting-input-capable. So the quiet-worker path can produce the wrong indicator, which is the class the PR exists to end. The driver derivation added in the second commit (`app/readoption.rs:184-197` _(PR head)_) makes that promotion _correct for the driver_, but it does not make it correct for the worker's actual state.

**D5 — `run_reconcile` and `durable_liveness` can contradict.**
`confirm_execution_dead:236` treats a cube `Unknown` as not-dead, and the DB-fallback auto-reap gates on it. `durable_liveness` treats a probe `Unknown` as not-alive. Both are individually correct (each errs toward its own caller's safe direction) but they are opposite defaults for the same word, and the two feed adjacent decisions about the same execution. This is a documentation hazard rather than a bug today.

### 2.4 The unowned failure class: terminal executions still holding resources

This is the finding I would put in front of the operator first.

**Every** query in `work/` that looks for a lease to reconcile filters terminal rows out:

- `list_in_flight_executions:490` — `status NOT IN (…terminal…) AND cube_lease_id IS NOT NULL`
- `list_non_terminal_executions_with_workspace:520` — `status NOT IN (…terminal…)`
- `list_active_revision_executions_for_chain` — same predicate
- `lease_to_execution_map:404` — no status filter, but it is **read-only reporting** for `workspace_pool_summary`

`app/engine_meta.rs:45-49` says so verbatim:

> Annotate each entry with the engine's view: which execution row (if any) currently records this workspace's lease. Drift (cube reports a lease the engine has no execution for) shows as `None`.

So drift is _observed and reported by the engine and remediated by nobody_ — which is exactly the condition the brief asked me to adjudicate. My answer: **this is not correct boundary respect, it is an unowned failure class**, and §6 says why.

Now the mechanism that fills the pool. The orphan sweep's redispatch marks the stale execution `abandoned` (`work/dispatch_helpers.rs:1263`), which sets `status` and `finished_at` and **nothing else** — `cube_lease_id`, `cube_workspace_id` and `workspace_path` all survive. And unlike an `orphaned` predecessor, an `abandoned` one does **not** carry its workspace forward: `work/dispatch.rs:551` sets `preferred_workspace_id` only when the predecessor's status is `Orphaned`, and the comment at `:540` says abandoned/failed/cancelled predecessors "are intentional throwaways and don't carry forward". So the fresh execution leases a _different_ workspace, and the abandoned row's lease is now held by a terminal execution.

From that moment:

- `cube_lease_heartbeat` stops beating it (non-terminal filter) — correct, and it is what lets the TTL eventually fire.
- `lost_workspace_sweep`, `dead_pane_sweep`, `remote_lease_reconcile` all skip it (`is_live()` gate).
- `terminal_work_sweep` force-releases a lease only in its `work_item_missing` branch (`terminal_work_sweep.rs:404`); the `execution_terminal` branch reaps the pane and leaves the lease, on the stated assumption that "this sweep is the terminalizer for this candidate" — which is not true when something else terminalized it.
- `stale_lease_to_reclaim_for_workspace:1458` reclaims it only if some _later_ execution happens to hard-prefer that exact workspace.
- `execution_retention_sweep` prunes the row after 14 days (`work/execution_retention.rs:76`, keep-5 floor at `:80`) — far outside the 24 h TTL, so pruning is not a plausible source of the `execution=-` class.

**Net: every orphan-sweep redispatch strands one cube workspace for up to the 24-hour lease TTL.** The TTL is the only bound, and shortening it is off the table (it was raised to 24 h deliberately after a crash-restart incident).

### 2.5 What cube's own state shows

Read directly from `cube workspace list --json` on 2026-07-29 (read-only; no Boss DB involved):

| Measure                                        | Value                                                                                             |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Workspaces total                               | 101                                                                                               |
| `leased`                                       | 78                                                                                                |
| `free`                                         | 23                                                                                                |
| Leases past their expiry but still held        | 0                                                                                                 |
| Oldest lease                                   | 18.4 h                                                                                            |
| Distinct task labels among the 78 leases       | 33                                                                                                |
| Leases whose task label appears more than once | **63 (81%)**                                                                                      |
| Largest single-label group                     | **9** (`task_implementation Make pre-trust and config-dir gitignore driv…`, ages 14.8 h → 18.4 h) |

Second-largest groups: 7× (`task_implementation Wire Grok hooks…`, 15.0 h → 18.4 h), 6× (`pr_review Reviewer driver and model…`, 1.9 h → 3.2 h), 6× (`revision_implementation Resolve merge conflict against main`, all within 42 minutes), 4×, 4×, 3×, 3×.

**Caveat, stated plainly:** the cube `--task` label is `<execution_kind> <work-item name>`, not a unique key. Some multiplicity is legitimate — six distinct merge-conflict revisions against six distinct PRs genuinely share the name "Resolve merge conflict against main". But nine leases spread over 3.6 hours under one _specific_ task name is not that, and no lease exceeds the 24 h TTL, which is precisely the signature of "acquired repeatedly, released never, reclaimed only by TTL".

This is also why PR #2502's approach — compacting _free_-state workspaces and setting per-repo high-water marks — cannot fix the disk problem on its own. It reclaims only the 23 free workspaces; the 78 leased ones are the actual growth, and they are a Boss-side lease-lifecycle bug, not a cube-side disk bug. **The two efforts are not proceeding on incompatible assumptions, but #2502 is treating a symptom whose cause lives in the subsystem #2503 is in.**

### 2.6 A second, narrower mechanism for the `execution=-` class

The brief hypothesised a crash between `lease_workspace_with_fallback` (`coordinator/execution.rs:803`) and `start_execution_run_on_host` (`:1184`). That window is real and wide — roughly 380 lines including a `cube workspace list` round trip for the occupancy guard — and the handled failure paths inside it _do_ hand the lease back (`:894-903`, "Hand the workspace straight back so it isn't stranded"). So handled failures are fine; an engine crash or restart in that window leaves a lease with no durable trace. I could not confirm this happened.

There is a second mechanism I _can_ establish from code alone, and it needs no crash. `force_release` (`completion/release.rs:25`) claims lease ownership atomically by nulling `cube_lease_id` **before** calling cube (`:53`), then calls `cube_client.release_workspace` at `:76`. If that call fails, it returns `ForceReleaseOutcome::LeaseReleaseFailed { lease_id }` at `:83` — and at that point the lease id exists only in a log line and an enum variant. `preempt_worker` (`completion.rs:1340`) logs it and returns `Failed`; nothing retries, and no DB row carries the id any more. Cube still holds the lease. The workspace shows `leased` with `execution=-` forever, until TTL.

That is an ownership-claim protocol with no compensating record: it correctly prevents double-release and thereby guarantees zero-release on failure.

## 3. Verdict, confidence, falsifiers

### 3.1 The verdict, argued

**The accumulation is appropriate incremental convergence, not a fundamental design problem.** The strongest evidence is not the module docs — the brief rightly forbids taking those at face value — but the _pattern of deletions_ in the code:

- `sweep_loop.rs` extracted the shared loop body _and_ `confirm_two_pass:68`, now used by both `terminal_work_sweep` and `husk_pane_sweep`.
- `execution_liveness.rs` extracted the never-attached classifier and wrote down the governing rule at `:59`: "exactly one reaper per death signal". `lost_workspace_sweep.rs:18-23` explicitly declines to reimplement the dead-pid signal and names `dead_pane_sweep` as its owner.
- `worker_process_exit.rs` extracted "is a vanished process a death **for this driver**?" out of three call sites at once.
- `dead_pane_sweep::shell_pid_death_evidence:194` became the single pid-death implementation, and `cube_lease_heartbeat`'s new DB-fallback gate (#2500, merged) reuses it rather than copying it — the module doc at `cube_lease_heartbeat.rs:84-88` says so and the code matches.
- #2503 itself migrates `dead_pane_sweep` onto the new primitive in the same diff.

A codebase that were merely layering workarounds would not keep doing this. It would have five copies of `kill(pid, 0)` semantics and no shared classifiers. It has one raw probe (`dead_pid_sweep::probe_pid:1137`), one durable-pid-by-execution wrapper, and a documented one-reaper-per-signal rule that is largely observed.

**What _is_ wrong is narrower and more specific than "the design".** Two things:

- Convergence is bolted to detectors rather than reconciled from state (§1.3, §1.4). This is a _repeatable_ mistake — it is what produced each prior gap — and #2503 repeats it.
- Terminal-executions-holding-resources has no owner (§2.4), and the reporting surface that sees it is explicitly non-remediating (§2.4).

### 3.2 Confidence

- **High** that §2.4 (unowned lease class) is real: it is provable from four SQL predicates and confirmed by the cube-side distribution.
- **High** that §1.4 (the `created_at` anchor) is real: two adjacent queries in the same file, different anchors, and the PR body describes only one.
- **High** on the verdict itself.
- **Medium** on D3 (the re-adopt/re-reap oscillation): derived from code, not executed. A test would settle it in minutes.
- **Medium-low** on the relative severity ordering of D1 vs §2.4 — both are serious; which bites first depends on dispatch volume.

### 3.3 What would change my mind

- **Toward "fundamental problem":** if a fifth incident in this subsystem lands in the next month whose root cause is again a new cell of the §2.1 state vector rather than a regression in a known cell. Four incidents in ten weeks is convergence; six in fourteen is a state machine begging to be written down.
- **Also toward "fundamental problem":** if the state-scan change in §4.1 turns out not to be expressible without a new candidate query and a new loop. If closing gap 1 genuinely requires a twelfth mechanism, then the decomposition really is wrong.
- **Toward "no problem at all":** if `orphan_sweep`'s early exits turn out to be unreachable in practice — specifically, if `has_idle_worker()` is essentially always true in production (spillover pools, `dispatch_spillover.rs`) and the churn guard fires rarely enough that it never coincides with a live-but-untracked worker. I could not measure either without engine access. Dispatch-event counts for `redispatch_blocked_live_process` versus `churn_guard_parked` after #2503 lands would answer it.
- **Toward "the lease class is smaller than I think":** if the Boss DB shows most of the 78 leases mapping to genuinely distinct, genuinely live `work_executions` rows. §5 gives the query.

## 4. What should change — none of it a twelfth mechanism

### 4.1 Make convergence a state scan, and delete the orphan-sweep trigger

**The gap:** the invariant is about state; enforcement is at events.

**The change:** widen `dead_pane_sweep`'s candidate query and make its verdict two-sided. Today `dead_pane_sweep::run_one_pass:146` iterates `list_non_terminal_executions_with_workspace:520` and acts only on `WorkerProcess::Gone`. It already owns the durable-pid signal end to end (`execution_liveness.rs:56-60` names it as the exclusive owner), already runs at 60 s, already fires on boot, and already has the host-safety rail baked into its pid lookup. It is the correct home.

Concretely:

- Add a sibling query that selects executions with a recorded local `shell_pid` **regardless of terminal status** (or relax the existing one and branch on status inside the pass).
- In the pass: `non-terminal + Gone` → today's reap, unchanged. `terminal-by-inference + Alive` → `converge_live_worker(id, "durable_state_scan")`.
- **Delete** the `convergence.converge_live_worker(...)` call at `orphan_sweep.rs:385` _(PR head)_. Keep the guard at `:341` — blocking the duplicate dispatch is still the orphan sweep's job, and it is the only thing that has to happen synchronously with the dispatch decision. Convergence stops being its responsibility, so the pool-full return, the churn guard, and the candidate query stop gating it.

This adds no loop, no module, and no candidate set that does not already exist in spirit. It removes one trigger and makes the remaining one exhaustive over the state.

### 4.2 Fix the re-dispatch guard's anchor

Change `latest_local_worker_process_for_work_item`'s cutoff (`work/executions_runs.rs:2286` _(PR head)_) from `CAST(r.created_at AS INTEGER)` to `CAST(COALESCE(r.finished_at, r.created_at) AS INTEGER)`, matching its sibling at `:2229` and matching what the PR body already claims. One-line change; it makes the guard's coverage independent of how long the worker has been running, which is the property the PR argues for.

If there is a reason to keep the `created_at` anchor, the PR body and `REDISPATCH_PID_TRUST_SECS`'s doc must say so and say what covers long-running workers instead.

### 4.3 Give "terminal execution still holding a lease" an owner

**No new sweep.** `terminal_work_sweep` already has the two-pass confirmation machinery, already reasons about terminal executions, and already force-releases a lease in one branch. Extend that branch:

- Add a query for terminal executions with a non-null `cube_lease_id` whose latest run's pid is `Gone` or `Unknown` (never `Alive` — an alive pid means §4.1's convergence owns it, not this).
- Two-pass confirm, then `clear_execution_workspace` + `cube_client.release_workspace`, reusing `force_release`'s existing atomic ownership claim so a concurrent releaser cannot double-release.
- Emit a dispatch event per release. A sustained non-zero count is the signal that some terminalizer upstream is not doing its job — which is diagnosis, not suppression.

This must **not** be built as "if in doubt, release" — a lease held by a live worker must never be released, hence the `Alive` exclusion and the two-pass confirm.

**Also fix the two known producers rather than only the backstop.** The backstop should be idle in steady state:

- `work/dispatch_helpers.rs:1263`: the abandon-stale transition should either carry the workspace forward as a soft prefer (making the lease genuinely reusable, as the `Orphaned` path already does at `work/dispatch.rs:551`) or release it. Doing neither is the current behaviour and is the main producer.
- `completion/release.rs:76-83`: `LeaseReleaseFailed` must leave a durable record. The cheapest correct shape is to re-stamp `cube_lease_id` back onto the row on failure so the ownership claim is _released_ rather than _lost_, letting the backstop retry. The alternative — a dedicated `leaked_leases` table — is more machinery for the same effect.

### 4.4 Fix the lease-time occupancy guard (D1)

`coordinator/execution.rs:844` should corroborate the registry snapshot with `durable_liveness::probe_execution_worker` for the execution cube's chosen workspace is recorded against. This is the same one-line-of-reasoning change #2503 applies everywhere else, applied to the one guard it missed — and it is the guard protecting against the worst outcome in the whole subsystem (two workers interleaving edits in one working copy). It should arguably be _in_ #2503.

### 4.5 One husk-sparing predicate, not two

Fold `app/pane_ops.rs:595` _(PR head)_ into `husk_pane_sweep::live_process_evidence_with:182` as a second constructor over the same predicate, so the "no live-state entry" branch and the "terminal live-state entry" branch produce one auditable rule. Today they accept different sufficient conditions for an irreversible action, which is how D2 arises.

### 4.6 Deletions that are pure win

- `app/server.rs:166` (`process_is_alive`) and `app/server.rs:1821` (`pid_is_alive`) are the same function, in the same file, both live: `process_is_alive` is re-exported (`app.rs:103`) and used by `engine_control.rs:134` and the isolation-guard integration test; `pid_is_alive` is used by `register_app_session_trust_ok` at `:1852`. Delete one.
- Route both remaining raw-`kill` sites through `dead_pid_sweep::probe_pid:1137` so `EPERM` handling cannot drift.

### 4.7 Make the latch a guard object

`app/readoption.rs:82`/`:90` _(PR head)_: return an RAII guard from `begin_terminal_convergence` so a panic or cancellation cannot permanently strand a run.

## 5. Should PR #2503 land?

**Land it, amended. Do not split it.**

Splitting is the wrong call here: the durable probe, the second verb, and the four wiring points are one idea, and landing the probe without the verb reproduces the exact state the PR diagnoses — a guard that blocks forever with nothing to resolve the contradiction (which is the argument `orphan_sweep.rs:377-384` _(PR head)_ makes about itself, correctly).

**Blocking before merge:**

1. **§4.2** — the `created_at`/`finished_at` anchor. One line, and without it the PR body's central safety claim is not what the code does.
2. **§1.6** — the convergence reap must release the cube lease, or must state in a comment why it deliberately does not and which path will. Right now it silently adds an instance to the class in §2.4.
3. **D3** — add a test for re-adoption of a run with **no** recorded pid (the shape `spawn_ack_sweep` actually terminalizes) and confirm it does not oscillate against the ack sweep. If it does, `readopt_inferred_terminal_execution` should also refresh `started_at`, or the ack sweep should skip a run that was re-adopted within its grace window.

**Strongly recommended in the same PR (each is small, each is squarely in scope):**

4. **§4.4** — corroborate the lease-time occupancy guard. This is the one remaining registry-only irreversible decision, and it guards the worst outcome.
5. **§4.7** — RAII latch.

**Follow-up PRs (out of scope for #2503):** §4.1 (state scan + delete the orphan trigger), §4.3 (lease owner + the two producers), §4.5 (one husk predicate), §4.6 (delete the duplicate helper).

**One documentation correction required.** `tools/boss/docs/worker-liveness-contract.md` _(PR head)_ is a good document and should ship. But `durable_liveness.rs:30-35` _(PR head)_ claims "every path consults reality through one implementation… Every caller goes through here". That is true for the durable-pid-by-execution question and false as written: `dead_pid_sweep::probe_pid` is still called directly from `husk_pane_sweep.rs:176`, `cube_lease_heartbeat.rs:633`, `ladder_lease_reap.rs:185`, `coordinator/execution.rs:851`, `app/server.rs:1941` and `dead_pid_sweep.rs:405`, each with its own pid source. Those are legitimate — they probe an in-memory pid, not a durable one — but the doc should say "the single implementation for _durable_ pid liveness" rather than implying universal funnelling. Overstated module docs in this subsystem are not cosmetic: they are how the next author concludes a check already exists.

**Query that would settle §2.4/§2.6** (coordinator to run; needs the Boss DB, which workers cannot read):

```sql
-- Leases held by rows that no reclaim path can see.
SELECT status, COUNT(*), MIN(finished_at), MAX(finished_at)
FROM work_executions
WHERE cube_lease_id IS NOT NULL
  AND status IN ('completed','failed','abandoned','cancelled','orphaned')
GROUP BY status;

-- Leases cube holds that no row claims at all (the `execution=-` class).
-- Compare `cube workspace list --json | .workspaces[].lease_id`
-- against SELECT cube_lease_id FROM work_executions WHERE cube_lease_id IS NOT NULL;
```

If the first query returns most of the 78, §2.4 is the whole story and §2.6 is a footnote. If it returns few and the set difference in the second is large, §2.6 (the lost-ownership-claim path) is the dominant mechanism and §4.3's producer fix should target `completion/release.rs` first.

## 6. Ownership

The brief asks who should own four things. Answers, with the existing boundary rules held fixed: the engine owns reconciliation, the app is a thin renderer, cube owns workspace usability and Boss trusts the lease rather than re-implementing health checks in the dispatcher.

| Concern                       | Owner                                                                                                                                                                                                  | Status today                                                                                                                                                                                                                                                                                                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Classifying a dead worker** | Engine, and specifically **one module per death signal** — the rule already written at `execution_liveness.rs:59`.                                                                                     | Correct and largely honoured. #2503 adds the durable-pid-alive signal to the roster and unifies its one prior duplicate. The remaining drift is the in-memory pid probes (§5) and the two identical raw helpers (§4.6).                                                                                                                                          |
| **Retrying it**               | Engine dispatcher (`orphan_sweep`, `dispatch_failure_recovery_sweep`, `transient_recovery`).                                                                                                           | Correct. #2503's guard belongs here and should stay here. Its _convergence trigger_ does not (§4.1).                                                                                                                                                                                                                                                             |
| **Reclaiming the slot**       | Engine `WorkerPool`, via `release_worker_pane` → `release_worker_and_kick`, with `pool_claim_sweep` as the backstop.                                                                                   | Correct. #2503's `reclaim_slot` (`coordinator.rs:1259` region _(PR head)_) is the right inverse verb and correctly refuses to yank a claim held by a different execution.                                                                                                                                                                                        |
| **Releasing the lease**       | **Engine, and it must be the terminalizer** — the path that writes the terminal status is the path that decides the lease's fate, because only it knows whether a resume will want the workspace back. | **Unowned in one branch.** The convention exists (`mark_execution_orphaned` deliberately retains for resume; `force_release` releases; `cube_lease_heartbeat`'s DB-fallback actively force-releases because "a DB-fallback row has no other mechanism watching it") but it is nowhere written down, and the `abandoned` terminalizer honours neither half. §4.3. |

**On cross-system drift specifically.** The engine currently _reports_ it (`app/engine_meta.rs:45-49`) and remediates nothing. The brief asks whether that is correct boundary respect or an unowned failure class. It is an **unowned failure class**, and the boundary argument does not save it, for a concrete reason: the drift is not cube's error. Cube is holding a lease the engine asked for and never gave back. Cube cannot know the engine is finished with it — that is precisely the knowledge asymmetry `cube_lease_heartbeat.rs:9-12` already names ("only the engine knows a worker is still running, so the engine is the only thing that can call it"). The same asymmetry that makes the engine the only possible _heartbeater_ makes it the only possible _releaser_.

Respecting cube's boundary means trusting cube's answer about workspace usability, not declining to release leases the engine itself holds. The 24-hour TTL is cube's backstop against an engine that crashes; it is not a garbage collector the engine may lean on in normal operation, and today it is the only thing bounding the pool.

## 7. Corrections to the scoping brief

Re-verified against source, as instructed. The brief was a good map; these are the places it is out of date or wrong.

- **"`dead_pane_sweep` still hand-rolls its own pid-probe copy."** No longer true at the PR head. The second commit (`39fb66d9`) routes it through the shared primitive (`dead_pane_sweep.rs:236` _(PR head)_). The finding was accurate against the first commit.
- **"`reap_untracked_worker_process` signals a process group with no age bound."** Addressed in the same second commit — it now uses the bounded probe with `REDISPATCH_PID_TRUST_SECS` (`app.rs:1572-1580` _(PR head)_), with a test that spawns a real bystander process and asserts it survives.
- **"The concurrency test does not demonstrate serialization."** Addressed. `concurrent_hooks_for_one_run_converge_once` now takes the latch directly and asserts both the refusal and the post-release resolution, with a comment explaining why racing two futures would have been a vacuous test. The narrower point that survives is §1.5: the latch serializes convergence against convergence only.
- **"`cube_lease_heartbeat` reuses `dead_pane_sweep`'s pid-probe _copy_ rather than #2503's new primitive."** It reuses `dead_pane_sweep::shell_pid_death_evidence:194`, which #2503 rewrites internally — so it inherits the new primitive transitively and needs no change. Not a duplication.
- **"18 `*_sweep.rs` files."** Correct count, but only ~8 of them are worker-liveness sweeps; the rest are retention, proposals, dependencies, postmortems, branch PRs, and the shared `sweep_loop.rs` scaffold. Thirteen liveness-relevant loops are wired in `app/server.rs`.
- **"Eleven mechanisms."** Under-counts. §2.2 lists twenty-one distinct checkpoints, including two the brief missed that matter: `coordinator::occupying_live_worker:1653` (the lease-time occupancy guard, still registry-only — §2.3/D1) and `worker_process_exit.rs` (the per-driver "is a vanished process a death?" classifier, which is a _consolidation_).
- **"At least three reapers take three different positions on whether reaping releases the cube lease."** Substantially true — I count at least five positions across `force_release`, `terminal_work_sweep`, the `mark_execution_orphaned` retain-for-resume family, `cube_lease_heartbeat`'s DB-fallback, and #2503's convergence reap. But they are not _incompatible_: there is a coherent implicit rule (the terminalizer owns the lease; inferred terminals retain it for resume), and the individual choices mostly follow it. The problem is that the rule is unwritten and the `abandoned` terminalizer does not follow it. See §6.
- **"PR #2500 reuses `dead_pane_sweep`'s pid-probe copy rather than #2503's new primitive."** See above — transitively fine. #2500 and #2503 are not in conflict.
- **"Pruned rows could explain `execution=-`."** Ruled out: retention is 14 days with a keep-5 floor (`work/execution_retention.rs:76`, `:80`), far outside the 24 h TTL. §2.6 gives a mechanism that does not require a crash.

## 8. Smaller findings

- **`live_worker_state.rs`** _(PR head)_: the new `awaiting_input_capable` getter was inserted between `set_awaiting_input_capable`'s doc comment and its signature, so the setter is now undocumented and the getter carries a doc that opens by describing the setter's no-op behaviour. Doc hygiene only.
- **The PR body names `get_execution_driver_slug`** as the source of the re-adopted driver; the code calls `driver_transcript::driver_for_execution` (`app/readoption.rs:184` _(PR head)_). Cosmetic, but it is the kind of drift that makes a body un-greppable later.
- **`orphan_sweep::OrphanSweepOutcome`** gained `#[derive(bon::Builder)]` _(PR head)_ with a comment explaining it exists only to satisfy the >5-field convention while production still uses `Default::default()`. Correct per the repo rule, and the comment is the right way to note the tension.
- **`app/readoption.rs:296`** _(PR head)_: `hosted_pane_slot_for_run` issues a `ListHostedPanes` round trip with a 5 s timeout per convergence. The latch bounds this to one in flight per run, but N stranded runs converging in the same sweep pass produce N sequential round trips inside `orphan_sweep`'s loop. Worth a glance if the state scan in §4.1 lands, since it will process more candidates per pass.

## 9. Follow-ups

Recorded separately as proposals for the operator to file; listed here so the document is self-contained.

1. Make convergence a durable-state scan in `dead_pane_sweep` and delete the `orphan_sweep` trigger (§4.1).
2. Give terminal-executions-holding-a-lease an owner in `terminal_work_sweep`, and fix the two producers (§4.3).
3. Corroborate the lease-time occupancy guard with the durable pid (§4.4) — or fold it into #2503.
4. Unify the two husk-sparing predicates (§4.5).
5. Delete the duplicate `process_is_alive`/`pid_is_alive` pair and route raw `kill` sites through `probe_pid` (§4.6).
6. Write down the lease-ownership rule in `tools/boss/docs/worker-liveness-contract.md` alongside the liveness rule (§6).
