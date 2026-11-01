# Worker liveness: what the agent indicator derives from, and how the engine converges

This document states a contract that had never been written down. Three layers each looked locally correct while disagreeing with each other, which is why "several tasks have attached agents but are showing the yellow dot or no agent icon" was so hard to attribute: no layer was obviously wrong.

## The three layers

| Layer                   | Lives in                                                                  | Lifetime                                                                                                                                                                                                                        | What it actually knows                                                                                                    |
| ----------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| **Durable state**       | `work_executions.status`, `work_runs.shell_pid`, `work_runs.host_id`      | Survives engine restart, survives the execution going terminal                                                                                                                                                                  | The recorded facts: what status the engine last wrote, and the tmux pane pid (`#{pane_pid}`) recorded at spawn / adoption |
| **Derived bookkeeping** | `LiveWorkerStateRegistry`, `WorkerPool` claims, `WorkerRegistry` slot map | In-memory; empty after any engine restart; cleared unconditionally by `release_worker_pane`. Rebuilt at boot (see below) before anything else gets a turn, from durable state rather than the app — tmux is the sole pane host. | The engine's _belief_ about what is running                                                                               |
| **Presentation**        | `LiveWorkerState` on the wire → `AgentActivityState` in the macOS app     | Per broadcast                                                                                                                                                                                                                   | Whatever the layer above told it                                                                                          |

## The contract

**The agent indicator derives from `LiveWorkerState.activity`**, mapped by `AgentActivityState.init(runtime:liveState:)` in `Models+WorkerActivity.swift`. When no `LiveWorkerState` exists for the row, it falls back to `WorkTaskRuntime.executionStatus` — the DB projection.

Both inputs are engine-owned. **The app has no independent liveness signal and must not acquire one.** Painting from `bossctl agents list`-style polling, from pane existence, or from a `waiting_human` special case would each hide a disagreement rather than end it — the indicator would look right while the engine still could not see, stop, or avoid duplicating the worker.

So: **a wrong indicator is an engine bug, essentially always.** The app is a faithful renderer of what it is told.

## The row must not disagree with the pane

The two inputs above are not peers. `LiveWorkerState.activity` is fed event-by-event from the worker's own hook stream and is the authority on moment-to-moment activity. `work_executions.status` is the durable projection of it, and it is what every other consumer reads — dispatch admission, the liveness sweeps, the reaper, attention surfacing, the kanban's fallback path, and every CLI/JSON reader. The merge-poller's staged-URL recheck also reads `activity` directly to gate revision finalization while a worker is `Working`: it defers a revision's staged PR URL only within `DEFAULT_STAGED_PR_MID_TURN_DEFER_SECS` of when the URL was staged, so a genuinely mid-turn worker still gets to record its own contributed head at its turn boundary; once that horizon expires (or the worker is not observed live), the staged URL finalizes on its own. A projection does not get to contradict what it projects, and liveness alone — outside that bounded window — does not double as a completion signal.

Until mono#2673 it did. `PaneSpawnRunner` stamped `waiting_human` on the row the instant the pane came up and nothing ever cleared it, so a worker mid-`Bash`-call stored `waiting_human` for its entire life:

```text
bossctl agents:  slot 4  activity: working  tool: Bash   last_tool_end: 35s ago
work_executions: exec_…   status: waiting_human          finished_at: null
```

Two live surfaces, flatly contradicting each other, with the wrong one being the one everything downstream reads. Anything hunting stuck workers could not see a working worker; anything hunting workers that need an operator saw every worker in the system.

The two statuses now mean what they say, and both are `is_live()`:

| Status          | Meaning                                                                                                               | Written by                                                                                 |
| --------------- | --------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `running`       | The agent is working — the engine is inside `run_execution`, or the pane is up and its agent owns the turn loop.      | The dispatch path (`RunWaitState::WorkerPaneAlive`), and `awaiting_input_status` on resume |
| `waiting_human` | The agent is blocked on a person: it emitted its driver's awaiting-input signal and will not progress until answered. | `awaiting_input_status` only                                                               |

`waiting_human` has exactly one writer and exactly one clearer, both in `engine/core/src/awaiting_input_status.rs`, driven off the `WorkerActivity::WaitingForInput` transition on the ordinary hook-dispatch path. That signal is the driver's own notification (Claude's `Notification` hook), honoured only for a driver declaring `Capability::AwaitingInputSignal` — never inferred from tool timing or elapsed silence. The write is guarded in SQL to the `running` ⇄ `waiting_human` pair, so a late hook can never drag a settled row (terminal, `waiting_review`, `waiting_merge`) back into a live status.

### A worker is busy for the whole turn, including while it waits on its own background command

`activity` is `Working` from the first turn activity the driver reports (`UserPromptSubmit`, `PreToolUse`, `PostToolUse`) until the driver reports the turn boundary (`Stop`). Nothing in between lowers it: not a balanced tool call, not a long think, not minutes of silence. `Idle` means one thing — the driver said the turn ended — and the only honest way to get there is a `Stop` the driver actually emitted.

Two driver facts make this matter more than it sounds:

- **A Codex background-terminal wait is silent by design.** The rollout translator (`engine/driver/src/codex/progress.rs`) announces a command once, when its cell starts (`PreToolUse`), and treats every `wait`/`write_stdin` poll of that cell as a continuation of the announced command: no `PreToolUse` for the poll, no `PostToolUse` while the command is still running. `• Waiting for background terminal (12m 49s • esc to interrupt)` on the TUI is a chain of 30-second polls inside one turn, and the engine sees no event for any of them. The command's own `PreToolUse` is the last tool-boundary event, so the slot stays `Working` with `current_tool` set — correctly — until the command's real output or the turn's `task_complete` arrives. A Claude worker's equivalent is a long foreground `Bash` call (one `PreToolUse`, then nothing); a Claude worker that backgrounds a command _ends its turn_ (`Stop`) and is woken by Claude Code when the command finishes, so `Idle` there is the truth, not this bug.
- **The durable checkpoint carries the turn, not just the tool.** `work_runs.semantic_tool_condition` (`in_flight` / `idle` / `unknown`) answers only "is a tool call unbalanced right now?", which is `idle` for most of a live turn — between tools, during a think, and for the whole of a background wait whose polls emit nothing after an earlier command completed. `work_runs.semantic_turn_condition` (`in_turn` / `ended` / `unknown`, schema 50) mirrors the reducer's turn gate: set to `in_turn` by the main session's turn-activity events, to `ended` by `Stop`, and left alone by everything else. Both columns are persisted from the reducer's latest post-apply state, captured under a dedicated persistence lock (the registry mutex is released before the database write, so the registry and database locks are never nested) (`WorkDb::record_semantic_checkpoint`, `tools/boss/engine/work-db/src/semantic_progress.rs`), so an event the reducer refuses as stale (a `Stop` that arrives after the next turn's tool hook) or does not count (a subagent's tool hooks) never changes them. Both are restored to the slot by `LiveWorkerStateRegistry::seed_semantic_progress` on re-adoption, where the turn condition decides busy-vs-idle: `in_turn` seeds `Working`; `ended` seeds `Idle`; a row from before the column (`unknown`) seeds `Working` only when a tool was in flight and otherwise leaves the slot unknown (`Spawning`) until the next driver event. A balanced last tool call is not evidence that the turn ended, so no `Idle` is inferred from it. This is a one-time gap at upgrade: a worker that was mid-turn in a silent background wait remains unknown (`Spawning`) until its next driver event, for either driver. Checkpoint-seeded slots are exempt from startup-stall promotion, so the 30-second timer cannot turn this uncertainty into `Idle` or `WaitingForInput`.

A re-adopted slot seeded from the tool condition alone reads `Idle` over a worker that is mid-turn in a background wait: the last tool boundary was an earlier command completing, and the polls since emit nothing. Seeding from the turn condition, and re-establishing the rollout tail on adoption (below), is what keeps the pane header at `Working` for such a worker.

A pane going `Terminated`/`Errored` is deliberately NOT mirrored. That is a death signal, and death detection has its own owners with their own corroboration rules — see Convergence below.

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

### A file-tailing driver's "hook" is the engine attaching to its rollout

For a driver whose progress ingress is a file the engine tails (Codex: `ProgressIngress::AgentJsonlFile`), there is no `boss-event` shim and no `events.sock` traffic at all — its settings file carries an empty `hooks` map by design. Every `WorkerEvent` the engine attributes to such a run, and therefore `driver_signal_at`, comes from `engine/core/src/agent_jsonl_progress.rs` discovering the run's rollout under its per-run sessions directory and tailing it. Missing events can reflect absent rollouts, discovery that never attached, or a failure after attachment in reading, dispatch, or persistence. `work_runs.transcript_path = NULL` only means no transcript path was persisted. Use `IngressCheckpoint::Armed` versus `Attached` to distinguish discovery from attachment, and the discovery record for its verdict; absence of historical checkpoint evidence leaves that stage uncertain.

Discovery has no give-up point of its own: it runs until the run is torn down or hits a failure polling cannot cure, and at `DISCOVERY_OVERDUE_AFTER` (120 s) it records itself overdue — on the run's `IngressCheckpoint::Armed` as a `DiscoveryRecord`, and on the dispatch timeline as `file_ingress_discovery_overdue` — and keeps polling once per second, refreshing the checkpoint when rejection diagnostics change. A successful attach emits `file_ingress_attached` with `discovery_secs`; a terminal failure emits `file_ingress_discovery_failed`. The driver-start reap derives its narrative, attention body and error-log reading from that checkpoint. With file ingress, its attention title says "Worker produced no driver signal", and it directs investigation toward ingress diagnostics; an overdue or failed record says the driver may have started and run unobserved. The former 120-second discovery deadline preceded the 300-second reap by 180 s. In the incident, four silent Codex rollouts appeared at 123–126 s, consistent with missing that deadline; three were reaped and one completed before reap. A fifth at 76 s remains unresolved — see [the investigation](investigations/chore-hook-silence-breaker-incident-2026-09-13.md).

### Boot-time tmux adoption: the other half of "empty after restart"

`worker_readoption` resolves a live-vs-terminal contradiction whenever it is
detected — a hook, or the redispatch guard. It never runs unprompted, so a
tmux-hosted worker whose execution is still **non-terminal** (the row was
never wrong) would otherwise sit invisible to `bossctl agents list` from the
moment the engine restarts until it happens to hook. `engine/core/src/tmux_adoption.rs`
closes that gap, once at boot and periodically thereafter, before
`run_reconcile` gets a turn on the boot pass: it enumerates the private
`boss` tmux server, exact-matches each live session's authoritative
`BOSS_SPAWN_TOKEN` (`show-environment`, never the `@boss_spawn_token` option
mirror) against the non-terminal tmux-tracked `work_runs` rows, and rebuilds
the pool slot claim, `WorkerRegistry` entry, `LiveWorkerState` entry
(seeded from the durable semantic-progress checkpoint, turn condition
first — see "busy for the whole turn" above), live-status summarizer, and
**progress ingress** for every match. The last of those is what keeps the
rebuilt slot from freezing at its seeded activity: a hook-callback driver's
shim reconnects to the events socket on its own, but a file-tailing driver
(Codex) is observed only through the rollout tail the previous engine
process held, and that tail died with it. Adoption therefore calls the same
`readopt_progress_ingress` the terminalized-survivor path uses
(`WorkerSpawner::reestablish_progress_ingress`), resuming the tail at the
recorded byte or filing the "Re-adopted worker has no progress ingress"
attention item when it cannot; the `tmux_adopt` dispatch event records the
outcome as `details.progress_ingress`. Without this step every engine restart
would leave every live Codex worker unobserved for the rest of its run. The
periodic sweep re-runs adoption for every live session, so a run whose slot
this process already holds is skipped (`details.progress_ingress =
already_observed`): its tail is already in place, and re-establishing it
would only repeat failures. A matching session whose worker pane
is dead (`#{pane_dead}=1`) is not a live worker: `remain-on-exit` keeps the
session listed after the command has exited, and that is terminal evidence.
The sweep records the pane exit status (and a pane-capture snippet when
readable), terminalizes a still-live execution once, and does not hand the
session to re-adoption — a dead pane must not disprove an inferred death.
A session whose token instead resolves to a **terminal** execution and whose
pane is still live is handed to `worker_readoption` exactly as above, via
the trigger `tmux_session_sweep`, and emits `tmux_adopt` (rebuilt) or the
usual `live_worker_readopted` / `husk_pane_reconcile` events (handed off)
accordingly. A session whose token resolves to no row at all is left for
`engine/core/src/husk_pane_sweep.rs`'s periodic two-pass reap instead of
being handled here.

### Retained worker exits

Confirmed session loss is reconciled independently of the last transcript
event and the activity it left behind: idle and waiting workers can die too.
The engine marks the pane terminated before asynchronous teardown, preserving
the final driver timestamp and summary. A surviving recorded PID still blocks
destructive cleanup for an absent session or mismatched token; it does not
make that missing pane display as working.

The `observed_dead` fence drops buffered hooks until same-run re-adoption or
a later `Alive` inspection restores the activity saved before termination,
falling back to semantic evidence only when no activity was saved. Without
either, activity is unchanged.
Ordinary buffered hooks never clear it. Inferred death that still has a live
PID skips the live-status refresh without fencing hooks or stopping the
status loop; the stale-worker sweep still uses a reversible terminated
display. Confirmed death (an observed pane exit, or inferred absence
whose PID is gone) sets the fence, parks the live-status loop, and stays
eligible for the ordinary death and pool-claim sweeps so the registry entry
and pool claim are released even when the refresh guard noticed first. A
later Alive inspection or same-run re-adoption un-parks the loop on its next wake.
Only engine-inferred or observed-dead terminations can be revived this way;
a driver SessionEnd remains under the authority of the hook stream.

The live-status timer and the stale-worker sweep share one confirmed-death
predicate. The timer checks process/session liveness before using a cached
transcript and again before publishing a model response. Confirmed death
stops the loop, and the registry rejects summary writes after termination.
The app renders that activity in the header and the exited body; it does not
infer death from summary age or viewer attachment.

Before Boss creates its first private-server window, it starts the server and
sets the global `history-limit=2000` and `remain-on-exit=on` defaults. The
former makes scrollback a bounded diagnostic convenience; driver JSONL remains
the durable transcript. The latter keeps an exited worker pane available long
enough for the real tmux wrapper to read `#{pane_dead}` and
`#{pane_dead_status}`. The stale-worker reconciler records that observed exit
status, and the ordinary release path removes the retained session only after
its live `BOSS_SPAWN_TOKEN` exactly matches the durable run identity.

## Hook events are applied in the worker's emission order

`LiveWorkerState.activity` is reduced from the hook stream by
`LiveWorkerStateRegistry::apply_sequenced_event`
(`engine/core/src/live_worker_state.rs`), and the reducer is simple:
`UserPromptSubmit` / `PreToolUse` / `PostToolUse` → `Working` (with the
tool on `PreToolUse`), `Stop` → `Idle` (or `WaitingForInput` when a
`Notification` preceded it), `SessionEnd` → `Terminated`. There is no
"turn gate": a `PreToolUse` after a `Stop` is a new turn, whether or not a
`UserPromptSubmit` came first. It must be, because Claude Code wakes a
stopped session **on its own** when a background command finishes, and
that resumed turn opens with tool hooks and no prompt.

What can go wrong is ordering. The events-socket accept loop
(`engine/core/src/app/events_accept.rs`) dispatches every connection on its
own task, so two hooks the worker emitted in order can reach the reducer in
either order — and the race is not symmetric. A turn boundary persists the
transcript path and folds the transcript into the run's cost snapshot (a
bounded settle loop with sleeps, plus transcript reads and DB writes whose
latency is not bounded by those sleeps) on its way to the reducer; a
`PreToolUse` goes almost straight there. When a `Stop` is slow and the
re-woken turn's first `PreToolUse` is fast, the `Stop` lands second and
rewinds the slot to `Idle` with no tool — and it stays that way until the
next hook, which for a multi-minute `checkleft run` is the whole run.

The 2026-10-15 "Idle while visibly running `checkleft`" incident is
**consistent with** that race but the race is not proven to be its cause:
the engine log lines for the incident window (`hook event received`, `hook
payload arrived at dispatcher`, `dropping hook fan-out`) were not available
when the fix was written. The settle loop's sleeps alone (~200ms) are far
shorter than the minutes the pane stayed Idle, so a delay that long would
have to come from elsewhere in the `Stop`'s ingress (transcript read, DB
lock contention). Another unruled-out cause is the shim's resilience path:
after its roughly ten-second connection retry budget, it appends the hook
to `.boss/events-pending.jsonl`, exits successfully, and retries delivery
only when another hook fires. A buffered `PreToolUse` may therefore arrive
with `PostToolUse` after the entire long command. Check shim stderr for
`buffering event for later delivery` and the pending file for that payload.
An engine `hook event received` followed by `dropping hook fan-out`
distinguishes a slot-less drop from shim buffering; `hook payload arrived
at dispatcher` confirms dispatch. Missing receipt alone cannot distinguish
buffering from a hook that never fired. Every `Stale` refusal logs
`ingress_seq` and `last_applied_seq`, proving reducer reordering, but not
by itself proving the cause of the historical incident. The incident's
engine logs and original pending buffer require coordinator investigation;
neither delivery failure nor slot-less dropping is repaired by ordering.

Three things close the ordering hole:

- **Accept order is emission order, and the reducer honours it.** The
  `boss-event` shim connects synchronously from inside the driver's hook,
  so the order the accept loop sees connections is the order the worker
  emitted them. The loop stamps that order as `ingress_seq` before spawning
  the task; the registry remembers the newest sequence it applied per slot
  (`LiveWorkerStateRegistry::apply_hook_event`) and treats an older event as
  late (`ApplyEventOutcome::Stale`). A late event does not touch `activity`,
  `current_tool` or the display timestamps, and does not trigger the
  live-status loop's `Stop`/`PostToolUse` refresh. Hook events are
  incremental, though, so late events still contribute: a late
  `Notification` repairs only an Idle slot whose newest `Stop` was its
  first clearing event. An earlier `PreToolUse` or `Stop` consumes it;
  Errored and Terminated slots are never repaired to waiting. A late
  `Stop` reports the turn boundary it opened (below), and stale main-session
  activity can consume an older boundary without changing the display.
  Liveness bookkeeping (`record_driver_signal`) already happened at ingress.
  Unsequenced callers (the Codex stdout reader, engine-synthesized events,
  tests) keep the always-apply `apply_event`.
- **The live-state transition and its durable projection run before the
  slow half of ingress.** `dispatch_live_worker_state`
  (`engine/core/src/app/worker_events.rs`) applies the event, broadcasts,
  and mirrors the awaiting-input state onto the execution row, and only then
  persists the transcript path and captures cost. The row write is decided
  from the pane's activity at write time, inside a gate that serializes the
  writes, so an older handler that stalls can never leave `waiting_human`
  over a pane that has since moved on.
- **A declared wait ends on turn ordering, not on the activity the slot
  showed.** A wait (`boss propose wait`) describes the idle period that
  follows the declaration. The registry tracks the turn boundary (`Stop`)
  the main session is parked behind and reports when the main session
  starts a new turn after it (`HookApplied::resumed_after`): the first
  main-session `UserPromptSubmit`/`PreToolUse`/`PostToolUse` after the
  `Stop`, or the late `Stop` itself once main-session activity numbered above
  it has already been applied. The ingress then calls
  `WaitRegistry::end_if_declared_before`, which ends the wait only if it was
  declared before that boundary (the wait records the newest accepted hook
  sequence, including hooks still outstanding), so a late `Stop` cannot end a wait declared in the turn
  that overtook it. Hooks stamped with an `agent_id` (a backgrounded
  subagent's tools) are not main-session activity, a permission-prompt
  `Notification` is not a turn boundary, and a wait declared mid-turn
  survives the rest of that turn. The per-execution wait budget stays
  charged. Workers are told, in their prompt, to re-declare before ending a
  resumed turn if they are still waiting.

## Rules of thumb for future work here

- **A dead pane is terminal evidence.** `remain-on-exit` keeps a tmux session listed after its worker command has exited. `#{pane_dead}=1` must not be treated as a live worker, and it must not disprove an inferred death.
- **Never make a liveness decision from derived bookkeeping alone** when the decision is irreversible (killing a worker) or duplicating (spawning a second one). Corroborate with `work_runs.shell_pid` via `engine/core/src/durable_liveness.rs`.
- **`Unknown` is not `Gone`.** A worker with no recorded pid is mid-spawn, not dead. `WorkerProcess::Unknown` and `PaneReleaseOutcome::NoLiveWorker` both exist to keep that case from being read as death — treating it as death reaps live-but-slow spawns and releases cube leases out from under workspaces a worker is about to occupy.
- **`EPERM` means alive.** `kill(pid, 0)` returning `EPERM` proves the process exists.
- **A pid is a durable number, not a durable handle.** Once the OS recycles it — macOS wraps at ~99999 — the same integer names an unrelated process, and the reap signals its whole process _group_. Anything that signals a process or restores state from the pid must use a bounded probe (`probe_execution_worker_within`, `probe_work_item_worker`), which declines to answer once the run row ages past `REDISPATCH_PID_TRUST_SECS`. The unbounded `probe_execution_worker` is only for callers whose failure direction is to decline to act, where a stale pid costs a missed reconciliation rather than a killed bystander.
- **Never infer activity from pane output or timers.** `activity` is reduced from the hook stream, in emission order (`ingress_seq`). If a pane looks busy while the slot says `Idle`, the bug is in ingress ordering, the reducer, or — after an engine restart — the re-adoption seed or a tail that was never re-established; fix that, not the symptom.
- **A balanced tool call is not a turn boundary.** `semantic_tool_condition = idle` says no tool is unbalanced; only `semantic_turn_condition = ended` (a driver `Stop`) says the worker is parked. Anything that restores or classifies a slot's activity from durable state must read the turn column, and a Codex worker polling its own background command — silent on both columns for as long as the wait lasts — is `Working`, not idle and not stuck.
- **Don't lengthen a timeout to fix a contradiction.** A longer spawn-ack window narrows the race without closing it, and leaves the system with no answer for the times it still loses.

## Suspected hung drivers

The engine publishes a separate `hung_since` diagnostic and a “Worker hung”
attention. It does not change the hook-derived activity, execution status, or
the two-hour idle reap. The app shows “Hung” on the Doing card and worker pane,
so an operator can inspect, interrupt, or restart the session without treating
silence as a confirmed failure. The ten-second activity sweep reconciles the
warning; resumed progress, a running tool, a declared wait, an operator hold,
or release of the worker clears it.

Only a Rich driver with a confirmed open turn and an idle or yielded tool state is
eligible. Unknown state, ended turns, permission waits, and blocking tool calls
are exempt. Codex poll results persist a yielded checkpoint: the command is still
running, but the model must respond within 900 seconds. Starting the next poll
restores the blocking exemption until its result returns. Yielded commands remain
protected from the destructive stale-worker sweep.
Codex model-message/reasoning events advance progress, but `token_count`
records do not, including unchanged usage during retries. Notifications, including
Grok background task completions, do not advance the semantic deadline. Hook-only drivers
provide tool/turn boundaries rather than token-level output, so their warning
is necessarily a suspicion based on that coarser evidence.

The thresholds are 600 seconds before any tool has completed and 900 seconds
after a tool completes within the current prompt; prompt submission resets the
in-memory tool-completion marker. This threshold is approximate after restart:
completed-tool history is not restored, so an idle checkpoint uses 600 seconds
until another tool completes. A persisted yielded checkpoint always uses 900
seconds. These are warning thresholds, not termination deadlines.
They use the 2026-10-27 transcript measurement: 3,454 completed, never-reaped
Codex sessions (September 10–October 27), 1,500 sampled Claude sessions out of
3,829 (September 27–October 27), and 647 Grok sessions (September 10–October 27).
Observed prompt/tool-result gaps were at most 268 seconds for Codex (128 for
first response), 608 seconds per Claude request (984 for the worst
single-request gap), and 413 for Grok; p99.9 was 75–165 seconds. The 600-second
initial warning exceeds the observed Codex first-response maximum; 900 seconds
after a tool leaves additional room for the slower Claude sample. Neither is
a guarantee: the 984-second Claude outlier exceeds the latter threshold.

At 600 seconds, recovered stalls affected 3.9% of Codex, 5.3% of Claude, and
6.8% of Grok sessions. Long gaps involved retries/errors; a Grok stream could
stall for 3.4 hours. Those observations justify an inspect/restart warning
rather than killing a worker at the new threshold. Tool duration is not model
latency: Claude Bash reached 2,991 seconds and Grok tools 900 seconds, so tools
are exempt regardless of elapsed time. These measurements come from
the local transcript survey; the detector's boundary tests use synthetic clocks
and do not claim to repeat that survey.
