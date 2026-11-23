# Run process teardown

What the engine and cube do about processes a worker run leaves behind, why the mechanism is working-directory ownership rather than process groups, and how the one deliberate exception (a workspace's Bazel server) is told apart.

## The leak

A finished run's tmux session was torn down, its pane shell's process group was signalled, its cube lease was released and the pool trim later removed the workspace directory. Four shell loops from one of the run's tool commands kept running for four days with their working directory in `.removing-<workspace>`, creating hundreds of processes per second. A second orphan from the same run was an `engine_lib_test` binary stranded in a Bazel sandbox directory. Both had re-parented to launchd.

The loops came from a tool command that backgrounded busy subshells as a CPU load generator for a test run and was meant to kill them at the end. The command was cut short before its final `kill`, and nothing else ever looked for them.

## Why process groups are not enough

Observed on a live worker (pids from one session):

| process                                                          | pgid            | controlling tty |
| ---------------------------------------------------------------- | --------------- | --------------- |
| pane shell (`zsh`, started by `tmux new-session -c <workspace>`) | 30509           | `ttys017`       |
| agent (`claude`)                                                 | 31286           | `ttys017`       |
| the agent's tool shell (`zsh -c …`)                              | 35428 (its own) | none            |
| `( sleep 30 ) &` from that tool shell                            | 35428           | none            |

The agent's tool runner spawns each tool shell detached into its own process group and session, so signalling the pane shell's group (`kill(-30509, …)`) reaches neither the agent nor any tool shell. A backgrounded subshell inherits the tool shell's group, and when the tool shell exits it re-parents to launchd. Walking down from the pane finds nothing; signalling the pane's group finds nothing. Cleanup therefore follows verified process identities rather than numeric process groups.

## Ownership

The rules live in `lib/rust/workspace_procs` and are shared by the engine and cube. A process belongs to a run when either holds:

- **Descendant**: its parent chain still reaches the run's pane shell, whatever its process group. This catches everything still attached.
- **Working directory**: its current directory is inside the run's workspace, or inside that workspace's Bazel output base. The kernel reports a process's working directory by the directory's _current_ path, so this survives re-parenting and follows a rename: a loop started in `mono-agent-549` is found inside `.removing-mono-agent-549` once cube has staged the directory for removal. The output base is found from the workspace's `bazel-out` symlink and from the `--output_base=` argument of a live Bazel server whose `--workspace_directory=` is the workspace; that is what reaches a test binary stranded in a Bazel sandbox.

Cube's kill sweeps add a third directory to the working-directory rule: the workspace's Claude Code scratch tree, `/tmp/claude-<uid>/<slug>/` (`/private/tmp/…` on macOS), where `<slug>` is the workspace path with every character outside `[A-Za-z0-9]` replaced by `-`. A session keeps its scratchpad there, and a job it backgrounds from the scratchpad never has a working directory in the workspace. The tree is keyed by workspace path and shared by every session started there, so it is swept only where the workspace itself is (release, force-release, setup-failure release and removal), never by the read-only expiry and claim checks. A session that sets its own temp directory is outside this rule.

The process running the sweep and its own ancestors are never candidates, so `cube workspace release` typed from a shell inside the workspace cannot kill that shell.

Only the workspace's Bazel server is spared and reported. It is recognised by the renamed argv[0] form `bazel(<workspace>)`, or the `A-server.jar` and `--output_base=` pair, never by the executable name alone. Server action children and processes holding a controlling terminal are subject to the same ownership rules as other tools.

## Run identity

Working directory is the wrong signal for a job that was started somewhere else and then lost its parent. On 2026-11-12 a worker's tool shell started a private tmux server (`tmux -S s start-server`) from its scratchpad and ran `nohup python3 …` inside it, which launched 22 long-running sessions. The server daemonized to pid 1, nothing in that tree had a working directory in the workspace, and when the worker's pane was later found absent everything kept running for about three hours.

The engine therefore owns a process by the run identity it inherited, not by where it sits (`tools/boss/worker-processes`, added in mono#3536). Every worker process is started with `BOSS_RUN_ID=<execution id>` in its environment; cleanup reads each same-user process's environment (`KERN_PROCARGS2` on macOS, `/proc/<pid>/environ` on Linux) and signals exactly the ones carrying the run's id, wherever they run and whoever their parent now is. A tmux server a worker starts inherits the variable and passes it to every pane, so the server and its pane jobs are owned too. `nohup` and `setsid` change the signal disposition, the session and eventually the parent, none of which the rule looks at.

macOS does not return the environment of platform binaries (`/bin/sh`, `/bin/sleep`, the system shells). Three things cover them: each worker shell registers its own pid and start time with its identity as it starts; cleanup extends ownership to live children of an owned process and records them before signalling; and anything left in the workspace or its scratch tree falls to cube's working-directory sweep above. What remains unattributable is a platform binary that was re-parented before any cleanup pass saw it under an owned parent and whose working directory is outside the workspace, its Bazel output base and its scratch tree. It is not killed: a process is never signalled on a guess.

Where the run-identity cleanup runs:

- **Lease release** (`HostOccupancy::cleanup`, behind `apply_release`): every local release that follows a worker having run, including the dead-pid and pane-death reconciles, the stale-worker reap, forced release and normal completion. It runs before preservation and before cube is called. A survivor keeps the lease and files `workspace_lease_unreleased` naming the pids; the leftover-lease sweep retries every minute.
- **Runs with no lease** (`reap_leaseless_run_processes`): a `progression` run, or a row whose lease columns are already clear, never reaches the release gate. The dead-pid and pane-death reconciles stop its processes directly, and a survivor files `run_processes_survived_cleanup` naming the pids.
- **Session teardown** (`kill_session_verified`, and the session-already-gone branch of `reap_tmux_worker`): the same cleanup keyed on the session's `BOSS_SPAWN_TOKEN`.
- **Remote hosts**: the owning host runs the same script through its adapter (`cleanup_execution_processes`).

Two paths deliberately do not clean up: a pane that is still present, and a pane-death reconcile whose run the live registry still tracks. In both the worker may be alive, and the lease is kept instead.

## Killing and logging

The ladder sends `SIGTERM` to owned processes and retains their (pid, start-time) identities through the grace period. Escalation scans those identities and descendants of targets still alive with the same start time; it never discovers new targets by workspace path. This keeps re-parented TERM-resistant tools in scope without touching a later lease's occupants or a recycled root pid. The KILL pass uses that identity fence. Final verification also reports new directory occupants without signalling them, so a newly orphaned process blocks removal. Process-list and individual process inspection failures retain the partial signal report and make verification inconclusive.

Every process killed is logged with pid, command, age, which signal, why it was owned and its argument vector:

- engine: `run process reap: killed leftover process` lines in the engine log, with `execution_id`, `pid`, `command`, `age_secs`, `signal`, `owned_by`, `argv`; survivors log at error level as `process survived SIGKILL`.
- cube: `cube release: <workspace>: killed leftover process pid … (…) age …s SIGTERM [working directory inside …]: …` on stderr, plus a `workspace.processes_reaped` audit event (`phase` is `release`, `force-release`, `remove`, `remove-leftover` or `remove-staged`) carrying the killed, surviving and spared lists, and a `processes` object in the `--json` output of `workspace release`.

## Where it runs

- **Pane release** (`tmux_teardown.rs`): tmux verifies the spawn token and awaits cleanup of registered and token-inheriting process identities before destroying the session. It never adopts the recorded numeric pane pid at teardown; retained dead panes cannot confer ownership on a replacement process.
- **Engine completion** (`finish_worker_teardown`): releases through Cube, which verifies that the lease still exists before its workspace sweep. A stale completion with only a recorded workspace path does not sweep that path.
- **cube `workspace release` and `force-release`**: sweep and kill before the reset; the release proceeds whatever happened, with survivors in the message, stderr and audit log. Every engine termination path releases its lease through this command, so it is the net under the engine-side passes.
- **cube removal** (`workspace remove --expunge`, pool trim): sweep and kill before the directory is staged, again under the staged `.removing-<id>` name before it is unlinked, and before a leftover staged directory is cleared. A survivor or inconclusive process scan fails the removal with `refusing to remove workspace directory …: processes are still running inside it after SIGTERM and SIGKILL: <pid (command)>`; the directory is left in place for a later pass.

- **Expired leases and claims**: read-only scans quarantine occupied or uninspectable workspaces without signals or resets, with an audit listing the processes or inspection error. Selection continues to another workspace or provisions a new one.
- **Setup failures**: Cube sweeps before release, logging and auditing the `setup_failed` phase.
- **Removal audit**: a spared Bazel server is noteworthy even when no signals were sent; its pid remains in the removal audit.

## Verifying

`bazel test //lib/rust/workspace_procs/...` includes an integration test that starts a worker-like `sh` which backgrounds an infinite `sleep` loop and exits, leaving the loop re-parented to pid 1 with its working directory in a temporary "workspace", alongside a `sleep` renamed to `bazel(…)`. The reap kills the loop with `SIGTERM` alone and leaves the look-alike server running. A second test renames the workspace to `.removing-<id>` first and finds the loop under its new name. `bazel test //tools/cube/... //tools/boss/engine/core:engine_lib_test` covers scan failures at removal, deterministic identity fencing across workspace reuse and re-parenting, and the engine's real `reap_workspace` entry point with a detached loop and Bazel look-alike.

The incident shapes have their own tests. `//tools/boss/engine/core:engine_lib_test` drives the observed-death reap (`reap_observed_worker_death`) against real jobs started with `nohup` and in a new session from a directory outside the workspace, re-parented and carrying the run's `BOSS_RUN_ID`; it asserts both are gone before the lease is returned and that a job carrying another run's identity, and one carrying none, are untouched. The same file covers a run with no lease. `//tools/boss/engine/core:tmux_recovery_integration_test` (tagged `manual` because it needs the host's tmux) does the same for a tmux server the worker started itself with `tmux -S <socket> new-session` and the job in its pane. `//lib/rust/workspace_procs:reap_integration_test` starts `nohup sleep` from a scratch tree, where macOS hides the environment, and checks that the working-directory sweep kills it and leaves a neighbouring workspace's scratch tree alone.
