# Boss: Engine Dispatch Instrumentation

## Motivating incident

Chore `task_18ad7731f2f64e68_7` was created with `autostart=true` but never
bound to a worker. The human dragged the kanban card into Doing — `tasks.status`
flipped to `'active'`, but no dispatch happened. An explicit
`bossctl work start` produced a `ready` execution that still didn't bind to a
slot. The cube pool was healthy (12+ free `mono` workspaces), so the failure
was inside Boss's dispatch pipeline rather than at the cube boundary.

Every observable surface the operator could read — kanban status, `bossctl
agents list`, `bossctl workspace summary` — gave the _outcome state_ but no
signal about _which step in the pipeline failed_. Engine logs in
`/tmp/boss-engine.log` carried the necessary detail, but had to be tailed and
grepped by hand and weren't attributable back to a specific `execution_id`
without re-deriving state.

## Goal

Make engine dispatch, lease, and pane-spawn behaviour diagnosable from outside
the engine. Each stage of the pipeline emits a structured event with enough
context to attribute success or failure without re-deriving state. Logs land
on disk — never via `events.sock`, which is itself one of the failure modes
the operator may be diagnosing — and are surfaced through `bossctl` so an
operator (or a coordinator session) can read them.

## Non-goals

- Engine-side decision-making off log content (no auto-retry, no escalation
  rules driven by parsed log lines). The stream is for humans and tooling.
- Replacing `tracing` for general engine logging. This design adds a _parallel_
  structured stream targeted at the dispatch pipeline; ad-hoc engine info /
  warn / error logging still goes through `tracing` to
  `/tmp/boss-engine.log`.
- A general-purpose engine telemetry framework. We need exactly one stream,
  scoped to the dispatch / lease / spawn / handshake pipeline.
- Cross-host or distributed observability. Boss is single-host today; this
  ships against that shape.
- Metrics aggregation (counts, histograms). Discrete events only — counts can
  be derived from the stream by `jq` / `sqlite` / a follow-up.

## The pipeline being instrumented

The dispatch pipeline begins at any of three trigger surfaces and runs through
seven discrete stages until the worker pane is exchanging hook events with the
engine. Today, only some of these stages emit `tracing` output, and none of
them emit a stable, queryable record.

```
trigger                                   stage
───────                                   ─────
boss task/chore create (autostart=true)
bossctl work start <id>            ─►  1. request_recorded
kanban drag → UpdateWorkItem
                                       2. ready_or_reused
                                          (dedupe vs. live execution)
                                       3. worker_claimed
                                          (claim_worker / claim_worker_force)
                                       4. cube_repo_ensured
                                       5. cube_workspace_leased
                                       6. cube_change_created
                                       7. run_started
                                          (start_execution_run; tasks.status → active)
                                       8. pane_spawned
                                          (SpawnWorkerPane → app, slot_id + shell_pid)
                                       9. handshake_observed
                                          (first hook event seen on events.sock,
                                           run_id correlated)
```

Stage 9 is what closes the loop — once a hook event for the spawned run has
been correlated, dispatch is fully wired and the worker can drive
`live_worker_states.activity` past `Spawning`. Anything that exits before
stage 9 leaves a measurable gap.

The motivating incident was almost certainly a stall at stage 2 or 3:
`request_execution_with_live_check` either reused a stale non-terminal row
without the live-worker check tripping, or `kick()` ran but the row didn't
appear in `list_ready_executions`. We could have diagnosed this in seconds with
a per-execution stage-by-stage record — and we couldn't.

## Design

### A separate, structured, file-backed stream

A second writer alongside `tracing`, with a narrow, stable schema. The schema
is a JSONL file — one event per line — keyed by `execution_id` and stage. This
is the canonical persistence surface. `tracing` continues to log freely; the
structured stream is never the _only_ place a fact lives, but it _is_ the only
place an operator should need to read.

**File layout** (under `~/Library/Application Support/Boss/`, the same root
Boss already uses for state):

```
boss-state-root/
  dispatch-events/
    current.jsonl       # active stream, append-only
    YYYY-MM-DD.jsonl    # daily rolls, optional
  executions/<execution-id>/
    dispatch.jsonl      # per-execution slice (mirror of relevant lines)
```

The flat `dispatch-events/current.jsonl` is the source-of-truth stream. The
per-execution mirror is a convenience copy written at the same time so a
single-execution diagnose verb doesn't have to scan the whole stream. Both
files are append-only; the engine never rewrites past lines.

We deliberately put this **on local disk**, not on `events.sock`. The events
socket is the engine's hook-event ingest path from worker shims; it is one of
the failure modes the operator may be diagnosing (a stuck handshake, a peer-pid
lookup race, a missing `_boss_run_id`). Routing dispatch instrumentation
through it would mean dispatch instrumentation breaks first whenever the thing
we most need to debug is broken. Files don't share that fate.

### Event schema

```json
{
  "ts_epoch_ms": 1714867200123,
  "ts_iso": "2026-05-04T18:40:00.123Z",
  "stage": "cube_workspace_leased",
  "outcome": "ok" | "error" | "skipped",
  "execution_id": "exec_18ad...",
  "work_item_id": "task_18ad...",
  "run_id": "run_18ad..." | null,
  "worker_id": "worker-3" | null,
  "slot_id": 3 | null,
  "shell_pid": 42111 | null,
  "cube_repo_id": "..." | null,
  "cube_lease_id": "..." | null,
  "cube_workspace_id": "..." | null,
  "duration_ms": 142 | null,
  "trigger": "kanban_drag" | "work_start" | "agents_launch"
             | "autostart_create" | "rescan_active" | "kick_internal" | null,
  "details": { ... },
  "error_chain": ["..."] | null
}
```

The schema is deliberately wide and permissive: a writer that doesn't yet have
a `worker_id` for a stage emits `null`, not an empty string or a missing key.
Readers do not need to know about every field — `jq '.stage, .outcome,
.execution_id'` is the canonical "did dispatch succeed for this execution"
query.

`details` is the per-stage open object. Recommended payloads:

| stage                   | recommended `details` keys                                                                                    |
| ----------------------- | ------------------------------------------------------------------------------------------------------------- |
| `request_recorded`      | `priority`, `preferred_workspace_id`, `force`, `live_check_result`                                            |
| `ready_or_reused`       | `existing_execution_id`, `existing_status`, `decision` ("inserted_ready" / "reused_live" / "abandoned_stale") |
| `worker_claimed`        | `pool_capacity`, `idle_count_before`, `affinity_match`, `forced`                                              |
| `cube_repo_ensured`     | `repo_remote_url`                                                                                             |
| `cube_workspace_leased` | `prefer_workspace_id_request`, `prefer_workspace_id_actual`, `affinity_hit`                                   |
| `cube_change_created`   | `change_id`, `change_title`                                                                                   |
| `run_started`           | `auto_advance_status_to_active`                                                                               |
| `pane_spawned`          | `app_session_registered`, `summary`                                                                           |
| `handshake_observed`    | `first_hook_event`, `via` ("payload_run_id" / "ancestor_walk")                                                |

`error_chain` is `format!("{err:#}")` split on `: `, so a `cube workspace
lease` failure surfaces every layer of the anyhow chain (cube CLI stderr →
error context → top-level wrapper).

### Writer placement

A `DispatchEventSink` trait, owned by `ExecutionCoordinator` and threaded into
the spawn flow. Sample shape:

```rust
#[async_trait]
pub trait DispatchEventSink: Send + Sync {
    async fn emit(&self, event: DispatchEvent);
}
```

The production implementation appends to `current.jsonl` and to the
per-execution mirror. Both writes are best-effort: an emit that fails to write
to disk logs once via `tracing::warn!` and is dropped, never blocks dispatch.
The trait makes test doubles trivial — every existing coordinator unit test
can register a `RecordingSink` and assert on the event timeline directly,
which today requires fragile string-grep over `tracing` output (see
`record_start_failure`'s test in `coordinator.rs` for the cost of that
approach).

Stage emission points:

- `request_execution_with_live_check` → 1, 2 (inside `app.rs` /
  `work.rs::request_execution_in_tx_with_live_check`).
- `ExecutionCoordinator::run_scheduler` → 3 on claim success, plus 3-skipped
  when `claim_worker` returns `None` (already partially logged via the
  `pool_capacity` warn at `coordinator.rs:657`; convert to a structured
  `worker_claimed` skip).
- `ExecutionCoordinator::schedule_execution` → 4, 5, 6, 7 around each cube
  call and the `start_execution_run` boundary.
- `start_worker` (`spawn_flow.rs`) → 8 on `SpawnWorkerPane` success/error.
- `events_socket::handle_connection` → 9 on the _first_ successfully
  correlated hook event per run id. The sink needs an internal "have we seen
  a handshake for run X yet" flag so subsequent hook events don't re-emit.

### Time bounds and stuck-stage detection

A second class of event the stream surfaces: `stage_stalled`. The coordinator
already tracks executions in flight; on a periodic tick (default 30s) the
sink emits one `stage_stalled` event for every execution whose latest stage
is older than a per-stage threshold:

| stage                                      | stall threshold                                                         |
| ------------------------------------------ | ----------------------------------------------------------------------- |
| `request_recorded` → `worker_claimed`      | 60s (pool exhaustion is the common case; threshold gates "real" stalls) |
| `worker_claimed` → `cube_workspace_leased` | 30s                                                                     |
| `cube_workspace_leased` → `pane_spawned`   | 30s                                                                     |
| `pane_spawned` → `handshake_observed`      | 60s (worker shell + claude startup is slow)                             |

These thresholds are conservative defaults; the operator changes them in the
runtime config if the local machine is slow. A stalled stage is not an error —
it's a hint. The motivating incident would have surfaced as a
`stage_stalled` at `request_recorded` within 60s, with the live-check result
in `details` showing exactly which branch in
`request_execution_in_tx_with_live_check` was taken.

### bossctl surface

Three new verbs, all read-only, all backed by file scans (no engine RPC).
Read-only file scan is a deliberate choice: if the engine is wedged, `bossctl`
must still be able to surface the dispatch stream.

```
bossctl dispatch tail [--execution <id>] [--stage <name>] [--outcome <ok|error|skipped|stalled>] [--lines N]
bossctl dispatch diagnose <execution-id>
bossctl dispatch ghost-active
```

- `tail` is a `tail -f`-style live stream over `current.jsonl`, with optional
  filters. The default rendering is one human-readable line per event:
  `18:40:00.123  exec_…  cube_workspace_leased  ok   (lease=lease-… ws=mono-agent-003 dur=142ms)`.
  `--json` emits the raw JSONL.

- `diagnose <execution-id>` reads the per-execution mirror file and prints a
  vertical timeline of every stage the execution went through, including any
  `stage_stalled` events. The motivating incident becomes:

  ```
  exec_18ad77803644d5b0_f  (Engine dispatch instrumentation)
  18:40:00.123  request_recorded   ok      trigger=kanban_drag  decision=inserted_ready
  18:40:00.401  worker_claimed     skipped pool_capacity=8 idle_count_before=0
  18:41:00.401  stage_stalled      —       at=request_recorded for=60s
  ```

  …and the operator immediately sees that the pool was exhausted, not that
  cube was down or the events-socket peer-pid lookup failed.

- `ghost-active` is a snapshot of the existing
  `WorkDb::list_active_chores_without_live_run` invariant, surfaced through a
  user-facing verb instead of buried in the engine log when the pool stalls.
  Output is the work item list with the latest `dispatch.jsonl` line for each.

### Subscription topic (optional, deferred)

The Boss-session UX would benefit from a live tail surfaced through the
existing topic broker (`worker.live_states` is the precedent), so the
coordinator session can react to `error` / `stalled` events without polling.
That's deferred to a follow-up: the file-backed stream is the foundation, and
a topic that just broadcasts each event line is a small later addition.

## What this replaces / augments

- The current `tracing::warn!(pool_capacity, "worker pool exhausted; deferring
dispatch …")` becomes a structured `worker_claimed` skip event. The tracing
  log stays for human readability; the structured event is what tooling reads.
- The `tracing::warn!(ghost_active = ?orphans, …)` invariant log becomes a
  surfaced `bossctl dispatch ghost-active` query plus a `stage_stalled` for
  every offending execution.
- The detailed `tracing::error!(execution_id, work_item_id, worker_id,
cube_repo_id, error = "{err:#}", "cube workspace lease failed; …")` line at
  `coordinator.rs:752` is the model for how stage failures should already be
  attributed. The structured stream just makes that the _contract_ rather
  than a per-callsite convention.

## Implementation phases

Vertical slices, each independently mergeable.

### Phase 1 — Sink trait + file writer

- `DispatchEventSink` trait, `JsonlFileSink` production implementation,
  `RecordingSink` test double.
- `DispatchEvent` struct + `Stage` enum in `engine/src/`.
- File path resolver under the existing Boss state root.
- Wire up exactly _one_ stage end-to-end (stage 5: `cube_workspace_leased`,
  ok + error) so the file writer has a real consumer and the schema is
  tested before fanning out.

### Phase 2 — Stage 1–9 coverage

- Add the remaining stage emissions at the call sites listed above.
- Per-execution mirror file written alongside `current.jsonl`.
- Unit tests in `coordinator.rs` and `spawn_flow.rs` that swap in
  `RecordingSink` and assert on the stage timeline (replaces the existing
  fragile `tracing` string-grep tests).

### Phase 3 — `bossctl dispatch tail` + `diagnose`

- Two new bossctl verbs reading the file stream directly. No engine RPC,
  no broker plumbing, no protocol additions.
- Pretty rendering for the default human surface; `--json` for tooling.

### Phase 4 — `stage_stalled` detector + `ghost-active`

- 30s tick in the coordinator that emits `stage_stalled` events.
- `bossctl dispatch ghost-active` reads the existing
  `list_active_chores_without_live_run` query and the per-execution mirror
  files; no new engine state.

### Phase 5 — Topic broadcast (deferred)

- Add a `dispatch.events` topic that broadcasts each dispatch event line as
  it's written, so the Boss session and any future UI surfaces can react in
  real time. Optional; the file-backed stream is the foundation everything
  else builds on.

## Open questions for human review

1. **Disk hygiene.** `current.jsonl` grows unboundedly. Daily roll is
   straightforward, but should we also age out per-execution mirrors after
   the execution reaches a terminal status (with N days grace)? Or keep them
   forever as part of the broader transcript-storage story
   ([`work-execution.md`](work-execution.md) Phase F)? Keeping them forever
   is the cleaner answer if Phase F lands soon.

2. **Stage definition for `request_recorded` vs. `ready_or_reused`.** These
   two are entangled inside `request_execution_in_tx_with_live_check`. We
   could collapse them into a single stage (`request_recorded`, with
   `decision` in `details`) and lose the ability to time the dedupe path
   independently. Recommendation: keep them split; the dedupe path is
   exactly where the motivating incident hid.

3. **Error chain shape.** `format!("{err:#}")` then split on `": "` is a
   pragmatic flattener but assumes anyhow's display format. If we ever
   re-shape error types this becomes brittle. Acceptable for now; revisit
   if we add a `thiserror`-driven typed error tree across the dispatch path.

4. **bossctl is read-only.** If the engine is wedged and the file is being
   actively written, two cooperating writers + a reader is fine on local
   disk. We should still verify by running `tail -F` against a synthesised
   `current.jsonl` under load before depending on the assumption.

5. **Rust crate placement.** The sink + event types are engine-internal
   today. If a future Boss session probe wants to compose dispatch events
   into its own diagnosis output, the structs may need to lift into
   `boss-protocol`. Defer until there's a real second consumer; premature
   protocol-ization is what made the existing `worker.live_states`
   subscription harder to evolve.

## Related designs

- [`main`](main.md) — V2 spec; this design fills in the operability story
  Phase 4 (Execution layer) and Phase 6 (libghostty embedding) deferred.
- [`work-execution`](work-execution.md) — execution lifecycle. This stream
  is the on-disk evidence of the dispatch half of that lifecycle, and the
  natural seed for the Phase F per-run transcript model.
- [`worker-live-status`](worker-live-status.md) — `live_worker_states.activity`
  / `live_status` are the _steady-state_ worker observability surface. Once a
  run is past stage 9 (handshake observed), live-status takes over. This
  design owns the gap between "execution requested" and "first hook event
  observed."
- [`engine-app-rpc`](engine-app-rpc.md) — `SpawnWorkerPane` is the stage 8
  callsite; nothing in that contract changes.
