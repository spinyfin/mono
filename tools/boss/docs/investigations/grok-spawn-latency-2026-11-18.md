# Grok spawn latency: where the time before `pane_spawned` goes

A September sample showed Grok workers taking about 17 s longer than Claude or Codex between `run_started` and `pane_spawned`. The roughly 17 s cost was not observed in the measured direct-subprocess and CLI-start paths on current `main`; the full engine offset and its distribution remain undetermined.

The seven commands Grok runs before its pane, timed directly, took under 5 s in all 138 local runs, and its CLI accepted a brief under 5 s after starting in all 84. The only candidate found for a ten-second cost on every spawn in September was the bare-name `cube` probe that mono#3428 removed, inferred from code history and not measured.

The direct timings exclude the engine's refresh-lock wait and credential reprobe, which were not measured.

Source paths below are relative to `tools/boss/`. Everything under "Measured" was timed for this write-up on one macOS host on 2026-11-18. The section on the September cost is reasoning from code history and says so.

## What the interval covers, then and now

`run_started` is emitted when the coordinator hands an execution to its runner (`engine/dispatch/src/coordinator/execution.rs:2027`). `pane_spawned` is emitted after the runner's `run_execution` returns (`engine/dispatch/src/coordinator/run.rs:462`). What happens inside `run_execution` has changed since the sample was taken, so the same query now measures a different thing.

| Step inside `run_execution`                                                               | Mid-September | Current `main` |
| ----------------------------------------------------------------------------------------- | ------------- | -------------- |
| Compose the prompt, resolve model and driver                                              | yes           | yes            |
| Driver `provision_workspace`                                                              | yes           | yes            |
| Permission and hook config, pane title, launcher script                                   | yes           | yes            |
| Create the tmux pane                                                                      | yes           | yes            |
| Wait for the CLI to show its composer, take the brief and report a turn (since mono#3034) | no            | yes            |

In mid-September the runner returned as soon as the pane existed, so the 17 s had to be spent before the pane: in composition, provisioning, config or pane creation. Grok provisioning adds the seven subprocesses listed below; permission/config work also differs by driver. Today the interval also includes the CLI's start and the brief delivery (`engine/execution-runner/src/runner/pane_spawn.rs:1470`), for every driver.

## What only Grok does before its pane

Claude's and Codex's `provision_workspace` write files (`engine/driver/src/claude.rs:873`, `engine/driver/src/codex.rs:1998`). Grok's runs seven subprocesses in sequence on the blocking pool (`engine/driver/src/grok.rs:419`), from `provision_grok_home_with_kind` (`engine/driver/src/grok/home.rs:373`):

1. `grok inspect --json`, the posture check (`engine/driver/src/grok/home.rs:549`).
2. `grok models`, the OAuth check, after waiting for any live token refresh to finish (`engine/driver/src/grok/preflight.rs:64`, `engine/driver/src/grok/environment.rs:106`).
3. The bundled `cube --json workspace status --workspace <path>` (`engine/driver/src/grok/preflight.rs:251`).
4. `gh auth status --active --hostname github.com`.
5. `jj root`.
6. `jj git remote list`.
7. `/usr/bin/touch` on a probe file in Grok's session store.

Steps 2 to 7 are `run_worker_preflight_with` (`engine/driver/src/grok/preflight.rs:202`). That is the list for a leased cube workspace, which is what was measured here. A progression run's scratch directory has no checkout, so since mono#3718 it replaces step 3 with a directory listing and skips steps 5 and 6.

The comparable costs for the other drivers are a `python3` import check that is cached after its first success in an engine process (`engine/driver/src/initial_prompt.rs:208`), and for Codex a `codex debug models --bundled` call made only when the Sol model was selected (`engine/driver/src/codex.rs:198`).

## Measured

Host: 12-core macOS laptop running other workers' builds, whose load swung widely during the day. Where recorded, batches below give the 1-minute load average read just before and after them; load was not recorded for the final setup fixtures. Grok CLI 1.0.70, Claude Code 2.1.327, Codex CLI 0.169.0. The `preflight` figures are direct `subprocess.run` durations of the seven commands in the engine's order and environment. They do not exercise the engine's own preflight, so they exclude its wait for a live refresh lock and its reprobe after a credential refresh (see "What can still stretch a Grok spawn"). Each run used a fresh `GROK_HOME` and process `HOME` with the engine's environment and delegates, the shared `GROK_AUTH_PATH`, and a leased checkout of this repository as the working directory. `grok_spawn_latency_probe.py` beside this file is the harness that produced them, kept as a dated record (see "Reproducing"). The first batch of each kind was run before it was consolidated into that one script, and the rest with it as committed.

### Grok's seven subprocesses

Seconds, one spawn at a time, the first batch of 30 runs:

| Step                    | min   | median | p90   | max   |
| ----------------------- | ----- | ------ | ----- | ----- |
| `grok inspect --json`   | 0.021 | 0.023  | 0.065 | 0.177 |
| `grok models`           | 0.645 | 0.792  | 1.152 | 2.262 |
| `cube workspace status` | 0.069 | 0.082  | 0.112 | 0.293 |
| `gh auth status`        | 0.283 | 0.352  | 0.411 | 0.440 |
| `jj root`               | 0.011 | 0.030  | 0.039 | 0.155 |
| `jj git remote list`    | 0.038 | 0.051  | 0.108 | 0.418 |
| `touch`                 | 0.003 | 0.003  | 0.006 | 0.009 |
| All seven               | 1.210 | 1.350  | 1.839 | 3.334 |

The totals in that batch fall in one group: 23 runs between 1.0 and 1.5 s, 4 between 1.5 and 2.0 s, 2 between 2.0 and 2.5 s, and one at 3.3 s. There is no second mode.

All seven commands, by batch. Every check passed in every run.

| Condition                                        | Load average | runs | median | max   |
| ------------------------------------------------ | ------------ | ---- | ------ | ----- |
| One at a time (the batch above)                  | 16 to 31     | 30   | 1.350  | 3.334 |
| One at a time                                    | 16           | 30   | 1.823  | 4.847 |
| Three started together                           | 16 to 31     | 30   | 1.470  | 2.714 |
| Three started together                           | 16 to 41     | 30   | 2.725  | 4.106 |
| One at a time                                    | 185 to 235   | 12   | 2.999  | 4.195 |
| Three together during this workspace's own build | not recorded | 6    | 3.368  | 4.592 |

That is 138 runs, none over 4.9 s. `grok models` is the largest step in every batch and the only one that is a round trip to xAI: its median ranged from 0.79 s to 1.90 s across batches. `gh auth status` is a round trip to GitHub. The two one-at-a-time batches at similar load differ by half a second, most of it in `grok models`.

### Grok from process start to an accepted prompt

The repository's launcher (`engine/driver/src/initial_prompt.py`) around the engine's `grok` command line, in a detached tmux pane, with a 40 KB brief. Seconds from creating the pane, the first batch of 16 runs, one at a time:

| Point reached                  | min   | median | p90   | max   |
| ------------------------------ | ----- | ------ | ----- | ----- |
| `SessionStart` hook starts     | 0.922 | 0.998  | 1.215 | 1.360 |
| Composer ready                 | 1.065 | 1.183  | 1.355 | 1.534 |
| Return written                 | 1.071 | 1.190  | 1.383 | 1.540 |
| `UserPromptSubmit` hook starts | 1.145 | 1.282  | 1.629 | 1.683 |

Writing the 40 KB paste took 57 to 221 ms and the composer showed it within 2 ms. Grok's own log for one of these runs puts `startup complete` 0.70 s after its first line.

Pane creation to the `UserPromptSubmit` hook, by batch. Every pane reached an accepted prompt.

| Condition                       | Load average  | runs | median | max   |
| ------------------------------- | ------------- | ---- | ------ | ----- |
| One at a time (the batch above) | 17 to 31      | 16   | 1.282  | 1.683 |
| One at a time                   | 41 to 139     | 15   | 2.164  | 3.359 |
| One at a time                   | 165 to 185    | 8    | 2.455  | 4.713 |
| Three started together          | 12 at the end | 18   | 1.769  | 2.102 |
| Three started together          | 139 to 140    | 18   | 2.777  | 4.009 |
| One at a time, during the build | not recorded  | 3    | 2.280  | 2.820 |
| One at a time, hook held open   | not recorded  | 6    | 2.121  | 2.791 |

That is 84 pane starts, none over 4.8 s.

A `SessionStart` hook held open for 3 s did not delay the composer or the first prompt (6 runs): the composer was ready 0.07 to 0.19 s after that hook started, against 0.14 to 0.20 s without the hold. A slow progress forwarder at session start is therefore not a spawn-latency mechanism on this CLI version.

### Claude and Codex

These were not re-measured as a population. The launcher log of the Claude worker that produced this write-up shows the composer ready 0.59 s after the launcher started and Return written 0.11 s later (one run). `docs/worker-prompt-delivery.md` records 0.5 to 1.9 s to composer ready and 0.6 to 4.9 s to Return over 71 Claude runs at normal load. The corrected local setup batch below measures the exact `python3` import check and the Codex bundled-model probe, including their spread.

A Codex pane was not started. Its per-run home holds a snapshot of the machine's Codex login, and a token refreshed inside a discarded probe home would not be adopted back.

### Local steps around the subprocesses

`grok_spawn_latency_probe.py setup`, 30 runs one at a time (seconds), rerun with the exact engine command `python3 -c 'import fcntl, pty, termios'`:

| Step                                        | n   | min   | median | p90   | max   |
| ------------------------------------------- | --- | ----- | ------ | ----- | ----- |
| Grok home stand-in and delegate paths       | 30  | 0.002 | 0.002  | 0.003 | 0.004 |
| Detached `tmux new-session -d`              | 30  | 0.018 | 0.020  | 0.022 | 0.025 |
| Python import, uncached first-success check | 30  | 0.024 | 0.026  | 0.029 | 0.032 |
| `codex debug models --bundled`              | 30  | 0.014 | 0.015  | 0.017 | 0.022 |

The Python check is not a per-spawn charge: `PREFLIGHT_OK` caches its first success in each engine process. The Codex probe applies only when selecting Sol. These local subprocess checks do not start either CLI worker.

The `//tools/boss/engine/core:spawn_setup_probe` harness also calls the real Rust code paths, 30 fresh fixtures per driver, sequentially, with a roughly 40 KB chore description. Seconds (six decimal places retain the sub-millisecond steps):

| Driver | Step                         | n   | min      | median   | p90      | max      |
| ------ | ---------------------------- | --- | -------- | -------- | -------- | -------- |
| claude | composition                  | 30  | 0.000409 | 0.000472 | 0.000597 | 0.002062 |
| claude | workspace settings and hooks | 30  | 0.003137 | 0.003485 | 0.018792 | 0.254829 |
| claude | permission config            | 30  | 0.000001 | 0.000002 | 0.000004 | 0.000017 |
| claude | spawn invocation             | 30  | 0.000003 | 0.000006 | 0.000012 | 0.000014 |
| claude | pane title (no credential)   | 30  | 0.000245 | 0.000285 | 0.000344 | 0.000540 |
| claude | pane title (cached)          | 30  | 0.000240 | 0.000256 | 0.000325 | 0.000352 |
| codex  | composition                  | 30  | 0.000429 | 0.000506 | 0.000534 | 0.000620 |
| codex  | workspace settings and hooks | 30  | 0.001737 | 0.001906 | 0.002052 | 0.002240 |
| codex  | permission config            | 30  | 0.068520 | 0.072471 | 0.095171 | 0.140546 |
| codex  | spawn invocation             | 30  | 0.000014 | 0.000017 | 0.000018 | 0.000032 |
| codex  | pane title (no credential)   | 30  | 0.000305 | 0.000351 | 0.000375 | 0.000525 |
| codex  | pane title (cached)          | 30  | 0.000248 | 0.000263 | 0.000272 | 0.000387 |
| grok   | composition                  | 30  | 0.000372 | 0.000432 | 0.000535 | 0.000564 |
| grok   | workspace settings and hooks | 30  | 0.001724 | 0.001943 | 0.002097 | 0.002260 |
| grok   | permission config            | 30  | 0.001561 | 0.001658 | 0.001785 | 0.001855 |
| grok   | spawn invocation             | 30  | 0.000059 | 0.000082 | 0.000109 | 0.000148 |
| grok   | pane title (no credential)   | 30  | 0.000235 | 0.000267 | 0.000304 | 0.000321 |
| grok   | pane title (cached)          | 30  | 0.000234 | 0.000245 | 0.000269 | 0.000353 |

This is local attribution, not a full engine dispatch. Each fixture uses a private database, workspace, process home and per-run driver homes. It calls `compose_worker_spawn`, `write_workspace_files` (including Claude's actual settings and hooks), each driver's `write_permission_config`, `spawn_invocation`, and `title_for_spawn`. Codex permission config includes the real installed CLI's hook-trust attestation; Grok includes `hooks::write_hooks` and permission artifacts, not the Python canary stand-in. Claude's driver permission method is empty; its work is in the workspace-settings row. The first Claude settings call was the 255 ms maximum, including initial guard-script materialization; subsequent fixtures reuse those content-addressed scripts, as an engine does.

Provisioning is excluded from these Rust timings: fresh empty driver homes are created before measurement, without copying credentials or running preflight. The separate Grok subprocess measurements and home stand-in remain separate observations. Pane creation is the detached tmux measurement above (not the engine's `pane_start_ms`, which also covers workspace file writes and the serialized app viewer attach; see `workspace_files_ms`, `tmux_start_ms` and `viewer_attach_ms` on the same log line); Grok driver start is the 84-run accepted-prompt sample. Title rows cover the real cached and missing-credential branches, without a utility-model inference request. An uncached credentialed title can make a network request with up to two 5 s attempts and a 250 ms backoff; these local title figures do not bound that branch. The fixture does not include database scheduling, refresh-lock waits, credential reprobes, a production repo-guidance fetch or hook ingest. These differences prevent treating the rows as an end-to-end distribution or assuming identical setup costs across drivers.

### Older Grok CLIs

Two earlier builds are still on the host. `grok inspect --json` plus `grok models`, 8 runs each with a fresh home: 0.2.111 (July) median 0.924 s, max 0.978 s; 1.0.62 (October) median 0.825 s, max 1.454 s. The build in use in mid-September was not available.

## Is the effect real today

The roughly 17 s cost was not observed in the measured direct-subprocess and CLI-start paths. The full engine offset and its distribution remain undetermined. At the host's calmer moments the seven Grok-only direct subprocesses totalled 1.4 to 1.8 s, and its accepted-prompt timing was about half a second later than the available Claude observation. A roughly 2 s difference is only a conditional estimate: it requires no refresh-lock wait or credential reprobe, comparable remaining setup costs, and representative post-pane samples. It is not a measured cross-driver engine interval.

With the 1-minute load average between 165 and 235 on 12 cores, the seven-command direct-subprocess total had a median of 3.0 s and the pane reached an accepted prompt in a median 2.5 s. The worst figures seen in those measured paths were 4.8 s for the direct subprocesses and 4.7 s after pane creation. Neither measurement bounds the unmeasured engine waits.

This is a local reproduction. The production dispatch-event window was not re-measured.

## Where the September cost was

Inferred, not measured.

Mid-September's interval ended at pane creation, so the cost was somewhere in pre-pane preparation; the subprocess hypothesis below is inferred. Until mono#3428 merged on 2026-11-02, the preflight ran the cube probe by bare name, `cube`, resolved through the engine's `PATH`. Of the seven commands, that is the only one whose cost is not bounded by a sub-second process start or a single network round trip:

- A host with a repobin `cube` shim on `PATH` runs that shim instead of the bundled binary. From inside a checkout, repobin builds `//tools/cube:cube` there before running it. Its cache of the built path is keyed by repository root and checked against source and binary mtimes, so a newly leased or repositioned workspace misses it.
- `bazel build //tools/cube:cube` in the leased workspace used for this write-up took 11.3 s with every action cached, because the workspace's Bazel server had to start and analyse (one run). With the server up it took 0.8 to 1.9 s. Any action that actually has to run adds to the first figure.
- mono#3428 replaced the bare name with the bundled binary's absolute path (`engine/driver/src/grok/preflight.rs:256`). Its regression test records that a repobin shim earlier on the engine's `PATH` did run for this probe and tried to build `//tools/cube:cube` from source. The worker launch guard describes the same shim as building a CLI from a checkout "for ~30s" (`engine/driver/src/claude.rs:519`).

That mechanism fits a fixed double-digit cost on a driver that alone runs `cube` during provisioning, and a spread up to 43.5 s when a build step is not cached. It would also be bimodal, cheap when repobin's cache hits. The sample cannot confirm any of this: nine spawns, no per-step timing, and no record of which `cube` the engine resolved.

Other candidates and why they do not fit a cost on every spawn:

- The Grok CLI. Builds from before and after the sample complete both Grok commands in under 1.5 s.
- The wait for a live OAuth refresh. It is bounded at 30 s and only applies while a running Grok process holds a refresh lock less than 30 s old (`engine/driver/src/grok/environment.rs:243`).
- `cube workspace status` waiting on the jj maintenance gate, fixed by mono#3375. That was an occasional stall of up to 120 s, not a steady offset.
- `gh` and `jj`. Sub-second, as above.
- Host load. With the load average near 200 the seven commands as they are today still finished in under 5 s.

## Repository or upstream

The measured Grok-only direct-subprocess cost comes from this repository's choice to run seven checks before each pane. These measurements do not establish whether a repository or CLI change is needed to address the full reported offset. The floor of `grok models` is a network call to xAI and is upstream.

The reason the question was raised has also changed. Grok was the third member of every pre-merge review batch, so its spawn time was the batch's. mono#3073 removed the Grok leaf reviewer from those batches. Grok still pays this cost as an implementation and progression driver.

## What can still stretch a Grok spawn

- **Token refresh.** When the shared OAuth token has expired, `grok models` refreshes it and is run a second time (`engine/driver/src/grok/preflight.rs:320`). Not exercised by the probe, which calls `grok models` directly and does not reprobe.
- **A refresh lock held by a live process.** The Grok CLI leaves `auth.json.lock` in place after a refresh: on this host it was still there, naming an exited process, more than an hour after the refresh that wrote it. If the process that refreshed is still running, such as a long-lived worker pane, a preflight starting within 30 s waits until the lock is 30 s old (`engine/driver/src/grok/environment.rs:243`). Not measured: the probe does not read, wait on or reproduce the lock, so these runs say nothing about the fresh-live, stale or absent cases. It is now logged when it lasts 250 ms or more.
- **The bounds.** 30 s for `grok models` and 60 s for each other command (`engine/driver/src/grok/preflight.rs:23`). A hung tool fails the spawn at its bound.
- **Host load.** Heavy load roughly doubled every figure here. Seven process starts in sequence are more exposed to it than the file writes the other drivers do.

## What could be done, and what it would cost

| Change                                                                     | Saves per Grok spawn                                  | Cost                                                                                                                                                 |
| -------------------------------------------------------------------------- | ----------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Run the independent checks concurrently                                    | about 0.5 s (to the slowest check, about 0.8 s)       | More threads per spawn, and a failure no longer stops the later checks from running                                                                  |
| Reuse a recent `grok models` or `gh auth status` result for a short window | up to about 1.2 s                                     | A credential revoked inside the window passes the check. These checks exist because a worker that starts without them parks holding a slot           |
| End the refresh-lock wait once `auth.json` is newer than the lock          | up to 30 s, only on spawns that hit a live fresh lock | Depends on the CLI writing the lock before the credential, which would need characterising first                                                     |
| Leave it as it is                                                          | nothing                                               | Full engine offset remains undetermined; about 2 s is conditional on no refresh-lock wait/reprobe and comparable remaining setup and post-pane costs |

None of these was made. The measured direct-subprocess cost alone does not justify weakening a fail-fast check, and the lock wait was not measured by the probe.

## Reading a spawn's timing from the engine log

The engine log now carries enough to attribute a slow spawn without an export. For one execution:

| Log message                                                                            | Fields                                                                                                     |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `Grok spawn preparation step finished`, one per subprocess                             | `grok_home` (its path contains the execution id), `step`, `elapsed_ms`                                     |
| `Grok spawn preparation waited for a live OAuth refresh before probing the credential` | `grok_home`, `waited_ms`                                                                                   |
| `pane spawned for execution`                                                           | `driver`, `compose_ms`, `provision_ms`, `permission_config_ms`, `title_ms`, `pane_start_ms`, `pre_pane_ms` |
| `driver start confirmed; awaiting prompt submission`                                   | `elapsed_ms`                                                                                               |
| `prompt submit confirmed: the driver started a turn`                                   | `elapsed_ms`                                                                                               |

`provision_ms` is where drivers differ. For Grok it is the sum of the step lines plus file writes. The step lines are written for a failed preflight too.

`step` is one of `grok inspect`, `grok models`, `cube workspace status`, `gh auth status`, `jj root`, `jj git remote list` and `touch`. A scratch workspace reports `ls .` in place of the cube and jj steps. The `grok inspect` line is written when that command ends. The others are written together when the preflight ends, so read `elapsed_ms`, not the lines' own timestamps.

## Limits

- The production window was not re-measured. To confirm these figures in production, compare `provision_ms` and `pre_pane_ms` by `driver` across `pane spawned for execution` lines, and the Grok step lines, for spawns made after this logging is deployed.
- The September window can only be re-read as `run_started` to `pane_spawned` totals. Those can confirm the size of the offset over more spawns, but no retained record splits it by step, so they cannot confirm its cause.
- Local fixture timings cover real prompt composition, per-driver settings/hooks, permission config, spawn invocation and cached/missing-credential pane titles. They do not measure a credentialed title inference request or hook ingest, and do not establish production cross-driver equality.
- The seven-command totals exclude the engine's refresh-lock wait and credential reprobe. "Only candidate that would cost ten seconds on every spawn" refers to a cost present on every spawn; the lock wait and a token refresh can also cost that long on the spawns they hit.
- The repobin shim itself was not run, because the worker launch guard refuses it. The 11.3 s figure is the Bazel build that shim performs, run directly.
- All timings are from one host and one day.

## Reproducing

To reproduce the real-code setup attribution, build `//tools/boss/engine/core:spawn_setup_probe` with `bazel build --remote_download_outputs=toplevel`, then invoke the resulting `bazel-bin/tools/boss/engine/core/spawn_setup_probe` binary with one absolute, nonexistent scratch-directory argument. It emits tab-separated driver, run, step and seconds columns, 30 runs per driver. It is a measurement binary, not a test or an engine; it starts neither worker panes nor an engine, isolates every writable runtime home, and needs an installed `codex` for hook-trust attestation. Keep its scratch directory and output for analysis.

`grok_spawn_latency_probe.py` is a dated record of how the subprocess, `tui` and `setup` figures above were taken, not a supported tool. It hand-copies Grok's provisioning, environment and pane command as of Grok CLI 1.0.70 and nothing ties it to the engine, so a rerun would measure that snapshot rather than the current engine. For a current measurement, use the Boss log lines (`Grok spawn preparation step finished`, `pane spawned for execution`) from a real spawn.
