# Ghost worker slots, slot/exec/pane desync, and hookless runs wedged in `doing`

**Status:** investigation (no code changes in this PR)
**Date:** 2026-06-01
**Subsystem:** `tools/boss/engine` — worker dispatch, slot/pane registry, hook ingestion, reapers, cube lease integration
**Trigger:** a live cluster of worker-tracking anomalies captured 2026-06-01 (symptoms reproduced below)

This writeup reasons **only from the engine source** (`tools/boss/engine/core/src/…`) plus the symptom list. The runtime traces that recorded the live events (`engine-trace.jsonl`, per-execution `dispatch.jsonl`, `engine-audit.log`) live under the coordinator-only Boss data dir and were **not** read. Where a trace excerpt would change the conclusion, the exact events needed are listed in [§7](#7-what-runtime-trace-evidence-would-disambiguate).

---

## 0. The observed symptoms (the spec to explain)

1. **Hookless ghost run.** `bossctl agents list` shows slot 1 "Riker" = run `exec_18b514031ed9e100_a7`, `activity=waiting_for_input`, `work_item=task_18b514014d34ec40_a3` (T1159, a "Fix failing CI: buildkite/mono/checks" revision on parent T1021 / PR mono#1202). But `agents status` reports `shell_pid=0`, and `agents transcript` errors with *"engine has not yet received a hook event carrying transcript_path"* — i.e. the engine never received **any** hook for this run, yet it shows a definite activity state. T1159 is wedged in `status=doing` and is never reaped.

2. **Slot→exec mapping desynced from the actual pane.** The operator's on-screen "Riker" pane is running a **different** execution, `exec_18b515ba1c238f20_ae`, which does **not** appear in `agents list` at all. That exec is a legitimate CI-remediation worker for a different PR (mono#1203 / T1150): its transcript shows `cwd=mono-agent-053`, it ran `gh pr view 1203`, pushed to `boss/exec_18b512af7472f598_69`, and called `boss engine ci classify --attempt-id cir_18b515b6968c8978_aa`. So one pane shows `…ae` while the engine's slot record for that pane says `…a7`.

3. **Workspace reuse across execs.** `…ae` ran in workspace `mono-agent-053`; that workspace is now leased to a third execution `exec_18b518c405426c38_e6` (O'Brien, T1168, also a mono#1203 revision). `…ae` predates `…e6` and no longer holds a lease, so this is *plausibly* sequential warm-reuse — but the question is whether two executions can ever **operate** in the same workspace concurrently.

ID creation order (Boss IDs are time-prefixed): `…a7` (`18b51403`) **<** `…ae` (`18b515ba`) **<** `…e6` (`18b518c4`).

---

## 1. TL;DR

There is **one underlying root cause** with **three distinct surface bugs**.

**Root cause.** Worker identity — the `run ↔ slot ↔ pane ↔ lease` binding — is held only in **volatile in-memory maps** that are (a) never reconciled against the two sources of ground truth (the macOS app's actual panes and the OS process table), and (b) only re-correlated to incoming hooks through a map (`run_to_slot`) that is wiped on engine restart and removed on release. Every liveness/reap decision keys off a signal (`shell_pid`, `activity==Working`, `last_event_at`, pool-claim membership, DB status) that **degrades to "treat as live / skip"** exactly when no hook ever arrived or when `shell_pid == 0`.

- **Symptom 1** is the reapers' blind spot: a run with `shell_pid=0`, no hook ever, and a synthetic `WaitingForInput` activity matches the skip predicate of *every* sweep.
- **Symptom 2** is the hook→slot fan-out depending on `WorkerRegistry::slot_for_run`, which is wiped at restart and removed on release, combined with the total absence of any reconciler that compares `by_slot`/the pool against the app's real panes.
- **Symptom 3**: cube guarantees lease **exclusivity** (two execs can never *hold* a lease at once), but lease release is **decoupled** from a guaranteed process kill. A stale process can keep *operating* in a workspace after its lease is released and re-leased — co-occupancy of *processes*, not of *leases*.

The three are facets of the same identity/reconciliation gap, but each needs its own targeted fix ([§8](#8-recommended-fixes)).

---

## 2. Background: the registries and the hook path

### 2.1 Three in-memory identity registries (none persisted)

| Registry | Keyed by | Holds | Set at | Cleared at |
|---|---|---|---|---|
| `WorkerPool` (`coordinator.rs:601-925`) | `worker_id` (`worker-N` ⇄ slot N, `slot_id_from_worker_id` / `worker_id_for_slot`, `coordinator.rs:902-949`) | `execution_id`, `last_workspace_id` | `claim_worker` (`coordinator.rs:691`) | `release_worker` / `release_worker_if_execution` (`coordinator.rs:790, 823`) |
| `WorkerRegistry` (`worker_registry.rs:26-150`) | `pid_to_run` (pid→run), `run_to_slot` (run→slot) | run↔pid↔slot | `register` / `register_run_slot` (spawn flow) | `unregister` (pid exit), `take_slot_for_run` (release) |
| `LiveWorkerStateRegistry` (`live_worker_state.rs:33-359`) | `by_slot` (slot→`LiveWorkerState`) | `activity`, `shell_pid`, `run_id`, `execution_id`, `last_event_at`, … | `register_spawn` (`live_worker_state.rs:69`) | `release_slot` (`live_worker_state.rs:86`) |

`bossctl agents list` reads **only** `LiveWorkerStateRegistry::snapshot()` (`app/panes.rs:192-209` → `live_worker_states_snapshot`). The persona name ("Riker", "O'Brien") is **derived from `slot_id`** (`name_for_slot`), not stored per run. So "Riker" ≡ slot 1, always.

All three registries are plain in-memory structures. There is **no durable slot↔exec table**; on engine restart they start empty (`run_reconcile.rs:1-16`: *"When the engine restarts it has no in-memory live-worker state… empty until workers send their first hook event."*).

### 2.2 The spawn write-order

`start_worker` (`spawn_flow.rs:202-386`) does, in order:

1. Write workspace files, build the env allowlist. The env includes `BOSS_RUN_ID` (`spawn_flow.rs:246-259`) — the worker's `boss-event` shim stamps this as `_boss_run_id` on every hook, *"so the engine can correlate hook events to runs without depending on a working shell-pid lookup. `proc_listpids` in the app side is still a TODO, and without it `WorkerRegistry`'s pid map stays empty…"*
2. Send `SpawnWorkerPane{slot_id, run_id, …}` to the app; receive `SpawnWorkerPaneResult{slot_id, shell_pid}` (`spawn_flow.rs:306-333`). **The app's `shell_pid` may be `0`** (the `proc_listpids` TODO, `spawn_flow.rs:352-360`).
3. `register_run_slot(run_id, slot_id)`; `register(shell_pid, run_id)` **only if `shell_pid > 0`** (`spawn_flow.rs:349-361`).
4. `register_spawn(slot_id, run_id, model, shell_pid, binding)` → `by_slot[slot_id]` with `activity = Spawning`, `last_event_at = None` (`spawn_flow.rs:366-373`, `live_worker_state.rs:69-82`).

Steps 3–4 land **before any hook fires** and are accepted **optimistically** — the engine never confirms the pane is actually alive or that the slot it asked for was free.

### 2.3 The hook → activity path

Hooks arrive over the events socket and land in `dispatch_live_worker_state` (`app/worker_events.rs:27`). The relevant control flow:

- The run id comes from `_boss_run_id` (or a peer-pid ancestor walk; `worker_registry.rs:112-123`). If absent, the hook is dropped (`worker_events.rs:40-48`).
- `transcript_path` is persisted to `work_runs` **before** the slot lookup, keyed only on `run_id` (`worker_events.rs:52-145`). This persist was *deliberately moved ahead of the slot lookup* to survive "an engine restart that wipes the in-memory `WorkerRegistry` while pre-existing workers keep firing hooks" (`worker_events.rs:62-70`).
- **The live-state fan-out is gated on `worker_registry.slot_for_run(run_id)`** (`worker_events.rs:146-153`). If the run is not mapped to a slot, the hook is dropped here — `apply_event` never runs, so `by_slot` is never updated.

`apply_event` (`live_worker_state.rs:155-225`) is the **only hook-driven** activity writer:

| Hook | activity becomes |
|---|---|
| `SessionStart` (Startup, from `Spawning`) | `Idle` |
| `UserPromptSubmit` / `PreToolUse` / `PostToolUse` | `Working` |
| `Notification` | `WaitingForInput` (+ sets `notification_pending`) |
| `Stop` | `WaitingForInput` if a notification was pending, else `Idle` |
| `SessionEnd` | `Terminated` |

There are **two non-hook** activity writers:

- `mark_errored` → `Errored` (events-socket decode failure; `live_worker_state.rs:285`).
- **`mark_stalled_spawns` → `WaitingForInput`** (`live_worker_state.rs:319-349`). This is the crux of symptom 1.

---

## 3. Symptom 1 — the hookless ghost run (`…a7` / T1159)

### 3.1 What sets `activity=waiting_for_input` with zero hooks

`mark_stalled_spawns` runs on a **10-second** timer (`app/server.rs:789-823`) and flips any slot that is **still `Spawning`** with **`last_event_at == None`** (no hook ever) and **spawned more than 30s ago** to `WaitingForInput`:

```rust
// live_worker_state.rs:328-346
if state.activity != WorkerActivity::Spawning { continue; }
if state.last_event_at.is_some() { continue; }          // a real hook already fired
let Some(&age_secs) = spawned_at.get(slot_id) else { continue; };
if age_secs > cutoff { continue; }                       // still fresh
state.activity = WorkerActivity::WaitingForInput;        // ← set WITHOUT any hook
state.last_event_at = Some(iso8601_utc(now_epoch_secs)); // ← SYNTHETIC timestamp
```

This is intentional: Claude Code's initial directory-trust prompt fires **before** `SessionStart`, so a headless worker blocked on it emits no hook at all and the normal `Notification→WaitingForInput` path never triggers (`live_worker_state.rs:299-318`, `app/server.rs:789-797`). The sweep paints the slot `WaitingForInput` so the kanban dot / waiting indicator fire.

So the answer to *"what sets activity in the absence of hooks?"* is **the stalled-spawn sweep, not a `Notification` hook**. Note the side effect: it **stamps a synthetic `last_event_at`** — the run now *looks* like it received a hook for any check that only tests `last_event_at.is_some()`, even though no real hook ever arrived (this matters in §3.3).

### 3.2 Why `agents transcript` errors and `agents status` shows `shell_pid=0`

`resolve_transcript_for_tail` (`app/handler_helpers.rs:1091-1133`) tries, in order: the transcript-path cache, `work_runs.transcript_path`, `work_executions` transcript path — all empty, because **no hook ever carried a `transcript_path`** (the persist at `worker_events.rs:99-145` never ran). It then checks `is_run_live(run_id)`:

```rust
// handler_helpers.rs:1125-1133
let is_live = server_state.live_worker_states.is_run_live(run_id);
if is_live { return TranscriptResolution::Buffering; }   // ← this branch
```

`is_run_live` returns `true` because `by_slot[1]` exists with `activity = WaitingForInput`, which is **non-terminal** (`live_worker_state.rs:138-144, 366-371`). `Buffering` produces the exact observed error (`app/executions.rs:665-674`). Because the stall sweep flipped the slot to a non-terminal activity, the engine will report "buffering, retry in a few seconds" **forever**.

`shell_pid=0` is the value stamped at `register_spawn` and never updated — the app never reported a real pid (the `proc_listpids` TODO, `spawn_flow.rs:352-360`). This single fact (no real pid) is what later disarms the dead-PID reaper.

### 3.3 Why T1159 stays in `doing` and is never reaped

Every sweep skips this row:

| Sweep | Cadence | Skip predicate that fires for `…a7` | Cite |
|---|---|---|---|
| `dead_pid_sweep` | 60s | `state.shell_pid <= 0` → `unknown_pid_skipped` | `dead_pid_sweep.rs:131-135` |
| `stale_worker_sweep` | 60s | `state.activity != Working` (it's `WaitingForInput`) → `not_working_skipped` | `stale_worker_sweep.rs:161-169` |
| `orphan_sweep` | 60s | `…a7` is in `claimed_execution_ids()` (pool claim never released) → treated as live, not redispatched | `orphan_sweep.rs:129`, predicate `claimed.contains(id)` |
| `pool_claim_sweep` | 60s | only releases claims for **terminal** executions; `…a7` is non-terminal | `pool_claim_sweep.rs` (terminal-status gate) |
| `run_reconcile` | startup only | lease-based; not a steady-state reaper | `run_reconcile.rs:92-215` |

Note the trap in `stale_worker_sweep`: even though the synthetic `last_event_at` from §3.1 *would* pass the "no hook yet" guard (`stale_worker_sweep.rs:181-186`), the sweep never reaches it — it bails at the `activity != Working` gate first (`:166`). And `dead_pid_sweep` would be the natural catch for a stuck spawn, but `shell_pid=0` disarms it. The only transitions **out** of `doing` are a `Stop` hook (never arrives — no hook channel) or manual intervention. → **wedged indefinitely.**

### 3.4 Causal chain for `…a7`

```
dispatch (claim slot 1) ─▶ start_worker: SpawnWorkerPane(slot 1)
   app returns shell_pid = 0   (proc_listpids TODO)
   register_run_slot(a7, 1); register(pid) SKIPPED (pid 0)
   register_spawn(1, a7, shell_pid=0)  →  by_slot[1] = {Spawning, last_event_at=None}
        │
        ▼  no hook ever fires
   (blocked on directory-trust prompt, OR pane never launched / collided — see §4)
        │
        ▼  +30s, 10s stall sweep
   mark_stalled_spawns: by_slot[1].activity = WaitingForInput,
                        last_event_at = <synthetic>
        │
        ▼  observable state
   agents list:  slot 1 = a7, waiting_for_input
   agents status: shell_pid = 0
   agents transcript: Buffering ("no hook carrying transcript_path")  [forever]
        │
        ▼  reaper passes
   dead_pid (shell_pid≤0) skip · stale (≠Working) skip · orphan (pool-claimed) skip · pool_claim (non-terminal) skip
        │
        ▼
   T1159 wedged in `doing`, no live process, never reconciled.
```

---

## 4. Symptom 2 — slot↔exec desync from the actual pane

`agents list` shows `by_slot[1] = …a7`; the physical Riker (slot-1) pane is running `…ae`; `…ae` is absent from the list. Two facts from the code explain this:

### 4.1 Why `…ae` is **unlisted** despite being live and working

`…ae` is doing real work (its transcript persisted — that's why the operator can read it via `execution_transcript`), which means its hooks *are reaching the events socket*. But it is absent from `by_slot`. The only way that happens: **`worker_registry.slot_for_run("…ae")` returns `None`, so the fan-out is dropped at `worker_events.rs:146-153`** while `transcript_path` persistence (which runs earlier, `:52-145`) still succeeds. The transcript-persist-survives-but-live-state-doesn't asymmetry is the exact shape the comment at `worker_events.rs:62-70` documents.

`run_to_slot` loses `…ae`'s entry in exactly two code paths:

- **(A) Engine restart / `WorkerRegistry` wipe.** The map is in-memory; a restart empties it while the macOS app keeps `…ae`'s pane (and process) alive. `…ae` keeps firing hooks, but nothing re-registers its `run_to_slot` entry (only `start_worker` does, and `…ae` is not re-spawned). Every subsequent `…ae` hook persists its transcript and is then **dropped at the slot lookup** → `…ae` never re-enters `by_slot`. This is precisely the scenario `run_reconcile.rs:1-16` was written for, and the duplicate-dispatch incident it references (2026-05-07).
- **(B) Release-without-reap in a single engine lifetime.** `release_worker_pane` (`app.rs:1146-1246`) calls `take_slot_for_run(run_id)` first (`:1147`), which **removes** `run_to_slot[…ae]` (`worker_registry.rs:78-84`). It then relies on the app's pane teardown plus `reap_worker_process_tree(shell_pid, …)` to kill the process — but **that backstop is a no-op when `shell_pid == 0`** (`app.rs:1158-1166, 1203-1214`). If `shell_pid` was 0 (or the app teardown failed: unresponsive, wedged surface, detached child), `…ae`'s `claude` survives. Its later hooks then hit the same `slot_for_run → None` drop. The `#975` comment at `app.rs:1206-1211` describes this exact leak (*"the engine slot and the cube lease were freed but the worker's `claude` process kept running"*).

### 4.2 Why the engine's slot-1 record points at `…a7`

`by_slot[1] = …a7` requires a `register_spawn(1, …a7)` (`spawn_flow.rs:366-373`). That happens when slot 1 is **free in the pool** at the moment `…a7` is dispatched — which is true in both branches above: after a restart the pool is empty; after a release the slot was freed (`app.rs:1224-1227`). The engine sends `SpawnWorkerPane(slot=1)` to the app **while the app's slot-1 pane is still hosting `…ae`'s surviving process**. The engine treats the app's echoed slot as authoritative and registers `by_slot[1] = …a7` optimistically (`spawn_flow.rs:321-373`; the `debug_assert_eq!(slot_id, claimed_slot)` at `:339` is compiled out in release, so a slot mismatch is silently accepted). `…a7`'s spawn into an already-occupied pane produces no hook → it becomes the symptom-1 ghost.

### 4.3 Exact code conditions for the desync

1. The `run↔slot↔pane↔exec` binding lives only in volatile maps (§2.1); nothing persists it and **nothing reconciles it against the app's real panes**.
2. The hook→`by_slot` fan-out depends on `run_to_slot` (`worker_events.rs:146`), which is **wiped on restart** and **removed on release** (`take_slot_for_run`), with **no fallback** that rebuilds it for an already-running worker.
3. `register_spawn` is **optimistic**: the engine never verifies the target slot's pane is actually free before claiming `by_slot[slot]` (`spawn_flow.rs:321-373`).
4. `release_worker_pane`'s process kill is **best-effort and `shell_pid`-dependent** (`app.rs:1203-1214`), so a "released" slot can still be physically occupied when the next spawn is aimed at it.
5. **No sweep queries the app for ground truth.** `dead_pid_sweep` keys on PID liveness of the *engine-recorded* pid; `pool_claim_sweep` cross-checks the pool against `by_slot` and DB status (`pool_claim_sweep.rs`); neither asks "which `_boss_run_id` is actually in each pane?" So a `by_slot[1]=…a7` / pane=`…ae` divergence is never detected.

### 4.4 Most parsimonious reading

The **engine-restart** branch (A) explains all of symptom 2 *and* symptom 1 in a single event: the restart drops `…ae` from `by_slot` while its pane survives, the wiped pool lets a fresh `…a7` dispatch claim slot 1, and `…a7`'s spawn collides with `…ae`'s still-occupied pane → hookless ghost. Branch (B) (release-without-reap) produces the identical observable state without a restart. The two are distinguished only by trace evidence ([§7](#7-what-runtime-trace-evidence-would-disambiguate)); the code permits both and the fixes overlap.

---

## 5. Symptom 3 — workspace co-occupancy: definitive answer

**Can two executions ever HOLD a workspace lease at the same time? No.**
Cube enforces lease exclusivity. The engine leases via `lease_workspace_with_fallback` (`coordinator.rs:2343+`), and `run_reconcile` uses *"`state == "leased"` AND `lease_id` matches ours"* as its liveness oracle (`run_reconcile.rs:177-192`). A second lease on the same workspace would either fail or change `lease_id` — which the engine reads as "the previous holder is dead." There is no path that grants two concurrent leases on one workspace.

**Can two executions ever OPERATE in the same workspace at the same time? Yes — and this is the real hazard.**
Holding a lease and running a process in the working copy are **decoupled**. The lease is released by the engine on several end-paths:

| End path | Releases lease at | Process kill? |
|---|---|---|
| Normal completion (terminal `wait_state`) | `record_run_completion` → `release_workspace(lease)` (`coordinator.rs:3331-3348`) | separate `release_worker_pane` step (best-effort) |
| `waiting_human` / `waiting_review` / `waiting_merge` | **not released** — `RunWaitState::release_workspace()` false (`runner.rs:69-76`) | — |
| Mid-spawn cancel | `release_workspace` (`coordinator.rs:2817`) | — |
| Spawn error | `release_workspace` (`coordinator.rs:2948`) | — |
| Force-release / cancel-and-release | `force_release` → `release_workspace` (`completion.rs:2387`) | separate |

In **none** of these is the lease release gated on a confirmation that the prior worker's **process tree is dead**. The process kill is the separate, best-effort `release_worker_pane` path, whose reap backstop is a **no-op at `shell_pid=0`** (`app.rs:1203-1214`). Meanwhile re-lease is by **warm affinity** — `claim_worker` picks an idle slot whose `last_workspace_id` matches, **without checking the prior occupant exited** (`coordinator.rs:698-708`).

So the unsafe interleaving is:

```
exec A finishes/cancels in workspace W
   record_run_completion → release_workspace(W's lease)   [cube: W now free]
   release_worker_pane(A): app teardown + reap_worker_process_tree(shell_pid)
        └─ shell_pid == 0  ⇒  reap is a NO-OP;  A's `claude` survives in W's dir
        │
        ▼  (W is free in cube; A still running jj/gh/bazel in W)
exec B dispatched, prefers W (warm affinity) → claim_worker matches last_workspace_id
   lease W (cube grants — only one lease, held by B)
   start_worker in W
        │
        ▼
   A and B now BOTH operating in the same working copy of W  ← unsafe co-occupancy
```

The engine has two guards, but **both protect bookkeeping, not the filesystem**:

- `execution_superseded_in_workspace` (`work/dispatch.rs:515-536`) makes the completion handler **ignore a stale `Stop`** from A so it can't mis-release B's lease (`completion.rs:1053-1067`, `StopOutcome::SupersededInWorkspace`). It does **not** kill A.
- `purge_leaked_worker_hooks` (`worker_setup.rs:841`, called from `worker_setup.rs:1096`) strips A's stale `boss-event` hook registrations out of W so B's hooks aren't misattributed. It does **not** kill A.

**Verdict for the specific case.** `…ae` no longer holds a lease and `…e6` does, so the *lease* handoff is clean and sequential. Whether it is **safe** depends entirely on whether `…ae`'s **process** was actually reaped:

- If `…ae` had a real `shell_pid` and the app/backstop killed it → safe sequential warm-reuse.
- If `…ae` is the surviving ghost the operator still sees in a pane (consistent with it being **unlisted but alive** per symptom 2, i.e. released-without-reap) → `…ae` and `…e6` are **co-occupying `mono-agent-053`** right now, racing on the same jj working copy.

This is the same `#975` failure class the reap backstop was added to cover; the gap is that the backstop cannot act when `shell_pid == 0`. (Cross-references the prior T1089 "cancel-without-reap / redispatch-trusts-row-status" concern: redispatch trusts that a freed lease ⇒ a dead worker, which is only true when the reap actually succeeded.)

---

## 6. One root cause or distinct bugs?

**One root cause, three distinct bugs.** The shared root is the **identity/reconciliation gap**: the engine tracks `run↔slot↔pane↔lease` only in volatile maps, never reconciles them against the app's panes or the OS process table, and lets `shell_pid=0` / "no hook ever" silently degrade every liveness check.

- **Bug 1 (reaper blind spot):** a `shell_pid=0` + hookless + `WaitingForInput`(synthetic) + pool-claimed row matches the skip predicate of every sweep. Fixing requires a reaper that keys on "no *real* hook ever + no confirmed live pid."
- **Bug 2 (slot/pane desync):** hook→`by_slot` re-correlation depends on a wipeable/removable `run_to_slot`, plus optimistic `register_spawn`, plus no pane reconciliation. Fixing requires rebuilding/relinking `run_to_slot` from app ground truth and a pane-vs-registry reconciler.
- **Bug 3 (process co-occupancy):** lease release is not gated on confirmed process death, and re-lease doesn't verify the prior occupant exited. Fixing requires a death barrier before lease release/re-lease.

They will not be fixed by one patch, but they all trace to "in-memory identity, never reconciled, `shell_pid=0` everywhere."

---

## 7. What runtime-trace evidence would disambiguate

To pin the *exact* interleaving (especially restart-branch A vs release-branch B in §4) the coordinator can supply these from the data dir (worker must not read it):

1. **Engine restart marker between the three spawns.** `engine-audit.log` / `engine-trace.jsonl`: any engine boot / `build_info` / startup-reconcile (`run_reconcile`) event with a timestamp **between** `…ae`'s first hook and `…a7`'s `register_spawn`. Confirms branch A.
2. **`register_run_slot` / `register_spawn` events for slot 1.** The `dispatch.jsonl` lines (or `worker_pool_claim` log) showing which exec claimed `worker-1`/slot 1 and at what time — specifically whether `…a7` claimed slot 1 *after* `…ae` had been running there.
3. **`take_slot_for_run("…ae")`** — is there a `release_worker_pane` / "released worker pane" log for `…ae`'s run id (branch B), or none (branch A)?
4. **`…ae`'s recorded `shell_pid` and the reap outcome.** Whether `reap_worker_process_tree` ran with pid 0 (no-op) — i.e. the `spawn_flow.rs:357-360` "spawn returned shell_pid 0" warn for `…ae`, and the `release_worker_pane` log line for it.
5. **`SpawnWorkerPane(slot=1)` IPC + app response for `…a7`** (`ipc_log` / `dispatch.jsonl`): did the app echo slot 1, report a pid, or error — and was its pane already occupied?
6. **Dropped-hook counter for `…ae`.** `dispatcher_stats` / the `worker_events.rs:147-151` warn *"dropping hook fan-out — run_id is not registered against a slot"* for `…ae`'s id confirms the §4.1 mechanism directly.
7. **Cube `workspace list` snapshot + `ps` for `…ae`'s pid** in `mono-agent-053` — settles symptom 3: is `…ae`'s process still alive in `…e6`'s leased workspace?

---

## 8. Recommended fixes (pointers — not implemented here)

Targeted, smallest-blast-radius first:

1. **Close the `proc_listpids` TODO (highest leverage).** Have the app return a real `shell_pid` in `SpawnWorkerPaneResult` (`spawn_flow.rs:321-360`). This single fix re-arms **both** `dead_pid_sweep` (Bug 1) **and** `reap_worker_process_tree` (Bugs 2 & 3) for ghost/leaked workers. Until then, `shell_pid=0` is a silent reaper kill-switch.
2. **Make the reap backstop not depend on `shell_pid`.** When `shell_pid == 0`, `release_worker_pane` should ask the app to kill **by slot** and *verify* the pane is gone, and should **not** report `Reaped` (which frees the lease, `app.rs:1242-1245`) until death is confirmed. (`app.rs:1203-1245`.)
3. **Add a hookless-spawn reaper.** Either (a) have `mark_stalled_spawns` set a distinct flag (or leave `last_event_at = None`) instead of a synthetic timestamp, so a sweep can recognize "never received a *real* hook" (`live_worker_state.rs:344-345`); and (b) add a sweep that reaps executions stuck in `WaitingForInput`/`Spawning` with **no real hook ever** and **no confirmed live pid** past a threshold — the case all current sweeps skip (§3.3).
4. **Rebuild `run_to_slot` on restart from app ground truth.** Add an app→engine "here are my live panes and their `_boss_run_id`/slot" handshake at startup so pre-existing workers' hooks re-correlate and `by_slot` repopulates (`worker_events.rs:146`, `run_reconcile.rs`). Equivalently, fall back to an app-sourced `run_id→slot` lookup when `slot_for_run` misses, instead of dropping the hook.
5. **Add a pane-reconciliation sweep.** Periodically ask the app which `_boss_run_id`/exec occupies each slot and compare to `by_slot` and the pool; flag and repair divergences (kill orphaned panes, adopt or surface unlisted-but-live ones). This is the missing reconciler that would have caught symptom 2 directly. (New sweep alongside `dead_pid_sweep` / `pool_claim_sweep`.)
6. **Refuse to spawn into an occupied slot.** Before `register_spawn`, verify the target pane is free (or treat an app "slot occupied" response as a spawn failure rather than optimistically registering `by_slot[slot]`). Re-enable the slot-mismatch check beyond `debug_assert` (`spawn_flow.rs:339-342, 366-373`).
7. **Gate workspace re-lease on confirmed process death.** Don't allow warm-affinity re-lease of a workspace (`claim_worker`, `coordinator.rs:698-708`) until the prior occupant's process tree is confirmed gone — or don't release the lease (`record_run_completion`, `coordinator.rs:3331-3348`) until reap is confirmed. Closes the Bug 3 window.
8. **Surface unlisted-but-live panes in `agents list`.** Either source `agents list` partly from app ground truth, or have the dropped-hook path (`worker_events.rs:147-151`) raise an "orphaned live worker" signal instead of silently dropping, so an operator/coordinator sees `…ae` rather than nothing.

---

## 9. Appendix — code map

| Concern | File:line | Symbol |
|---|---|---|
| Slot persona name from slot id | `live_worker_state.rs` (`name_for_slot`) | `LiveWorkerState::new_spawning` |
| `agents list` data source | `app/panes.rs:192-209` | `handle_list_worker_live_states` |
| Spawn write-order (optimistic) | `spawn_flow.rs:202-386` | `start_worker` |
| `shell_pid=0` / `proc_listpids` TODO | `spawn_flow.rs:246-259, 352-360` | env `BOSS_RUN_ID`, pid-register skip |
| Hook ingestion + transcript persist | `app/worker_events.rs:27-153` | `dispatch_live_worker_state` |
| Hook→slot drop (the gate) | `app/worker_events.rs:146-153` | `slot_for_run` lookup |
| Hook-driven activity | `live_worker_state.rs:155-225` | `apply_event` |
| Non-hook activity (the ghost-maker) | `live_worker_state.rs:319-349` | `mark_stalled_spawns` |
| Stall sweep wiring (10s) | `app/server.rs:789-823` | spawned timer task |
| Transcript resolution / Buffering | `app/handler_helpers.rs:1091-1133`; `app/executions.rs:665-674` | `resolve_transcript_for_tail` |
| `is_run_live` (non-terminal ⇒ live) | `live_worker_state.rs:138-144, 366-371` | `is_run_live` |
| `run_to_slot` set/remove | `worker_registry.rs:55-95` | `register_run_slot` / `take_slot_for_run` |
| Pane release + best-effort reap | `app.rs:1146-1246` | `release_worker_pane` |
| Pool claim / affinity / release | `coordinator.rs:691-849` | `claim_worker` / `release_worker_if_execution` |
| dead-PID reaper (skips pid≤0) | `dead_pid_sweep.rs:130-201` | `run_one_pass` |
| stale-worker reaper (Working only) | `stale_worker_sweep.rs:161-231` | `run_one_pass` |
| orphan reaper (pool-claim oracle) | `orphan_sweep.rs:112-171` | `run_one_pass` |
| pool-claim reaper (terminal only) | `pool_claim_sweep.rs` | `run_one_pass` |
| startup lease reconcile | `run_reconcile.rs:88-215` | `probe_in_flight_runs` / `classify` |
| restart redispatch (`is_live` gate) | `work/dispatch.rs:206-275` | `reconcile_active_dispatch` |
| lease release on completion | `coordinator.rs:3323-3399` | `record_run_completion` |
| waiting states retain lease | `runner.rs:69-76` | `RunWaitState::release_workspace` |
| superseded-in-workspace guard | `work/dispatch.rs:515-536` | `execution_superseded_in_workspace` |
| leaked-hook purge on reuse | `worker_setup.rs:841, 1096` | `purge_leaked_worker_hooks` |
