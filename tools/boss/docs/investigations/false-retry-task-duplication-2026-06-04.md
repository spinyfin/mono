# Investigation: False-retry task duplication from `failed_will_retry` on automations

**Status:** Closed — no duplicates created; open-task cap held on all automations.
**Date:** 2026-06-04
**Area:** Automation scheduler (`automation_scheduler.rs`, `work/automations.rs`)
**Trigger:** Audit requested after observing repeated Opus triage runs caused by `failed_will_retry` runs that never emitted a decision marker.

## TL;DR

Every `failed_will_retry` occurrence (a dispatched triage run that ends without a decision marker) advances the schedule and fires a new triage run on the next occurrence. That causes an expensive Opus run every 15 min per automation until either a marker is emitted or a task is created by a later run. However, **no duplicate tasks are created**: the `open_task_limit` gate holds at two independent layers — the scheduler's pre-dispatch check and the transactional re-check inside `create_automation_task` — and a failed run that never calls `boss task create` cannot contribute to the open-task count regardless. The root cause of most false retries was a transcript scanner bug (PR #1345) now fixed.

---

## Background: what `failed_will_retry` means

Every triage dispatch records its `automation_runs` row with `outcome = failed_will_retry` immediately as a **pessimistic default** (`automation_scheduler.rs:425`). The task-6 completion handler is supposed to overwrite this with `skip`, `produced_task`, or `failed_gave_up` when the triage worker's Stop fires. If the worker ends without the completion handler updating the row (crash, reaped, emitted no marker), the row stays `failed_will_retry`.

Two distinct paths reach `failed_will_retry`:

| Path | Triage execution created? | Schedule advances? | Cost |
|---|---|---|---|
| **Dispatched**: execution created, worker runs but emits no marker | Yes | **Yes** (to following occurrence) | Full Opus triage run per occurrence |
| **TransientFailure**: pre-start failure (cube lease error, VPN down) | No | **No** (occurrence held) | Near-zero: no worker spawned; upserts same DB row on every tick |

The expensive path is **Dispatched** + no marker.

---

## Retry mechanics: how the scheduler re-fires

When a dispatched run stays `failed_will_retry`, the next scheduler pass sees the **new** `next_due_at` (the following occurrence timestamp). At that point the scheduler evaluates in order (`automation_scheduler.rs:271–449`):

1. **Not due?** — passes if the new occurrence is in the past.
2. **Catch-up collapse** — collapses a backlog to the single most-recent occurrence.
3. **Skip-if-stale** — occurrence older than `catch_up_window` (default 15 min)? If so, check `automation_run_for_occurrence` for the new occurrence. Because the new occurrence was never previously attempted, `already_attempted = false`, so the run is recorded as `skipped` and the schedule advances again. This fires only when the system has been offline long enough for a whole new occurrence to fall stale.
4. **Open-task gate** — `count_open_tasks_for_automation` counts tasks with `source_automation_id = ?` and `status IN ('todo', 'ready', 'active', 'in_review', 'blocked')`. A `failed_will_retry` run that never called `boss task create` contributes **zero** to this count. Gate passes.
5. **Fire** — dispatch succeeds → record another `failed_will_retry`, advance schedule again.

Result: on a 15-min cron (or a daily cron at a fixed hour), one `failed_will_retry` occurrence leads to a new Opus triage run at every scheduled interval until either a later run emits a decision marker or an operator intervenes.

For the transient-failure path, no Opus run is spawned. The held occurrence is re-attempted until it goes stale (> 15 min from `now`), at which point it's written `failed_will_retry` with detail `"stale: catch-up window elapsed before retry"` and the schedule advances. From the DB perspective, transient retries are upserts on the same `automation_runs` row (keyed `(automation_id, scheduled_for)`) — they never pile up duplicate rows (`automation_scheduler.rs:626–641`).

---

## A1 / A2 / A3: can the cap create duplicate tasks?

The cap is enforced at two independent layers:

### Layer 1 — scheduler pre-dispatch gate

`count_open_tasks_for_automation` fires **before** `dispatch_triage` is called (`automation_scheduler.rs:386–407`). If any task with `source_automation_id = this_automation` is in an open status, the fire is recorded `suppressed_at_limit` and the schedule advances past it. This gate is cheap (one `SELECT COUNT(*)`) and fast.

A `failed_will_retry` run **has not created a task**, so it contributes nothing to this count. The gate passes, which is what triggers the wasteful retry sequence described above. The gate is correct on its own terms — it is protecting against "too many open tasks" not "too many in-flight triage runs". For the duplication question, this gate's correctness means: once any prior run *has* created a task, the next occurrence is suppressed until that task closes.

### Layer 2 — transactional cap re-check at task creation time

`create_automation_task` opens an `IMMEDIATE` transaction and re-runs the same `COUNT(*)` query inside it before inserting (`work/automations.rs:813–830`). If a concurrent triage worker (from a parallel occurrence or from an unusual race) already inserted a task between the scheduler's fire-time gate and this call, the transaction sees `open >= open_task_limit` and returns an error. The triage worker's `boss task create --automation` call fails; the preamble instructs the worker that a second call will be rejected; the worker still emits a marker (either reconciling the task it tried to create or emitting a skip), and the run is finalised normally.

This is the backstop the design doc (`maintenance-tasks.md:162`) explicitly calls out as the fan-out guard: "The transactional cap re-check at `boss task create --automation` (see `WorkDb::create_automation_task`) is the backstop against a misbehaving agent fanning out."

### Gap: no "triage already running" gate

Neither the scheduler nor the task-creation path checks whether a triage execution for this automation is currently `triage_running` or `pool_throttled`. If three consecutive occurrences all end `failed_will_retry` and the three resulting triage executions are queued in the automations pool simultaneously, all three are allowed to proceed. Any one of them that calls `boss task create` could succeed (open count = 0 at the moment of its re-check); the other two would see `open >= 1` and fail their create calls. The double-failing workers would emit no marker and stay `failed_will_retry`, continuing the cycle.

In practice this only arises when triage runs accumulate faster than the automations pool (size 3) can drain them, which requires the pool to be persistently backlogged. For the observed window (false retries on A1/A2/A3 on a daily cron or ~15-min cron), the pool drain rate is much faster than the occurrence rate. **Concurrent in-flight triage for the same automation is not the normal path.**

### Confirmed outcome for all three automations

The combination of Layer 1 + Layer 2 guarantees:

- A false `failed_will_retry` run that never calls `boss task create` → open_task_count = 0 → next fire is NOT suppressed (costly Opus retry) → but also zero tasks created.
- A run that calls `boss task create` successfully → open_task_count becomes 1 → next occurrence is `suppressed_at_limit` until the task closes.
- Two concurrent runs both calling `boss task create` → one succeeds; the second fails with "at its open-task limit" → at most one task created per automation at any time.

**No duplicate tasks were possible across A1, A2, or A3.**

---

## Root cause of false `failed_will_retry` (now fixed)

Two root causes, both addressed before this investigation:

1. **Transcript scanner read only the last assistant turn** (PR #1345, merged to `main` as `klolpysn`). The `parse_triage_decision` call was invoked on `iter().rev().find_map(AssistantText)` — the last assistant text event. When the triage worker calls `boss task create`, the decision marker appears in the *second* assistant turn (after the tool result). If the Stop hook fired before the post-tool turn was fully written to disk, the scanner saw only the pre-tool analysis text (no marker) and recorded `failed_will_retry`. The fix concatenates **all** `AssistantText` events from the JSONL transcript, which finds the marker regardless of which turn it appears in.

2. **Triage CLAUDE.md contained the implementation PR mandate** (fixed alongside the triage preamble work). The standard worker CLAUDE.md says "a task is not complete until a PR exists" and "PR creation is your terminal act". Workers caught between this and the triage preamble's marker contract would chase a PR (find `jj diff` empty, stop without a marker) and leave the run `failed_will_retry`. The `render_triage_claude_md` function now emits a triage-specific CLAUDE.md that omits the PR mandate entirely (verified in `automation_triage.rs` tests `triage_claude_md_restates_marker_contract_and_omits_pr_mandate` and `preamble_includes_contract_and_canonical_selector`).

---

## Query playbook for future monitoring

These SQL queries against the Boss SQLite database answer the questions the audit was asked:

```sql
-- Count false-retry runs per automation (finished_at NULL = run completed
-- without a Stop-handler finalisation, i.e. stuck failed_will_retry)
SELECT a.short_id, a.name, COUNT(*) AS stuck_retries
FROM automation_runs r
JOIN automations a ON a.id = r.automation_id
WHERE r.outcome = 'failed_will_retry'
  AND r.finished_at IS NULL
GROUP BY a.id
ORDER BY stuck_retries DESC;

-- Confirm open-task cap held: should show 0 or 1 rows per automation
SELECT a.short_id, a.name, COUNT(t.id) AS open_tasks,
       a.open_task_limit
FROM automations a
LEFT JOIN tasks t ON t.source_automation_id = a.id
  AND t.status IN ('todo','ready','active','in_review','blocked')
  AND t.deleted_at IS NULL
GROUP BY a.id
HAVING open_tasks > a.open_task_limit;  -- should return 0 rows

-- Full run history for an automation (replace 'auto_...' with the id)
SELECT scheduled_for, outcome, detail, triage_execution_id,
       finished_at IS NULL AS still_open
FROM automation_runs
WHERE automation_id = 'auto_...'
ORDER BY scheduled_for DESC
LIMIT 40;

-- Confirm no automation has more produced tasks than its open_task_limit
-- would allow if they were all still open simultaneously
SELECT a.short_id, a.name,
       COUNT(DISTINCT t.id) AS ever_produced,
       MAX(a.open_task_limit) AS limit_
FROM automations a
JOIN tasks t ON t.source_automation_id = a.id
  AND t.deleted_at IS NULL
GROUP BY a.id
HAVING ever_produced > limit_
  AND COUNT(DISTINCT CASE
        WHEN t.status IN ('todo','ready','active','in_review','blocked')
        THEN t.id END) > limit_;
```

---

## Recommendations

1. **Gate the scheduler on in-flight triage executions (enhancement, not a correctness fix).** Adding a check like `count_open_triage_executions_for_automation` before the fire (same pattern as `count_open_tasks_for_automation`) would prevent wasteful Opus retries when a previous triage run for the same automation is still queued or running. This is a cost-saving measure, not a data-integrity fix — the existing cap already prevents duplicate tasks.

2. **Add `failed_gave_up` finalisation for dispatched runs that stay `failed_will_retry` past a threshold.** Currently, a dispatched run with no finalisation stays `failed_will_retry` indefinitely. Implementing the `failed_gave_up` transition from `maintenance-tasks.md:206` (backoff capped at `catch_up_window`; once backoff would push past the next occurrence, advance) would bound the retry storm at the occurrence level.

3. **`automation: runs show A<n>` CLI verb** (if not yet implemented) to surface run history and spot stuck retries without raw SQL.
