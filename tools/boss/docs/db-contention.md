# SQLite contention: absorbed, logged, reported as engine health

The engine never returns transient SQLite lock contention to a client. A
"database is locked" result is something the user can do nothing about,
so the macOS app's `Work Error` modal and the CLI's exit 6 for it were
wrong by construction. This document is the operator view of what
replaced them.

## What happens instead

- **Classification** lives in one place,
  `tools/boss/engine/work-db/src/contention.rs`:
  `SQLITE_BUSY` and `SQLITE_LOCKED`, with every extended code
  (`BUSY_RECOVERY`, `BUSY_SNAPSHOT`, `BUSY_TIMEOUT`,
  `LOCKED_SHAREDCACHE`, …), are transient contention. Everything else is
  a real database failure and still fails loudly.
- **Every connection the engine opens** (its writer, per-query read
  connections, bulk readers, the backup's independent connection)
  carries a SQLite busy handler instead of a fixed `busy_timeout`. The
  handler retries the lock acquisition with SQLite's own backoff schedule
  (1 ms … 100 ms) and does not give up. Because it runs inside the
  statement that could not take the lock, the enclosing transaction is
  never rolled back and re-run from outside: for `BEGIN IMMEDIATE` and
  autocommit statements the lock is taken before any row is touched, so a
  retried write is applied exactly once and the duplicate-create guard
  sees a consistent snapshot.
- **`bossctl` direct-DB connections** do not absorb: they wait a bounded
  time (250 ms for read-only inspection, 5 s for commands that write) and
  then fail with SQLite's own error, so an operator tool never hangs on a
  stuck lock.
- **Reads** are also coalesced per session
  (`tools/boss/engine/app/src/app/read_coalesce.rs`): identical requests
  pipelined on one socket cost one handler run, and the followers are
  answered from the leader's responses under their own request ids. A
  follower only joins a leader whose snapshot is not taken yet (it is
  still queued for admission, or blocked on a SQLite lock), so an answer
  is never older than the request that received it.
- **`GetWorkTree`** is not coalesced that way, because each response is
  encoded against its session's delta cache. Its database load is shared
  engine-wide instead (`work_tree_loads`, on the `boss_single_flight`
  crate): requests for one product share one in-flight tree scan across
  sessions for as long as nothing has been committed since that scan
  began, which includes the whole time it is blocked on a lock. A failed
  load answers every request that joined it.
- **Read admission** (`tools/boss/engine/app/src/app/read_admission.rs`)
  still bounds concurrency
  and queue depth, but it never rejects: a read that finds its queue full
  or every permit taken waits, and the reader loop stops taking requests
  off that socket until a slot frees.

## What to look for in the engine log

Every lock wait is logged at `WARN`, with the caller that holds the
connection, the frontend request kind being served (`internal` for
sweeps and pollers), the attempt count and the total wait:

```
work DB contention: SQLite lock wait cleared caller=…/work-db/src/create_entities.rs:231 request=CreateChore attempts=7 waited_ms=312
```

A wait still in flight logs `work DB contention: still waiting for the
SQLite lock` after one second and every five seconds after that, with
the same fields, so a lock that never clears keeps logging.

If contention ever reaches a client response anyway (it should not), the
response funnel logs at `ERROR`:
`transient SQLite contention reached a client response; the busy handler
did not absorb it`. The error text is passed through unchanged.

Slow admission waits log `read admission holders` with `reason` set to
either `read waiting: admission queue full` or
`read waiting: admission permits taken`, plus
the reads holding the waiter up. The `BOSS_RPC_*_READ_WAIT_MS` settings
now set that slow-wait log threshold; they were deadlines before.

## Engine health

A lock wait longer than five seconds — the `busy_timeout` the engine
previously ran with, so the point at which contention was already
user-visible — is an engine-health issue while it lasts:
`GetEngineHealth` reports `database_contention` (severity `error`) naming
each waiting operation, its caller, attempt count and wait so far, plus
lifetime counters. The issue clears on its own once the lock is released
and the waits complete. There is no retry cap: nothing is lost, the
operations complete once the holder lets go, and the health issue is how
a stuck holder becomes visible.

The usual stuck holder is another process with the database open for
writing (a direct-DB script, an interrupted backup). Find it with
`lsof <state.db>`.
