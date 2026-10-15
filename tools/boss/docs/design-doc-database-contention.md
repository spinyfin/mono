# Design-document refresh database contention

## Source and amplification

Before this fix, `ChatViewModel.applyWorkTree` called `refreshDesignDocStates`
for every board snapshot. That method sent one `ResolveProjectDesignDoc` per
project with a document pointer. Thus 61 eligible projects produced 61 RPCs.
Each handler independently called `list_in_flight_executions`, including all
execution columns, to derive the same repository-to-workspace map.

Board snapshots arrive at connection, selection, manual refresh, item refetch,
and debounced work invalidation. More worker state transitions and failed
dispatch/recovery bookkeeping create more invalidations; each new snapshot
multiplied that activity by the number of projects. The code establishes this
amplification mechanism, not the exact mix of events in the reported production
minute. No production database was opened, so its historical row count and the
precise source of each old unattributed warning remain unknown.

`DesignDocBatchRefreshTests` exercises 61 projects and 15 board refreshes in a
minute. The new path sends one batch, refreshes changed pointers immediately,
and refreshes unchanged workspace hints at most once per minute. Reconnection
clears the attempt timestamps. The engine computes the map once per batch.

## Query and connection lifetime

The isolated database regression seeds 50,000 terminal execution rows with
2 KiB payloads and runs 61 simultaneous original scans without the new index.
The query-plan regression verifies a table scan and temporary ordering tree
without the index, versus `work_executions_in_flight_idx` and no temporary
ordering tree with it. The partial index excludes terminal and unleased rows;
its ordered keys also cover the new two-column repository/workspace lookup.
The tests print measured timings rather than assuming a hardware-specific
one-second duration. The original scan held its connection through SQL and
row conversion; it did not run document resolution inside that same guard.
The separate resolver now explicitly releases its guard before URL rendering
and the workspace lookup callback.

On the isolated macOS run on 2026-10-15, the unindexed 61-way scan had a
6,698 ms median and 6,961 ms maximum. With the index, 61 readers each doing
five resolutions alongside 100 writes completed in 156 ms; every write
succeeded with a zero busy timeout. These are synthetic workload measurements,
not measurements of production state. The socket integration separately sends
61 concurrent legacy RPCs plus a batch against an isolated engine while an
independent writer commits 100 updates.

Board snapshots independently used the configured immediate transaction
default, unnecessarily reserving the WAL writer for the entire projection.
They now use a deferred, consistent read snapshot. A regression holds an
independent writer, proves `BEGIN IMMEDIATE` fails, and verifies the board is
still readable.

## Other long holds and diagnostics

Attention reconciliation ran a sequence of updates under one guard. Its
20-second total could include multiple SQL waits and scans; a five-second
busy timeout is per SQLite operation, not per guard. It now releases the guard
between kinds and avoids updates for kinds without open candidates. The
reported semantic-progress, attention-resolution, PR-state, and coordinator
liveness writers perform SQL under their guards, not subprocess or network
work. Their roughly 5.2-second holds are consistent with the existing busy
timeout plus execution overhead; old hold-only logs cannot prove that split.

Task-local RPC/session/request or background attribution, SQLite call time,
and actual write-transaction ownership now make that distinction observable.
BUSY diagnostics count every failure while rate-limiting holder dumps per
caller. The rusqlite patch observes prepare/step/reset/finalize boundaries:
ordinary statement tracing includes time between row steps and cannot measure
non-SQL work separately. Existing WorkDb acquisition and metrics are reused;
no timeout, admission limit, retry, or error severity changes are involved.

Pre-start typed SQLITE_BUSY failures are marked as engine database failures.
All churn queries exclude those rows without resetting or hiding genuine
worker failure streaks. Historical migration only classifies matching failed
pre-start rows with no worker start timestamp.
