# SQLite contention: absorbed, logged, reported as engine health

The engine never returns transient SQLite lock contention to a client. A
"database is locked" result is something the user can do nothing about,
so the macOS app's `Work Error` modal and the CLI's exit 6 for it were
wrong by construction. This document is the operator view of what
replaced them.

## What happens instead

- **Classification** lives in one place, `work/contention.rs`:
  `SQLITE_BUSY` and `SQLITE_LOCKED`, with every extended code
  (`BUSY_RECOVERY`, `BUSY_SNAPSHOT`, `BUSY_TIMEOUT`,
  `LOCKED_SHAREDCACHE`, …), are transient contention. Everything else is
  a real database failure and still fails loudly.
- **Every engine-configured connection** (the engine's writer, per-query
  read connections, the CLI's direct read-only inspection connection)
  carries a SQLite busy handler instead of a fixed `busy_timeout`. The
  handler retries the lock acquisition with SQLite's own backoff schedule
  (1 ms … 100 ms) and does not give up. Because it runs inside the
  statement that could not take the lock, the enclosing transaction is
  never rolled back and re-run from outside: for `BEGIN IMMEDIATE` and
  autocommit statements the lock is taken before any row is touched, so a
  retried write is applied exactly once and the duplicate-create guard
  sees a consistent snapshot.
- **Reads** are also coalesced per session (`app/read_coalesce.rs`):
  identical requests pipelined on one socket cost one handler run, and the
  followers are answered from the leader's responses under their own
  request ids. A follower only joins a leader that has not started, so an
  answer is never older than the request that received it.
- **Read admission** (`app/read_admission.rs`) still bounds concurrency
  and queue depth, but it never rejects: a read that finds its queue full
  or every permit taken waits, and the reader loop stops taking requests
  off that socket until a slot frees.

## What to look for in the engine log

Every lock wait is logged at `WARN`, with the caller that holds the
connection, the frontend request kind being served (`internal` for
sweeps and pollers), the attempt count and the total wait:

```
work DB contention: SQLite lock wait cleared caller=…/work/create_entities.rs:231 request=CreateChore attempts=7 waited_ms=312
```

A wait still in flight logs `work DB contention: still waiting for the
SQLite lock` after one second and every five seconds after that, with
the same fields, so a lock that never clears keeps logging.

If contention ever reaches a client response anyway (it should not), the
response funnel logs at `ERROR`:
`transient SQLite contention reached a client response; the busy handler
did not absorb it`. The error text is passed through unchanged.

Slow admission waits log `read admission holders` with `reason` set to
one of `read waiting: admission queue full`, `read waiting: behind an
earlier identical read` or `read waiting: admission permits taken`, plus
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
