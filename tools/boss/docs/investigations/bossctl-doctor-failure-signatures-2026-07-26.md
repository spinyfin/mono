# bossctl doctor: mechanically-detectable failure signatures

- **Date:** 2026-07-26
- **Question:** What exact, mechanically-checkable predicates should a future `bossctl doctor` verb match against dispatch / trace / state artifacts so an operator (or a wedged engine) can name the active failure class without grepping by hand?
- **Method:** Evidence-driven. The live Application Support store is not readable from cube workers; this note derives every predicate from (a) the coordinator-inlined corpus snapshot of 2026-07-24 (16,187 execution dirs, 274,623 dispatch records, 6 engine-trace segments) and (b) the current emitter code under `tools/boss/` on `main`. No engine code is changed here; the follow-on implementor task builds the verb from this spec.
- **Out of scope:** Implementing `bossctl doctor`; inventing signatures without exemplars; re-encoding the three wrong premises listed below.

## TL;DR

`bossctl doctor` should be a **file-scan-first, signature-matching diagnostic** over `dispatch-events/current.jsonl` (primary), `engine-trace.jsonl` (+ rotated segments), and — only where the JSONL timeline is terminal-event-blind — a read-only join to `state.db`. Reuse `boss_dispatch_reader` / `boss_log_files`; do not invent a second parser. There is **no** `bossctl doctor` today (verified against the binary's `--help`).

This note specifies **15 signatures** (SIG-1…SIG-6, SIG-5b/SIG-5c, plus the additional shapes found in the corpus), each with: exact predicate, artifact(s), whether a `state.db` join is required, false-positive rate against the 2026-07-24 counts, emitter file:line on current `main`, and an actionable vs. noise disposition.

**Three original premises are wrong and must not be encoded:**

| Wrong premise                                                          | Reality                                                                                                                                                                                         |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `before_commit_sha` is an emitted trace field for merge-queue rebounce | It is **not**. Store hits on that string are embedded worker `PostToolUse` payloads. Real pair is `fields.discriminator` + `fields.head_sha_at_trigger`; equality **is** the before==head test. |
| `shell_pid: 0` then completion ~ms later is a failure                  | That is the **normal** provisional-spawn path (455/456 heal in 0.3–1.1 s). Real failure is `stage=="spawn_ack_timeout" && details.shell_pid==0`.                                                |
| `pool_exhausted` with idle workers is a leak signature with exemplars  | **Zero** leaked-claim exemplars in the retained window. Cap holds balance. Nearest real evidence is `pool_claim_reconcile`.                                                                     |

Dispatch records have **no** `event`, `ts`, or `run_id` keys. Discriminators are `stage` / `ts_epoch_ms` / `execution_id` / `outcome` / `details` / `error_message`.

---

## 1. Artifact inventory (what doctor may read)

| Path (under Boss state root)                    | Shape                                | Doctor role                                                                                                                |
| ----------------------------------------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| `dispatch-events/current.jsonl`                 | JSONL `DispatchEvent`                | **Primary source.** Single append-only global concat (~108 MB, never rotated). Prefer this over walking 16k per-exec dirs. |
| `executions/<exec_id>/dispatch.jsonl`           | same schema                          | Per-exec mirror only; use for `dispatch diagnose`-style deep dives, not fleet scan.                                        |
| `executions/engine/dispatch.jsonl`              | same, `execution_id:"engine"`        | Engine-scope only (`dispatch_paused`/`resumed`, engine-level `stage_stalled`).                                             |
| `engine-trace.jsonl` (+ `.jsonl.<unixsecs>` ×5) | `{timestamp, level, target, fields}` | Trace-only signatures (SIG-5, scheduler wakeup, reap contradiction). Rotates at 100 MB, keeps 5.                           |
| `state.db`                                      | SQLite                               | **Only place execution status lives.** Required for SIG-2 terminality and SIG-3 claim/status cross-check.                  |
| `recovery/<exec_id>.patch`                      | git patch                            | Referenced by `details.recovery_patch`; optional context, not a signature source.                                          |

**There is no live-status artifact file.** Live status is broadcast on `worker.live_states` and logged into `engine-trace.jsonl` (`boss_engine::live_status_loop`). Operator surface today: `bossctl live-status debug`.

### Record shape (dispatch)

Always present: `ts_epoch_ms`, `stage`, `outcome`, `execution_id`, `work_item_id`, `worker_id`, `cube_repo_id`, `cube_lease_id`, `cube_workspace_id`, `details`.

Conditional: `cube_command` + `cube_cwd`, `error_message`.

`outcome` ∈ {`ok`, `error`, `skipped`}. `details` is `null` or an arbitrary object.

### Record shape (engine-trace)

Uniform `{timestamp, level, target, fields}`. Discriminator is **`fields.message`** — a full English sentence prefix, not a stable error code. Common `fields` keys: `run_id`, `kind`, `execution_id`, `slot_id`, `work_item_id`, `outcome`, `worker_id`, `pool`, `shell_pid`, `live_workers`, `cap`, `pr_url`, `attempt_id`, `failure_kind`, `discriminator`, `head_sha_at_trigger`.

**`fields.run_id` is overloaded:** `spawn_flow` / `pane_spawn` / `worker_events` set it to an `exec_*` id; `coordinator` sets a real `run_*` id. Any join on `run_id` must accept both shapes.

### Schema / emitter map (current `main`, paths relative to `tools/boss/`)

| Concern                                                                     | Location                                                                                                                                                                                 |
| --------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Stage` enum (44 variants) / wire strings                                   | `engine/dispatch-events/src/lib.rs` (`enum Stage` ~:43, `as_str` ~:521–566)                                                                                                              |
| `Outcome` / `DispatchEvent` / `JsonlFileSink` dual-write to `current.jsonl` | same crate (`Outcome` ~:576, `DispatchEvent` ~:598, sink ~:750+)                                                                                                                         |
| `read_current` / `parse_lines` (skips unparseable) / stall detector         | `engine/dispatch-reader/src/lib.rs`                                                                                                                                                      |
| `StageThresholds`                                                           | `engine/dispatch-reader/src/timeline.rs`                                                                                                                                                 |
| Stall thresholds wiring (15 s sweep)                                        | `engine/core/src/app/server.rs` ~:1528–1555                                                                                                                                              |
| `PERSISTENT_STALL_THRESHOLD` (5 min attention)                              | `engine/core/src/dispatch_stall_escalation.rs`                                                                                                                                           |
| `engine-trace.jsonl` path / rotation                                        | `log-files/src/paths.rs`, `engine/core/src/trace_rotation.rs`, `log-files/src/segments.rs`                                                                                               |
| Terminal-event predicate used by ghost-active / stall                       | `is_terminal_event` in `dispatch-reader/src/timeline.rs`: **any `outcome=="error"`**, or **`pane_spawned`+`ok`**. Note: normal worker _completion_ is **not** a dispatch terminal event. |

### Torn-write tolerance

The 274,624-record corpus contains **one** unparseable line. `boss_dispatch_reader::parse_lines` already drops unparseable lines with a warning (`lib.rs` ~:93–110); the incremental index counts `malformed_lines` and continues. Doctor must do the same — never abort a pass on a single bad line.

### Never-observed Stage variants (8 of 44)

Zero hits in the 2026-07-24 window for:

`cube_repo_ensure_failed`, `transient_recovery_nudge`, `remote_lease_reconcile`, `host_drain_reconcile`, `spawn_capability_recovered`, `workspace_recovery`, `automation_preempted`, `abandoned_branch_pr_recovery`.

**None are dead code** — each still has a live emitter on `main`:

| Wire stage                     | Emitter (current `main`)                                                    |
| ------------------------------ | --------------------------------------------------------------------------- |
| `cube_repo_ensure_failed`      | `engine/core/src/coordinator/execution.rs` (~`Stage::CubeRepoEnsureFailed`) |
| `transient_recovery_nudge`     | `engine/core/src/transient_recovery.rs`                                     |
| `remote_lease_reconcile`       | `engine/core/src/remote_lease_reconcile.rs`                                 |
| `host_drain_reconcile`         | `engine/core/src/host_reconcile.rs`                                         |
| `spawn_capability_recovered`   | `engine/core/src/spawn_health.rs`                                           |
| `workspace_recovery`           | `engine/core/src/coordinator/execution.rs`                                  |
| `automation_preempted`         | `engine/core/src/coordinator/scheduler.rs`                                  |
| `abandoned_branch_pr_recovery` | `engine/core/src/abandoned_branch_pr_sweep.rs`                              |

Disposition: **rare / fleet-window miss**, not removable. Doctor should still accept them if they appear; do not treat absence as "stage retired."

---

## 2. Compose with existing verbs (do not duplicate)

Verified against the built `bossctl --help` / subcommand help on this host (2026-07-26). There is **no** `doctor` subcommand.

| Existing verb                      | What it already does                                                       | Doctor relationship                                                                              |
| ---------------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `bossctl dispatch tail`            | Recent events from `current.jsonl`, filterable by stage/outcome            | Doctor may print hit samples; do not reimplement tail                                            |
| `bossctl dispatch diagnose <exec>` | Full per-exec timeline + durations + error_message                         | Doctor findings should link "run `dispatch diagnose <id>`"                                       |
| `bossctl dispatch ghost-active`    | Timelines that never reached a terminal _dispatch_ stage                   | Overlaps SIG-1/ghost class; doctor names _why_, ghost-active lists _who_                         |
| `bossctl dispatch stats`           | Wait-time aggregates by defer reason                                       | Orthogonal capacity view; not a failure signature                                                |
| `bossctl live-status debug`        | One-shot live-status pipeline snapshot (engine RPC)                        | Complements SIG-4; doctor stays file-scan-first when engine is wedged                            |
| `bossctl logs`                     | File-scan over engine-trace / audit / dispatch / spawn / population-timing | Doctor should call into the same path resolution (`boss_log_files`) rather than hardcoding paths |

**Design rule:** doctor is the **signature catalog + multi-signal correlation** layer. Existing verbs remain the raw inspectors.

### Recommended I/O architecture for the implementor

1. Resolve state root via existing defaults (`boss_dispatch_reader::default_state_root` / `boss_log_files`).
2. Stream `dispatch-events/current.jsonl` with the existing reader (tolerate torn lines).
3. Optionally stream `engine-trace.jsonl` + rotated segments via `boss_log_files` segment helpers.
4. Open `state.db` read-only only for signatures marked **needs state.db**.
5. Emit structured findings: `{sig_id, severity, title, count, exemplar_execution_ids[], evidence_snippet, next_step}`.
6. Default to a recent time window (e.g. last 24 h of `ts_epoch_ms`) so historical zombie storms do not drown the present; support `--all-history` for forensics.

---

## 3. Signature catalog

Severity legend: **P0** fleet-blocking / multi-day wedge · **P1** recurring self-inflicted damage · **P2** real but rare · **info** observability / capacity noise (not a "fault" by itself).

For each signature:

- **Predicate** — mechanical match rule
- **Artifacts** — files required
- **state.db** — yes / no
- **Corpus count** — 2026-07-24 window unless noted
- **False-positive rate** — naive vs refined predicate
- **Emitter** — current `main` (may differ from brief line numbers after the coordinator split)
- **Disposition** — ship / ship-with-window / not-yet-specifiable / do-not-encode

---

### SIG-1 — Persistent `worker_claimed` stall (not every `stage_stalled`)

**Problem.** `stage_stalled` with `details.stalled_stage=="worker_claimed"` fires from the 30 s per-stage override. Most hits are benign chain-serialization holds that clear once the timeline advances. A naive match is almost entirely noise.

**Naive predicate (DO NOT SHIP ALONE):**

```
stage == "stage_stalled"
  && details.stalled_stage == "worker_claimed"
```

Count: **2,720**. Actionable subset ≈ **~1%**.

**Why "> 30 s" on the record is vacuous, and why `details.elapsed_in_stage_ms > 120_000` is the wrong gate.** The emitter's threshold for `worker_claimed` is 30 s (`app/server.rs` StageThresholds override). A `stage_stalled` record **cannot exist** below that threshold. Observed corpus `elapsed_in_stage_ms/1000`: ~80% land in 30–44 s, cliff, thin tail to 3121 s. More importantly, the wire field `details.elapsed_in_stage_ms` is a **snapshot at first detection** (when the 30 s override tripped). The detector then **dedupes** further `stage_stalled` emissions for the same `(stalled_stage, stalled_at_ts)` pair (`TimelineState::already_flagged` in `dispatch-reader/src/timeline.rs`), so a multi-minute wedge often still has only one `stage_stalled` row whose frozen `elapsed_in_stage_ms` is ~30–45 s. Requiring `details.elapsed_in_stage_ms > 120_000` on that row **misses the durable stall**.

**How the emitter actually measures stall (recompute this):**

1. Fold the per-`execution_id` timeline in append order.
2. Ignore `stage_stalled` for "last real event" purposes (`TimelineState::apply` treats only non-`stage_stalled` as `last_real`).
3. `elapsed_in_stage_ms = now_ms - last_real.ts_epoch_ms` (or scan-window end if offline).
4. Flag when `last_real` is non-terminal, `last_real.stage == "worker_claimed"`, and recompute elapsed exceeds the severity threshold.

This is the same model as `pending_stalls` / `persistently_stalled` / `dispatch_stall_escalation` (`PERSISTENT_STALL_THRESHOLD` = 5 min). A prior `stage_stalled` row is optional corroboration (via `details.stalled_stage` / `stalled_at_ts_epoch_ms`), not the elapsed source of truth.

**Benign pattern (chain hold):**

1. `worker_claimed` / `skipped` with `details.reason ∈ {chain_serialized, chain_serialized_review_held}`
2. One or more `stage_stalled` / `ok` with `stalled_stage=worker_claimed` (first trip ~30–45 s)
3. Later **any non-`stage_stalled` event** for the same `execution_id` (clears the stall)

**Progress is not only `request_recorded`.** Clearing progress includes (non-exhaustive) a later successful `worker_claimed`, `host_selected`, `cube_*`, `run_started`, `pane_spawned`, terminal `outcome=="error"`, etc. Using "no later `request_recorded`" as the sole clear signal both false-negatives (never re-request paths) and false-positives (progressed via other stages while an earlier request still exists).

**Actionable predicate (SHIP):**

```
For each execution_id in the scan window, reduce the timeline as the
dispatch-reader does:

  last_real = last event where stage != "stage_stalled"
  IF last_real is None OR is_terminal_event(last_real): skip
  IF last_real.stage != "worker_claimed": skip   # this SIG's stage focus

  elapsed_ms = now_ms - last_real.ts_epoch_ms     # RECOMPUTE; do not
                                                  # trust a frozen
                                                  # stage_stalled.details
                                                  # .elapsed_in_stage_ms

  FLAG when elapsed_ms > WARN_MS   (recommend 120_000)
  escalate severity when elapsed_ms >= PERSISTENT_STALL_THRESHOLD_MS
                           (300_000 — same as attention escalator)

Optional corroboration (not required): a prior stage_stalled row for the
same execution with details.stalled_stage == "worker_claimed" and
details.stalled_at_ts_epoch_ms == last_real.ts_epoch_ms.
```

Optional severity ladder:

- **warn** if recomputed `elapsed_ms > 120_000` and still no non-`stage_stalled` progress
- **critical** if recomputed `elapsed_ms` past `PERSISTENT_STALL_THRESHOLD` (5 min)

**Artifacts:** `current.jsonl` only (per-exec correlation by `execution_id`).
**state.db:** no (progress is visible as a later non-`stage_stalled` dispatch event).
**False-positive rate:** naive stage_stalled match ~99% FP; recomputed long-tail targets ~1% of the 2,720 first-trip stalls.
**Emitter:** `build_stalled_event` / `spawn_stage_stalled_detector` / `TimelineState` in `engine/dispatch-reader/src/{lib,timeline}.rs`; thresholds `engine/core/src/app/server.rs` (~:1528–1555); attention escalator `engine/core/src/dispatch_stall_escalation.rs` (`PERSISTENT_STALL_THRESHOLD`). Do **not** confuse the 30 s `worker_claimed` override with `CUBE_LEASE_TIMEOUT` (currently **90 s** — see SIG-6).

**Disposition:** ship recomputed-elapsed predicate only; never gate on `stage_stalled.details.elapsed_in_stage_ms > 120s`.

---

### SIG-2 — `redundant_spawn` zombie (recurring live_execution_id)

**Problem.** `host_selected`/`error`/`redundant_spawn` is the **normal supersede guard** (2,625 records). Healthy supersedes are singletons with a young live peer. Zombies are the **same** `details.live_execution_id` blocking peers for days while that target itself makes no progress and only emits untracked-lease heartbeats.

**Naive predicate (normal path — not a failure):**

```
stage == "host_selected"
  && outcome == "error"
  && details.reason == "redundant_spawn"
```

**Zombie predicate (SHIP — temporal):**

```
Group host_selected/error/redundant_spawn by details.live_execution_id.
Flag a target id when:
  ref_count >= N              (recommend N=10)
  && (max(ts) - min(ts)) >= W (recommend W=1 hour)
```

Corpus exemplars:

| target live_execution_id                         | refs     | window                   |
| ------------------------------------------------ | -------- | ------------------------ |
| three automation execs (2026-06-16 → 2026-07-03) | 608 each | **420.2 h** (~17.5 days) |
| `exec_18bf065637793c20_483`                      | 31       | 1.6 h                    |

Healthy contrast: single ref, `live_execution_liveness: "alive"`, `live_execution_age_secs: 1`.

**state.db join (recommended second gate):**

```
target execution status is terminal in state.db
  OR target has no live run / no tracked lease
```

**Why JSONL alone is incomplete.** Normal-path completion does **not** emit a dispatch terminal event (hooks write status into `state.db`). "Is the blocking execution completed?" is unanswerable from JSONL alone. The recurrence heuristic is the JSONL-only fallback; the DB join is the precise gate.

**False-positive rate:**

- Naive stage/reason match: **~100% FP** relative to "zombie" (almost all are healthy supersedes).
- Absence of `live_execution_liveness`: present on only 54/2,625 → **~98% FP** if used as "missing ⇒ zombie".
- Recurrence heuristic with N=10, W=1h: matches the multi-day storms; may still flag a busy work item that legitimately re-fires for ~1–2 h — prefer AND with state.db terminal/lost.

**Emitter:** `engine/core/src/coordinator/execution.rs` (redundant_spawn details including `live_execution_id`, `live_execution_liveness`, `live_execution_age_secs`).

**Disposition:** ship recurrence heuristic; **flag state.db join as required for high-confidence**. Pair findings with SIG-A (untracked-lease storm) on the same target.

---

### SIG-3 — Leaked pool claim — **NO EXEMPLAR** (honest non-signature)

**Problem.** Original brief wanted `pool_exhausted` while workers are idle. Across the retained window:

- Claim/release replay found **zero** events where outstanding claims < reported `live_workers`.
- 902 cap-hold events: claim ledger **balanced exactly**.
- `worker_claimed`/`skipped`/`pool_exhausted` carries only `pool` + `pool_capacity` (modern schema) — **no live-worker roster**, so it cannot distinguish saturation from leak by itself.

**Older schema variant (historical only):**

```
stage == "worker_claimed" && outcome == "skipped"
  && details.reason == "pool_exhausted"
  && details.ghost_active is a non-empty array
```

Count of older records with `ghost_active`: 1,270. That roster is what the pool _believed_ busy — closest in-JSONL leak discriminator, but still not proof of leak without liveness of each id.

**Nearest real evidence — post-hoc reconcile, not a leak detector:**

```
stage == "pool_claim_reconcile"   # 72 records, execution_status always "failed"
```

Measured gap between terminal `pane_spawned`/`error` (e.g. SlotBusy) and later `pool_claim_reconcile`: **667 s and 912 s** in exemplars — that interval is "pool still thinks busy, execution is dead."

**Proposed future predicate (implement only with care):**

1. Replay `worker_pool_claim` / `worker_pool_release` (and/or dispatch claim events) in timestamp order from **engine-trace**.
2. At each cap-hold (`spawn_attempt … held reason=interactive_concurrency_cap` with `live_workers`, `cap`), assert outstanding claims == `live_workers`.
3. Cross-check each claimed `execution_id` against non-terminal status in **state.db**.

**False-positive warning:** `live_workers == cap` is the **designed saturation path** (889 holds in one day in the evidence window) — not a fault.

**Disposition:** **do not ship a "leaked claim" signature as a hard fault.** Document as:

- `SIG-3a` **info**: `pool_exhausted` rate / cap-hold rate (capacity pressure)
- `SIG-3b` **info**: `pool_claim_reconcile` count in window (engine already reaped a stuck claim)
- `SIG-3c` **not-yet-specifiable as P0**: claim/release ledger imbalance — zero exemplars; leave as optional deep mode for the implementor with explicit "no positive exemplar in 2026-07-24 window"

**Emitter:** pool skip path `engine/core/src/coordinator/scheduler.rs` (`pool_exhausted`); reconcile stage `Stage::PoolClaimReconcile`.

---

### SIG-4 — Spawn-ack timeout with `shell_pid == 0` (real hang)

**Problem.** Provisional `shell_pid: 0` on successful spawn is **normal**; the app later calls `update_worker_shell_pid`. Encoding "pid 0 then quick completion" as failure would fire on nearly every spawn.

**Correct predicate (SHIP):**

```
stage == "spawn_ack_timeout"
  && details.shell_pid == 0
```

Count: **292**. `details.threshold_secs` always **60** (`SPAWN_ACK_GRACE_SECS` in `spawn_ack_sweep.rs`).

Meaning: no real pid **and** no hook event within 60 s of spawn; sweep reaped the never-started worker.

**Related (distinct) — pane never attached / liveness:**

```
stage == "execution_liveness_reconcile"
  && details.reason == "pane_never_attached"
```

**Related (distinct) — spawn capability circuit breaker:**

```
stage == "spawn_capability_unhealthy" && outcome == "error"
```

Count: 5.

**Related (distinct) — reap contradiction (trace-only):**

```
target contains "worker_events"   # or fields from boss_engine::app::worker_events
  && fields.message starts with
       "[engine-reconcile] live hook event arrived for a TERMINAL execution"
  && fields.kind == "session_end"
```

Count: **22**. A terminalized execution whose worker is still emitting hooks — **spec separately** (SIG-4b).

**Artifacts:** dispatch for the primary; engine-trace for SIG-4b.
**state.db:** no for primary match; optional to confirm execution status after reap.
**False-positive rate:** primary predicate is the engine's own reap decision — **~0% FP** relative to "ack never arrived." Do **not** add a bare `shell_pid==0` rule without the stage.
**Emitter:** `engine/core/src/spawn_ack_sweep.rs` (`SPAWN_ACK_GRACE_SECS=60`, details `shell_pid` + `threshold_secs`); provisional pid path `engine/core/src/spawn_flow.rs`, `runner/pane_spawn.rs`.

**Disposition:** ship SIG-4; ship SIG-4b as separate low-count finding.

---

### SIG-5 — Merge-queue rebounce (trace-only)

**Problem.** Queue-side CI failure on the synthetic merge commit. Lives **only** in `engine-trace.jsonl` — **zero** hits across all 274,623 dispatch records.

**Predicate (SHIP):**

```
Parse each engine-trace line as JSON (never substring-match the raw line).
target == "boss_engine::ci_watch"
  && fields.failure_kind == "merge_queue_rebounce"
```

Count: **6** in the store. On all 6, `fields.discriminator == fields.head_sha_at_trigger` held — equality is structural for this failure kind (emitter sets `head_sha_at_trigger` from the before-commit discriminator). **`failure_kind` alone is sufficient.**

**Do not use:**

- `before_commit_sha` as a required field key in trace — not emitted for doctor matching; naive `grep before_commit_sha engine-trace.jsonl` returns **197** hits, **all** inside embedded `PostToolUse` worker payloads.
- Substring match on the raw line for any of `before_commit`, `discriminator`, `merge_queue`.

**Adjacent and NOT rebounce (do not conflate):**

```
target == "boss_engine::completion::metadata_gate"
  && fields.message starts with
       "sha-delta gate: bound PR head unchanged — worker did not contribute"
```

Counts: 446 unchanged / 4 moved / 3 fetch-failed. Meaning: worker didn't contribute a new head — different failure class (optional SIG-5b info).

**Artifacts:** `engine-trace.jsonl` + rotated segments only.
**state.db:** optional (`ci_remediations` rows) for deeper forensics; not required to match.
**False-positive rate:** structured `failure_kind` match ≈ **0% FP**. Substring grep on `before_commit_sha` ≈ **100% FP**.
**Emitter:** `engine/core/src/ci_watch.rs` (`on_merge_queue_rebounce_detected` / `on_queue_side_failure_detected`; `discriminator` stored as `head_sha_at_trigger`); GraphQL `beforeCommit.oid` in merge poller; SQL in work/blocking mappers.

**Disposition:** ship; document JSON-parse-only rule in bold in the implementor notes.

#### SIG-5c — Trunk queue eviction (sibling of SIG-5; ship)

**Not a non-goal.** Current `main` emits a second queue-side failure kind that shares the same engine-trace message and field shape as SIG-5, differing only in `failure_kind` and provenance:

```
Parse each engine-trace line as JSON.
target == "boss_engine::ci_watch"
  && fields.failure_kind == "trunk_queue_eviction"
  && fields.message starts with
       "ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure"
```

| Dimension                             | SIG-5 `merge_queue_rebounce`                       | SIG-5c `trunk_queue_eviction`                                                                                                 |
| ------------------------------------- | -------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Trigger                               | GitHub merge-queue FAILED_CHECKS / synthetic merge | Trunk merge queue eviction (`trunk_queue_poller::handle_trunk_queue_eviction` → `ci_watch::on_trunk_queue_eviction_detected`) |
| `failure_kind`                        | `merge_queue_rebounce`                             | `trunk_queue_eviction`                                                                                                        |
| Discriminator / `head_sha_at_trigger` | synthetic merge `beforeCommit.oid`                 | `trunk_eviction_discriminator(entry_id, stateChangedAt)`                                                                      |
| `before_commit_sha` column            | set                                                | **None** (Trunk has no synthetic-merge SHA to persist)                                                                        |
| Shared helper                         | `on_queue_side_failure_detected`                   | same                                                                                                                          |
| `is_queue_side_failure_kind`          | true                                               | true                                                                                                                          |

Both are queue-side (evidence lives off the PR's own head). Doctor should treat them as sibling SIGs (shared matcher, branch on `failure_kind`), not fold Trunk into "rebounce" wording.

**Corpus note:** the 2026-07-24 window's 6 queue-side hits were all `merge_queue_rebounce`; Trunk eviction may be zero in that window but is a live emitter on current `main` (`ci_watch.rs` + `trunk_queue_poller.rs`). Ship the predicate anyway — same FP profile as SIG-5.

**Disposition:** ship as SIG-5c; do not list as non-goal.

---

### SIG-6 — Cube workspace lease timeout

**Predicate (SHIP):**

```
stage == "cube_workspace_lease_failed"
  && outcome == "error"
  && details.reason == "timeout"
```

Corpus count: **115** of 5,227 lease failures.

Corroboration (optional, schema-dependent):

- `error_message` matches `cube workspace lease timed out after Xs` (verbatim template in `coordinator/execution.rs`)
- Preceding `cube_workspace_lease_attempted` for same `execution_id` has `details.timeout_ms` equal to the configured bound

**Code vs corpus note (important for implementor):**

| Source                                | Timeout                                                           |
| ------------------------------------- | ----------------------------------------------------------------- |
| 2026-07-24 corpus error text          | `… timed out after 30s`, `timeout_ms: 30000`                      |
| Current `main` (`CUBE_LEASE_TIMEOUT`) | **`Duration::from_secs(90)`** in `engine/core/src/coordinator.rs` |

Doctor must match **`details.reason == "timeout"`**, not a hard-coded `30000` or the string `30s`. Prefer parsing the seconds from the error message or reading `timeout_ms` from the attempt record.

**Fleet-wide variant (SHIP as severity escalate):**

```
Within a short window (e.g. 10 min):
  count(distinct execution_id where SIG-6) >= K   (recommend K=3)
  && details.attempt escalating
  && details.fallback_policy degrading any_free → none
```

**False-positive rate:**

- Match on stage alone (`cube_workspace_lease_failed`): **~45× over-fire** (5,015 `cube_error` + 97 `workspace_occupied_by_live_worker` + 115 timeout).
- Dominant non-timeout: `jj git fetch` failed (~4,199) under `reason:"cube_error"` — different signature if desired (optional SIG-6b).

**Artifacts:** `current.jsonl`.
**state.db:** no.
**Emitter:** `CUBE_LEASE_TIMEOUT` in `engine/core/src/coordinator.rs`; attempt/fail emission in `engine/core/src/coordinator/execution.rs` (`timeout_ms` on attempt, `reason: "timeout"` on fail, error `cube workspace lease timed out after {}s`).

**Disposition:** ship reason-filtered predicate; optional fleet aggregation.

---

## 4. Additional recurring failure shapes (required by evidence)

### SIG-A — Untracked-lease heartbeat storm (**loudest failure in the store**)

```
stage == "cube_lease_heartbeat"
  && outcome == "error"
  && error_message matches /lease `[^`]+` is not tracked/
```

Count: **13,479 (≈5% of ALL dispatch records)**. Six lease UUIDs account for 12,958 hits; each retries ~every 300 s for days against the same three zombie executions as SIG-2.

**Ship as:** group by `cube_lease_id` (or parsed UUID) and/or `execution_id`; emit one finding per storm with `count` and duration, not 13k rows.

**False-positive rate:** each record is a real heartbeat failure; the FP risk is **alert spam** if unaggregated. Aggregation is mandatory.

**Emitter:** `engine/core/src/cube_lease_heartbeat.rs`.

**Severity:** P0 when co-occurring with SIG-2 on the same execution; else P1.

---

### SIG-B — SlotBusy spawn collision (engine/app slot desync)

```
stage == "pane_spawned"
  && outcome == "error"
  && details.slot_busy != null
```

Count: **500**. This is **not** capacity exhaustion; it is slot desync (`occupying_run_id` in details). Always followed by `pool_claim_reconcile` in observed exemplars.

**Emitter:** `engine/core/src/coordinator/run.rs` (builds `error_details["slot_busy"]`).

**Severity:** P1.

---

### SIG-C — Auto-bind / deleted-row write storm (**historical forensic; demoted**)

**Corpus-only predicate (2026-07-24 window — DO NOT treat as current-main hard predicate):**

```
stage == "status_transitioned"      # historical wire name only
  && outcome == "error"
  && details.source == "auto_bind_poller"
```

Count in that window: **2,276**. Only three error strings (~759× each):

- `unknown task for execution: proj_…`
- `cannot complete a deleted task: task_…` (two task-id variants)

**Current-main status (verified against emitters):**

| Claim                                  | Current `main`                                                                                                                                                                                                                                      |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Stage wire `status_transitioned`       | **Gone.** Live enum is `Stage::StatusTransition` → `"status_transition"` (`dispatch-events/src/lib.rs`). Kanban / control-verb tests emit only `status_transition`.                                                                                 |
| `details.source == "auto_bind_poller"` | **No emission site** remains on current `main`. Grep finds this string only in this investigation note.                                                                                                                                             |
| Error text still possible              | Yes — `engine/core/src/work/pr_flow.rs` still bails with `unknown task for execution: {id}` and `cannot complete a deleted task: {id}` — but those are call-site errors, not a dedicated auto-bind storm emitter with the corpus stage/source pair. |

**Disposition: demote from "ship hard fault on current main" to historical forensic.**

- **Historical / `--all-history` (optional):** keep the corpus predicate + error-message prefixes for forensics on old `current.jsonl` segments.
- **Current-main default doctor pass:** **do not ship** `stage ∈ {status_transitioned,status_transition} && details.source == "auto_bind_poller"` as a hard P1 — it cannot fire on a fleet that never wrote those records.
- **Optional weak current-main probe (info only, not a hard fault):** `error_message` starts with `unknown task for execution:` or `cannot complete a deleted task:` on any dispatch stage — useful if a future caller reintroduces unbounded retry against deleted rows, but **zero evidence** that today's emitters still produce a storm shape.

**Severity (historical):** P1 when the corpus storm shape is present. **Severity (current main default):** not a shipped hard signature.

**Emitter (error text only):** `engine/core/src/work/pr_flow.rs`.

---

### SIG-D — Transient-recovery exhaustion (wire `reason` / `class` split)

**Current-main predicate (SHIP):**

```
stage == "transient_recovery_exhausted"
  && outcome == "error"
  && details.reason ∈ {"retries_exhausted", "permanent_error"}
```

These are the **only** values `EscalateReason::as_str` emits (`engine/transient-error/src/lib.rs`):

| `details.reason` (why policy stopped) | Meaning                                                          |
| ------------------------------------- | ---------------------------------------------------------------- |
| `permanent_error`                     | `ErrorClass::Permanent` — escalate immediately, no retries spent |
| `retries_exhausted`                   | retry cap hit for `Transient` **or** `Indeterminate`             |

`details.class` is a **separate** field (`ErrorClass::as_str`) and is **not** a substitute for `reason`:

| `details.class` | Meaning                                                                             |
| --------------- | ----------------------------------------------------------------------------------- |
| `transient`     | confirmed retryable infrastructure error                                            |
| `permanent`     | confirmed non-retryable                                                             |
| `indeterminate` | unrecognized API error text — still gets the same bounded retry budget as transient |

**Do not match** `details.reason == "unrecognized_error"` — that string is **not** on the wire on current `main`. An unrecognized error that exhausts its budget emits:

```
reason == "retries_exhausted" && class == "indeterminate"
```

(pinned by `unrecognized_error_at_cap_escalates_with_indeterminate_class` in `transient_recovery.rs`).

**Severity ladder on the shipped predicate:**

- `reason == "permanent_error"` → P1 (auth/billing/invalid; expected escalate-at-0)
- `reason == "retries_exhausted" && class == "indeterminate"` → P1 (true classifier gap after budget spent)
- `reason == "retries_exhausted" && class == "transient"` → P1 (infra kept failing through the budget)

**Historical corpus narrative (2026-07-24 — pre-classifier fix; do not re-encode as current main):**

Corpus count **15** used a now-obsolete shape (`reason`/`class` conflation around unrecognized text). All 15 were `API Error: Unable to connect to API (ConnectionRefused)` with `prior_attempts: 0`, `max_attempts: 3`. On **current `main`**, `ConnectionRefused` (and related network shapes) classifies as **`ErrorClass::Transient`** (`connection_refused_and_network_errors_are_transient` in `transient-error`), and at `prior_attempts == 0` the recovery path **resumes / nudges** rather than escalating (`connection_refused_resumes_instead_of_escalating_at_zero_attempts` in `transient_recovery.rs`). Doctor must **not** claim "ConnectionRefused ⇒ indeterminate / zero retries spent" against current emitters.

Contrast: stage `transient_recovery` (non-exhausted resume events) still exists for successful retry scheduling.

**Emitter:** `engine/core/src/transient_recovery.rs` (`Stage::TransientRecoveryExhausted` details: `reason`, `class`, `prior_attempts`, `max_attempts`, `error`); classifier `engine/transient-error/src/lib.rs`.

---

### SIG-E — libghostty surface nack

```
stage == "spawn_nack"
```

Count: **4**. Observed `details.reason`:

`libghostty surface creation failed (ghostty_surface_new returned NULL — likely no active display after sleep/wake)`

**Severity:** P2 (host/display condition).

**Emitter:** `spawn_ack_sweep.rs` AppNack → `Stage::SpawnNack`.

---

### SIG-F — Worker-id parse failure

```
stage == "pane_spawned"
  && outcome == "error"
  && error_message matches /does not parse as worker/
```

Count: **7** historically (`received worker_id "auto-worker-2" that does not parse as worker-{N}` — pool-prefix naming bug).

**Code note:** current `runner/pane_spawn.rs` message accepts `worker-{N}`, `auto-worker-{N}`, or `review-{N}`. Historical records still match the regex; new fleet may be clean. Keep the signature for regressions.

**Severity:** P2.

---

### SIG-G — Scheduler wakeup drop (trace-only)

```
fields.message starts with
  "scheduler heartbeat: ready execution(s) older than the heartbeat interval found"
```

Count: **19**. Kick/drain handoff dropped a wakeup (status flipped but scheduler never drained).

**Emitter:** `engine/core/src/coordinator/scheduler.rs`.

**Severity:** P1 when paired with `status_transition` and no follow-up `request_recorded` for the same work item (see also SIG-1 / ghost-active).

---

### SIG-H — Orphan-sweep churn (capacity / noise, not a fault by itself)

```
stage == "dispatch_decision"
  && details.loop == "orphan_active_sweep"
  && details.live_execution_claimed == true
```

Count: **10,058 of 10,091** `dispatch_decision` records. Re-evaluates the same live executions thousands of times.

**Disposition:** **info** metric — rate of re-evaluation per execution_id. Flag only if volume is pathological relative to fleet size (e.g. top execution alone accounts for thousands of decisions/day). Do not page on raw presence.

**Emitter:** `engine/core/src/orphan_sweep.rs`.

---

## 5. Summary table (implementor checklist)

| ID     | Name                            | Primary artifact | state.db?                   | Ship?                                              | Severity    | Corpus scale                            |
| ------ | ------------------------------- | ---------------- | --------------------------- | -------------------------------------------------- | ----------- | --------------------------------------- |
| SIG-1  | Persistent worker_claimed stall | dispatch         | no                          | yes (**recompute** elapsed from last real event)   | P1–P0       | 2720 naive / ~1% actionable             |
| SIG-2  | redundant_spawn zombie          | dispatch (+db)   | **yes** for high confidence | yes (recurrence)                                   | P0          | 2625 supersedes; few multi-day zombies  |
| SIG-3  | Leaked claim                    | trace+db         | yes                         | **no hard fault**; info only                       | info        | 0 leak exemplars                        |
| SIG-4  | spawn_ack_timeout pid 0         | dispatch         | no                          | yes                                                | P1          | 292                                     |
| SIG-4b | hook after terminal             | trace            | optional                    | yes                                                | P2          | 22                                      |
| SIG-5  | merge_queue_rebounce            | trace            | optional                    | yes                                                | P1          | 6                                       |
| SIG-5b | sha-delta gate (unchanged head) | trace            | no                          | optional info                                      | info        | 446/4/3                                 |
| SIG-5c | trunk_queue_eviction            | trace            | optional                    | yes (sibling of SIG-5)                             | P1          | 0 in 2026-07-24 window; live emitter    |
| SIG-6  | lease timeout                   | dispatch         | no                          | yes (`reason==timeout`)                            | P1–P0 fleet | 115                                     |
| SIG-A  | untracked lease heartbeat       | dispatch         | no                          | yes (**aggregate**)                                | P0/P1       | 13479                                   |
| SIG-B  | SlotBusy                        | dispatch         | no                          | yes                                                | P1          | 500                                     |
| SIG-C  | deleted-row auto-bind storm     | dispatch         | no                          | **historical forensic only** (demoted)             | P1 hist.    | 2276 (corpus); no current-main storm    |
| SIG-D  | transient_recovery_exhausted    | dispatch         | no                          | yes (`reason∈{retries_exhausted,permanent_error}`) | P1          | 15 historical; wire shape fixed on main |
| SIG-E  | spawn_nack / libghostty         | dispatch         | no                          | yes                                                | P2          | 4                                       |
| SIG-F  | worker_id parse                 | dispatch         | no                          | yes                                                | P2          | 7                                       |
| SIG-G  | scheduler wakeup drop           | trace            | no                          | yes                                                | P1          | 19                                      |
| SIG-H  | orphan_active_sweep churn       | dispatch         | no                          | info only                                          | info        | 10058                                   |

---

## 6. False-positive rules of thumb (encode in doctor UX)

1. **Never substring-match raw JSONL lines** for field names that appear inside nested worker payloads (`before_commit_sha`, module paths, stage names in prompt text). Always `serde_json` then field access.
2. **Never treat a stage as a failure without its outcome/reason vocabulary** (`cube_workspace_lease_failed` alone is 45× too broad; `redundant_spawn` alone is the happy supersede path; `stage_stalled` alone is mostly chain-hold telemetry).
3. **Never treat `shell_pid == 0` without `spawn_ack_timeout` (or a proven absence of healing `update_worker_shell_pid` after the grace window).**
4. **Never claim "execution completed" from dispatch alone** — join `state.db` or use a recurrence / heartbeat-storm proxy.
5. **Aggregate storms** (SIG-A, SIG-2, historical SIG-C, SIG-H) to one finding per key; print counts and windows.
6. **Default time window** so multi-week historical zombies do not dominate a fresh doctor run unless `--all-history`.
7. **Never gate SIG-1 severity on `stage_stalled.details.elapsed_in_stage_ms`** — recompute from last non-`stage_stalled` event; progress is any later real event, not only `request_recorded`.
8. **Never match SIG-D on `reason == "unrecognized_error"`** — wire reasons are only `retries_exhausted` / `permanent_error`; unrecognized lives in `class == "indeterminate"`.

---

## 7. Validation performed for this note

| Check                                                          | Result                                                                                                                                                        |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bossctl --help` lists `doctor`?                               | **No** — `error: unrecognized subcommand 'doctor'`                                                                                                            |
| `bossctl dispatch` subcommands                                 | `tail`, `diagnose`, `ghost-active`, `pause`, `resume`, `state`, `stats`, `concurrency`                                                                        |
| `bossctl live-status`                                          | `debug` only                                                                                                                                                  |
| `bossctl logs` sources                                         | `engine`, `audit`, `dispatch`, `spawn`, `population-timing`                                                                                                   |
| `Stage` enum still 44 variants with documented wire strings    | yes (`dispatch-events/src/lib.rs`)                                                                                                                            |
| 8 never-observed stages still have emitters                    | yes — rare, not dead                                                                                                                                          |
| Stall thresholds                                               | default 120 s; overrides include `worker_claimed`/`host_selected`/`cube_workspace_lease_attempted` @ 30 s; detector interval 15 s; persistent attention 5 min |
| Stall elapsed source of truth                                  | `now - last non-stage_stalled event` (`TimelineState`); `stage_stalled.details.elapsed_in_stage_ms` is first-trip snapshot only                               |
| `EscalateReason` wire                                          | only `retries_exhausted` / `permanent_error`; `class` is independent (`transient`/`permanent`/`indeterminate`)                                                |
| `ConnectionRefused` on current main                            | `ErrorClass::Transient`; resumes at `prior_attempts==0` (not escalate-as-indeterminate)                                                                       |
| `CUBE_LEASE_TIMEOUT`                                           | **90 s** on current `main` (corpus showed 30 s)                                                                                                               |
| `is_terminal_event`                                            | error outcome OR `pane_spawned`+ok — not worker completion                                                                                                    |
| `parse_lines` tolerates bad lines                              | yes                                                                                                                                                           |
| `failure_kind` queue-side pair                                 | `merge_queue_rebounce` (SIG-5) and `trunk_queue_eviction` (SIG-5c) both live on current main                                                                  |
| SIG-C `auto_bind_poller` / `status_transitioned`               | **historical only** — no current-main emitter for that stage/source pair                                                                                      |
| Wrong premises (before_commit_sha / shell_pid0 / leaked claim) | explicitly excluded                                                                                                                                           |

This investigation did **not** re-read the live Application Support tree (unavailable to cube workers). Counts and exemplars are those inlined by the coordinator on 2026-07-24; emitter paths were re-verified against the mono workspace at investigation time.

---

## 8. Recommended doctor UX sketch (non-normative)

```
$ bossctl doctor
doctor: scanned dispatch-events/current.jsonl (last 24h) + engine-trace + state.db (read-only)

P0  SIG-2  redundant_spawn zombie
    live_execution_id=exec_…  refs=608  window=17.5d  state=running (lease untracked)
    also: SIG-A untracked-lease storm on lease a2a7ae36-… (n=4319)
    next: bossctl dispatch diagnose exec_… ; consider force-release / retention sweep

P1  SIG-6  cube lease timeouts
    n=12 distinct executions in 10m  fallback_policy→none
    next: cube workspace list ; check free pool / jj fetch health

info SIG-C  (historical only; suppressed unless --all-history)
    n=759  error="cannot complete a deleted task: task_…"  stage=status_transitioned source=auto_bind_poller
    next: forensic only — no auto_bind_poller emitter on current main

info SIG-3a pool saturation
    pool_exhausted skips=…  cap holds balanced (no claim leak detected)
```

Exit codes (suggestion): `0` clean / info-only; `1` any P1+; `2` scan failure (missing root, unreadable db).

---

## 9. Hand-off to the implementor task

1. Add `bossctl doctor` as a new top-level subcommand; file-scan-first; reuse `boss_dispatch_reader` + `boss_log_files`.
2. Implement **ship** matchers: SIG-1 (recompute elapsed; any real progress clears), SIG-4, SIG-4b, SIG-5, **SIG-5c**, SIG-6, SIG-A, SIG-B, SIG-D (`reason∈{retries_exhausted,permanent_error}` + independent `class`), SIG-E, SIG-F, SIG-G. SIG-2 with recurrence + optional state.db. SIG-3 and SIG-H as info-only. **SIG-C historical-only** (optional under `--all-history`), not a default current-main hard fault.
3. Unit-test each **shipped** predicate against the exemplar JSON blobs in Appendix B (copy into `bossctl` tests as fixtures — do not require a live store). SIG-C fixtures are forensic-only.
4. Document `--window`, `--all-history`, `--json`, and "works when engine is wedged" next to `dispatch diagnose`.
5. Do **not** reintroduce the three wrong premises, the SIG-1 frozen-elapsed gate, `reason==unrecognized_error`, or "ConnectionRefused ⇒ indeterminate at zero retries" against current main.

---

## Appendix A — Top `stage[outcome]` frequencies (context)

From the 2026-07-24 corpus (abbreviated): `request_recorded[ok]` 65341, `worker_claimed[ok]` 52517, `cube_workspace_lease_attempted[ok]` 15589, **`cube_lease_heartbeat[error]` 13479**, `cube_repo_ensured[ok]` 12893, `worker_claimed[skipped]` 12852, `host_selected[ok]` 10693, `cube_workspace_leased[ok]` 10419, `dispatch_decision[ok]` 10091, `run_started[ok]` 9972, `pane_spawned[ok]` 8730, `stage_stalled[ok]` 6531, `cube_workspace_lease_failed[error]` 5227, `host_selected[error]` 2643, `status_transitioned[error]` 2276, `pane_spawned[error]` 1208, `spawn_ack_timeout[ok]` 292, `transient_recovery_exhausted[error]` 15, `spawn_nack[ok]` 4.

Key `details.reason` vocabularies:

- `worker_claimed[skipped]`: `pool_exhausted` 5716 / `chain_serialized_review_held` 4927 / `chain_serialized` 2209
- `host_selected[error]`: `redundant_spawn` 2625 / `work_item_unresolved` 13 / …
- `cube_workspace_lease_failed`: `cube_error` 5015 / `timeout` 115 / `workspace_occupied_by_live_worker` 97
- `stage_stalled.stalled_stage`: `worker_claimed` 2720 / `dispatch_decision` 1154 / `husk_pane_reconcile` 1019 / …

## Appendix B — Exemplar records (for fixture tests)

Fixtures below cover every **ship** SIG (plus historical SIG-C). Shapes match current-main emitters unless marked historical. Minimal fields only — real records also carry null cube/worker columns.

### SIG-1 — Benign chain hold (stall then clear via non-request progress)

Note: clear event here is `request_recorded`, but doctor must also treat any later non-`stage_stalled` event as progress. Frozen `elapsed_in_stage_ms: 42874` must **not** be the severity gate.

```json
{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_18c5513362ac2ed8_148","work_item_id":"task_18c5513362a6cbf0_147","details":{"reason":"chain_serialized_review_held","review_held":true,"live_sibling_execution_id":"exec_18c55030d2848c40_f6","live_sibling_work_item_id":"task_18c54f8b8399c960_b7"}}
{"ts_epoch_ms":1784923334223,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{"stalled_stage":"worker_claimed","stalled_outcome":"skipped","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}
{"ts_epoch_ms":1784923379833,"stage":"request_recorded","outcome":"ok","execution_id":"exec_18c5513362ac2ed8_148","details":{"preferred_workspace_id":null,"pool":"main","dispatch_class":3,"dispatch_class_label":"pr_review_revision","pool_ready_count":1,"beaten_candidates":0}}
```

### SIG-1 — Actionable durable stall (synthetic; recompute elapsed)

Last real event is still `worker_claimed` at T0; a single first-trip `stage_stalled` freezes `elapsed_in_stage_ms` near the 30 s threshold. At scan time `now_ms = T0 + 400_000`, recomputed elapsed is 400 s → critical (≥ `PERSISTENT_STALL_THRESHOLD`), even though the wire field is only ~43 s.

```json
{"ts_epoch_ms":1784923279414,"stage":"worker_claimed","outcome":"skipped","execution_id":"exec_sig1_durable","work_item_id":"task_sig1_durable","details":{"reason":"chain_serialized","pool":"main","pool_capacity":12}}
{"ts_epoch_ms":1784923322288,"stage":"stage_stalled","outcome":"ok","execution_id":"exec_sig1_durable","details":{"stalled_stage":"worker_claimed","stalled_outcome":"skipped","stalled_at_ts_epoch_ms":1784923279414,"elapsed_in_stage_ms":42874}}
```

### SIG-2 zombie host_selected + target heartbeat (also seeds SIG-A):

```json
{"ts_epoch_ms":1781945100914,"stage":"host_selected","outcome":"error","execution_id":"exec_18babda0cc3a99b0_1164","work_item_id":"auto_18b509b3e8944fa8_5a","worker_id":"auto-worker-2","details":{"reason":"redundant_spawn","live_execution_id":"exec_18b9288b7fd1c568_3"}}
{"ts_epoch_ms":1781515001787,"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"exec_18b9288b7fd1c568_3","cube_lease_id":"a2a7ae36-0e16-4858-8e50-a8e54055e71a","error_message":"Cube command failed: {\n  \"error\": \"lease `a2a7ae36-0e16-4858-8e50-a8e54055e71a` is not tracked\"\n}","details":{"ttl_secs":1800,"cube_workspace_id":"mono-agent-035"}}
```

### SIG-A — Untracked-lease heartbeat (aggregate by lease / execution)

```json
{"ts_epoch_ms":1781515001787,"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"exec_18b9288b7fd1c568_3","cube_lease_id":"a2a7ae36-0e16-4858-8e50-a8e54055e71a","error_message":"Cube command failed: {\n  \"error\": \"lease `a2a7ae36-0e16-4858-8e50-a8e54055e71a` is not tracked\"\n}","details":{"ttl_secs":1800,"cube_workspace_id":"mono-agent-035"}}
{"ts_epoch_ms":1781515301787,"stage":"cube_lease_heartbeat","outcome":"error","execution_id":"exec_18b9288b7fd1c568_3","cube_lease_id":"a2a7ae36-0e16-4858-8e50-a8e54055e71a","error_message":"Cube command failed: {\n  \"error\": \"lease `a2a7ae36-0e16-4858-8e50-a8e54055e71a` is not tracked\"\n}","details":{"ttl_secs":1800,"cube_workspace_id":"mono-agent-035"}}
```

### SIG-4 spawn_ack_timeout:

```json
{
  "ts_epoch_ms": 1784895019484,
  "stage": "spawn_ack_timeout",
  "outcome": "ok",
  "execution_id": "exec_18c5387f08addde0_211",
  "details": { "slot_id": 25, "shell_pid": 0, "recovery_patch": null, "threshold_secs": 60 }
}
```

### SIG-4b — Hook after terminal (engine-trace):

```json
{
  "timestamp": "2026-07-24T12:00:00.000000Z",
  "level": "WARN",
  "fields": {
    "message": "[engine-reconcile] live hook event arrived for a TERMINAL execution — the engine believes this run is dead but its worker is still emitting hooks. This is the ack-timeout / stale-reap contradiction (a run that should have stayed tracked was terminalized). Not resurrecting the row here; surfacing the live-liveness signal so the reconcilers and operators can act instead of silently dropping it.",
    "run_id": "exec_18c5387f08addde0_211",
    "kind": "session_end",
    "status": "orphaned",
    "work_item_id": "task_example"
  },
  "target": "boss_engine::app::worker_events"
}
```

### SIG-5 merge_queue_rebounce (trace):

```json
{
  "timestamp": "2026-07-24T21:45:44.118022Z",
  "level": "INFO",
  "fields": {
    "message": "ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure",
    "work_item_id": "task_18c5549a33ae7248_29c",
    "pr_url": "https://github.com/spinyfin/mono/pull/2298",
    "discriminator": "8d075ec06ac101f204a04da9f3f86b65a88d705f",
    "head_sha_at_trigger": "8d075ec06ac101f204a04da9f3f86b65a88d705f",
    "failure_kind": "merge_queue_rebounce",
    "task_transitioned": true,
    "task_unblocked_for_revision": true
  },
  "target": "boss_engine::ci_watch"
}
```

### SIG-5c trunk_queue_eviction (trace; synthetic from current-main emitter shape):

Same message as SIG-5; branch on `failure_kind`. Discriminator is `trunk_eviction_discriminator(entry_id, stateChangedAt)`, not a git SHA; no `before_commit_sha` on the attempt row.

```json
{
  "timestamp": "2026-07-26T00:00:00.000000Z",
  "level": "INFO",
  "fields": {
    "message": "ci_watch: queue-side failure detected; parent flipped to blocked: ci_failure",
    "work_item_id": "task_trunk_example",
    "pr_url": "https://github.com/spinyfin/mono/pull/9999",
    "discriminator": "trunk:entry-abc@2026-07-26T00:00:00Z",
    "head_sha_at_trigger": "trunk:entry-abc@2026-07-26T00:00:00Z",
    "failure_kind": "trunk_queue_eviction",
    "task_transitioned": true,
    "task_unblocked_for_revision": true
  },
  "target": "boss_engine::ci_watch"
}
```

### SIG-6 lease timeout:

```json
{
  "ts_epoch_ms": 1784879999312,
  "stage": "cube_workspace_lease_failed",
  "outcome": "error",
  "execution_id": "exec_18c52adc4298baf8_ea",
  "worker_id": "worker-7",
  "cube_repo_id": "mono",
  "error_message": "cube workspace lease timed out after 30s",
  "details": {
    "attempt": 1,
    "reason": "timeout",
    "fallback_policy": "any_free",
    "allow_dirty": false,
    "excluded_workspace_ids": []
  }
}
```

### SIG-B SlotBusy → pool_claim_reconcile:

```json
{"ts_epoch_ms":1784927838618,"stage":"pane_spawned","outcome":"error","execution_id":"exec_18c55660875e0648_3ad","worker_id":"review-2","error_message":"spawning worker pane for run exec_18c55660875e0648_3ad: app reported spawn error: SlotBusy { occupying_run_id: Some(\"exec_18c5565d7a4a6990_3a8\") }","details":{"released_workspace":true,"slot_id":26,"slot_busy":{"slot_id":26,"occupying_run_id":"exec_18c5565d7a4a6990_3a8"}}}
{"ts_epoch_ms":1784928505704,"stage":"pool_claim_reconcile","outcome":"ok","execution_id":"exec_18c55660875e0648_3ad","worker_id":"review-2","details":{"pool":"review","worker_id":"review-2","execution_status":"failed"}}
```

### SIG-C — Historical auto-bind storm only (not current-main hard predicate)

```json
{"ts_epoch_ms":1784000000000,"stage":"status_transitioned","outcome":"error","execution_id":"exec_hist_autobind","work_item_id":"task_deleted_example","error_message":"cannot complete a deleted task: task_deleted_example","details":{"source":"auto_bind_poller"}}
{"ts_epoch_ms":1784000001000,"stage":"status_transitioned","outcome":"error","execution_id":"exec_hist_autobind2","work_item_id":"proj_unknown_example","error_message":"unknown task for execution: proj_unknown_example","details":{"source":"auto_bind_poller"}}
```

### SIG-D — Current-main wire shapes (`reason` / `class` split)

Retries exhausted after unrecognized error (true classifier-gap exhaustion):

```json
{
  "ts_epoch_ms": 1784900000000,
  "stage": "transient_recovery_exhausted",
  "outcome": "error",
  "execution_id": "exec_sigd_indet",
  "work_item_id": "task_sigd_indet",
  "details": {
    "reason": "retries_exhausted",
    "class": "indeterminate",
    "prior_attempts": 3,
    "max_attempts": 3,
    "error": "some never-before-seen API blurb"
  }
}
```

Permanent error (escalate at zero retries — expected):

```json
{
  "ts_epoch_ms": 1784900001000,
  "stage": "transient_recovery_exhausted",
  "outcome": "error",
  "execution_id": "exec_sigd_perm",
  "work_item_id": "task_sigd_perm",
  "details": {
    "reason": "permanent_error",
    "class": "permanent",
    "prior_attempts": 0,
    "max_attempts": 3,
    "error": "authentication_error: invalid x-api-key"
  }
}
```

Confirmed-transient exhausted (infra never cleared):

```json
{
  "ts_epoch_ms": 1784900002000,
  "stage": "transient_recovery_exhausted",
  "outcome": "error",
  "execution_id": "exec_sigd_trans",
  "work_item_id": "task_sigd_trans",
  "details": {
    "reason": "retries_exhausted",
    "class": "transient",
    "prior_attempts": 3,
    "max_attempts": 3,
    "error": "API Error: Unable to connect to API (ConnectionRefused)"
  }
}
```

**Negative fixture (must NOT match historical wrong predicate):** there is no current-main emission of `details.reason == "unrecognized_error"`. ConnectionRefused at `prior_attempts==0` resumes, so no `transient_recovery_exhausted` row is written for that case on current main.

### SIG-E — spawn_nack / libghostty:

```json
{
  "ts_epoch_ms": 1784890000000,
  "stage": "spawn_nack",
  "outcome": "ok",
  "execution_id": "exec_sig_e",
  "work_item_id": "task_sig_e",
  "details": {
    "slot_id": 3,
    "shell_pid": 0,
    "recovery_patch": null,
    "reason": "libghostty surface creation failed (ghostty_surface_new returned NULL — likely no active display after sleep/wake)"
  }
}
```

### SIG-F — Worker-id parse failure (historical message; regex still valid):

```json
{
  "ts_epoch_ms": 1783000000000,
  "stage": "pane_spawned",
  "outcome": "error",
  "execution_id": "exec_sig_f",
  "worker_id": "auto-worker-2",
  "error_message": "PaneSpawnRunner received worker_id \"auto-worker-2\" that does not parse as worker-{N}",
  "details": { "slot_id": 2 }
}
```

Current-main message accepts `worker-{N}`, `auto-worker-{N}`, or `review-{N}`; keep the regex `/does not parse as worker/` for regressions.

### SIG-G — Scheduler wakeup drop (engine-trace):

```json
{
  "timestamp": "2026-07-24T18:00:00.000000Z",
  "level": "WARN",
  "fields": {
    "message": "scheduler heartbeat: ready execution(s) older than the heartbeat interval found — kick/drain handoff may have dropped a wakeup; re-kicking now",
    "count": 2,
    "oldest_age_ms": 45000,
    "execution_ids": ["exec_stranded_1", "exec_stranded_2"]
  },
  "target": "boss_engine::coordinator::scheduler"
}
```
