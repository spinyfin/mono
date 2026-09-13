# Why five `chore_implementation` workers produced no hook events during the 2026-09-13 breaker incident

**Status:** root-caused, with one execution left partially explained. Instrumentation and a fix landed alongside this note (see "What changed in the tree").

**One-line answer:** the five silent executions were all Codex-driven. A Codex worker has no hook socket at all — every "hook event" the engine attributes to it is synthesized from the rollout file the engine tails in the worker's per-run `CODEX_HOME`. The engine's rollout _discovery_ had a fixed 120-second give-up point measured from spawn acknowledgement. Under the post-breaker admission burst every Codex process in the cohort took roughly two minutes to reach the point where it creates its rollout. The five chores were admitted in the first second of the burst, so their rollouts landed 123–129 s after activation, 3–9 s past the give-up point; discovery had already exited (one `warn` line, nothing durable), the ingress never attached, and the workers ran unobserved until the 300-second driver-start reap called them "driver binary never started". The non-chore executions were admitted 2–50 s later and their rollouts fell inside their (later) windows. The split is admission-order, not kind.

## The hook path for a worker, end to end

There are two ingresses, and which one a run has is decided by the driver, not the execution kind.

|              | Claude                                                                                                  | Codex                                                                                                                                 |
| ------------ | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| What emits   | `boss-event` shim (`tools/boss/event-shim`), wired into the `--settings` file for all seven hook events | Nothing. `$CODEX_HOME/config.toml` carries only `PreToolUse` guard hooks; they write `guard-trace.jsonl` and never talk to the engine |
| Channel      | `events.sock` (one connection per event, `_boss_run_id` spliced into the payload)                       | The engine tails `$CODEX_HOME/sessions/**/rollout-*.jsonl` (`ProgressIngress::AgentJsonlFile`)                                        |
| Attribution  | `_boss_run_id` → execution → driver slug → normaliser                                                   | Trivial: the tail was opened for a known run id                                                                                       |
| Engine entry | `events_socket::handle_connection` → `dispatch_worker_event_fanout`                                     | `agent_jsonl_progress` → `stdout_progress` → the same `dispatch_worker_event_fanout`                                                  |

For Codex, `worker_setup::settings_value` renders an **empty** `hooks` map by design (`merges_hooks_into_worker_settings` is false for a byte-stream ingress). So "no hook event" for a Codex run means exactly one of: the rollout was never written, or the engine never attached to it. Which of those it was is decidable from `work_runs.transcript_path`: it is written on the first event dispatched from the tail and stays `NULL` if the ingress never attaches.

### How discovery worked before this change

`spawn_flow` arms the ingress (`prepare_run`: snapshot the per-run sessions dir as a baseline, write `IngressCheckpoint::Armed`) before the pane exists, and activates it (`activate_run`) right after the spawn acknowledgement. Discovery then polled every 100 ms for a new file whose first line is a complete `session_meta` record with `cwd` canonicalising to the workspace and whose name ends in the session id. On success it wrote `IngressCheckpoint::Attached` and started tailing.

On the 120-second deadline (`DISCOVERY_TIMEOUT`) it returned an error, the task logged `agent JSONL progress: discovery failed` at `warn` and exited. Nothing durable recorded that this had happened, no dispatch event was emitted, the checkpoint stayed `Armed`, and nothing ever retried. The run then stayed live and invisible until `spawn_ack_sweep` pass 2 reaped it at `DRIVER_START_GRACE_SECS` (300 s) with a narrative asserting the driver binary never ran.

That 120 s clock was strictly shorter than the 300 s liveness clock, so there was a 180-second window in which a driver that started late was guaranteed to be reaped as never-started.

## Evidence

All times UTC. Engine-side facts come from `boss task executions --json` (the execution and run rows). Worker-side facts come from the per-run `CODEX_HOME` directories under `$TMPDIR/boss-codex-homes/<execution id>/`, which are outside Boss's data dir: Codex's own `state_5.sqlite` (`threads.rollout_path`, `created_at`, `tokens_used`), `session_index.jsonl` (the session id is a UUIDv7, so its first 48 bits are the rollout creation time to the millisecond), `guard-trace.jsonl` (every `PreToolUse` the worker made), `version.json` (Codex's startup update check) and `logs_2.sqlite`. Three of the homes were reclaimed by the hourly retention sweep while this was being written; the numbers below were captured before that.

The cohort admitted after the breaker resumed (engine restart ≈ 21:39:30; the two executions with `_1` / `_2` suffixes are the first two of the new engine session):

| Execution                    | Kind     | Workspace        | Spawn acked (activation) | Rollout created | Δ        | Old give-up point | `transcript_path` | Outcome                                                |
| ---------------------------- | -------- | ---------------- | ------------------------ | --------------- | -------- | ----------------- | ----------------- | ------------------------------------------------------ |
| `exec_18d4f1b11552c1a0_1b51` | chore    | (released)       | 21:40:28                 | 21:42:34.1      | 126 s    | 21:42:28          | NULL              | completed 21:44:03 via `propose done`, no event ever   |
| `exec_18d4f22c887f1068_1b66` | chore    | flunge-agent-004 | 21:40:28                 | 21:42:31.0      | 123 s    | 21:42:28          | NULL              | orphaned 21:45:30 (driver-start timeout)               |
| `exec_18d4f2321412fd88_1b6a` | chore    | flunge-agent-009 | 21:40:29                 | 21:42:34.6      | 126 s    | 21:42:29          | NULL              | orphaned 21:45:30                                      |
| `exec_18d4f234d2dbadf8_1b6c` | chore    | mono-agent-007   | 21:40:29                 | 21:42:34.5      | 126 s    | 21:42:29          | NULL              | orphaned 21:45:30                                      |
| `exec_18d4ff3b75065868_2`    | chore    | flunge-agent-011 | 21:40:26                 | **21:41:42.1**  | **76 s** | 21:42:26          | NULL              | orphaned 21:45:30 — see below                          |
| `exec_18d4ff389dbc7e48_1`    | task     | mono-agent-441   | 21:40:42                 | 21:42:21.4      | 99 s     | 21:42:42          | set               | completed 21:52:05                                     |
| `exec_18d4c89af1615480_1633` | revision | mono-agent-444   | 21:41:21                 | 21:42:36.5      | 75 s     | 21:43:21          | set               | ran on, later abandoned at 22:12 for unrelated reasons |

Reading the table:

- **Every reaped run has `transcript_path = NULL`; every surviving run has it set.** The engine end never attached. This is the "which end failed" answer: the rollouts existed (Codex's `threads` table names them, under the per-run `sessions/` link the ingress was watching, with `cwd` equal to the workspace) and were being written to — `tokens_used` was 279,361 for `…_1b66` and 307,879 for `…_1b6a` by the time they were killed; `guard-trace.jsonl` shows their first `PreToolUse` at 21:43:16 and 21:43:25 and their last within seconds of the reap.
- **Four of the five chores missed the old give-up point by 3–9 seconds.** The rollouts of the whole cohort were created within a 15-second band (21:42:21–21:42:36) regardless of when each pane was spawned: Codex's startup update check (`version.json`) ran at 21:42:15–21:42:38 for processes whose panes came up 21:40:26–21:41:21. Whatever stalled the cohort's startups — the machine was simultaneously hosting eight fresh worker spawns, an engine that had just restarted, and a Claude worker — released for all of them at about the same moment. A run's deadline is activation + 120 s, so the runs activated first had the earliest deadlines and were the ones the shared release fell after.
- **The chores were activated first because they were admitted first.** All five started at 21:40:25–26; the task at 21:40:27 (its spawn acknowledgement took 15 s) and the revision at 21:40:47 (its home provisioning alone took 30 s under the load). Nothing on the spawn path branches on `chore_implementation`: prompt assembly, `AGENTS.md`, per-run home, auth snapshot, guard arming, hook trust attestation and the ingress wiring are identical, and the successful re-runs of three of these same chores later that hour attached within 3–16 s of provisioning. The prior investigations' finding that the chore path is not broken in general is confirmed.
- **The earlier kind ↔ spawn-failure correlation is a separate phenomenon.** These spawns all succeeded (each run row records a slot and shell pid within 1–34 s). What failed was observation after a successful spawn.

### Concurrency is a necessary condition; kind is not

Steady state (the 03:39 UTC chore, and the re-runs `…_21`/`…_22`/`…_23`/`…_24` at 21:48–22:01): rollout within 3–16 s of provisioning. Burst: 75–129 s. The failure needs both the burst (to push Codex startup past 120 s) _and_ an early admission slot (to put the deadline before the shared release). Any kind admitted in the first second would have failed the same way; a chore admitted last would have survived. Kind entered only through admission order.

### The one that does not fit: `exec_18d4ff3b75065868_2`

Its rollout was created at 21:41:42, 76 s after activation and 44 s inside its window, and its home shows the same one-thread, correct-`cwd`, per-run-sessions-dir shape as the others — yet its ingress never attached. The old code's only record of why is one `warn` line in `engine-trace.jsonl` with `run_id=exec_18d4ff3b75065868_2` and one of these messages: `discovery failed` (with the reason: root identity changed, a scan error, or `N new rollout files matched one run`) or, if the file simply never validated, the `no correlated rollout appeared … within 120s` reason at about 21:42:26. That line is the next thing to read. Candidate mechanisms that the data here cannot separate: the first `session_meta` line exceeding the 64 KiB read cap (`MAX_SESSION_META_BYTES`) so it never validated, or the durable sessions directory changing identity under the ingress on the first execution after the restart. Its home was reclaimed before the session-meta size could be checked. With the change below, the checkpoint row and the dispatch timeline record the verdict and the rejected-candidate count, so the next occurrence answers itself.

## What changed in the tree

1. **Discovery no longer has a give-up point shorter than the liveness clock.** `DISCOVERY_TIMEOUT` is gone. Discovery runs until the run is torn down (`stop_run`) or hits a failure polling cannot cure (root identity change, scan error, two rollouts claiming one run). At the old threshold — now `DISCOVERY_OVERDUE_AFTER`, still 120 s — it reports itself overdue once and keeps polling. In the incident every one of the four late rollouts would have been attached within ten seconds of that notice.
2. **Every unattached state is durable and on the timeline.** `IngressCheckpoint::Armed` carries an optional `DiscoveryRecord` (`overdue` or `failed`, when, how long it had waited, how many rollout-shaped files it rejected as not this run's, and why). Three dispatch stages were added: `file_ingress_attached` (with `discovery_secs`, so the startup-latency distribution under bursts is now visible), `file_ingress_discovery_overdue`, and `file_ingress_discovery_failed`. They are post-dispatch observations and do not start a stall clock.
3. **The driver-start reap reads the checkpoint before it narrates.** `driver_start_timeout`'s orphan reason, `[engine-reconcile]` audit note, attention item and `details.file_ingress` now say what the ingress recorded — "never attached: discovery was overdue at 121 s … the driver may have started and run unobserved" — instead of asserting the binary never started. Without any file-ingress record (a hook-socket driver) the old reading stands, phrased as a likelihood.

## What would settle the remainder

- For `exec_18d4ff3b75065868_2`: the one trace line named above, if `engine-trace.jsonl` has not rotated past 21:42 UTC on 2026-09-13.
- For the two-minute startup stall itself: this note establishes _that_ Codex startup took ~120 s across the cohort and that the stall released simultaneously, not _why_. `file_ingress_attached.discovery_secs` will now show the distribution on every burst; correlating it with host load at spawn time is the follow-on. The separate work on scaling spawn timeouts with admission concurrency should treat the file-ingress overdue threshold as a reporting threshold, not a timeout — there is no longer a timeout to scale.
