# Work DB read volume

How much of `state.db` the engine reads, who reads it, and how to keep a recurring read from walking a table. Written after the 2026-11-21 report of the engine issuing 1.3 million 4 KiB `pread` calls against `state.db` in 15 seconds.

## What a page read is

Every engine connection has its own SQLite page cache, 2 MiB by default. A statement that touches a page not in that cache fetches it from the file with one `pread`. A walk of a table larger than the cache therefore costs one `pread` per page of the table every time it runs, and it evicts whatever the next statement needed. Two things make a walk larger than the table's row count suggests:

- `tasks.description`, `work_executions.pr_body_before` and `worker_proposals.payload_json` are long enough to spill onto overflow pages, and they are stored before most of the columns a filter tests (`status`, `deleted_at`, `state`, `pr_url`). SQLite has to follow a row's overflow chain to reach a column stored after it, so a walk that filters on `status` reads every description.
- A join can read a whole table without scanning it in the plan. When the planner expects many probes of a table that also has its own `WHERE` terms, it pre-filters the join with a Bloom filter, and it builds the filter by reading every row of that table. `EXPLAIN QUERY PLAN` shows this as `BLOOM FILTER ON t`, and SQLite's full-scan counter stays at zero.

## Surfaces

The engine counts pages fetched (SQLite's pager cache-miss counter) for every connection hold and attributes them to the statement text that caused them. See `tools/boss/engine/work-db/src/db_diagnostics/read_volume.rs`.

| Surface                          | Where            | What it says                                                                                                                                                                                                                                                                                                                                                               |
| -------------------------------- | ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `work DB heavy read`             | engine log, WARN | One hold read at least 16,384 pages (64 MiB). Names the `caller` (the `connect()` call site), the RPC `verb` or `background` task, `pages_read`, `statements`, `full_scan_steps`, and the statement text that read the most with its own page and run counts. At most one line per caller every 10 s; `suppressed_holds` and `suppressed_pages` carry what was not logged. |
| `db read summary`                | engine log, INFO | Once a minute that saw a hold of 256 pages or more: total pages, heavy holds, and the five callers that read the most, each with its hold count, pages and heaviest statement. This is the "who, and how often" line.                                                                                                                                                      |
| `work DB connection lock timing` | engine log, WARN | The existing slow-hold line now also carries `pages_read`.                                                                                                                                                                                                                                                                                                                 |
| `db_read.pages`                  | metrics counter  | Pages fetched by all work DB connections.                                                                                                                                                                                                                                                                                                                                  |
| `db_read.heavy_holds`            | metrics counter  | Holds over the heavy-read threshold.                                                                                                                                                                                                                                                                                                                                       |
| `db_read.heaviest_hold_pages`    | metrics gauge    | Largest single hold in the last minute.                                                                                                                                                                                                                                                                                                                                    |
| `database size report`           | engine log, INFO | After the first backup snapshot of an engine run and every 24th after it: the live file's page and free-page counts, and the largest tables and indexes by bytes, measured from the snapshot file.                                                                                                                                                                         |

The backup snapshot copies the whole file by design. Its read is counted and appears in `db read summary`, but it does not raise `work DB heavy read` or count as a heavy hold.

Statement text is logged without bound values. `BOSS_WORK_DB_HEAVY_READ_PAGES` lowers or raises the heavy-read threshold; setting it to a few hundred on an isolated engine lists every reader.

Nothing here changes a cache size, a plan or a lock. It observes.

## What the 2026-11-21 investigation found

The scans come from the engine's own timers, not from the app. With no app connected and no workers, an idle engine on a 371 MiB database of production shape (see "Fixture") re-read the equivalent of the whole file 6.4 times in its first ten seconds and about 5.5 times every minute after that. The reads arrive as a burst when the once-a-minute sweeps fire together, which is why a CPU sample taken at another moment looks quiet.

Each of these reads looks for a handful of live rows: tasks in one open status, executions that have not finished, undecided proposals, runs on a remote host. None of the columns that say "live" had an index, so each read walked its table:

| Recurring read                                                                 | Timer |  Pages per pass before | Why it walked                                                                                   |
| ------------------------------------------------------------------------------ | ----- | ---------------------: | ----------------------------------------------------------------------------------------------- |
| Review verdict applier (`list_proposed_review_verdicts`)                       | 10 s  |                 19,884 | no index on `worker_proposals.state`; `?N IS NULL OR` filters; `state` stored after the payload |
| Scheduler beat: unexplained waits (`list_unexplained_dispatch_waits`)          | 15 s  |                 20,612 | pending executions found by table walk; tasks walked to name waits                              |
| Scheduler beat: queued displays (`queued_displays`)                            | 15 s  |                 12,257 | planner started from `tasks`; later a Bloom filter over `tasks`                                 |
| Dependency unblock (two reads)                                                 | 30 s  |        13,982 + 11,937 | no index on `tasks.status`                                                                      |
| Merge poller candidate lists (five reads sharing `query_pending_merge_checks`) | 60 s  |            10,168 each | no index on `tasks.status`                                                                      |
| Merge poller: review admission, stalled reviewer, late PR detection            | 60 s  | 12,202, 11,964, 10,277 | no index on `tasks.status`                                                                      |
| Orphan sweep, review recovery                                                  | 60 s  |         12,003, 12,003 | no index on `tasks.status`                                                                      |
| Pane-death sweep: recent terminal executions                                   | 60 s  |                 20,602 | visited every finished execution and its latest run                                             |
| Pane-death, lease release, lost workspace, answer-agent completion             | 60 s  |             7,844 each | no index covering unfinished executions                                                         |
| Remote lease reconcile (two reads)                                             | 60 s  |             7,468 each | no index on `work_runs.host_id`                                                                 |

Measured by the engine's own heavy-read log on the fixture. The fixture's `tasks` table is 12,192 pages, `work_executions` 7,836, `work_runs` 7,467 and `worker_proposals` 18,757.

The app's work-tree requests do not trigger these reads. They run on separate read-only connections against the incremental board projection, and every figure above was taken with no app connected. The two problems meet only on the machine: the app's main thread and the engine's sweep burst compete for the same cores.

## The fix

Migration `20261121_1800_recurring_read_indexes` (`tools/boss/engine/work-db/src/recurring_read_indexes.rs`) adds eight indexes. Six are partial, so they hold only live rows and stay a few pages however much history accumulates; `tasks_status_idx` and `work_executions_finished_at_idx` cover every row.

| Index                                | Serves                                                          |
| ------------------------------------ | --------------------------------------------------------------- |
| `tasks_status_idx`                   | every `status = '...'` task read                                |
| `tasks_open_idx`                     | reads written as `status NOT IN ('done', 'archived')`           |
| `tasks_dispatch_failed_idx`          | clearing stale dispatch-failure banners                         |
| `work_executions_terminal_lease_idx` | finished executions still holding a workspace lease             |
| `work_executions_finished_at_idx`    | finished-time windows written as `CAST(finished_at AS INTEGER)` |
| `worker_proposals_undecided_idx`     | undecided proposals by kind, oldest first                       |
| `work_runs_remote_host_idx`          | runs on a remote host                                           |
| `work_runs_local_shell_ended_idx`    | local runs with a shell pid, by end time                        |

Several statements were rewritten or pinned so the plan does not depend on planner statistics:

- The verdict applier reads undecided proposals through a dedicated statement with `state = 'proposed'` as a literal.
- The pane-death sweep's terminal read starts from the runs that ended inside its window instead of from every finished execution.
- Unfinished executions are sought through the existing status index with the list of non-terminal statuses, generated from the `ExecutionStatus` enum. A partial index on `status NOT IN (<terminal>)` was tried first and rejected: it took over the plan of the in-flight workspace read from the index built for it.
- The scheduler beat's two reads name the status index and start from pending executions. `queued_displays` returns "still on the board" as a result column rather than a `WHERE` term, which is what stops the Bloom filter.
- Joins whose order matters use `CROSS JOIN`, and statements that depend on one partial index name it with `INDEXED BY`.

The migration analyzes only the indexes it creates. Building them reads each of the four tables once, on the first start after the upgrade.

### Result on the fixture

Same fixture, same isolated engine, idle, no app, no workers. Pages are 4 KiB.

| Measure                                                     |        Before |         After |
| ----------------------------------------------------------- | ------------: | ------------: |
| One pass of the recurring reads (budget test)               | 233,146 pages |   6,506 pages |
| Engine, steady state, pages per minute (`db read summary`)  | about 495,000 |  about 16,000 |
| Engine, steady state, holds of 200 pages or more per minute |            45 |            25 |
| Engine, first 10 s after start                              | 582,201 pages | 127,901 pages |

The budget-test "before" is the 25 reads measured before the fix; "after" is those 25 plus five added since. The first ten seconds after the upgrade include the one-time index build, 82,809 pages; without it the figure is 45,092. The engine rows are single runs of about six minutes each on 2026-11-21. Re-measure with the recipe below before quoting them for a later build.

## Rules for a recurring read

1. A read that runs on a timer must cost what is live, not what is stored. If it filters on a column, that column needs an index, and if only a small class of rows is ever wanted the index should be partial.
2. A partial index can only serve a statement whose `WHERE` repeats the index predicate literal for literal. `state = ?1` does not match `WHERE state = 'proposed'`; `status IN ('todo', 'active')` does not match `WHERE status NOT IN ('done', 'archived')`.
3. Do not write an optional filter as `?1 IS NULL OR column = ?1` on a recurring read. It leaves the planner no usable term. Write a statement per shape.
4. Pin the plan when it matters. The planner estimates an empty partial index at the size of its table and prefers a wider index, and with average-only statistics it estimates a list of live statuses at most of the table and prefers a walk. `INDEXED BY` and `CROSS JOIN` take the choice away from the statistics.
5. Keep single-table conditions on the inner table of a join out of `WHERE` when the outer side is small, or the planner may build a Bloom filter over the inner table.
6. A new index can displace the plan of an existing statement. Two existing plan tests caught this for the indexes above; a scoped read that must seek by id keeps the unary `+` on the indexed column for that reason.

## The budget test

`tools/boss/engine/work-db/src/tests/sweep_read_budget_tests.rs` runs every recurring read against the fixture and fails any that fetches more than 2,500 pages, naming the read and its heaviest statement. It runs under four sets of planner statistics: none, fresh, average-only (as the system `sqlite3` writes them) and "analyzed before the upgrade".

```sh
bazel test //tools/boss/engine/work-db:work_db_test --test_filter=sweep_read_budget --test_output=all --test_arg=--nocapture
```

Add a new timer-driven read to `recurring_reads()` in that file. To put a budget on any other code path, wrap it in `boss_work_db::db_diagnostics::read_volume::measure`, which returns the pages, statements and full-scan steps of every hold released on the calling thread.

### Fixture

The wait-scale board (`tests/wait_scale_fixture.rs`: 7,305 tasks, 14,986 executions, 14,898 runs) grown to the payload sizes that make a walk expensive: a third of the descriptions at 9 KiB, 3 KiB PR bodies, one proposal per execution with 6 KiB review payloads. For the engine measurements the same rows were loaded into an isolated engine's database together with 400,000 GitHub call rows, 120,000 task-field ledger rows and 2,800 review-guide versions, for a 371 MiB file. It is synthetic. It reproduces row counts and payload sizes, not production's revision topology or write pattern.

### Measuring an engine

```sh
env -u BOSS_EVENTS_SOCKET BOSS_WORKER_POOL_SIZE=0 BOSS_WORK_DB_HEAVY_READ_PAGES=200 \
  bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-test-<id>.sock
```

Place a copy of the database to measure at `/tmp/boss-test-<id>.db` first. The engine derives that path from the socket path and never touches production state. Read `work DB heavy read` and `db read summary` from `/tmp/boss-test-<id>.engine-trace.jsonl`. Never point this at the live database.

## Reads that still walk

Measured on the same fixture after the fix. None of these runs more often than every five minutes.

| Read                                                                      | Timer  | Pages per pass |
| ------------------------------------------------------------------------- | ------ | -------------: |
| Preserved-work cleanup (`preserved_cleanup.rs`, two reads)                | 300 s  |     8,700 each |
| Positioning-hold recheck (`pre_start_failure.rs`)                         | 300 s  |          7,640 |
| PR binding repair (`pr_binding.rs`)                                       | 600 s  |          9,960 |
| Execution retention (`execution_retention.rs`)                            | 3600 s |         57,600 |
| Review-guide source retention (`review_guide_source_retention.rs`)        | 3600 s |         49,700 |
| Driver home retention, three sweeps (`executions_runs.rs`, `run_rows.rs`) | 3600 s |    15,300 each |
| Backup snapshot (`VACUUM INTO`)                                           | 3600 s | the whole file |

The backup reads the whole file by design. The review-guide retention pass is the one to look at next: its delete stepped 5.7 million rows in a nested loop over 2,800 guide versions, so it grows with the square of the guide history.

## Size of the file

Three facts from the code, and one measurement the engine now takes for itself:

- `auto_vacuum` is off and nothing runs `VACUUM` on the live database. Pages freed by a delete go on the freelist and are reused by later inserts; the file never shrinks. `VACUUM INTO` writes backups compacted, which does not change the live file.
- Rows are pruned from `github_api_calls` (14 days), never-started terminal `work_executions` and their dependents, review-guide sources, settled `host_commands`, `spoke_ingress` and `board_changes`.
- Nothing prunes `task_status_audit`, `worker_proposals`, started `work_executions` and their `work_runs`, `work_attention_items`, `comment_thread_entries`, `editorial_actions`, `planner_runs`, `automation_runs`, `progression_actions` or the `spoke_effects` and `spoke_applied_writes` ledgers.
- Which of those accounts for a given file is what `database size report` answers: it lists the largest tables and indexes by bytes, measured from a backup snapshot so the walk never touches the live database, with the live file's page and free-page counts next to it.

Retention for the unpruned tables is a separate decision. It deletes history people read, so it needs a retention design per table and the size report from a real database to say which tables are worth one.
