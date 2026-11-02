# Background-sweep telemetry

The engine runs a handful of periodic reconciliation sweeps that walk the
live worker slots once a minute, plus a per-worker tail that polls the
agent's JSONL rollout every 50 ms. Between dispatch bursts those sweeps are
most of what the engine does, so their per-pass cost is what idle engine
CPU is made of. This page lists the telemetry each sweep publishes, what
its cost scales with, and what the 2026-10-21 sample found.

## Metrics

Every sweep below publishes duration, items and pass counts through the engine
metrics registry (`bossctl metrics list --prefix <sweep>`; values flush to
`state.db` every 30 s):

| Series                     | Meaning                                                       |
| -------------------------- | ------------------------------------------------------------- |
| `<sweep>.pass_duration_ms` | wall-clock of the most recent pass                            |
| `<sweep>.items_per_pass`   | rows / slots / files the most recent pass touched             |
| `<sweep>.tick_interval_ms` | observed gap between the starts of the two most recent passes |
| `<sweep>.passes`           | monotonic pass count; its rate is the real tick rate          |

| Sweep prefix                  | What a pass is                                                                             | What `items_per_pass` counts                                                       |
| ----------------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| `envelope_watch`              | one 60 s envelope-watchdog sweep                                                           | live worker slots evaluated                                                        |
| `terminal_work_sweep`         | the candidate-gathering phase of one 60 s terminal-work pass                               | live worker slots inspected                                                        |
| `dead_pane_terminal`          | the terminal execution scan plus the process probes of one pane-death sweep pass           | recent terminal execution rows probed                                              |
| `agent_jsonl_stream`          | one 50 ms tail poll iteration of any live JSONL stream                                     | sessions-root revalidations (`lstat` + `canonicalize`) performed in that iteration |
| `execution_bookmark_pointers` | one recovery-pointer snapshot or restore over a PR chain (mechanical / speculative rebase) | recovery-bookmark records covered                                                  |

Only the singleton envelope and terminal-work sweeps publish tick intervals.
`dead_pane_terminal` has no `tick_interval_ms`. Concurrent tails and bookmark
operations have no shared cadence. Root revalidation counts are isolated per
tail, excluding discovery and other tails.

The periodic `speculative_conflict` sweep drives the pointer snapshot/restore
series, as do mechanical rebases. The separate five-minute
`abandoned_execution_bookmarks` pass is not part of the sampled pointer hotspot
and is not represented by this series.

Two extra counters:

- `agent_jsonl_stream.root_revalidations` — total root revalidations across
  every stream. Its rate relative to `agent_jsonl_stream.passes` is the
  per-poll pathname-walk cost; after the fix below an idle poll contributes
  zero.
- `execution_bookmark_pointers.jj_spawns` — total `jj` child processes
  spawned by pointer passes. Spawns are the whole cost of those passes.

`agent_jsonl_stream.*` gauges are last-sample values across all live
streams (each stream overwrites them); the counters are the aggregate.

## What each sweep cost, and why

A 10 s `sample` of the live engine on 2026-10-21 (~8 workers, between
bursts, 70–130 % CPU) attributed the non-idle time as follows.

**~48 % in `WorkDb::get_work_item` called once per live slot per pass.**
Three callers needed one field each — `effort_level` (envelope watchdog),
`status` (terminal-work sweep, via `is_bound_work_item_closed`), and the
`T<n>` label / name / `pr_url` of a chain sibling (the dispatch drain's
wait-reason text) — but the single-item read builds the full kanban card:
revision-chain slices plus the AI-review badge, whose query is a recursive
revision-family CTE with a `ROW_NUMBER` window over `pr_review_verdicts`.
That is SQLite page I/O (`pread` was 48 % of leaf samples), repeated N+1
per pass. Fix: narrow reads in `tools/boss/engine/work-db/src/sweep_reads.rs`
(`task_effort_levels`, `task_closed_states`, `task_pr_url`,
`task_wait_label`), batched per pass with one `IN (...)` query where the
pass has the id set up front. `is_bound_work_item_closed` itself now reads
one column. The pane-death sweep's terminal-convergence scan likewise maps
three columns per row instead of every `work_executions` column.

**~15 % in `agent_jsonl_discovery` root revalidation.** The tail loop
revalidated the sessions root (`lstat` + `canonicalize`) and re-walked the
descriptor-vs-path identity on _every_ 50 ms poll, idle or not — roughly
ten `canonicalize` calls per iteration, ~160 iterations/s at 8 workers.
Fix: the idle poll is one `fstat` plus one `lstat`; every pathname-level
check runs only on the iterations that are about to publish bytes or a
delimiter, where it is still performed before and after the read exactly
as before.

**~10 % in `map_execution`, mostly the pane-death sweep's terminal scan.**
Full-row mapping of every recent terminal execution once a minute to read
three columns. Fix: `list_recent_terminal_execution_refs_with_local_shell_pid`.

**~7 % spawning `jj` for recovery-pointer snapshots and restores.** Each
record in a PR chain cost three `jj log` spawns to snapshot and three
`jj bookmark set` spawns to restore, including pointers that had not
moved. Fix: one `jj log` resolves all three bookmarks of a record; a
restore reads the current pointers once, leaves unchanged ones alone, and
groups the moved ones per target commit into one `bookmark set`.

**Sweeps ran their SQLite work inline on tokio async worker threads.** The
envelope watchdog's pass, deadline reconciliation and deadline handling,
terminal-work gathering, dead-pane scans and probes, and PR-review recovery
candidate reads run on the blocking pool via `db_diagnostics::spawn_blocking`.

Sweep intervals, item limits and the review-badge projection are
unchanged; the gauges above are what to read before changing any of them.
