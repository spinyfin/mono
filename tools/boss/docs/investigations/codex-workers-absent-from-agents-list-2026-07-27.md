# Why Codex workers never show up in `bossctl agents list`

**Date:** 2026-07-27
**Trigger:** a Codex-driver worker was watched working in a pane while `bossctl agents list` reported `no active workers`; a Claude control run in the same session listed correctly.
**Status:** root cause identified. The cause is _not_ a missing registration on the Codex path, so this document is the deliverable rather than a behavioural fix — see "What was and was not changed".

## Short answer

Live-worker registration is driver-agnostic and Codex reaches it. What Codex does not do is _stay_ registered: `CodexDriver` deliberately makes the worker process the pane's own child (`exec codex exec …`), so the moment `codex exec` exits — whether it failed to start or finished its turn successfully — the app reports the pane child's exit and the engine reaps the execution immediately, dropping the live-state entry `agents list` renders.

Across the 41 dispatches of the Codex smoke-test work item, the median registered lifetime is **2 seconds**. A worker that exists for two seconds is not observable by a human typing `bossctl agents list`.

Why most of those exits are so short is a **separate, still-open failure** — 33 of the 36 post-#2447 dispatches that got a pane exited in ~1 s without opening a Codex session at all. That is not explained here; see "Open gaps".

## Hypothesis 1 vs hypothesis 2

Two hypotheses were on the table, deliberately unranked. Both are answerable from code plus the execution ledger.

### Hypothesis 2 — "the Codex path has no registration equivalent to Claude's" — REFUTED

There is no `match driver { … }` in the live-worker-state path with an absent or no-op Codex arm. There is no per-driver branch at all. Both production spawn paths are shared:

- `tools/boss/engine/core/src/spawn_flow.rs:488` — the one and only local-pane registration, reached by every driver. It derives the capability flag from the resolved driver (`input.driver.capabilities().provides(Capability::AwaitingInputSignal)`) rather than assuming Claude, and stamps pool/kind from `StartWorkerInput`. `tools/boss/engine/core/src/runner/pane_spawn.rs:633` calls it for Claude and Codex identically, with the driver `Arc` resolved once at `pane_spawn.rs:408`.
- `tools/boss/engine/core/src/app/worker_events.rs:613` — the _remote_-worker lazy registration. This one does hardcode `ClaudeDriver`, but it is only reached for runs whose `host_id != "local"` (`worker_events.rs:562-575`); a local Codex pane never enters it.

`bossctl agents list` (`tools/boss/bossctl/src/main.rs:1069` → `tools/boss/bossctl/src/agents.rs:429`) sends `ListWorkerLiveStates`; the handler at `tools/boss/engine/core/src/app/panes.rs:184` returns `live_worker_states.snapshot()` (`tools/boss/engine/core/src/live_worker_state.rs:371`) with no filtering of any kind. `no active workers` therefore means the registry was genuinely empty, not that a Codex row was hidden.

This is now pinned by two paired regression tests in `spawn_flow.rs` (`codex_spawn_registers_live_worker_state_like_claude` / `claude_spawn_registers_live_worker_state`). They drive the real `start_worker` against the real `CodexDriver`, with a real `LiveWorkerStateRegistry` and a real `AgentJsonlProgressManager` wired into the stub spawner — so the byte-stream ingress is genuinely prepared before the pane request and activated after registration, which is the ordering that would break first if the two ever diverged.

### Hypothesis 1 — "the runs died before whatever populates the registry ever ran" — CLOSE, BUT WRONG IN AN IMPORTANT WAY

Registration happens _first_, at pane-spawn ack. The runs then die within seconds and take the entry with them. The difference matters: nothing needs to be added to the spawn path; something needs to stop tearing the run down.

## Evidence

### The execution ledger

`boss task executions task_18c61b532e88dfd0_28` ("Hello world from Codex", `driver: codex`, repo `brianduff/checkleft-sandbox`) returns 41 executions between 02:25 and 15:31 on 2026-07-27, America/Los_Angeles.

- **41 of 41 terminated `orphaned` (39) or `failed` (2). Zero ever reached `completed`.**
- 38 of them lived 0–8 seconds; median lifetime across all 41 is 2 s. The remaining three lived 43 s, 45 s and 48 s.
- The two `failed` rows (`exec_18c623ab4e9d2a78_b`, `exec_18c63f7d76b1e420_4ea`) have no `cube_workspace_id` — they never got a workspace and never spawned a pane. They are a separate, earlier failure and not part of this chain.

The retry cadence is the orphan sweep redispatching, punctuated by `churn_guard_parked` attentions naming `orphan_sweep` — the sweep is the _consequence_ of the terminal rows, not their cause.

### What Codex actually did, from its own rollouts

The per-run `CODEX_HOME` trees under `$TMPDIR/boss-codex-homes/<execution_id>/` are Boss-owned and outside the engine state directory, so they are directly readable. Counting `sessions/**/rollout-*.jsonl`:

- **38 of the 41 homes contain no rollout at all.** Codex never opened a session — it exited before writing its first record. Every one of these is a 0–8 s execution.
- **3 homes contain a real rollout**, each ending in `event_msg/task_complete`, one of them with `patch_apply_end` (the model actually wrote the file it was asked for):

| execution                   | rollout first → last record (PDT) | row `started_at` → `finished_at` |
| --------------------------- | --------------------------------- | -------------------------------- |
| `exec_18c620ba075a6980_1`   | 04:05:14 → 04:05:49               | 04:05:06 → 04:05:49              |
| `exec_18c6375e4674e9c8_4d7` | 10:59:44 → 11:00:30               | 10:59:42 → 11:00:30              |
| `exec_18c6378831f49d98_4d9` | 11:02:44 → 11:03:26               | 11:02:41 → 11:03:26              |

**The row's `finished_at` equals the rollout's last record, to the second, in all three.** The engine terminalizes a Codex execution at the instant the `codex` process ends — not on a 30/60 s sweep tick, and not through the completion path.

#### Split by PR #2447

PR #2447 ("Fix three blockers that made every Codex dispatch fail") merged at `2026-07-27T10:21:40Z` = **03:21:40 PDT**. Splitting the 41 rows on `created_at` against that instant matters, because the pre-#2447 rows are that PR's already-fixed failure and are noise for everything below:

| cohort                                              |   n | no rollout | rollout |
| --------------------------------------------------- | --: | ---------: | ------: |
| pre-#2447 (02:25–02:30)                             |   3 |          3 |       0 |
| post-#2447 (03:26–15:31)                            |  38 |         35 |       3 |
| — of which never got a workspace (the two `failed`) |   2 |          2 |       0 |
| — of which spawned a pane                           |  36 |         33 |       3 |

So the honest post-#2447 figure is **33 of 36 dispatches that actually spawned a Codex pane still wrote no rollout at all**, and only 3 opened a session. #2447 did not move this: the pre-#2447 sample is only three rows, all of them zero-rollout too.

#### This is a second, still-undiagnosed failure

Those 33 rows are _not_ explained by the pane-exit mechanism below. That mechanism explains why a run is reaped and delisted **once `codex exec` exits**; it says nothing about why `codex exec` exits in ~1 s having opened no session. And the process demonstrably got some distance in: each of those homes contains Codex-authored state — `installation_id`, `logs_2.sqlite`, `memories_1.sqlite`, `goals_1.sqlite`, `state_5.sqlite` and a `tmp/arg0/codex-arg0XXXXXX` scratch file, none of which Boss writes (Boss provisions only `config.toml`, `AGENTS.md`, `auth.json`, `guards/`, `skills/`, `hook-trust-attestation.json`). So `codex` started, initialised its `CODEX_HOME`, and then exited before its first rollout record. `logs_2.sqlite` is present but has zero rows, so it carries no diagnostic.

**Naming it plainly: "post-#2447 `codex exec` exits within ~1 s having written nothing" is a distinct, open failure, and it is the larger of the two** — it is what keeps 33 of 36 dispatches from doing any work at all, whereas the pane-exit mechanism only governs how the remaining 3 are terminalized. Diagnosing it needs the pane's own stderr or a `codex exec` run reproduced by hand under one of these `CODEX_HOME` trees, neither of which is available from a worker session. It is listed alongside the trace-correlation gap in "The observation this does not fully close".

### The mechanism

`CodexDriver` launches the pane with shell `exec`, by design:

- `tools/boss/engine/driver/src/codex.rs:512` — `wrap_codex_command_for_pane` prefixes the command with `exec` so "the pane does not return to an interactive prompt after the worker process exits" (ghostty-codex-pane-viability Q2, choice (a) — it closes a real tty-inject footgun).
- `tools/boss/engine/driver/src/codex.rs:491` — the body is `codex exec --json … < /dev/null`, a one-shot non-interactive command that exits when its turn ends.

The consequence is that **the pane's child process _is_ the worker**. For Claude the pane's child is a login shell that outlives the `claude` process, which is why a Claude worker parks in `waiting_human` and stays listed.

The app wires an exit handler on worker panes only:

- `tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:86-94` — `onPaneDied` fires from `onSurfaceFailed` **or `onChildExited`**, "which only worker panes wire up; the Boss pane instead restarts itself".
- `tools/boss/app-macos/Sources/ContentView.swift:192` → `ChatViewModel+BossSession.swift:37` → `EngineClient+Requests.swift:577` sends `worker_pane_died`.

And the engine treats that report as unconditional proof of death:

- `tools/boss/protocol/src/wire.rs:2216` — `WorkerPaneDied`; the doc states the engine reaps "immediately … skipping its grace period and PID-liveness probe since the app's report is a direct observation".
- `tools/boss/engine/core/src/app/sessions.rs:314` → `tools/boss/engine/core/src/dead_pid_sweep.rs:510` `reap_reported_pane_death` → `reap_dead_execution`: orphans the execution, releases the pool slot, and drops the live-state entry.

So for Codex, **a successful turn and a dead pane are the same observable event**. This mechanism is what makes _every_ row terminalize as `orphaned` and vanish from `agents list` the moment its `codex exec` returns — whether that return is a completed turn (the 3) or the ~1 s zero-rollout exit (the 38). It is the answer to the question this investigation asked.

It is deliberately **not** an answer to _why_ those 38 exit in ~1 s: this mechanism starts at the child's exit and says nothing about what caused it. That is the separate, still-open failure named above. Both are real; conflating them would make the ~1 s exits look like an already-explained consequence of the `exec` wrapping, which they are not.

### Ordering check

`reap_reported_pane_death` is what writes the terminal row, and it fires _after_ the child exit — so the terminal status is a consequence of the exit, not a cause of it. Symmetrically, the `orphan_sweep` churn-guard attentions are stamped minutes after the executions they name. Nothing in the ledger is a cause of anything earlier than itself.

## Open gaps

Two, tracked separately below.

### Gap 1 — why `codex exec` exits in ~1 s having written nothing

The larger of the two, and untouched by this investigation: 33 of the 36 post-#2447 dispatches that actually spawned a pane exited within ~1–8 s with an initialised `CODEX_HOME` and an empty `sessions/`. See "This is a second, still-undiagnosed failure" above for the evidence and for why it cannot be diagnosed from a worker session.

### Gap 2 — the observation this does not fully close

The pane that prompted this was watched for "well over a minute" while the list stayed empty. No execution row in this series exceeds 48 seconds, and the closest run in time to the cited Claude control run (`exec_18c6210adb239868_5`, ≈04:10) is `exec_18c620ba075a6980_1` (04:05:06–04:05:49). During those 43 seconds a live-state entry _should_ have existed.

I could not settle that specific window. The engine trace, `engine-audit.log` and the dispatch-event mirrors all live under `~/Library/Application Support/Boss/`, which worker sessions are hard-blocked from reading, so the `execution_id`-keyed trace grep that would settle it was not available to me. Two readings remain open, and they are distinguishable from the trace alone by anyone who can read it:

1. The watched pane was one of the 38 fast-fail runs (registered ~1 s, reaped, then a dead-but-still-rendered surface), and the "over a minute" is wall-clock spent looking at a pane the engine had already let go.
2. The entry was released early during a run that was genuinely alive, by a path other than the child-exit report.

#### The instrumentation that closes Gap 2

`LiveWorkerStateRegistry::release_slot` already traces every removal with the clearing caller (`live_worker_state.rs:340`, added precisely because a live worker being killed was invisible in the logs). Its counterpart did **not**: `register_spawn_with_capabilities` emitted nothing. The trace could therefore show a slot being cleared with no record it was ever occupied, and "this run never appeared in `agents list`" was indistinguishable from "it appeared and was cleared 900 ms later" — which is exactly the question above.

This PR adds the missing half: registration now emits one `info` line carrying slot, run, model, shell pid, pool, kind, work item and the `#[track_caller]` spawn path, worded to match the removal line so the two grep as a pair. With both halves present, the next occurrence is answerable by diffing the two timestamps for the run id.

## What was and was not changed

Changed:

- `live_worker_state.rs` — registration trace (the instrumentation above).
- `spawn_flow.rs` — paired Codex/Claude registration tests.
- this document.

Deliberately **not** changed: the pane-death/turn-completion conflation itself. The cause is that runs die shortly after registering, which is a different bug from the one being chased here, and reporting it beats fixing the wrong thing. Making the engine tell a Codex worker's normal exit apart from a pane death is a behavioural change to the completion and reap paths (the ingress has to be drained and a Stop already read from the rollout honoured before the reap path terminalizes the row), and it is what stands between Codex and an execution that can reach `completed` at all. It needs its own change, not a rider on an instrumentation PR.

Explicitly not done, because both would be wrong: `agents list` does not synthesize Codex rows from execution rows, and it does not special-case Codex in any direction.
