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
| capacity, transcript, artifact, gap, worker request                                                                   | nothing yet                | owned by other consumers                                                                                                                                                                                                                                                                                                        |

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
- Transcripts, artifacts and gaps are not projected here, so retained `spoke_ingress` frames are not pruned yet; whichever consumer is last to need a frame decides when it can go. Capture tombstones (empty frames) are deleted once the projection cursor passes them.
- A projected `Stop` carries no hub-local transcript path; Stop-time consumers that read a transcript see unavailable evidence. Replayed hook events stamp live-state `last_event_at` and the semantic-progress checkpoint (the in-memory registry and `work_runs.semantic_progress_at`, which the stale-worker and hung-worker sweeps read) from the record's observed wall time, clamped so a spoke clock ahead of the hub cannot pin the monotonic checkpoint in the future, and that checkpoint never moves backwards, so a worker that died during an outage is not treated as freshly active when its backlog drains. Unretained pane-capture frames are stored as empty tombstones so they do not leave sequence holes.

## Diagnosing

- Lag: `WorkDb::spoke_projection_status()`; `received` vs `applied`, `pending_effects`, `failed_effects`, `halt_reason`, `stall_reason`. The same facts appear on `GetEngineHealth` as `spoke_projection_halted`, `spoke_projection_stalled`, and `spoke_projection_failed_effects` issues.
- Halted host: the log says `spoke observation projection halted` with the position, `halt_reason` is persisted, and the host is retried after a hold. A missing retained frame still halts; an unretained capture is an empty tombstone and does not. The upgrade backfill only tombstones holes above the prune point, which the previous schema could only produce for unretained captures.
- Stalled host: a record that keeps retrying past the stall bound persists `stall_reason` until it applies. A failed stall write is logged and retried on the next attempt.
- Quarantined frames and abandoned decisions are `spoke_effects` rows in state `failed` with `last_error`; they are never retried.
- Pane evidence: `WorkDb::spoke_pane_observation(execution_id)`.
- Parked remote workers: `spoke_turn_state` holds the latest main-session turn per assignment so a hub restart rebuilds the in-memory map. A restore that fails is retried on every projector sweep, not only when another hook arrives, and the map is published only once the whole set has been read. A hook's turn state is saved before its fan-out, so a persistence retry never re-dispatches the hook.
