# Probes accepted, then silently abandoned (2026-08-09)

A coordinator probe was accepted against a live `grok` investigation worker,
instructing it **not** to open its PR until a dataset arrived. The CLI printed:

```text
probe accepted for run <exec> (probe_id=probe-1); will be injected at the
worker's next turn boundary (its driver's mid-turn input handling is undeclared)
```

The worker finished its turn, opened its PR, and completed. Afterwards:

```text
probe-1: run=<exec> state=abandoned
  worker pane released; the worker was gone before the probe's delivery boundary arrived
```

The instruction that existed to prevent that exact outcome was accepted,
dropped, and nothing said so.

## 1. Which drivers declare mid-turn input handling

Swept from source rather than assumed. The property is
`AgentDriver::mid_turn_pane_input() -> MidTurnPaneInput`
(`tools/boss/engine/driver/src/lib.rs`), whose trait default is
`MidTurnPaneInput::Rejects`. Every built-in driver registered in
`DriverRegistry::default()`:

| slug     | declares                                 | where                                                                                                           |
| -------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `claude` | `Buffers` (override)                     | `driver/src/claude.rs`                                                                                          |
| `codex`  | `Buffers` (override)                     | `driver/src/codex.rs`                                                                                           |
| `grok`   | **undeclared** — trait default `Rejects` | `driver/src/grok.rs` (explicit comment: not overridden until mid-turn stdin consumption is proven, design G-10) |

So the suspicion that this is "grok-specific" is true of the _current
registry_ and false of the _model_: `Rejects` is the trait default, so every
driver added from here inherits it until someone measures otherwise. Two
further fail-closed paths reach the same state without any driver being
involved at all — `ServerState::run_mid_turn_pane_input` returns `Rejects`
when the run has no recorded driver, when the slug is not in the registry, or
when the DB lookup errors. The fix therefore targets the capability, not the
slug.

## 2. What "next turn boundary" meant, and why it never arrived

It did arrive. The engine did not use it.

`PaneInputPosture::resolve` refuses every write to a `Working` worker on a
`Rejects` driver, so for such a worker the only injectable posture is
`Parked` — reached at a `Stop`, after `dispatch_live_worker_state` applies the
`Stop` and flips the slot to `Idle`. Grok does wire the `Stop` hook and does
declare `Capability::TurnBoundary`, so boundaries reach the engine normally.

The defect was the order of the turn-boundary fan-out in
`dispatch_worker_event_fanout`:

```text
record_turn_boundary_on_stop
dispatch_probe_reply_on_stop
dispatch_completion_on_stop      <-- may conclude the run is finished and
                                     call release_worker_pane, whose teardown
                                     drain settles every queued probe Abandoned
dispatch_probe_on_stop           <-- arrives to an empty queue
```

The worker's final `Stop` _was_ the promised boundary. Completion consumed it
first, released the pane, and the teardown drain settled the probe
`Abandoned` before the probe dispatcher ran. The engine broke a commitment it
had already reported as accepted, using the very boundary it had promised.

The intervening tool calls are consistent with this and not evidence against
it: `dispatch_probe_on_post_tool_use` short-circuits at `PostureRefused` on a
`Rejects` driver, by design (the tty-leak guard), and logs at `debug`.

## 3. Race or certainty

Both, and which one depends on a property the caller cannot see.

- Probe issued while the worker is **parked** (`Idle`/`WaitingForInput`):
  posture `Parked`, expectation `Immediate`, written during the `ProbeRun`
  call. Works on every driver.
- Probe issued **mid-turn** on a `Buffers` driver: posture `MidTurnBuffered`,
  also `Immediate`. Works.
- Probe issued **mid-turn** on a `Rejects` driver: queued with a
  `NextTurnBoundary` commitment. It lands **iff a non-final `Stop` arrives
  first**. If the worker's next turn boundary is also its last — an
  autonomous run that ends by opening its PR, i.e. the normal shape of a Boss
  worker — abandonment was a **certainty**, not a race.

So delivery was possible for a `Rejects` driver, but only in the window where
the worker still had turns left, and nothing told the issuer which case they
were in.

## 4. Prevalence

**Not determined.** The retained engine trace
(`engine-trace.jsonl*`, `engine-audit.log`) lives under
`~/Library/Application Support/Boss/`, which
[`forensic-surfaces.md`](../forensic-surfaces.md) declares coordinator-only
and off limits to worker sessions, and no `boss` verb exposes probe history.
The sweep was not run, and its absence is not evidence of low prevalence.

What the retention window would have allowed had it been readable is worth
recording, because it bounds any future attempt: `engine-trace.jsonl` rotates
at 100 MB keeping 5 files and holds roughly **two days**, which is shorter
than the interval between the incident and this investigation for most
reports. `engine-audit.log` retains for months but records work-item
provenance events, not probe lifecycle. Probe lifecycle itself
(`probe_lifecycle`) is **in-memory only** and does not survive an engine
restart at all — `ProbeStatus` for an id from a previous engine process
answers "unknown probe id". Any real prevalence answer has to come from the
trace inside its two-day window, or from the durable attention items this
change starts filing.

## 5. Blast radius beyond coordinator nudges

Two other mechanisms ride this channel and had the same silent failure on a
`Rejects` driver:

- **Effort-escalation acknowledgement.** `handle_probe_run` resolves the
  run's open worker-signal attention items _at accept time_, before any
  delivery attempt. So the state change always lands — the engine's auto-nudge
  loop resumes — while the acknowledgement text the worker was waiting on may
  never arrive. That asymmetry is worse than either half alone: the worker is
  nudged to continue by an engine that thinks it has been told it may.
- **Description-edit propagation ("re-read the spec").** `handle_update_work_item`
  calls `send_input_to_worker`, and on `NotAcceptingInput` — which for a
  `Rejects` driver is _every_ mid-turn edit — falls back to `queue_probe`.
  Same queue, same boundary, same abandonment. An operator editing the spec of
  a running grok worker got no error and no delivery.

Both are fixed by the same change, since neither has its own transport.

## What changed

1. **Delivery.** `dispatch_probe_on_stop` now runs on _both_ sides of
   `dispatch_completion_on_stop`. The pre-completion pass spends the promised
   boundary on the probe it was promised to, before completion can tear the
   run down; the post-completion pass still delivers probes the completion
   handler queued during the same fan-out (`PROBE_NO_PR`). The run's single
   in-flight slot stops the two from double-delivering, and both are no-ops
   on the overwhelming majority of boundaries.

2. **Honesty about the delivered-then-killed case.** A probe written into the
   pane on a run's _final_ boundary gets no reply, because there is no further
   boundary. `release_worker_pane` now settles such a probe
   `Orphaned` — bytes reached the pane, nobody acted on them — instead of
   leaving it reading `Consumed` forever.

3. **Reporting at issue time.** `ProbeDeliveryExpectation::is_best_effort()`
   is true for `NextTurnBoundary`, and `bossctl probe` prints the caveat on
   stderr (and `best_effort: true` in `--json`) rather than a plain
   acceptance. Exit stays 0: the probe may well land, and refusing outright
   would remove the surface without fixing the capability.

4. **Active surfacing.** An abandonment now pushes
   `ProbeDeliveryEscalated` on the run's probe topic _and_ files a
   `probe_undelivered` attention item against the execution, which outlives
   the run, the pane, and every topic subscriber. `bossctl probe-status` was
   already correct; what was missing was any reason for the issuer to look.

## What did not change, and why

Grok is **not** declared `Buffers`. Mid-turn injection into a driver whose
foreground process has not been measured to consume stdin is the tty-leak
hazard the `Rejects` default exists for (`ghostty-codex-pane-viability`, Q2
Layer D): unread bytes survive in the tty input buffer and are executed by the
interactive shell once the process exits. Flipping the declaration without the
measurement would trade a dropped instruction for arbitrary shell execution in
the worker's workspace. Earning `Buffers` requires a mid-turn stdin spike
against a live grok TUI — the same evidence bar codex met — and that is a
measurement task, not a code change.

Delivery for a `Rejects` driver is therefore still boundary-bound. What is
fixed is that the boundary is now actually used, and that a caller is told up
front when the boundary is all they are getting.
