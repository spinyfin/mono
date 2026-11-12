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

`projection::reduce::reduce` is a pure function from a stored frame to observations. It fails closed: a frame that does not decode, fails validation, has the wrong sequence, or carries an event this build cannot read is **quarantined** (a `failed` `quarantine` effect for an operator, and the cursor moves on). It is never mapped to a success.

| Record                                              | Becomes                    | Applied by                                                                                                                                                                              |
| --------------------------------------------------- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `WorkerEvent`                                       | a hook event of the run    | `dispatch_worker_event_fanout`, the same fan-out a hook on the local events socket takes (live state, completion on `Stop`, probes, spoke turn map)                                     |
| `Proposal` (`proposal-json/1`)                      | a queued `propose` call    | `proposals::submit_for_caller`, the same validators, apply pipeline and run-done finalization as `SubmitProposal`; yields a **decision**                                                |
| `Proposal` (any other contract)                     | a refusal                  | a `Rejected` decision naming the contract; nothing is applied                                                                                                                           |
| `PaneState`, inventory page                         | a pane observation per run | durable `spoke_pane_observations` row, then live state: `Exited` marks the slot terminated, `Missing` shows it exited without fencing, `Alive` revives it, `Indeterminate` does nothing |
| `CommandResult`                                     | nothing                    | settled at ingress with its receipt                                                                                                                                                     |
| `PaneCapture`                                       | nothing                    | live display data, already fed to viewers by the stream manager                                                                                                                         |
| capacity, transcript, artifact, gap, worker request | nothing yet                | owned by other consumers                                                                                                                                                                |

A pane observation is evidence only. Whether absence or an exit changes a task's status is still decided by the engine's existing liveness policy.

Records of an assignment that a newer attempt superseded are dropped, so an old attempt's evidence never touches the new one. Its queued proposals are answered with a refusal so the old worker's receipt resolves.

## Why an effect happens once

1. **Ingress** stores a frame once per `(host, journal, sequence)`; a duplicate delivery changes nothing.
2. **The cursor** moves one record at a time, in order, and only in the transaction that records the effects the record owes. A crash before that commit replays the record. A crash after it never sees the record again.
3. **Effects** have a `(host, kind, key)` identity. For a proposal the key is `run_id:proposal_id`: two records describing the same proposal yield one decision, and once a decision exists a later record applies nothing.
4. **The applier** is idempotent by the engine's own keys. A proposal is deduplicated by its idempotency key, and run-done finalization re-checks liveness before each write. The one window a crash opens, after application and before the cursor commit, therefore repeats work that is already a no-op.

Hook events are the exception that is at-least-once: replay after that window repeats one dispatch. The live-state reducer and completion paths already tolerate a repeated event, as they do a shim retry. Hook events are stamped with a fresh `ingress_seq` at application time and carry no transcript path, since the host's path is never opened on the hub.

## Ordering and holding the cursor

A host's records are applied strictly in order, so a worker's queued `done` is applied before the `Stop` behind it, as the synchronous local call would have been. A record that cannot be applied yet (database busy) holds that host's cursor and is retried with backoff; it is never skipped. Different hosts project concurrently.

Work that waits on something other than the database is a durable **effect** and does not hold the cursor. The one such effect is delivering a proposal's decision, which needs the spoke reachable.

## Decisions

The decision for a queued proposal is stored in `spoke_effects` as the exact `ProposalDecision` command, because the spoke applies a decision by compare-and-swap and a changed decision is refused. The projector delivers it over the live stream:

- host unreachable, or outcome uncertain: stays pending, retried every few seconds with the same command id;
- spoke answers `Ok`: settled;
- spoke refuses: retried under a fresh id (`<id>-r<n>`) up to three times, then `failed` for an operator.

A decision is `Ok` with a bounded summary (hub proposal id, state, applied reference, whether it was a replay), or `Rejected` with the typed submission error.

## Not covered

- The run-scoped writes the gateway queues under their own contracts (`guide-outcome/1`, `answer-reply/1`, `conflict-producer/1`, `conflict-mark-failed/1`, `ci-mark-failed/1`) are refused with a decision that names the contract. Each needs its own hub-side ownership checks.
- `WorkerRequest` replies (the gateway's online reads) are a separate follow-on.
- Transcripts, artifacts and gaps are not projected here, so `spoke_ingress` is not pruned yet; whichever consumer is last to need a frame decides when it can go.
- Wall-clock age is not yet distinguished from liveness: replaying an old event refreshes the worker's last-activity time. Staleness judgments should not be trusted for a host until its backlog has drained.

## Diagnosing

- Lag: `WorkDb::spoke_projection_status()`; `received` vs `applied`, `pending_effects`, `failed_effects`.
- Halted host: the log says `spoke observation projection halted` with the position. Received evidence that cannot be applied is never skipped; this is a recovery incident for the host, retried after a hold.
- Quarantined frames and abandoned decisions are `spoke_effects` rows in state `failed` with `last_error`.
- Pane evidence: `WorkDb::spoke_pane_observation(execution_id)`.
