# All-pool tmux confidence gate — 2026-09-10

- **Project:** Make tmux the only pane hosting mode
- **Design:** [Tmux-only local worker panes after automatic-recovery parity](../designs/make-tmux-the-only-pane-hosting-mode.md), “Confidence gate before deletion”
- **Supersedes:** [All-pool tmux confidence gate — 2026-09-04](./tmux-all-pool-confidence-gate-2026-09-04.md)
- **Measurement:** 2026-09-10 15:50 CDT
- **Verdict: gate NOT met.** Deletion tasks (`Collapse local engine spawn and teardown to tmux only` onward) remain blocked.

## Summary

The all-pool tmux soak started when [#2903](https://github.com/spinyfin/mono/pull/2903) merged at **2026-09-04 14:11:26 CDT**. At measurement it had run for **6d 1h 39m**; its initial seven-day window closes **2026-09-11 14:11:26 CDT**.

Five criteria improved since the superseded report: #4 is now met through production auto-recovery and genuine redispatch, and #6–#8 are met after six days of real exposure. Criterion #3 moved from unobserved to demonstrably failing: the engine reaps its own live tmux workers on shutdown. Criterion #2 is a structural blocker that waiting cannot clear: automation has zero of its required five terminal executions because dispatch remains operator-paused.

The gate records these outcomes without relaxing, reinterpreting, or dropping any design criterion. Deletion stays blocked.

## Gate criteria and observed status

| #   | Criterion                                                                                                    | Status                                    | Evidence                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| --- | ------------------------------------------------------------------------------------------------------------ | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | Seven consecutive days, all local pools tmux-hosted                                                          | **NOT MET**                               | 6d 1h 39m had elapsed at measurement. On the strict stable-release reading, the engine binary changed four times in-window; the longest one-build run was 3d 16h 30m.                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 2   | At least 50 terminal local executions, including at least five each from review, automation, and interactive | **NOT MET — unmeetable by waiting**       | Aggregate **135** is sufficient: review **69**, interactive **66**, automation **0**. Automation dispatch has been paused since **2026-07-24 14:04:26 CDT**; the last `auto-*` worker ran at **2026-07-24 14:02:20 CDT**, and `automation_runs` has had zero rows since. Resuming automation and completing five terminal local tmux executions would clear this sub-floor; this report does not resume it.                                                                                                                                                                                                           |
| 3   | Successful real engine-restart and app-restart drills with work continuing                                   | **NOT MET — positive counter-evidence**   | The engine reaped all **3/3** live tmux workers across two shutdowns: two workers at **2026-09-04 14:02:33 CDT**, then slot 25 (`boss-25-18d29eefff06`, pid 4914) at **2026-09-05 22:59:12 CDT**. Each recorded `reap_tmux_worker: session reaped and identity columns cleared`; the latter was preceded by `release_worker_pane` reporting no app session and treating pane removal as unconfirmed. The app-only restart at **2026-09-10 15:33:03–15:33:06 CDT** preserved engine pid 6114, `boss-coordinator`, and Claude pid 45198, but no worker pane was live; app-restart worker continuity remains unobserved. |
| 4   | Successful two-threshold automatic recovery and redispatch through the genuine path                          | **MET**                                   | Four production `stale-worker sweep: two-hour token-verified auto-reap firing` events used `auto_reap_threshold_secs: 7200`: **2026-09-05 00:00:17 CDT** (slot 2), **2026-09-07 15:11:29 CDT** (slot 3), and **2026-09-07 21:39:19 CDT** (slots 6 and 7). Seven orphan redispatches followed, including a **2026-09-05 00:00:17 CDT** reap followed by redispatch at **00:07:07 CDT**; review recovery also refired at **2026-09-05 22:59:33 CDT** and respawned at **22:59:39 CDT**.                                                                                                                                 |
| 5   | Successful normal-exit observation through retained `pane_dead` state                                        | **INSUFFICIENT EVIDENCE — telemetry gap** | `pane_dead` is live on the wire and probed on every adoption pass (`tools/boss/engine/core/src/tmux_adoption.rs:263`), but a true value is neither logged at INFO/WARN (only unreadable values have `tracing::debug!` handling at lines 266 and 278) nor persisted. Zero trace hits therefore cannot prove normal-exit observation. This is not evaluatable, not passing.                                                                                                                                                                                                                                             |
| 6   | No duplicate-worker incident                                                                                 | **MET**                                   | Zero in-window executions had more than one run and zero same-slot tmux executions overlapped. The all-time query returned 124,992 rows, confirming a non-empty query; guards only prevented duplicates (ten live-execution revision skips and one in-flight review reuse).                                                                                                                                                                                                                                                                                                                                           |
| 7   | No unexplained token mismatch or tmux probe failure                                                          | **MET**                                   | Zero `TokenMismatch` and zero tmux probe-failure emissions appeared in-window from `boss_engine::tmux_adoption*`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| 8   | No leaked session after terminal reconciliation                                                              | **MET**                                   | In-window, 135 worker-pane attachments and 137 `reap_tmux_worker` events reconcile because two reaps were for panes attached just before the window; all 137 husks cleared. The current sessions (`boss-1-18d410597ad2`, `boss-2-18d410872e51`, and `boss-coordinator`) exactly match engine belief, with zero orphans.                                                                                                                                                                                                                                                                                               |
| 9   | Known re-adoption repair present, no periodic live-state displacement                                        | **MET, confirmed at HEAD**                | At main `bcd7c69c`, `TMUX_RUN_ADOPTABLE_PREDICATE` in `tools/boss/engine/core/src/work/run_rows.rs:18-24` still filters `e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')` rather than `r.status = 'active'`. There were 9,532 in-window retained-live-state re-adoptions and zero displacement events.                                                                                                                                                                                                                                                                                  |
| 10  | Quit-dialog correction merged or forward-ported                                                              | **NOT MET, unchanged**                    | [#2840](https://github.com/spinyfin/mono/pull/2840) remains open with `mergedAt: null`; it was last updated **2026-08-28 18:28 CDT**.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 10' | Bazel recovery integration target                                                                            | **UNCHANGED; not run in CI**              | The target exists at HEAD but remains deliberately manual (`tags = ["manual"]` in `tools/boss/engine/core/BUILD.bazel`) because Linux agents may lack host tmux. The existing PR’s three green Buildkite checks do not exercise it; its last recorded pass was manual on **2026-09-04 CDT**. It does not substitute for the supported-driver drill.                                                                                                                                                                                                                                                                   |

## Volume floor and scope of the soak

There are **135** terminal local tmux-hosted executions since soak start, plus two still-live executions; the same window has 135 `attached tmux-hosted worker pane` trace events.

```sql
-- 1788549086 = 2026-09-04T19:11:26Z (soak start)
SELECT COUNT(DISTINCT e.id)
FROM work_executions e JOIN work_runs r ON r.execution_id = e.id
WHERE r.tmux_hosted = 1 AND r.host_id = 'local'
  AND CAST(r.created_at AS INTEGER) >= 1788549086
  AND e.status IN ('completed','failed','abandoned','orphaned','cancelled');
```

- Pool totals: interactive **66**, review **69**, automation **0**.
- Driver totals: Claude **88**, Codex **27**, Grok **20**.
- Terminal statuses: completed **106**, orphaned **17**, cancelled **12**.
- This is a total, not a floor: completed rows are not pruned, the oldest execution is from **2026-04-06**, and 163 executions were created in-window.
- Fifteen local runs have `tmux_hosted=0`, but all are pre-spawn Cube lease/network failures (`created_at = started_at = finished_at`, with `error_text` beginning `Cube command failed: ...`) that never reached a pane. They are not app-hosted panes and do not break the all-pool soak.

## Stable-release and related evidence

| Engine start (CDT)  | Build SHA  | Build time              | Carries #2920–#2923 |
| ------------------- | ---------- | ----------------------- | ------------------- |
| 2026-09-04 14:02:44 | `b15472e4` | 2026-09-04 10:47:22 CDT | no                  |
| 2026-09-05 22:59:23 | `8a9619f8` | 2026-09-05 22:34:16 CDT | no                  |
| 2026-09-09 15:29:34 | `f5ebbacc` | 2026-09-09 00:47:28 CDT | yes                 |
| 2026-09-10 15:28:51 | `281f869f` | 2026-09-09 15:30:46 CDT | yes                 |

The four review-batch fixes entered the running engine only at **2026-09-09 15:29:34 CDT**. Only 20 tmux executions (13 review and seven interactive) have run on a build containing them.

[#2921](https://github.com/spinyfin/mono/pull/2921) is unproven, not validated: there have been zero `driver-start timeout` emissions versus 256 `driver-start verified` emissions. With no failed starts since the fix, zero is consistent with both a working fix and an exemption that remains ineffective; a deliberate negative-path drill is required.

The twelve `pr_review` zombies terminalized in the **2026-09-07 11:53:30–11:56:09 CDT** bulk sweep are not a tmux-soak failure. Their primary cause was review-batch leaves that did not terminalize (`review_batches.rs:1249`, `review_batches.rs:1508-1514`) and a consolidator that could not spawn (`are_same_review_batch_leaves`, `review_batches.rs:1183-1200`); those are addressed by #2920 and #2923. The secondary tmux-shaped contributor is #2921, now running but unexercised.

The seven tmux servers on the box are likewise not criterion-8 session leaks: they are unreachable, empty test leakage from the manual recovery integration test, started **2026-09-04 01:33–01:57 CDT**, about 12.4 hours before this soak began. Their sockets and directories were cleaned, but `exit-empty off` retained their server processes; this report does not kill them.

## Verdict

The following criteria remain unmet even after the original calendar window closes at **2026-09-11 14:11 CDT**; waiting cannot clear them:

- #2: automation’s five-execution sub-floor, which needs an operator decision to resume automation and then five terminal local tmux executions.
- #3: engine-restart continuity, which is failing because shutdown reaps live workers.
- #3: app-restart continuity with a live worker, which remains unobserved.
- #5: normal-exit `pane_dead` observation, which is blocked on missing telemetry.
- #10: the still-open quit-dialog correction, unchanged for 13 days at measurement.
- #1 on the strict stable-release reading: the new build’s clock began **2026-09-09 15:29 CDT** and closes **2026-09-16 15:29 CDT**.

The deletion tasks remain blocked. This report records the observed failures as the design requires; it does not change the gate or treat elapsed time as a substitute for the unmet criteria.
