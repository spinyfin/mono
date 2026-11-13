# Spoke observation projection

A spoke journals what its workers do and sends it to the hub. The hub **receives** a record, which is durable and acknowledged, and separately **applies** it, which is what changes work state. This page is how the second half works, what makes an effect happen exactly once, and how to follow one record through it. Design context: [hub-and-spoke-distributed-engine](designs/hub-and-spoke-distributed-engine.md), "Journal, acknowledgement and crash recovery".

Nothing here runs until a hub listener is configured and a spoke host is enrolled and `spoke_ready` ([listener setup](spoke-hub-listener.md)). With no listener the projector is never started.

## Receipt and application are separate

| Step         | Where                                                                       | Moves                      |
| ------------ | --------------------------------------------------------------------------- | -------------------------- |
| Receipt      | stream manager sink, `WorkDb::commit_spoke_record`                          | `spoke_journals.received`  |
| Application  | `boss_spoke_hub::projection::Projector`, `WorkDb::advance_spoke_projection` | `spoke_projection.applied` |
| Owed effects | `spoke_effects`, settled by `WorkDb::settle_spoke_effect`                   | each effect's own state    |

The sink stores the original frame bytes and advances the receipt in one transaction, then acknowledges. It never waits for application. After a crash the spoke does not resend anything: the hub finds `received > applied` and applies the stored frames itself. `received - applied` is the projection lag (`WorkDb::spoke_projection_status`).

## What a record does

`projection::reduce::reduce` is a pure function from a stored frame to observations. It fails closed: a frame that does not decode, fails validation, has the wrong sequence, or carries an event this build cannot read is **quarantined** (a `failed` `quarantine` effect is stored and never retried, and the cursor moves on). It is never mapped to a success.

| Record                                                                                                                | Becomes                    | Applied by                                                                                                                                                                                                                                                                                                                      |
| --------------------------------------------------------------------------------------------------------------------- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `WorkerEvent`                                                                                                         | a hook event of the run    | `record_spoke_turn_event` first (spoke turn map, durably, before the fan-out), then `dispatch_worker_event_fanout`, the same fan-out a hook on the local events socket takes (live state, completion on `Stop`, probes)                                                                                                         |
| `Proposal` (`proposal-json/1`)                                                                                        | a queued `propose` call    | `proposals::submit_for_caller`, the same validators, apply pipeline and run-done finalization as `SubmitProposal`; yields a **decision**                                                                                                                                                                                        |
| `Proposal` (`guide-outcome/1`, `answer-reply/1`, `conflict-producer/1`, `conflict-mark-failed/1`, `ci-mark-failed/1`) | a run-scoped write         | the same transport-independent write functions (`app/run_writes.rs`) the local RPC handlers call, so validation, comment invalidations, review-guide regeneration notices, failure broadcasts and metrics are identical; the result row commits in the same transaction as the write, so replay of an applied write is accepted |
| `Proposal` (any other contract)                                                                                       | a refusal                  | a `Rejected` decision naming the contract; nothing is applied                                                                                                                                                                                                                                                                   |
| `PaneState`, inventory page                                                                                           | a pane observation per run | durable `spoke_pane_observations` row, then live state: `Exited` marks the slot terminated, `Missing` shows it exited without fencing, `Alive` revives it, `Indeterminate` does nothing                                                                                                                                         |
| `CommandResult`                                                                                                       | nothing                    | settled at ingress with its receipt                                                                                                                                                                                                                                                                                             |
| `PaneCapture`                                                                                                         | an empty tombstone         | stored without quota so the sequence stays contiguous; the projector skips it as live-only and keeps applying later records                                                                                                                                                                                                     |
| `TranscriptDelta`, `ArtifactChunk`, `Gap`                                                                             | a custody write            | `boss_spoke_custody`: the bytes are written at their own offset and fsynced, then the ledger row moves; see [transcript and artifact custody](#transcript-and-artifact-custody). A resource the hub cannot trust is halted and stored as a failed `custody_halt` effect                                                         |
| capacity, worker request                                                                                              | nothing yet                | owned by other consumers                                                                                                                                                                                                                                                                                                        |

A pane observation is evidence only. Whether absence or an exit changes a task's status is still decided by the engine's existing liveness policy.

Records of an assignment that a newer attempt superseded are dropped, so an old attempt's evidence never touches the new one. Its queued proposals are answered with a refusal so the old worker's receipt resolves.

## Why an effect happens once

1. **Ingress** stores a frame once per `(host, journal, sequence)`; a duplicate delivery changes nothing.
2. **The cursor** moves one record at a time, in order, and only in the transaction that records the effects the record owes. A crash before that commit replays the record. A crash after it never sees the record again.
3. **Effects** have a `(host, kind, key)` identity. For a proposal the key is `run_id:proposal_id`: two records describing the same proposal yield one decision, and once a decision exists a later record applies nothing.
4. **The applier** is idempotent by the engine's own keys. A proposal is deduplicated by its idempotency key, and run-done finalization re-checks liveness before each write. The one window a crash opens, after application and before the cursor commit, therefore repeats work that is already a no-op. A transient failure of that commit (`SQLITE_BUSY`) retries only the commit; it does not re-run observations.

Hook events are the exception that is at-least-once: a crash in that window repeats one dispatch. The live-state reducer and completion paths already tolerate a repeated event, as they do a shim retry. Hook events are stamped with a fresh `ingress_seq` at application time and carry no transcript path, since the host's path is never opened on the hub.

## Ordering and holding the cursor

A host's records are applied strictly in order, so a worker's queued `done` is applied before the `Stop` behind it, as the synchronous local call would have been. A record that cannot be applied yet (database busy) holds that host's cursor and is retried with backoff; it is never skipped. Different hosts project concurrently.

Work that waits on something other than the database is a durable **effect** and does not hold the cursor. The one such effect is delivering a proposal's decision, which needs the spoke reachable.

## Decisions

The decision for a queued proposal is stored in `spoke_effects` as the exact `ProposalDecision` command, because the spoke applies a decision by compare-and-swap and a changed decision is refused. The projector delivers it over the live stream:

- host unreachable, or outcome uncertain: stays pending, retried every few seconds with the same command id;
- spoke answers `Ok`: settled;
- spoke refuses: retried under a fresh id (`<id>-r<n>`) up to three times, then stored as a failed effect and never retried.

Finalization that follows an accepted proposal (tearing down a reporting review member, finalizing a review guide or a declared run-done) is tied to the decision, not to one delivery attempt. A reporting member is finalized only once the decision is terminal (delivered, or given up), never after a transient miss. Guide and run-done finalization are idempotent and do not need the ack, so they run on the first pass even while the spoke is unreachable. The obligation is not kept only in memory: when a decision settles in a process that no longer holds the submission (the hub restarted), it is rebuilt from the stored proposal row. Finalization is spawned off the delivery pass, so a slow GitHub-backed finalize never delays delivery for other hosts.

A decision is `Ok` with a bounded summary (hub proposal id, state, applied reference, whether it was a replay), or `Rejected` with the typed submission error.

## Not covered

- Authorizing the worker gateway's online requests (`WorkerRequest` replies) is separate follow-on work; this projection does not touch them.
- `spoke_ingress` retains frames until a decision about their retention window is made. Capture tombstones (empty frames) are deleted once the projection cursor passes them. Custody was the last consumer that needed the transcript and artifact frames, so the retained ones can go once the cursor passes them; the window for that is a separate decision.
- A turn-ending `Stop` (and a queued `done` proposal) carries the transcript size the spoke saw. The projector parks that observation as a pending outbox item and advances the host cursor so later records — including the bulk bytes behind the boundary, and other runs' events — keep applying. The parked item is applied once custody holds those bytes and every output the spoke named at the boundary, or when the spoke declares them unavailable, or when `watermark_wait` runs out; a `Stop` then carries the hub's own copy of the transcript, and when the bytes are unavailable Stop-time consumers see unavailable evidence and custody records why ([custody](#transcript-and-artifact-custody)). Later observations of the same run stay parked until that completion applies, so per-run order is preserved. Replayed hook events stamp live-state `last_event_at` and the semantic-progress checkpoint (the in-memory registry and `work_runs.semantic_progress_at`, which the stale-worker and hung-worker sweeps read) from the record's observed wall time, clamped so a spoke clock ahead of the hub cannot pin the monotonic checkpoint in the future, and that checkpoint never moves backwards, so a worker that died during an outage is not treated as freshly active when its backlog drains. Unretained pane-capture frames are stored as empty tombstones so they do not leave sequence holes.

## Diagnosing

- Lag: `WorkDb::spoke_projection_status()`; `received` vs `applied`, `pending_effects`, `failed_effects`, `halt_reason`, `stall_reason`. The same facts appear on `GetEngineHealth` as `spoke_projection_halted`, `spoke_projection_stalled`, and `spoke_projection_failed_effects` issues.
- Halted host: the log says `spoke observation projection halted` with the position, `halt_reason` is persisted, and the host is retried after a hold. A missing retained frame still halts; an unretained capture is an empty tombstone and does not. The upgrade backfill only tombstones holes above the prune point, which the previous schema could only produce for unretained captures.
- Stalled host: a record that keeps retrying past the stall bound persists `stall_reason` until it applies. A failed stall write is logged and retried on the next attempt.
- Quarantined frames and abandoned decisions are `spoke_effects` rows in state `failed` with `last_error`; they are never retried.
- Pane evidence: `WorkDb::spoke_pane_observation(execution_id)`.
- Parked remote workers: `spoke_turn_state` holds the latest main-session turn per assignment so a hub restart rebuilds the in-memory map. A restore that fails is retried on every projector sweep, not only when another hook arrives, and the map is published only once the whole set has been read. A hook's turn state is saved before its fan-out, so a persistence retry never re-dispatches the hook.

## Transcript and artifact custody

A spoke streams a run's transcript, the output files its worker registers (structured output, PR-url output, attachments) and the gaps in what it could read as records in its journal. The hub keeps those records as received (above); **custody** turns them into files the engine's existing readers can open, addressed by run identity and never by a path on the host. Design context: [hub-and-spoke-distributed-engine](designs/hub-and-spoke-distributed-engine.md), "Journal, acknowledgement and crash recovery" and the transcript and structured-output rows of the adapter contract.

Nothing here runs until a hub listener is configured and a spoke host is enrolled and `spoke_ready` ([listener setup](spoke-hub-listener.md)). With no Boss state root to hold the bytes, the projector logs that custody is off and leaves transcript and artifact records received but unapplied-to-disk, as before.

### Where the bytes live

| What                  | Where                                                                                                       |
| --------------------- | ----------------------------------------------------------------------------------------------------------- |
| Files                 | `<Boss state root>/executions/<execution>/spoke/<run>/` (the root `BOSS_WORKER_TRANSCRIPTS_ROOT` overrides) |
| Transcript generation | `transcript-<16 hex of sha256(resource)>.jsonl`, one file per spoke-reported generation                     |
| Artifact              | `artifact-<16 hex of sha256(resource)>.bin`, one file per registered artifact id                            |
| Ledger                | `spoke_custody` in the work database: one row per `(execution, assignment generation, resource)`            |

No name comes from the peer: the execution and run segments are checked against `[A-Za-z0-9._-]`, and the file name is a hash of the resource. Each row records its file's path relative to the root, which is what retention acts on.

### Writing is exact-offset and replay-safe

`boss_spoke_custody::write::extend` is the only writer. A chunk is applied at **its own offset**, never appended:

1. The chunk's bytes must match its `sha256`; otherwise the resource halts.
2. A chunk that starts past the ledger's `materialized` prefix is a **hole**: the bytes between cannot be supplied, so the resource halts. It is never filled in.
3. Bytes the hub already holds under the chunk are **compared** with it. A difference is a changed overlap and halts the resource; identical bytes are a replay and write nothing.
4. New bytes are written at `materialized`, the file is truncated to the new end (dropping anything a crashed earlier write left past it), fsynced, and only then does the ledger row move.

A crash after the fsync and before the row replays the chunk and rewrites the same bytes at the same offset. A crash after the row and before the projection cursor moves replays the record, which is then a verified replay. A file shorter than the ledger says (deleted or truncated underneath) halts the resource: the hub does not pretend to hold bytes it lost.

An artifact is complete only when `materialized` equals the declared `total_bytes` **and** the file hashes to the digest in its identity (`<kind>:<sha256 hex>`). A mismatch is `invalid`; `eof` before the total, a changed declared size or kind, and a size past the hub's limits (512 MiB per transcript, the attachment cap per artifact) halt it.

### Resource states and what a reader is told

| State         | Meaning                                                                                              | Reader sees                                   |
| ------------- | ---------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| `open`        | Bytes may still arrive                                                                               | `Pending` / `Incomplete`                      |
| `complete`    | An artifact whole and verified                                                                       | `Ready`                                       |
| `unavailable` | The spoke said the rest will not arrive (a `Gap`: replaced or vanished source, spool full)           | `Unavailable(reason)`; held bytes still count |
| `invalid`     | The worker's output is positively bad (not JSON, too large) or its bytes do not match their identity | `Invalid(reason)`                             |
| `halted`      | The hub cannot trust its own copy (hole, changed overlap, bad digest, shortened file)                | `Unavailable("the hub's copy was halted")`    |

"Not there yet", "will not come" and "is bad" stay three different answers all the way to the consumer. A gap is never turned into a complete transcript, and a transcript closed by a gap still satisfies a watermark inside the bytes it holds. A bare `transcript` gap (no generation) is how a spoke says it has no transcript to name.

### Watermarks: how a boundary waits without deadlocking

A `Stop` or a queued proposal carries `(transcript_generation, transcript_through)`: the size the transcript had when the boundary was recorded. The spoke numbers the boundary at once and the bulk bytes only as send credit allows, so the bytes can sit **behind** the boundary in the journal. Holding the host cursor until they arrived would deadlock on that watermark and would block other runs on the same host.

The projector therefore records the completion as a pending `pending_observation` outbox item and advances the cursor. Custody records that follow are applied independently. The parked item is applied when

- the mark is met and every artifact identity the spoke's `output:concluded` marker named is complete, invalid, or unavailable: a `Stop` is dispatched carrying the hub's copy (`IncomingHookEvent::with_transcript_source`), confined to the run's custody directory, so cost capture, the run's recorded transcript path and completion read it exactly as they read a local transcript. A named output with no row yet is pending, not settled;
- the spoke declared the bytes unavailable, or the resource is halted: the boundary proceeds without a copy; custody's row holds the reason;
- `watermark_wait` (default 60 s) passes with the bytes still missing: the boundary proceeds without a copy and a failed `transcript_evidence_unavailable` effect is stored, so the host counts against engine health.

Later observations of the same run stay parked until that completion applies, so live-state for that run stays in order. Other runs on the host are not held.

### Who reads what

| Consumer                                  | Routed by                                                                                                                                                                                                                                                                                                                                        |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Live summary, transcript-tail, cost       | `work_runs.transcript_path`, advanced monotonically when a newer generation attaches (`ObservationApplier::transcript_attached`); the spoke run's slot gets a summarizer whose containment root is the run's custody directory, which never infers death from a missing local tmux session or pid (a spoke's liveness is what the spoke reports) |
| Completion at `Stop`                      | The same path, carried on the event once the watermark is met                                                                                                                                                                                                                                                                                    |
| `HostAdapter::read_transcript_tail_bytes` | The run's custody file, by path ownership; a path that is not a recorded custody file is still refused rather than opened on the hub                                                                                                                                                                                                             |
| `HostAdapter::read_worker_log_tail`       | The newest `WORKER_LOG` artifact of the run, if the spoke sent one                                                                                                                                                                                                                                                                               |
| `HostAdapter::collect_structured_output`  | `Custody::collect_output`: `Collected` installs the verified file at the destination atomically; `NotAvailable` for absent, still arriving or unavailable; `Failed` only for positive evidence of a bad output                                                                                                                                   |

Which artifact is which output: `PR_URL` artifacts are the PR-url output; `REVIEW_RESULT` artifacts carry `output_kind` (the kind's slug) in their metadata and serve the other kinds. A newer valid artifact beats an older gap that called the output bad, and a newer gap beats an older artifact: the later record wins, as the file's latest content does for a local reader.

### Retention

The execution-retention sweep calls the custody reclaim (`Custody::reclaim_execution`) for every execution that holds a ledger row and is **terminal** and finished more than 14 days ago. Liveness is the execution's status, never a file's age, and an execution this database cannot place keeps its bytes. Only paths the ledger recorded are deleted (then empty directories they leave); reclaim never removes extra files a directory scan might find. Files go before rows, so a crash between leaves rows whose files are already gone and the next pass removes them.

### Not covered by custody

- `spoke_ingress` is still not pruned. Custody was the last consumer that needed the transcript and artifact frames, so they can now go once the cursor has passed them; the retention window for that is a separate decision.
- Hub-initiated `ReadArtifact` (asking a spoke for a bounded range of a registered artifact it holds) is not issued: everything custody holds arrived through the journal.

### Diagnosing custody

- A resource's state and reason: `WorkDb::spoke_custody_for_execution(execution_id)`; `materialized` against the file's length; `source_sequence` names the record that last changed it.
- A halted resource appears as a failed `custody_halt` effect (key `<run>:<resource>`) and counts in `spoke_projection_failed_effects` on `GetEngineHealth`. A boundary that went ahead because bytes never arrived is a failed `transcript_evidence_unavailable` effect. Neither is retried; the evidence is gone or distrusted, not pending.
- A host that stalls at a `Stop`: the log says `a completion boundary goes ahead without the transcript bytes it named`, or a `pending_observation` effect is still pending while the spoke is still sending. The host's cursor is not held.
