# Boss forensic surfaces

Two independent surfaces for answering "what happened?" questions
against a live Boss install. Both are currently undocumented elsewhere
at the retention/semantics level this note covers.

**Workers:** paths under `~/Library/Application Support/Boss/` are
coordinator runtime state and off limits to worker sessions. This doc
exists so agents know _what exists_ and _what each surface can answer_
when reading investigation notes or writing recovery procedures — not
so workers open the files themselves.

## 1. `engine-audit.log` — best-retained forensic surface

Path (default):

```text
~/Library/Application Support/Boss/engine-audit.log
```

(Override: `BOSS_ENGINE_AUDIT_PATH`.)

Reach for this **first** for provenance questions ("who deleted this
row", "what surface issued this change").

### Retention

- Small, effectively unrotated for months of typical use.
- Measured 2026-07-26: ~1.3 MB back to 2026-05-08.
- Contrast: `engine-trace.jsonl*` retention is purely size-based
  (rotates at 100 MiB, keeps 10 backups; a restart no longer rotates a
  file that isn't yet full). Bound is 10 × 100 MiB of rotated volume —
  wall-clock coverage depends on write rate and is expected to be well
  over a week at observed volumes. Worst case on disk: 11 × 100 MiB
  ≈ 1.1 GiB (active + 10 backups); tune with
  `BOSS_ENGINE_TRACE_MAX_FILES` / `BOSS_ENGINE_TRACE_MAX_BYTES`. A
  high-volume incident can still rotate past the window you care about.

Lifecycle event shapes (`start` / `socket_bound` / `instance_lock_failed` /
`shutdown`) are also documented in
[`tools/boss/app-macos/README.md`](../app-macos/README.md) under
"Forensic / audit log". That README also notes an in-process ~2 MiB
half-drop bound; under normal load the file stays well under that
ceiling, which is why multi-month retention is observed in practice.

A `start` whose `parent_command` is launchd is not evidence that
launchd spawned the engine: the macOS app detaches with `nohup`, so
both app-spawned and orphaned CLI engines reparent to pid 1. Use
`launched_by` (`app` vs `standalone`) and `app_pid` (`BOSS_APP_PID`)
to name the launcher. A `start` followed ~seconds later by
`instance_lock_failed` / `error:instance lock held by pid …` is a
duplicate launch that lost the pid-file flock; historically this
showed up as `error:database is locked` after two 5s SQLite busy
timeouts because the pid file was written only after `WorkDb::open`.

### `work_item_deleted` events

Each delete emits one `work_item_deleted` event with:

| Field                 | Notes                                     |
| --------------------- | ----------------------------------------- |
| `work_item_id`        | The deleted item                          |
| `cascade_deleted_ids` | Items removed as a cascade of this delete |
| `actor`               | Coarse only — see below                   |
| `peer_pid`            | Process that issued the change            |
| `reason`              | Why the engine recorded the delete        |
| `ts`                  | Timestamp                                 |

### Reading deletes

**`reason` discriminates the call path:**

- `reason: "delete_work_item request"` → the `DeleteWorkItem` frontend
  RPC (a human / CLI / app delete).
- `reason: "unpopulate_project"` with `actor: "engine"` and no
  `peer_pid` → an engine-internal unpopulate. A different thing
  entirely from a user delete.

**`actor` is coarse:**

- Resolves only to `mac_app` vs `unknown`.
- There is no caller-stamped surface on deletes the way creates have
  `created_via`, so coordinator `boss`, worker `boss`, and any ad-hoc
  client all collapse to `unknown`.
- `unknown` means "not the app" — nothing finer.

**`peer_pid` traps:**

- Near-useless post-hoc (the process is gone by the time you read the
  log).
- **Not** monotonic across batches — never infer ordering from it.
- Distinct `peer_pid` per event + empty `cascade_deleted_ids` = N
  independent CLI calls, not one cascading action.

## 2. Per-task cost — transcripts only, ~29-day cliff

Per-task cost is **not** in `state.db`. No table holds tokens, model,
or turn counts.

### Where cost lives

The only source is the Claude Code session JSONL:

```text
~/.claude/projects/<workspace-slug>/<session-uuid>.jsonl
```

Reached via `work_runs.transcript_path`. The slug is derived from the
cube workspace path, e.g.:

```text
-Users-brianduff--local-share-cube-workspaces-mono-agent-070
```

### Retention cliff

Measured 2026-07-26 against the live state root:

| Age of transcript | Survival |
| ----------------- | -------- |
| ≤ 28 days         | 100%     |
| 28–35 days        | ~9.7%    |
| > 35 days         | 0%       |

This is Claude Code's own ~30-day cleanup (`cleanupPeriodDays` is
unset; default applies), **not** a Boss GC.

Of 12,411 `work_runs` ever recorded (same measurement window):

- 7,348 have a `transcript_path`
- only 4,883 still resolve on disk

Roughly two-thirds of Boss run history is permanently cost-blind.

### What is durable in `state.db`

- Wall-clock timing
- Queue wait
- Retry counts
- Execution / run structure **for executions that ever started**
- Review linkage
- The task-field ledger (`task_status_audit`), including the automatic stop
  history described below. Nothing prunes it, and it has no foreign key to
  `work_executions`, so its rows outlive the executions they describe.

"Execution structure" is **not** retained forever for every execution. The
retention sweep (`work-db/src/execution_retention.rs`) deletes an execution
row only when it is all of: in `abandoned`, `failed`, `orphaned` or
`cancelled`; never started (`started_at IS NULL`); created more than 14 days
ago; and outside the newest five such never-started rows of its work item.
A row that ever started a run, and every `completed` row, is kept
indefinitely. Deleting the execution also deletes its `work_runs`, attention
items and worker proposals (`ON DELETE CASCADE`).

Worked example, as of 2026-11-18: a never-started execution created
2026-10-29 is 20 days old and is outside the age bound, so it is pruned once
it also falls outside its item's newest five never-started rows. One created
2026-11-11 is 7 days old and is within the age bound. Whether a particular
row is present on a given installation cannot be told from source: it
depends on the sweep having run, on how many newer never-started rows the
item has, and on the clock at that moment.

### Counting traps

- **Rounds = distinct `.message.id`**, not assistant-record count.
  One API response is split across thinking / text / `tool_use`
  records that share an id. Naive counting overstates by ~2×.
- **`worker_stop_received` is not a turn counter.** It is an
  `auto_bind_poller` artifact (three executions carried 760 events
  each in the measurement corpus).
- **Use `started_at → finished_at`, never `created_at → finished_at`.**
  Queue-wait mean is ~370× its median. Inside a started execution,
  agent-active / run-wall median ratio is ~0.97, so run wall is a
  good agent-time proxy.
- **Read the model from `.message.model`; never infer it from
  effort / reasoning policy.** Models drift within a tier over time
  (e.g. both opus-4-8 and opus-5 have served `pr_review`), so a
  policy-derived model is wrong for historical rows.
  `tasks.reasoning` was populated on only 16 / 4,132 rows in the
  measurement corpus.
- **~44% of `pr_review` executions are unchanged-sha re-runs.**
  Group by `(work_item_id, pr_head_before)` to separate real review
  effort from lane flapping; report raw and dedup figures.
- **Output tokens is the best size proxy** — monotonic across all
  four effort levels. Rounds is not (medium 160 vs large 153 at the
  median in the corpus).

### Attribution

Attribution is exact; no PR heuristics required. A `pr_review`
execution carries the **reviewed item's** `work_item_id`. Join:

```text
tasks → work_executions → work_runs → transcript_path
```

Gap: review-findings follow-up work parented elsewhere links back via
`tasks.origin_task_short_id` / `origin_pr_number`, populated on only
27 / 4,132 rows in the measurement corpus.

## 3. Automatic stop history — `task_status_audit`

Every engine write that moves an execution to `abandoned`, and every write
to a task's `autostart`, `deferred` or `status`, appends a row to the
`task_status_audit` ledger **in the same transaction as the change**
(a rolled-back change leaves no row). Rows are read with
`boss task show <id> --json` (`field_audit`) and
`boss task executions <id>` (text, and `field_audit` in `--json`), over the
existing `ListTaskFieldAudit` request.

What a row carries:

| Field                                  | Meaning                                                                                                                 |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `property`                             | `execution_abandoned`, `autostart`, `deferred`, or a `status` move made in the same operation as one of those           |
| `old_value` / `new_value`              | Before and after; for an abandonment, the execution's previous status and `abandoned`                                   |
| `reason_code`                          | Stable machine-readable cause (never reworded once shipped); `unattributed_write` for a write that bypassed the helpers |
| `detail`                               | Diagnostic text for the cause; may be reworded                                                                          |
| `actor`, `basis`                       | Who the engine attributes the write to, and the writer (plus the socket caller's identity when one exists)              |
| `operation_id`                         | Shared by every row one atomic operation wrote (abandon + autostart off + status demotion)                              |
| `execution_id`                         | The abandoned execution; the execution row itself may since have been pruned                                            |
| `failure_stamp` / `failure_error`      | The task's dispatch-failure time, and the execution's last error (else the task's dispatch-failure diagnostic text)     |
| `execution_ever_ran` / `item_ever_ran` | Whether the execution, and any execution of the item, had started or produced a run at that moment                      |

Rows written before this ledger captured causes keep `NULL` in every added
field: the columns are not backfilled, and attribution that was never recorded
(which caller, which transaction) cannot be recovered retrospectively.
`autostart` and `status` rows take their `reason_code` from the writer's
helper name (the first word of `basis`); abandonment rows carry an explicit
code per writer (`abandon_cause` in `work-db/src/task_status_audit.rs`).
`basis` itself was recorded from the start, so a pre-enrichment autostart
writer is still named there even when `reason_code` is NULL.
Conflict-resolution and CI-remediation attempts that are marked `abandoned`
are separate tables, not executions: they appear here only through the
execution changes they cause.

### The card notice

The same rows drive a persistent notice on the item's card
(`Task.auto_stop_notice`, drawn by `WorkAutoStopBanner`), so an item the
engine stopped on its own is marked until a human sees it. A previous
finished run does not hide a later stop. The notice is derived from the
ledger on every read (full board, incremental card re-read, single-item
reads), never stored, so the card and `boss task show --json` cannot
disagree. No attention is raised for it.

A ledger row raises it when the engine wrote it and no run has started at
or after that row: either an `autostart` `1 → 0` write (not the normal
consumption at run start) while autostart is still off, or an abandonment of
a never-started execution whose cause is not one of the deliberate
withdrawals (`BENIGN_ABANDON_CAUSES` in `work-db/src/auto_stop_notice.rs`).
The abandonment branch does not require autostart to be off: an unreplaced
never-started execution is a stall even when the reconciler is supposed to
re-mint. It additionally needs the item to be live, not deferred or parked,
and to have no queued or running execution. It clears when a later run
starts, when an execution is queued again, when autostart is turned back on
(autostart-off notices), or when a human acknowledges it. The card names
whether autostart was turned off, the queued run was withdrawn, or both
(one operation), plus the reason code and time of the latest stop.

Autostart rows recorded before migration `20261118_1530_execution_abandon_audit`
keep NULL `reason_code`. The notice query derives the code the same way the
trigger does (`COALESCE(reason_code, first word of basis)`), so those rows
still raise when they otherwise qualify, labelled with the writer `basis`
already named rather than `unattributed_write`. A pre-enrichment run-start
consumption is excluded by that derived code, not only by the later-run
timestamp. Abandonment rows from the same era keep NULL `execution_ever_ran`
and therefore never raise: that branch requires `execution_ever_ran = 0`.
Writes that bypass the helpers still raise as `unattributed_write`, because
the trigger records that code.

Acknowledgment is the existing `UpdateWorkItem` request with
`acknowledge_auto_stop_through = <audit_id>` (the id the card showed). It is
stored per item in `task_auto_stop_ack` as the highest ledger row id seen,
clamped to the newest row the item has and never lowered, so a stop recorded
later is a new notice and a stale client cannot bring an old one back.

### Retention of the other surfaces, from source

- `engine-audit.log`: capped at 2 MiB (`MAX_LOG_BYTES` in
  `engine/core/src/audit.rs`); past the cap the oldest half is dropped on the
  next append. There is no guaranteed day window; coverage depends on write
  rate.
- Dispatch events (`current.jsonl`, `engine/dispatch-events`): rotated at
  100 MiB, five rotated segments kept (six counting the active file). Also
  volume-bounded, not time-bounded.
- The ledger above is the only one of these that is both durable and tied to
  the state change; prefer it for "why did this stop?" over the volume-bounded
  logs.

## Related

- Lifecycle audit events: [`tools/boss/app-macos/README.md`](../app-macos/README.md)
- Transcript viewer design: [`tools/boss/docs/designs/transcript-viewer.md`](designs/transcript-viewer.md)
- Post-crash recovery: [`tools/boss/docs/post-crash-recovery.md`](post-crash-recovery.md)
