# CLI read bursts and engine admission

The October 2026 incident has a reproducible write-contention path even though the triggering command is an inspection command. `bossctl review batches` opens `WorkDb` directly; it does not send a review RPC. Previously, that called `WorkDb::open`, which runs the full migration chain for every existing database. The chain includes writes, including the schema-version upsert. Eight CLI processes therefore competed for SQLite's writer lock before doing their reads.

The engine's ordinary `WorkDb` clones share one `Mutex<Connection>`. An engine statement waiting for an external writer retains that mutex throughout SQLite's five-second busy timeout. Other engine database operations then wait for the Rust mutex, including internal work and worker proposals. Synchronous frontend handlers could also occupy async runtime threads while waiting. This explains a low-CPU engine that recovers when the external callers stop; it does not require exhaustion of the blocking pool. The historical sample alone cannot identify the exact statement that held the external writer lock.

The review queries themselves use autocommit SELECTs. Resolving the cycle root, loading batches, and loading each batch's members release their connection guards separately. There is one member query per batch, but no surrounding read transaction or lock spanning the entire loop. Those queries do not issue `BEGIN IMMEDIATE`. Long-lived read snapshots blocking WAL checkpoints are therefore not needed to reproduce this failure. The initialization path's writes, rather than the N+1 member query shape, are the demonstrated source of writer contention.

## Changes

Review and proposal CLI inspection opens an existing database with SQLite's read-only flag, without initialization, migration, directory creation, or journal-mode changes. Missing databases or incompatible schemas produce ordinary errors; inspection never upgrades them. All other direct CLI opens also skip migrations, retaining write capability where required. Engine startup remains responsible for schema upgrades. These commands retain offline inspection support and do not require a running engine.

Frontend bulk queries receive independent connections after admission, so their SQL does not hold the engine writer's connection mutex. These connections skip migrations and journal-mode changes. They retain write capability because some Get requests explicitly refresh persisted state; proposal inspection additionally uses an enforced read-only connection. In-memory test databases retain their existing shared anchor.

Bulk read handlers execute on the blocking pool, with their admission permits held until the handler finishes. Cancellation of a client cannot release capacity while blocking SQL is still executing. The socket reader stays available for live work, and responses retain their request IDs. Writes remain ordered within their connection. Internal engine work, `SubmitProposal`, and the live-status requests used by `agents list` bypass the bulk lane. Status reads use a separate bounded lane with reserved capacity. This is active on deploy; no feature flag is required.

## Limits

Environment settings are read when engine state is constructed; restart the engine after changing them.

The defaults are deliberately very high pending measurement of normal traffic: the first shipped values (32 active globally, 500 ms wait) rejected the macOS app's own startup reads after a cold engine start. Lower them only from measured data, with a generous buffer above the observed peak.

| Setting                             | Default | Meaning                                                |
| ----------------------------------- | ------- | ------------------------------------------------------ |
| `BOSS_RPC_READ_CONCURRENCY`         | 4096    | Active bulk handlers across connections                |
| `BOSS_RPC_READ_PER_CONNECTION`      | 4096    | Active bulk handlers on one connection                 |
| `BOSS_RPC_READ_QUEUE`               | 100000  | Pending reads across connections                       |
| `BOSS_RPC_READ_WAIT_MS`             | 60000   | Bulk admission wait after the ordering turn is granted |
| `BOSS_RPC_LIVE_READ_CONCURRENCY`    | 1024    | Active status reads, reserved above the bulk limit     |
| `BOSS_RPC_LIVE_READ_PER_CONNECTION` | 1024    | Active status reads on one connection                  |
| `BOSS_RPC_LIVE_READ_QUEUE`          | 100000  | Pending status reads across connections                |
| `BOSS_RPC_LIVE_READ_WAIT_MS`        | 60000   | Live admission wait after the ordering turn is granted |

Each connection also has a pending queue of four times its active limit. Full queues reject immediately. Waiting for the ordering turn has a separate, fixed 60 s deadline (ORDER_WAIT). After that turn is granted, excess requests queue on FIFO admission semaphores and share one configured deadline across the connection and global permits. A connection obtains its local permit before joining the global semaphore, limiting its representation in the global queue. Invalid settings use the default via the shared environment parser; zero or values above one million also log a warning. Multiple separate CLI connections are constrained by the global limit; this is connection fairness, not authenticated per-user accounting.

Per-connection active settings use the same validation as global settings, with no additional clamp. Bulk and live wait settings are independent and both default to 60 s. Outbound-queue headroom needs separate measurement of normal traffic before limits can be sized with a generous buffer; assumed reply counts do not justify lowering admission limits.

Rejections produce a correlated `Error` response distinguishing a full admission queue, an admission deadline, and an ordering deadline behind an earlier read. The macOS app passes this message through to its Work Error alert. No retry loop is added to the client. Live status has an independent path rather than consuming the bulk quota.

## Background database contention and the October 14 report

The `boss-v1.0.747` release tag points to `ae3055fd04bb`. In that version, `acquire_ordered` returned the same error for both its ordering timeout and its permit timeout, and the socket dispatcher logged both as `admission deadline expired`. Thus the two supplied log lines do not establish exhausted permits, or even a permit wait: an earlier identical read could have held the ordering turn. The app does not translate this error; `ChatViewModel+EventHandling.swift` assigns the engine's message to `workErrorMessage`.

One reproducible starvation path is the shared `WorkDb` connection mutex. Background sweeps synchronously acquire it on Tokio runtime workers. With a slow holder, concurrent waiters can occupy the workers needed to schedule admission, timers and socket IO even when read permits are available. The contended acquisition now uses `block_in_place` on the production multithread runtime, handing the runtime worker core back before waiting. The mutex still serializes SQL, and neither admission capacity nor deadlines change. Synchronous callers and current-thread test runtimes retain synchronous acquisition. Waits and holds of at least 100 ms log their duration and calling source location.

`background_database_contention_keeps_read_admission_running` holds the real database connection on a separate thread, starts a background database operation on a one-worker runtime, and verifies an independently scheduled read can obtain its ordering turn and admission while the database remains locked. The holder has an independent wall-clock escape so regressions cannot hang the test. Temporarily restoring the old synchronous acquisition made the regression fail after that five-second escape: even the runtime timer could not progress. With the handoff in place, admission completes before the holder is released.

Admission diagnostics retain session/request IDs, verb, stage, an opaque ordering-key identity, an opaque connection identity and age until the read's permits leave the blocking handler. A rejection logs the rejected read itself plus up to sixteen reads that actually held what it waited for, selected before the bound is applied: for an ordering timeout, older reads on the same connection with the same ordering key; for a connection-permit timeout, that connection's permit holders; for an engine-permit timeout, engine-wide permit holders; for queue-full, the offending connection's reads. Older unrelated long-running reads therefore cannot crowd the real blocker out (`blockers_name_the_relevant_reads_despite_many_older_unrelated_reads`). Entries are copied under the lock and formatted after releasing it, and holder dumps are limited to one per second (suppressed dumps are counted in the next one) so a queue-full flood adds neither contention nor log volume. Request payloads are never logged.

The SSH hypothesis is not established by the supplied timing. `open_control_master` awaits a Tokio subprocess with a timeout; the adapter provider holds its async cache mutex during that operation, but admission does not use that mutex. Failed adapter creation is not cached, so subsequent leases and passes can repeat DNS failures. Heartbeat cleanup intentionally requires evidence that the worker is dead (or its host disabled), rather than treating network failure as proof. Old retained remote leases can therefore continue being heartbeated while unreachable. No leases are deleted by this change.

**The incident's root cause remains open.** The `block_in_place` handoff is hardening against a hypothesised path, not a proven fix.

Timing argument: a 60 s ordering or admission rejection needs either an identical earlier read holding its turn for about a minute, or the app connection's tasks unable to run for about a minute. The measured starvation path does not reach that: the heartbeat and SSH bursts that take the DB mutex last under half a second, and 1024 (live) / 4096 (bulk) permits exist, so permit exhaustion by routine traffic is implausible. A DB holder would have to stay slow for the whole minute, which the supplied logs cannot show. The mutex fix is a valid improvement (one worker per contended caller was pinned), but it is not demonstrated to explain a minute-long stall.

Synchronous waits on runtime workers actually checked: the shared `WorkDb` connection mutex (addressed above) and the SSH adapter provider's async cache mutex (not used by admission). Leaked permits on the app connection are ruled out by construction: permits are held by `ReadPermits` inside the blocking job and released on drop, and diagnostics entries leave with them. Blocking tmux, cube and sweep work on runtime workers has **not** been audited, so it remains a live suspect.

The new holder logs (rejected read plus the reads that held its ordering turn or permits) and the DB lock wait/hold timing with caller location are what will identify the cause on recurrence. The coordinator-reset flow is unchanged.

## Regression evidence

`work::read_only_tests` holds a WAL writer transaction and shows that the old initialization path fails with a database-lock error, while the read-only opener and review query complete within one second. It also verifies that the inspection handle cannot write or create a missing database.

`app::read_admission::burst_tests` runs eight and 32 simultaneous loops through the direct review-batch query path and real socket proposal reads, using a populated review batch. After every reader has completed a round, the test submits an attributed worker proposal and calls both RPCs used by `agents list`, asserting a two-second bound for the write and for the combined status calls. It prints measured latencies. Another socket test deterministically occupies every bulk permit and checks that an excess read returns the explicit busy error within two seconds while live status still succeeds, including when pipelined behind a queued read on the same connection. Admission unit tests cover per-connection isolation, global capacity, bounded queues, deadlines, and permit reclamation.

## Sizing for the macOS app

The app sends every UI read over one long-lived socket. On connect it pipelines roughly a dozen bulk reads (products, settings, work tree, attention groups and items, deferred scopes, planner runs, engine attempts, disabled live-status slots) plus comment reads per open viewer, and invalidation refetch adds one `ListExecutions` per visible history. A per-connection budget of 4 active reads with a 250 ms deadline would reject reads 5 and onward whenever a slow `GetWorkTree` holds its slot, and the app turns a busy reply into a generic error with no retry. The first shipped defaults (16 active, 64 pending and 500 ms per connection; 32 active and 128 queued globally) still rejected the app's startup reads after a cold engine start, so the defaults are now far higher (see Limits); the global cap remains the bound on a runaway CLI burst (each CLI process holds one socket and one request). `one_socket_pipelining_sixteen_bulk_reads_never_sees_busy` and `default_budget_admits_app_session_fan_out_behind_a_slow_read` pin this.

`GetPrStatus` with `refresh: true` awaits a GitHub probe and is not a bulk read; it is bounded by its per-execution refresh budget so network latency cannot occupy bulk slots.

## Outbound reply delivery

Busy and other correlated replies share the session's bounded outbound lane. Only uncorrelated pushes can be evicted; pressure that would evict a reply triggers an explicit disconnect. The request reader pauses with headroom derived from the configured active and pending bulk and live limits, reducing pressure before the enqueue-boundary guarantee is needed. The reader's reserve is capped at half the outbound queue (128 of 256 entries), so it pauses only when the lane is genuinely near full. The default limits imply far more outstanding replies than the outbound queue can hold, so the cap is what applies by default; the reserve then does not cover every in-flight reply, and the explicit-disconnect guard at enqueue is what protects correlated replies. Read admission limits are not clamped.

Design-doc foreground reads deliberately retain bulk admission through GitHub fetches and the correlated response, including cache misses and refreshes. Unlike PR-status refresh, they have no separate foreground budget. Background document revalidation coalesces by document before spawning and permits at most four concurrent probe/retry ladders across keys.

## Read ordering and lane classification

Every frontend verb is classified by an exhaustive match; adding a verb requires choosing Bulk, Live or Ordered. Operator-question lists and coordinator-guidance reads use Bulk. Trunk status probes are awaited within Bulk admission. Guidance fetches (up to 45 s per product, fetched concurrently) await completion and send their correlated response while retaining bulk permits and the ordering turn. An additional global limit allows four guidance handlers; saturation replies with a dedicated "coordinator guidance fetch limit reached" error (and a warning log), not read admission's text. GitHub authorization rechecks remain detached and bounded to one recheck. The Live lane is reserved for local status reads.

Admitted reads targeting the same response view on one connection reserve FIFO turns before dispatch; reads of different keys never wait on each other for ordering, and turns for completed keys are not retained. Design-doc listings key by product, excluding the refresh option; quota snapshots similarly exclude refresh. Other filters identify the requested result set and remain in the key. The turn is held through the correlated response and any blocking work; later snapshots of the same view therefore cannot overtake earlier ones. Waiting for a turn has its own 60 s deadline, separate from the lane's admission deadline (which starts once the turn is granted), and does not consume an active engine permit. Queue-full and deadline errors may overtake successful responses, but carry no snapshot to overwrite UI state.
