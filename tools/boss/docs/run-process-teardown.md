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

Only the workspace's Bazel server is spared and reported. It is recognised by the renamed argv[0] form `bazel(<workspace>)`, or the `A-server.jar` and `--output_base=` pair, never by the executable name alone. Server action children and processes holding a controlling terminal are subject to the same ownership rules as other tools.

## Killing and logging

The ladder sends `SIGTERM` to owned processes and retains their (pid, start-time) identities through the grace period. Escalation scans those identities and descendants of targets still alive with the same start time; it never discovers new targets by workspace path. This keeps re-parented TERM-resistant tools in scope without touching a later lease's occupants or a recycled root pid. The KILL pass uses that identity fence. Final verification also reports new directory occupants without signalling them, so a newly orphaned process blocks removal. Process-list failures retain the partial signal report and make verification inconclusive.

Every process killed is logged with pid, command, age, which signal, why it was owned and its argument vector:

- engine: `run process reap: killed leftover process` lines in the engine log, with `execution_id`, `pid`, `command`, `age_secs`, `signal`, `owned_by`, `argv`; survivors log at error level as `process survived SIGKILL`.
- cube: `cube release: <workspace>: killed leftover process pid … (…) age …s SIGTERM [working directory inside …]: …` on stderr, plus a `workspace.processes_reaped` audit event (`phase` is `release`, `force-release`, `remove`, `remove-leftover` or `remove-staged`) carrying the killed, surviving and spared lists, and a `processes` object in the `--json` output of `workspace release`.

## Where it runs

- **Pane release** (`tmux_teardown.rs`): the pane pid and start time supply the ancestry root; the execution's recorded workspace supplies the path. The pane's mutable cwd is never trusted. The full ladder runs on a blocking thread and is awaited before lease release. Broad path roots (root, home, ancestors of the default cube workspace pool) are rejected.
- **Lease release** (`finish_worker_teardown` and `force_release`): `run_process_reap::reap_workspace` with the workspace path captured before the terminalizing write nulled it, immediately before `cube workspace release`.
- **cube `workspace release` and `force-release`**: sweep and kill before the reset; the release proceeds whatever happened, with survivors in the message, stderr and audit log. Every engine termination path releases its lease through this command, so it is the net under the engine-side passes.
- **cube removal** (`workspace remove --expunge`, pool trim): sweep and kill before the directory is staged, again under the staged `.removing-<id>` name before it is unlinked, and before a leftover staged directory is cleared. A survivor or inconclusive process scan fails the removal with `refusing to remove workspace directory …: processes are still running inside it after SIGTERM and SIGKILL: <pid (command)>`; the directory is left in place for a later pass.

- **Expired leases and setup failures**: cube sweeps and audits these transitions too, and verifies the selected workspace before setup. Expiry sweeps use the `expire` phase. A failed verification refuses reuse.
- **Removal audit**: a spared Bazel server is noteworthy even when no signals were sent; its pid remains in the removal audit.

## Verifying

`bazel test //lib/rust/workspace_procs/...` includes an integration test that starts a worker-like `sh` which backgrounds an infinite `sleep` loop and exits, leaving the loop re-parented to pid 1 with its working directory in a temporary "workspace", alongside a `sleep` renamed to `bazel(…)`. The reap kills the loop with `SIGTERM` alone and leaves the look-alike server running. A second test renames the workspace to `.removing-<id>` first and finds the loop under its new name. `bazel test //tools/cube/... //tools/boss/engine/core:engine_lib_test` covers scan failures at removal, deterministic identity fencing across workspace reuse and re-parenting, and the engine's real `reap_workspace` entry point with a detached loop and Bazel look-alike.
