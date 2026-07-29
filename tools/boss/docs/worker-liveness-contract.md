# Worker liveness: what the agent indicator derives from, and how the engine converges

This document states a contract that had never been written down. Three layers each looked locally correct while disagreeing with each other, which is why "several tasks have attached agents but are showing the yellow dot or no agent icon" was so hard to attribute: no layer was obviously wrong.

## The three layers

| Layer                   | Lives in                                                                  | Lifetime                                                                                    | What it actually knows                                                                                    |
| ----------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| **Durable state**       | `work_executions.status`, `work_runs.shell_pid`, `work_runs.host_id`      | Survives engine restart, survives the execution going terminal                              | The recorded facts: what status the engine last wrote, and what pid the app reported for the pane's shell |
| **Derived bookkeeping** | `LiveWorkerStateRegistry`, `WorkerPool` claims, `WorkerRegistry` slot map | In-memory; empty after any engine restart; cleared unconditionally by `release_worker_pane` | The engine's _belief_ about what is running                                                               |
| **Presentation**        | `LiveWorkerState` on the wire → `AgentActivityState` in the macOS app     | Per broadcast                                                                               | Whatever the layer above told it                                                                          |

## The contract

**The agent indicator derives from `LiveWorkerState.activity`**, mapped by `AgentActivityState.init(runtime:liveState:)` in `Models+WorkerActivity.swift`. When no `LiveWorkerState` exists for the row, it falls back to `WorkTaskRuntime.executionStatus` — the DB projection.

Both inputs are engine-owned. **The app has no independent liveness signal and must not acquire one.** Painting from `bossctl agents list`-style polling, from pane existence, or from a `waiting_human` special case would each hide a disagreement rather than end it — the indicator would look right while the engine still could not see, stop, or avoid duplicating the worker.

So: **a wrong indicator is an engine bug, essentially always.** The app is a faithful renderer of what it is told.

### `live_status` is not the indicator's input

Two unrelated fields are named `live_status`, and neither drives the dot:

1. **`LiveWorkerState.live_status`** — a free-text one-sentence blurb ("investigating why the scroll handler doesn't fire"), produced only by the AI summarizer loop in `engine/core/src/live_status.rs`. It requires a utility-model credential and a transcript tail, and is legitimately `null` while a slot is spawning, before the first summary lands, or whenever the summarizer is unconfigured. It is rendered as the Doing-card _subtitle_.
2. **`live_status` in `bossctl dispatch diagnose` findings** (`doctor.rs`) — the _execution's DB status_, joined from `state.db` into SIG-2 finding details. It is `None` for every finding when the diagnose call had no `WorkDb` handle to join against, so a wholesale-null column there is a projection gap, not evidence about any worker.

A null `live_status`, in either sense, says nothing about whether a worker is alive. Reach for `LiveWorkerState.activity`, `work_executions.status`, and `work_runs.shell_pid` instead.

## Convergence: what happens when the layers disagree

The engine's belief can be wrong. Every path that writes `orphaned` or `abandoned` is _inferring_ death from the absence of a signal — no spawn ack, no pid, no pool claim, no hook — and a degraded network produces exactly that absence for a worker that is perfectly alive.

The rule, implemented in `engine/core/src/worker_readoption.rs`:

> A live worker for an execution the engine believes is dead must either be **re-adopted** or **reaped**, promptly and observably.

| Terminal status                                | Other live execution on the row? | Verdict                                                                                        |
| ---------------------------------------------- | -------------------------------- | ---------------------------------------------------------------------------------------------- |
| `orphaned` / `abandoned` (inferred)            | no                               | **Re-adopt** — withdraw the guess, restore tracking                                            |
| `orphaned` / `abandoned` (inferred)            | yes                              | **Reap** — the replacement owns the row; two workers on one row is the failure being prevented |
| `cancelled` / `completed` / `failed` (decided) | either                           | **Reap** — the record is authoritative, the surviving process is not                           |

The asymmetry is the safety argument: re-adoption rewrites a record and fails recoverably (the ordinary sweeps re-reap it), while reaping destroys in-flight work and cannot be undone.

### What triggers it

- **A hook for a terminal execution** (`app/worker_events.rs`). The strongest evidence available — produced by the worker's own process, in-band, and impossible for stale bookkeeping to forge.
- **The re-dispatch guard** (`orphan_sweep.rs`). Probes `work_runs.shell_pid` before creating a replacement execution. Covers the worker that is alive but quiet — parked inside a long foreground build, emitting nothing to converge on.

Both funnel into `ServerState::converge_terminal_execution`, serialized per run.

### What it emits

| Event                                                 | Meaning                                                                                                                                                                                                   |
| ----------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `live_worker_readopted`                               | A run came back out of a terminal status. `details.trigger` names the detector; `details.prior_status` names the guess that was withdrawn.                                                                |
| `husk_pane_reconcile` with `details.verdict = "reap"` | A surviving worker was torn down. `details.reason` distinguishes `terminal_by_decision` from `superseded_by_live_execution`.                                                                              |
| `redispatch_blocked_live_process`                     | A duplicate worker was prevented. `details.blocking_execution_status` is normally TERMINAL — that is the point, since a terminal row is what every bookkeeping-based guard reads as "safe to redispatch". |

Plus one greppable trace line per direction: `execution terminalized: …` and `execution re-adopted: inferred death disproven by a live worker`.

## Rules of thumb for future work here

- **Never make a liveness decision from derived bookkeeping alone** when the decision is irreversible (killing a worker) or duplicating (spawning a second one). Corroborate with `work_runs.shell_pid` via `engine/core/src/durable_liveness.rs`.
- **`Unknown` is not `Gone`.** A worker with no recorded pid is mid-spawn, not dead. `WorkerProcess::Unknown` and `PaneReleaseOutcome::NoLiveWorker` both exist to keep that case from being read as death — treating it as death reaps live-but-slow spawns and releases cube leases out from under workspaces a worker is about to occupy.
- **`EPERM` means alive.** `kill(pid, 0)` returning `EPERM` proves the process exists.
- **Don't lengthen a timeout to fix a contradiction.** A longer spawn-ack window narrows the race without closing it, and leaves the system with no answer for the times it still loses.
