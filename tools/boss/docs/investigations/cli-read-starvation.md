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

| Setting                             | Default | Meaning                                             |
| ----------------------------------- | ------- | --------------------------------------------------- |
| `BOSS_RPC_READ_CONCURRENCY`         | 32      | Active bulk handlers across connections             |
| `BOSS_RPC_READ_PER_CONNECTION`      | 16      | Active bulk handlers on one connection              |
| `BOSS_RPC_READ_QUEUE`               | 128     | Pending reads across connections                    |
| `BOSS_RPC_READ_WAIT_MS`             | 500     | Total admission wait across both concurrency limits |
| `BOSS_RPC_LIVE_READ_CONCURRENCY`    | 8       | Active status reads, reserved above the bulk limit  |
| `BOSS_RPC_LIVE_READ_PER_CONNECTION` | 2       | Active status reads on one connection               |
| `BOSS_RPC_LIVE_READ_QUEUE`          | 32      | Pending status reads across connections             |

Each connection also has a pending queue of four times its active limit. Full queues reject immediately. Other excess requests queue on FIFO semaphores and expire at a single deadline. A connection obtains its local permit before joining the global semaphore, limiting its representation in the global queue. Invalid settings use the default via the shared environment parser; zero or values above one million also log a warning. Multiple separate CLI connections are constrained by the global limit; this is connection fairness, not authenticated per-user accounting.

Rejections produce a correlated `Error` response containing `engine busy, retry: read admission limit exceeded` and a warning identifying the session and whether its queue was full or its deadline expired. No retry loop is added to the CLI. Live status has an independent path rather than consuming the bulk quota.

## Regression evidence

`work::read_only_tests` holds a WAL writer transaction and shows that the old initialization path fails with a database-lock error, while the read-only opener and review query complete within one second. It also verifies that the inspection handle cannot write or create a missing database.

`app::read_admission::burst_tests` runs eight and 32 simultaneous loops through the direct review-batch query path and real socket proposal reads, using a populated review batch. After every reader has completed a round, the test submits an attributed worker proposal and calls both RPCs used by `agents list`, asserting a two-second bound for the write and for the combined status calls. It prints measured latencies. Another socket test deterministically occupies every bulk permit and checks that an excess read returns the explicit busy error within two seconds while live status still succeeds, including when pipelined behind a queued read on the same connection. Admission unit tests cover per-connection isolation, global capacity, bounded queues, deadlines, and permit reclamation.

## Sizing for the macOS app

The app sends every UI read over one long-lived socket. On connect it pipelines roughly a dozen bulk reads (products, settings, work tree, attention groups and items, deferred scopes, planner runs, engine attempts, disabled live-status slots) plus comment reads per open viewer, and invalidation refetch adds one `ListExecutions` per visible history. The original per-connection budget of 4 active reads and a 250 ms deadline rejected reads 5 and onward whenever a slow `GetWorkTree` held its slot, and the app turns the busy reply into a generic error event with no retry. The defaults are now 16 active, 64 pending and 500 ms per connection, so one app session's connect and refetch fan-out is admitted outright. The incident is still bounded by the global cap of 32 active and 128 queued reads, which is what protects the engine from the eight-process CLI burst (each CLI process holds one socket and one request). `one_socket_pipelining_sixteen_bulk_reads_never_sees_busy` and `default_budget_admits_app_session_fan_out_behind_a_slow_read` pin this.

`GetPrStatus` with `refresh: true` awaits a GitHub probe and is not a bulk read; it is bounded by its per-execution refresh budget so network latency cannot occupy bulk slots. Design-document reads await their response inside the admission permits; only the follow-up revalidation is detached.

## Outbound reply delivery

Busy and other correlated replies share the session's bounded outbound lane, which evicts its oldest entry under pressure. The request reader now pauses (`wait_for_response_headroom`) while that lane is half full, so a client that pipelines faster than it reads backs up in its own socket buffer rather than losing replies.
