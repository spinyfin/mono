# Unverified local worker hold

Use this runbook when a launch or dispatch is refused with one of these messages, or when a work item shows the attention item "Dispatch held for an unverified local worker":

- `work item <id> has a quarantined historical local worker: execution <exec> has no complete tmux identity and its process is not proven dead …`
- `local dispatch paused: the engine could not read its local run inventory …`

The design is in [Make tmux the only pane hosting mode](../designs/make-tmux-the-only-pane-hosting-mode.md#unverified-local-workers). The incident that shaped it is [incident 008](../postmortems/incident-008-local-dispatch-quarantined-by-update-mid-spawn.md).

## What the hold is

The engine can only adopt, signal, or tear down a local worker through its tmux identity. A non-terminal execution whose local run has no complete identity, and whose process is not proven dead, might be a worker from before tmux hosting that is still running. The engine holds that execution: it does not reap it, re-drive it, or dispatch anything else for its work item.

A hold covers one work item. Every other work item dispatches normally. If unrelated work is not dispatching, the cause is something else; start with `bossctl dispatch diagnose <id>`.

A spawn that was interrupted before it reached tmux (an engine restart or app update during worker setup) is not held. Startup fails that execution with the reason "worker spawn never reached tmux" and queues the work item again.

## Inspect

```sh
bossctl dispatch quarantine status
```

This is read-only and works with the engine down. For each held execution it prints the work item, the execution's status, the evidence, and what releases it:

| Evidence          | Meaning                                                    | What releases it                                                                                 |
| ----------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `process_alive`   | A pid was recorded and a process with that pid exists.     | The process exits, or the execution goes terminal.                                               |
| `process_unknown` | No usable pid was recorded, or the probe could not answer. | The execution goes terminal. A reboot also proves it dead once the run is over an hour pre-boot. |

`--json` gives the same data for scripts.

## Release

Work down this list and stop at the first step that applies. None of them needs an engine restart; dispatch re-reads the hold on every decision.

1. **Do nothing if the cause is already gone.** The engine re-judges every hold before each dispatch decision (admission, host selection, and the active-dispatch rescan). If the process has exited or the execution has gone terminal, that decision releases it. To see the result immediately, or with the engine down:

   ```sh
   bossctl dispatch quarantine recheck
   ```

2. **`process_alive`: check what the pid is.**

   ```sh
   ps -o pid,ppid,lstart,command -p <pid>
   ```

   If it is a Boss worker from before tmux hosting, stop it, then run `recheck`. If it is an unrelated process, the pid number was recycled and no probe will ever release the hold; cancel the execution (step 3). A manual release (step 4) is refused while the recorded process is alive.

3. **Cancel the execution** if its work should not continue as that execution. A terminal execution never holds anything, so this releases the work item at once and for good:

   ```sh
   bossctl work cancel <execution-id>
   ```

   `work cancel` accepts a running execution. `bossctl executions cancel` does not: it only cancels executions that never started.

4. **Release the hold by hand** when the evidence is `process_unknown`, you have checked that no worker is running for the work item, and you want the execution left alone:

   ```sh
   bossctl dispatch quarantine clear --execution <execution-id>
   ```

   Without `--confirm` this prints what it would release and changes nothing. Re-run with `--confirm` to release. `--all --confirm` releases every hold. Either form refuses, and releases nothing, if a hold it names has a live recorded process.

   A manual release removes the hold only. The engine's liveness sweeps then reap the execution if its worker is gone. An execution that is still non-terminal and unverifiable when the engine next starts is held again, so cancel it (step 3) if it should not come back.

Releasing a hold while a worker really is still running lets the engine dispatch a second worker for the same work item. Check first.

## Local dispatch paused: unreadable run inventory

This is the only condition that pauses local dispatch for every work item. Startup could not read the `work_runs` / `work_executions` inventory, so it cannot tell which work items have an unverified worker. `status` prints the underlying error.

The engine retries the read on every dispatch decision and lifts the pause when one succeeds, so a transient failure clears itself. `bossctl dispatch quarantine recheck` retries immediately.

If the read keeps failing, the error names a real database problem (a missing column after a bad migration, a damaged file). Fix that first. `bossctl dispatch quarantine clear --all --confirm` lifts the pause without fixing the cause and should be a last resort: with the inventory unreadable, the engine cannot protect any unverified worker from a duplicate dispatch.

## What not to do

- Do not edit the `local_worker_startup_quarantine` metadata row with `sqlite3`. The verbs above do the same thing, check what they are releasing first, and resolve the attention items.
- Do not restart Boss to clear a hold. A restart re-judges the same evidence and reaches the same verdict.
