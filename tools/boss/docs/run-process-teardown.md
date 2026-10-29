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

The agent's tool runner spawns each tool shell detached into its own process group and session, so signalling the pane shell's group (`kill(-30509, …)`) reaches neither the agent nor any tool shell. A backgrounded subshell inherits the tool shell's group, and when the tool shell exits it re-parents to launchd. Walking down from the pane finds nothing; signalling the pane's group finds nothing. Process groups are therefore kept as a first pass but cannot be the ownership rule.

## Ownership

The rules live in `lib/rust/workspace_procs` and are shared by the engine and cube. A process belongs to a run when either holds:

- **Descendant**: its parent chain still reaches the run's pane shell, whatever its process group. This catches everything still attached.
- **Working directory**: its current directory is inside the run's workspace, or inside that workspace's Bazel output base. The kernel reports a process's working directory by the directory's _current_ path, so this survives re-parenting and follows a rename: a loop started in `mono-agent-549` is found inside `.removing-mono-agent-549` once cube has staged the directory for removal. The output base is found from the workspace's `bazel-out` symlink and from the `--output_base=` argument of a live Bazel server whose `--workspace_directory=` is the workspace; that is what reaches a test binary stranded in a Bazel sandbox.

The process running the sweep and its own ancestors are never candidates, so `cube workspace release` typed from a shell inside the workspace cannot kill that shell.

Three kinds of owned process are spared and reported instead of killed:

- **The workspace's Bazel server.** It is meant to stay warm across runs and exits on its own idle timeout. Its working directory is the workspace root, so it would otherwise be the first thing a working-directory sweep killed. It is recognised by its command line, never by being a `java` process: the Bazel client renames the server's `argv[0]` to `bazel(<workspace basename>)` and passes `--output_base=` and `--workspace_directory=`; the kernel still reports the executable as `java`. Either the renamed `argv[0]` or the `A-server.jar` plus `--output_base=` pair identifies it.
- **Live descendants of that server**, which are actions it is still running and cancels itself when its client goes away. A run's own root always wins over this rule, because under `bazel test` every test process descends from a server.
- **A process with a controlling terminal that is owned only by working directory**: a human's interactive shell `cd`'d into the workspace. Nothing a worker's detached tool runner spawns has a controlling terminal, so this spares humans without sparing orphans. A tty-attached descendant of the pane shell is still killed.

## Killing and logging

The ladder is `SIGTERM` to every owned process, a grace period waiting for them to exit, a rescan (a fork loop keeps producing children until its loop process is gone), `SIGKILL` to whatever is still owned, a short settle and a final rescan that reports survivors. Every signal is checked against the process start time recorded at the scan, so a pid recycled in between is never signalled.

Every process killed is logged with pid, command, age, which signal, why it was owned and its argument vector:

- engine: `run process reap: killed leftover process` lines in the engine log, with `execution_id`, `pid`, `command`, `age_secs`, `signal`, `owned_by`, `argv`; survivors log at error level as `process survived SIGKILL`.
- cube: `cube release: <workspace>: killed leftover process pid … (…) age …s SIGTERM [working directory inside …]: …` on stderr, plus a `workspace.processes_reaped` audit event (`phase` is `release`, `force-release`, `remove`, `remove-leftover` or `remove-staged`) carrying the killed, surviving and spared lists, and a `processes` object in the `--json` output of `workspace release`.

## Where it runs

- **Pane release** (`tmux_teardown.rs`, every path that reaps a verified tmux worker): `run_process_reap::reap_pane_tree`. The pane pid is the ancestry root and the pane shell's own working directory supplies the workspace path while the shell is still alive. `SIGTERM` is synchronous so the release stays prompt; the `SIGKILL` pass runs detached after the grace period. The pane process-group ladder still runs after it.
- **Lease release** (`finish_worker_teardown` and `force_release`): `run_process_reap::reap_workspace` with the workspace path captured before the terminalizing write nulled it, immediately before `cube workspace release`.
- **cube `workspace release` and `force-release`**: sweep and kill before the reset; the release proceeds whatever happened, with survivors in the message, stderr and audit log. Every engine termination path releases its lease through this command, so it is the net under the engine-side passes.
- **cube removal** (`workspace remove --expunge`, pool trim): sweep and kill before the directory is staged, again under the staged `.removing-<id>` name before it is unlinked, and before a leftover staged directory is cleared. A survivor fails the removal with `refusing to remove workspace directory …: processes are still running inside it after SIGTERM and SIGKILL: <pid (command)>`; the directory is left in place for a later pass.

## Verifying

`bazel test //lib/rust/workspace_procs/...` includes an integration test that starts a worker-like `sh` which backgrounds an infinite `sleep` loop and exits, leaving the loop re-parented to pid 1 with its working directory in a temporary "workspace", alongside a `sleep` renamed to `bazel(…)`. The reap kills the loop with `SIGTERM` alone and leaves the look-alike server running. A second test renames the workspace to `.removing-<id>` first and finds the loop under its new name. `bazel test //tools/cube/... //tools/boss/engine/core:engine_lib_test` covers the cube guard and the engine's two passes against a fabricated process table.
