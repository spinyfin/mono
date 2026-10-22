# Incident 007 — Engine failed to start after the tmux-only upgrade: a quarantined worker's task had been deleted

- **Date:** 2026-09-18
- **Severity:** High — the engine could not start at all on the operator's database after auto-updating to 1.0.665. No data loss; no worker was affected. A second defect, found during the investigation, would have paused all local dispatch indefinitely once the first was fixed.
- **Status:** Fixed forward in mono#2997 (startup crash) and mono#2998 (permanent quarantine), released in Boss 1.0.666. Engine confirmed healthy at 19:21:48 UTC; total outage 45 minutes.

## 1. Summary

Boss 1.0.665 shipped the tmux-only worker migration (mono#2993, merged 05:42 UTC). Its new startup step, `local_worker_quarantine::quarantine_historical_local_workers`, runs before ordinary recovery and holds every nonterminal historical local execution whose latest run lacks tmux identity, unless the durable pid probe proves the process gone. For each held execution it files an operator-facing attention item against the execution's work item.

The operator's database held 106 such executions from May and June 2026, all `waiting_human` with a completed run and no recorded shell pid, and one of them belonged to a design task the operator had soft-deleted on 2026-05-12. `upsert_external_tracker_attention` validates its work item with `product_id_for_work_item`, which filters out tombstoned tasks, so the attention insert failed with `unknown task: task_18aebf0ca9d580b8_a`. `app/server.rs` wraps the quarantine in `?`, so that error aborted engine startup nine seconds in, before the socket bound. The app's supervisor retried once with the identical result and surfaced the red banner.

Two defects, one causing the outage and one behind it:

**Root cause 1 — attention filing was on the fatal path (the outage).** The hold itself was already written to the `metadata` table before the attention loop ran, so nothing about safety depended on the attention item. The `?` on an advisory write turned a cosmetic failure into a boot failure. mono#2997 makes attention filing and resolution best-effort: a failure is logged at `error` and the sweep continues.

**Root cause 2 — a pid-less historical row could never leave the hold (latent, one restart away).** The quarantine's only proof of death was the pid probe returning `Gone`. The design was explicit that neither elapsed time nor a later terminal status is proof, and that the operator's remedy is to "roll back to the prior release to stop or drain the worker." But the 106 rows never recorded a pid, so the probe returns `Unknown` forever; the prior release had left them untouched for three months precisely because it also had nothing to probe; and `mark_execution_orphaned` and `cancel` are either refused or ignored by a hold that is re-established from persisted metadata on every boot. With mono#2997 alone the engine would have started and then reported "local dispatch quarantined" with no operator path out. mono#2998 adds a second proof of death: a kernel boot. No process survives it, so a run whose newest durable write predates `kern.boottime` by more than an hour is dead regardless of pid evidence. The operator's machine booted on 2026-09-13; every held row was last written in June.

## 2. Why the quarantine held 106 rows that were obviously dead

The rows are the normal _live_ shape of the pre-tmux pane-hosted worker. `PaneSpawnRunner` drove `start_execution_run` → `UpdateWorkerShellPid` → `finish_execution_run`, parking the execution in `waiting_human` with its run `completed` the instant the pane came up (`test_support::create_spawned_execution` documents this shape; mono#2673 later stopped writing it). So "run completed, execution waiting_human" carries no death information, and the quarantine is right not to read it as such.

What made these particular rows unreapable is that `UpdateWorkerShellPid` never landed for them: `shell_pid` is NULL on all 106 latest runs. Every subsequent liveness sweep (`dead_pane_sweep`, `dead_pid_sweep`, `husk_pane_sweep`) keys off a recorded pid and correctly declined to act on rows without one. They accumulated silently from May through June, harmless because nothing consulted them, until mono#2993 made "no pid" a reason to pause the whole local host.

The design considered "process evidence is unknown, including a missing pid" and chose the hold. That is the right default for a worker that might exist. It did not consider the case where the hold can never be lifted: the remedy assumed the prior release could name and drain the worker, and for a pid-less row it cannot.

## 3. Timeline

All times UTC on 2026-09-18.

| Time     | Event                                                                                                                                                                                  |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 02:20:37 | Engine pid 6336 (1.0.664) starts after a routine update. Runs cleanly for 16h15m.                                                                                                      |
| 05:42:32 | mono#2993 merges.                                                                                                                                                                      |
| 05:49:56 | Boss 1.0.665 published.                                                                                                                                                                |
| 18:36:06 | Auto-update installs 1.0.665. Engine 6336 shut down by RPC. Engine 44369 starts.                                                                                                       |
| 18:36:15 | Engine 44369 exits: `error:establishing historical local worker quarantine before startup recovery` (uptime 9s).                                                                       |
| 18:36:26 | App retries. Engine 44386 starts.                                                                                                                                                      |
| 18:36:35 | Engine 44386 exits with the same reason. App shows "Boss engine failed to start … unknown task: task_18aebf0ca9d580b8_a".                                                              |
| 18:37    | Operator reports the banner. Investigation begins from a standalone Claude Code session.                                                                                               |
| 18:46:37 | mono#2997 (crash) and mono#2998 (boot-time proof) opened as a stack. The first CI run fails on a clippy lint in the fix; rewritten and re-pushed, green on the rerun.                  |
| 19:03:34 | Operator merges both PRs.                                                                                                                                                              |
| 19:17:17 | Boss 1.0.666 published.                                                                                                                                                                |
| 19:21:48 | Auto-update installs 1.0.666. Engine 52570 starts cleanly: all 106 held rows proven dead by boot time (`run_predates_kernel_boot`), quarantine metadata empty, local dispatch resumes. |

Time to diagnose from first look at the banner to a reproducing SQL query against the live database: about ten minutes. The banner named the failing step and the offending task id, and `engine-audit.log` recorded the exact exit reason, so no log archaeology was needed.

## 4. Impact

- The engine was down from 18:36:06 to 19:21:48 UTC (45 minutes). The app, the coordinator, and every dispatch path were unavailable for that window. Remote hosts were not consulted because the engine never reached recovery.
- No execution, run or work item was modified by the failed starts: the quarantine writes its metadata row and then fails on the first attention insert, and every subsequent start rewrites the same row.
- Had the crash fix shipped alone, local dispatch would have stayed paused on every 1.0.665 install with pre-tmux history, with a banner telling the operator to roll back and drain workers that do not exist.

## 5. Detection

The app detected it immediately and said the right thing: the banner carried the failing step and the task id, which pointed directly at `product_id_for_work_item`'s tombstone filter. `engine-audit.log` had the same reason with the pid and uptime. Nothing alerted beyond the banner, which is adequate for a single-operator deployment.

What was not detectable in advance: no test exercised the quarantine against a deleted work item, and no test exercised a held row with no pid and no path to proof. Both shapes are ordinary in a database with history and absent from a fresh one, the same asymmetry as incident 003.

## 6. Fixes

### 6.1 Attention filing is best-effort (mono#2997)

`quarantine_with_probe` now logs and continues when `resolve_external_tracker_attention` or `upsert_external_tracker_attention` fails. The hold is durable before either runs. Regression: `tombstoned_work_item_keeps_its_hold_without_aborting_startup`.

### 6.2 Kernel boot time proves death (mono#2998)

`quarantine_with_probe` takes the boot epoch (`kern.boottime` on macOS, `/proc/stat btime` on Linux, `None` elsewhere or on read failure). A latest local run whose `MAX(created_at, started_at, finished_at)` is more than `PRE_BOOT_MARGIN_SECS` (one hour) before the boot is dead, outranking a pid probe that after a reboot can only report pid reuse. `None` and rows inside the margin keep the hold. The margin covers a wall clock corrected after the row was written; the kernel re-anchors its boot time on clock corrections, the row does not move. Regressions: `run_written_before_the_kernel_booted_is_proven_dead`, `run_inside_the_pre_boot_margin_or_without_boot_evidence_stays_held`, `kernel_boot_time_is_in_the_past_on_supported_platforms`. The design doc's evidence table gains the row.

## 7. Action items

1. **Startup steps that write advisory state must not be on the fatal path.** Audit `app/server.rs` for other `?` on attention or notification writes before the socket binds. (Recommend: a short pass; the pre-bind section is small and each `?` is visible.)
2. **Every fail-closed hold needs a documented path out that exists in the current release.** A design that says "roll back and drain" must show that rollback can act on the held row. (Recommend: add this to the design-review checklist for anything that pauses dispatch.)
3. **Test new startup sweeps against a database with history**, not only a fresh one: at minimum a tombstoned work item, a row with no pid, and a row older than the oldest supported release. (Recommend: a shared fixture in `test_support` that builds the pre-tmux pane-hosted shape with and without a pid; `create_spawned_execution` already builds the with-pid half.)
4. **Reap pid-less historical rows at the source.** 106 `waiting_human` executions with no pid sat for three months because every sweep keys off a pid. Boot time is now a proof; consider a periodic sweep that applies it outside startup so history does not accumulate. (Recommend: fold into the existing dead-pid sweep rather than a new module.)

## 8. What went well

- The banner and the audit log named the failing step and the offending id. Diagnosis was a `SELECT`, not a bisect.
- The hold was written before the attention item, so the safety property held even while the engine crashed; the fix could be narrowly about the fatal path.
- The second defect was found before the first fix shipped, by asking what the engine would do next rather than stopping at "it boots."

## 9. What went badly

- A large migration with a new fail-closed gate shipped through auto-update with no history-shaped test, and its first contact with a real database was the operator's.
- The design's remedy for the unknown case ("roll back and drain") was not checked against the row shapes it would actually hold. For the most common shape it was not executable.
- 1.0.665 was the third release of the day; the update cadence outran the time anyone spent watching the previous one boot.

## 10. Lessons

- A hold that can be established but never lifted is a bug of the same severity as a hold that is never established. Fail-closed needs an exit as much as it needs an entrance.
- "Unknown" is not one category. A pid that might be recycled and a row that never had a pid deserve different evidence, and only one of them can be resolved by probing.
- The kernel boot is the one clock that cannot lie about process survival. It is cheap, durable and already implied by "no process survives a reboot"; reach for it before inventing a timeout.
