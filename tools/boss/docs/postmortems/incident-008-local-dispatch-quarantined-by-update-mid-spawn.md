# Incident 008 — Local dispatch quarantined by an app update mid-spawn

- **Date:** 2026-10-29; all timeline times are CDT (UTC−05:00), the host's local zone.
- **Impact:** No new local worker could launch for roughly 20–25 minutes (19:11:16 to about 19:3x). The only remote host was disabled, so nothing could dispatch anywhere. Workers already running were unaffected. One unrelated task was archived as collateral damage.
- **Status:** Resolved by hand-deleting the quarantine metadata row. The underlying defect is still present on `main`; a fix task is filed (see §8). This document changes no code.
- **Provenance:** Notes and command captures taken during the incident (launch refusal text, the quarantine row's value before deletion, `bossctl agents list`, a timeline), file metadata on the installed app bundle, source inspection, and the text of the PRs cited below.
- **Source baseline:** `main` at `c7e2223826f3` (mono#3347). All `path:line` references below refer to this revision.

## 1. Summary

A conflict-resolution run for mono PR #3348 was force-launched at 19:10:50. Ten seconds later the Boss app auto-updated from 1.0.858 to 1.0.861, which restarted the engine while that run was still in its spawn setup: its run row existed, marked `host_id = 'local'`, but no tmux identity and no shell pid had been written yet. The new engine's startup scan for "historical local workers without tmux identity" — a migration guard introduced by PR #2993 for app-hosted workers from before tmux became mandatory — matched the half-spawned run, could not prove it dead (a missing pid probes as "unknown"), and recorded it in a persistent hold that blocks **all** local dispatch.

The hold could not be cleared by any supported means. Cancelling the run did not release it (held executions are re-added regardless of status), restarting Boss did not release it (release requires a dead recorded pid or a kernel boot well after the run's last write), and there is no CLI for it. The repo owner deleted the metadata row with `sqlite3`; because dispatch reads that row on every call, local launches resumed immediately.

### Verdict

The root cause is a false premise in the tmux-only hosting design: that "no code can spawn a new null-identity local run". The normal spawn path creates exactly such a run for a short window, and the startup scan has no way to tell that run from a genuine pre-migration row. Any engine restart inside that window freezes local dispatch until someone edits the database. Here the restart was an app auto-update; any crash or manual restart at the wrong moment would have done the same.

## 2. Evidence and method

- The refusal returned by `bossctl agents launch` after the update, captured verbatim (§3).
- The value of `metadata.local_worker_startup_quarantine` immediately before it was deleted: `{"executions":{"exec_18e3225a3baa3d78_bc":"task_18e3225a3b8c7478_bb"},"scan_error":null}`.
- `ctime` of `/Applications/Boss.app` and `Contents/Resources/bin/engine` (19:11:00–19:11:01). The bundle's `mtime`s are the 1979 zero date the build stamps, so only `ctime` dates the replacement. The installed version string changed from 1.0.858 to 1.0.861.
- The new engine process's start time (19:11:16) and a `bossctl agents list` capture at 19:24 showing every already-running worker `adoption_state=adopted`.
- Source at the baseline above, and the PR descriptions of #2993, #2997, #2998, #3181 and #3335.

That the engine restart caught the run _specifically between run-row creation and the tmux intent write_ is an **inference**: it is the only window in which the scan's predicate (§4.2) matches a live local run, and the row's recorded state (local, null tmux columns, null pid) is exactly that window's state. No engine log line pins the moment.

## 3. Timeline

| Time (CDT)        | Observation                                                                                                                                                                                                                                                   |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| evening           | Host load average is very high throughout, roughly 150–410 (one `uptime` reading: `374.19 411.86 358.33`). Remote host anaplian is disabled.                                                                                                                  |
| 19:10:50          | The conflict-resolution run for mono PR #3348 is force-launched with `bossctl agents launch`. Execution `exec_18e3225a3baa3d78_bc` is recorded running; its run row is created with host `local` and no tmux identity, as the normal spawn order does (§4.1). |
| 19:11:00–19:11:01 | `/Applications/Boss.app` is replaced by an auto-update, 1.0.858 → 1.0.861.                                                                                                                                                                                    |
| 19:11:16          | The new engine starts. `quarantine_historical_local_workers` finds the half-spawned run, probes it as unknown, and writes the hold. From here every local launch is refused. No tmux pane for the PR #3348 run ever appears in `bossctl agents list`.         |
| 19:11 onward      | Launches of several queued tasks are refused with: `local dispatch quarantined: historical workers lack tmux identity; roll back or drain them and restart (executions: exec_18e3225a3baa3d78_bc; scan error: none)`, alongside `anaplian: disabled`.         |
| 19:18:09          | The PR #3348 run is stopped and its execution cancelled. The hold persists.                                                                                                                                                                                   |
| 19:24:22          | A run of the isse PR #18 conflict-resolver task fails under the lockout.                                                                                                                                                                                      |
| 19:24:43          | The engine archives the isse PR #18 conflict-resolver task. Its updated brief is stranded; a fresh resolver later has to be filed by hand.                                                                                                                    |
| before ~19:3x     | Boss is restarted (exact time not verified against the host clock). The next launch is refused with the identical message: a restart does not clear the hold.                                                                                                 |
| ~19:3x            | The repo owner deletes the `local_worker_startup_quarantine` metadata row with `sqlite3`. Two local launches succeed immediately afterward, with no engine restart.                                                                                           |

## 4. Root cause

### 4.1 The spawn path has a window with no tmux identity and no pid

A local dispatch writes the run and its tmux identity in separate steps, with arbitrary setup in between:

1. Dispatch creates the run row via `start_execution_run_on_host_with_tmux_hosting` (`tools/boss/engine/core/src/coordinator/execution.rs:1901`). The row has `host_id = 'local'`; the tmux columns and `shell_pid` are NULL.
2. `start_worker_armed` (`tools/boss/engine/core/src/spawn_flow.rs:786`) does pre-pane setup: driver checks, progress wiring, writing files into the workspace.
3. `start_tmux_worker` records the tmux spawn intent — server label, session name and spawn token — (`spawn_flow.rs:351-365`, which issues the `UPDATE` at `tools/boss/engine/core/src/work/run_rows.rs:725-734`).
4. The tmux session is created (`spawn_flow.rs:383-391`).
5. The pane pid is read and persisted as both `tmux_pane_pid` and `shell_pid` (`spawn_flow.rs:437-457`, via `persist_tmux_identity_after_observation` in `run_rows.rs`).

Between steps 1 and 3 a live, legitimate local run has none of the identity columns and no pid. Under heavy load (as here) that window can be long.

### 4.2 The startup scan cannot distinguish that run from a pre-migration row

At startup, `app/server.rs:640` calls `quarantine_historical_local_workers` (`tools/boss/engine/core/src/local_worker_quarantine.rs:118`). Its candidate query (`local_worker_quarantine.rs:100-115`) selects every non-terminal execution whose latest run is local and has a NULL `tmux_session_name`, `tmux_spawn_token` or `tmux_server_label`. Nothing restricts it to rows written before the tmux-only migration (no age, schema-version, or build filter), so the step-1 run above matches.

For each candidate the scan releases the execution only on proof of death (`local_worker_quarantine.rs:207-212`):

- the durable pid probe returns `Gone`; but a NULL pid returns `WorkerProcess::Unknown` (`tools/boss/engine/core/src/durable_liveness.rs:389-391`), which is held — the test `missing_pid_is_unknown_even_when_no_tmux_worker_was_recorded` (`local_worker_quarantine.rs:375`) pins this; or
- the run's newest durable write predates the kernel boot by more than `PRE_BOOT_MARGIN_SECS = 3600` (`local_worker_quarantine.rs:33`, `run_predates_boot` at `:170-175`); a run written seconds ago on a host that has not rebooted can never satisfy this.

So the half-spawned run was held, and the hold was written to `metadata` under `local_worker_startup_quarantine` (`local_worker_quarantine.rs:23`, `:238`).

### 4.3 The hold is global, sticky, and only re-evaluated at startup

- **Global.** `local_dispatch_quarantine_reason` (`local_worker_quarantine.rs:56-66`) returns a refusal whenever the hold contains _any_ execution. Host selection drops the `local` host entirely when it does (`coordinator/execution.rs:308-312`), and the local pane spawner refuses outright (`tools/boss/engine/core/src/runner/pane_spawn.rs:1185-1187`). This is by design: PR #2993's description says the quarantine "pauses local dispatch" and "persists until rollback/drain and a restart can prove death, including when another path has terminalized the held row."
- **Sticky.** The scan merges previously held executions back into the candidate set regardless of their current status (`local_worker_quarantine.rs:187-190`, comment and `rows.extend(previous.executions…)`: "A status change from another recovery path is not proof of death"). That is why cancelling the run at 19:18:09 changed nothing.
- **Startup-only.** The hold is written only by the startup scan, but read on every dispatch (`local_worker_quarantine.rs:49-53` reads the metadata row each call). A restart re-runs the scan and re-holds the same execution, because neither release condition can become true for a null-pid run on an un-rebooted host. Deleting the row, conversely, takes effect on the next dispatch with no restart, which is what resolved the incident.

### 4.4 The design premise

`tools/boss/docs/designs/make-tmux-the-only-pane-hosting-mode.md:221` describes null-tmux-identity local rows as "migration input, not a supported mode" and states that "no code can spawn a new null-identity local run". §4.1 shows that premise is false for the duration of the spawn setup window. The same doc's evidence table (`:233`) deliberately treats a missing pid as unknown and quarantines it — correct for the app-hosted rows it was written for, and exactly wrong for a run that never reached the point of having a process.

### 4.5 Origin

The quarantine was introduced by PR #2993 "Require tmux for local worker spawn and teardown" (merged 2026-09-18 05:42:32 UTC). Follow-ups #2997 "Keep engine startup alive when a quarantined task is tombstoned" and #2998 "Prove historical local workers dead from the kernel boot time" (both merged 2026-09-18 19:03:34 UTC) added the tombstone tolerance and the boot-time release. #3181 "Add per-minute work DB lock summary" (merged 2026-10-15) touched `local_worker_quarantine.rs` only to add the lock-instrumentation attribute (`#[work_db_boundary::instrument]`, `:48`); from its description it does not change quarantine behaviour. PR #3335 "Verify worker PID identity and retain confinement through teardown" (merged 2026-10-28) is **not** involved, although it was initially suspected (§6.2).

The latent window has existed since PR #2993 shipped. Hitting it requires an engine restart while a local spawn is between steps 1 and 3; the high load this evening plausibly widened the window (**inference**).

## 5. Contributing factors

- **The app auto-update restarts the engine with no drain.** The update replaced the bundle and started a new engine 26 seconds after the spawn began, with nothing waiting for in-flight spawns to finish or roll back.
- **The error message prescribes remedies that cannot work.** "roll back or drain them and restart" assumes an older app-hosted worker exists. For a null-pid, never-started run there is nothing to roll back to or drain, and a restart re-holds it (§4.3). The attention text filed alongside (`local_worker_quarantine.rs:259-263`) gives the same rollback advice.
- **Restarting does not help**, and that is not discoverable from the message.
- **No supported clearing path.** No `boss`/`bossctl` command inspects or clears the hold; the only way out was a hand-written `DELETE` against the engine database.
- **One held execution blocks every local launch**, including work unrelated to the held task (§4.3).
- **A launch failure caused only by the dispatcher-wide gate archived an unrelated task.** The isse PR #18 conflict-resolver task's run failed at 19:24:22 purely because of the lockout; the engine archived the task at 19:24:43, stranding its updated brief.
- **No remote capacity.** With anaplian disabled, the local block was a total dispatch outage rather than a degradation.

## 6. What went well, what went poorly

### 6.1 Went well

- The refusal text named the exact execution and the metadata key was easy to locate from source, so the hold was identified precisely.
- Already-running workers were untouched and all adopted cleanly by the new engine.
- Because dispatch reads the hold on every call, clearing it took effect instantly, without a restart.
- The quarantine row's value was captured before deletion, preserving the evidence.

### 6.2 Went poorly (investigation)

- The gate was first attributed to PR #3335, the most recent engine change near worker identity. Reading the source showed it originates in PR #2993.
- Event times were first assumed to be PDT; the host runs in CDT, which briefly misaligned the update's `ctime` against the spawn time. Once corrected, the update fell 10 seconds into the spawn.
- A Boss restart was first recommended as the fix. It cannot work for this row (§4.3), and doing it confirmed that rather than resolving anything.
- The trigger (why the engine restarted mid-spawn) was found only by checking the app bundle's `ctime`; nothing in Boss records "engine restarted because the app updated" next to the dispatch failures.

## 7. Lessons

- A migration guard that keys on "this column is NULL" is only safe if nothing in the current code ever writes that column NULL. Here the spawn path writes it NULL transiently, so the guard eventually catches live work.
- A fail-closed hold needs a fail-out path that is reachable for every row it can hold. A rule that can hold a row it can never release turns an edge case into a manual database edit.
- Remedy text in errors must match the condition actually detected, not the condition the check was originally written for.

## 8. Action items

**Filed:** "Stop one unverifiable worker spawn from quarantining all local dispatch". It covers:

- closing the spawn window, so a live local run never exists without tmux identity;
- classifying a run with no identity, no pid and no session as never started, rather than unknown;
- scoping the hold to the affected task instead of all local dispatch;
- re-evaluating the hold on every dispatch instead of only at startup;
- fixing the error message so its remedy matches the condition.

**Recommended (not filed by this document):**

- The app update / relaunch path should drain or wait for in-flight spawns before restarting the engine.
- A launch that fails only because of a dispatcher-wide gate (quarantine, no eligible host) must not archive the task.
- Add a supported CLI to inspect and clear the startup hold, with an explicit confirmation step, so recovery never requires editing the database by hand.
