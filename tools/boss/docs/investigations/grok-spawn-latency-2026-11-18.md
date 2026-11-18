# Grok spawn latency: where the time before `pane_spawned` goes

A September sample showed Grok workers taking about 17 s longer than Claude or Codex between `run_started` and `pane_spawned`. On current `main` that offset does not reproduce. The work only Grok does before its pane exists took under 5 s in every one of 138 local runs, and its CLI accepted a brief under 5 s after starting in every one of 84. The one step able to cost ten seconds or more in September was removed by mono#3428.

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

In mid-September the runner returned as soon as the pane existed, so the 17 s had to be spent before the pane: in composition, provisioning, config or pane creation. Only provisioning differs by driver in a way that runs subprocesses. Today the interval also includes the CLI's start and the brief delivery (`engine/execution-runner/src/runner/pane_spawn.rs:1470`), for every driver.

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

Host: 12-core macOS laptop running other workers' builds, whose load swung widely during the day. Each batch below gives the 1-minute load average read just before and after it. Grok CLI 1.0.70, Claude Code 2.1.327, Codex CLI 0.169.0. Each run used a fresh `GROK_HOME` and process `HOME` with the engine's environment and delegates, the shared `GROK_AUTH_PATH`, and a leased checkout of this repository as the working directory. `grok_spawn_latency_probe.py` beside this file is the harness. The first batch of each kind was run before it was consolidated into that one script, and the rest with it as committed.

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

These were not re-measured as a population. The launcher log of the Claude worker that produced this write-up shows the composer ready 0.59 s after the launcher started and Return written 0.11 s later (one run). `docs/worker-prompt-delivery.md` records 0.5 to 1.9 s to composer ready and 0.6 to 4.9 s to Return over 71 Claude runs at normal load. The `python3` check took a median 0.050 s and `codex debug models --bundled` a median 0.028 s (20 runs each).

A Codex pane was not started. Its per-run home holds a snapshot of the operator's login, and a token refreshed inside a discarded probe home would not be adopted back.

### Older Grok CLIs

Two earlier builds are still on the host. `grok inspect --json` plus `grok models`, 8 runs each with a fresh home: 0.2.111 (July) median 0.924 s, max 0.978 s; 1.0.62 (October) median 0.825 s, max 1.454 s. The build in use in mid-September was not available.

## Is the effect real today

Not at the size reported. At the host's calmer moments Grok pays 1.4 to 1.8 s before its pane that the other drivers do not, and reaches an accepted prompt about half a second later than Claude does once the pane is up. That is roughly 2 s more than Claude per spawn, in one group.

Host load does not turn that into 17 s either. With the 1-minute load average between 165 and 235 on 12 cores, the pre-pane total had a median of 3.0 s and the pane reached an accepted prompt in a median 2.5 s. The worst figures seen in any condition were 4.8 s before the pane and 4.7 s after it.

This is a local reproduction. The production dispatch-event window was not re-measured.

## Where the September cost was

Inferred, not measured.

Mid-September's interval ended at pane creation, so the cost was in Grok's pre-pane subprocesses. Until mono#3428 merged on 2026-11-02, the preflight ran the cube probe by bare name, `cube`, resolved through the engine's `PATH`. Of the seven commands, that is the only one whose cost is not bounded by a sub-second process start or a single network round trip:

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

The remaining Grok-only cost is this repository's choice to run seven checks before each pane. Nothing in the Grok CLI needs to change for the reported offset. The floor of `grok models` is a network call to xAI and is upstream.

The reason the question was raised has also changed. Grok was the third member of every pre-merge review batch, so its spawn time was the batch's. mono#3073 removed the Grok leaf reviewer from those batches. Grok still pays this cost as an implementation and progression driver.

## What can still stretch a Grok spawn

- **Token refresh.** When the shared OAuth token has expired, `grok models` refreshes it and is run a second time (`engine/driver/src/grok/preflight.rs:320`). Not observed during these runs.
- **A refresh lock held by a live process.** The Grok CLI leaves `auth.json.lock` in place after a refresh: on this host it was still there, naming an exited process, more than an hour after the refresh that wrote it. If the process that refreshed is still running, such as a long-lived worker pane, a preflight starting within 30 s waits until the lock is 30 s old (`engine/driver/src/grok/environment.rs:243`). Not observed during these runs. It is now logged when it lasts 250 ms or more.
- **The bounds.** 30 s for `grok models` and 60 s for each other command (`engine/driver/src/grok/preflight.rs:23`). A hung tool fails the spawn at its bound.
- **Host load.** Heavy load roughly doubled every figure here. Seven process starts in sequence are more exposed to it than the file writes the other drivers do.

## What could be done, and what it would cost

| Change                                                                     | Saves per Grok spawn                                  | Cost                                                                                                                                       |
| -------------------------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Run the independent checks concurrently                                    | about 0.5 s (to the slowest check, about 0.8 s)       | More threads per spawn, and a failure no longer stops the later checks from running                                                        |
| Reuse a recent `grok models` or `gh auth status` result for a short window | up to about 1.2 s                                     | A credential revoked inside the window passes the check. These checks exist because a worker that starts without them parks holding a slot |
| End the refresh-lock wait once `auth.json` is newer than the lock          | up to 30 s, only on spawns that hit a live fresh lock | Depends on the CLI writing the lock before the credential, which would need characterising first                                           |
| Leave it as it is                                                          | nothing                                               | Grok spawns stay about 2 s slower than Claude spawns                                                                                       |

None of these was made. The measured cost does not justify weakening a fail-fast check, and the lock wait has not been seen to fire.

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
- The engine's own steps (prompt composition, pane title, pane creation, hook ingest) were not timed here. They are common to all drivers and are now reported per spawn.
- The repobin shim itself was not run, because the worker launch guard refuses it. The 11.3 s figure is the Bazel build that shim performs, run directly.
- All timings are from one host and one day.

## Running the probe

```sh
python3 tools/boss/docs/investigations/grok_spawn_latency_probe.py preflight <workspace> <scratch-dir> 30
python3 tools/boss/docs/investigations/grok_spawn_latency_probe.py preflight <workspace> <scratch-dir> 30 --parallel 3
python3 tools/boss/docs/investigations/grok_spawn_latency_probe.py tui <workspace> <scratch-dir> 15
python3 tools/boss/docs/investigations/grok_spawn_latency_probe.py tui <workspace> <scratch-dir> 4 --hold SessionStart:3
```

It needs a logged-in `grok`, `gh`, `jj`, `tmux` and the bundled `cube` (or `PROBE_CUBE` naming another build). It does not contact a Boss engine. The `tui` mode submits a brief asking for a one-word reply and stops the pane when the prompt is accepted.
