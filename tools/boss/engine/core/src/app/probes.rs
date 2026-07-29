//! Probe queueing and lifecycle tracking.
//!
//! A *probe* is a question the coordinator (or the completion handler)
//! injects into a live worker's pane, delivered at the earliest point that
//! pane will take a write — during the `ProbeRun` call itself for a pane in a
//! writable posture, otherwise at a `PostToolUse` or `Stop` boundary. This
//! module owns the per-run pending queue, the single in-flight slot awaiting
//! a reply, and the observable [`ProbeDeliveryState`] each probe id moves
//! through.
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;

/// Adapter so the completion handler can queue probes onto
/// `ServerState::pending_probes` without depending on `ServerState`
/// directly. Same late-bind dance as `ServerStatePaneReleaser` — the
/// completion handler is built before the `Arc<ServerState>` exists,
/// then `set_server_state` plumbs the upgrade target in. A probe queued
/// through this adapter is queued from inside a `Stop` fan-out, and
/// `dispatch_probe_on_stop` — which runs later in that same fan-out —
/// `SendToPane`s it as if the user had typed it.
#[derive(Default)]
pub(super) struct ServerStateProbeQueuer {
    server: std::sync::OnceLock<Weak<ServerState>>,
}

impl ServerStateProbeQueuer {
    pub(super) fn set_server_state(&self, weak: Weak<ServerState>) {
        let _ = self.server.set(weak);
    }
}

impl ProbeQueuer for ServerStateProbeQueuer {
    fn queue_probe(&self, run_id: &str, text: &str) {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "probe queuer called before server state was bound");
            return;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "probe queuer: server state already dropped");
            return;
        };
        // Completion-driven probes don't need the minted id — only
        // the human-driven `ProbeRun` RPC surfaces it back to the
        // caller. Discard it here. Completion probes are never urgent.
        let _ = server.queue_probe(run_id.to_owned(), text.to_owned(), false);
    }

    fn clear_pending_probes(&self, run_id: &str) {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "probe queuer called before server state was bound");
            return;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "probe queuer: server state already dropped");
            return;
        };
        server.clear_pending_probes(run_id);
    }
}

/// One queued probe that has not yet been dispatched into the worker.
#[derive(Debug, Clone)]
pub(super) struct PendingProbe {
    pub(super) probe_id: String,
    pub(super) text: String,
}

/// One probe that has been written into the worker's pane and is
/// waiting for the next `Stop` boundary so we can emit
/// `FrontendEvent::ProbeReplied` with the assistant turn that
/// landed in the transcript afterwards.
#[derive(Debug, Clone)]
pub(super) struct InFlightProbe {
    pub(super) probe_id: String,
    /// Transcript path captured at dispatch time. Stashing it here
    /// (rather than re-querying `WorkRun` on the follow-up Stop)
    /// keeps reply extraction tied to the file the worker was
    /// actually writing when the probe landed, even if the run row
    /// is later updated to point elsewhere.
    pub(super) transcript_path: Option<String>,
    /// Bytes-on-disk size of the transcript at dispatch time. The
    /// follow-up Stop reads `[offset_bytes..len]` and parses each
    /// new JSONL line — anything earlier already pre-dated the probe
    /// and isn't part of the reply.
    pub(super) offset_bytes: u64,
}

/// Everything the engine knows about one probe id after it was accepted:
/// which run it targets, whether it was urgent, how far delivery got, and
/// (optionally) an operator-facing note about how it got there.
///
/// The state itself is [`boss_protocol::ProbeDeliveryState`] — the same enum
/// the wire uses — so `bossctl probe-status` reports exactly what the engine
/// recorded, with no second vocabulary to keep in sync.
#[derive(Debug, Clone)]
pub(super) struct ProbeRecord {
    pub(super) run_id: String,
    pub(super) urgent: bool,
    pub(super) state: ProbeDeliveryState,
    pub(super) detail: Option<String>,
}

impl ServerState {
    /// Push probe text onto the queue for `run_id`, mint a fresh
    /// `probe_id`, and return it so the caller can correlate the
    /// queued probe with the eventual `FrontendEvent::ProbeReplied`
    /// push. Non-urgent probes append to the back (FIFO); urgent
    /// probes push to the front so they fire before any queued
    /// non-urgent probes.
    ///
    /// Queueing says nothing about *where* the probe is delivered — that
    /// is chosen from the worker's pane posture at each delivery
    /// opportunity (during this call for a writable pane, then at every
    /// `PostToolUse`, then at every `Stop`). One probe is delivered per
    /// reply cycle: see [`Self::has_in_flight_probe`].
    pub fn queue_probe(&self, run_id: String, text: String, urgent: bool) -> String {
        let probe_id = self.allocate_probe_id();
        let probe = PendingProbe {
            probe_id: probe_id.clone(),
            text,
        };
        // Urgency lives on the queue (position) and on the lifecycle record
        // below (`ProbeRecord::urgent`, what `bossctl probe-status` reports).
        // It is deliberately not carried on the pending entry: nothing after
        // insertion branches on it, because transport is chosen from the
        // worker's pane posture rather than from the caller's flag.
        self.probe_lifecycle
            .lock()
            .expect("probe_lifecycle mutex poisoned")
            .insert(
                probe_id.clone(),
                ProbeRecord {
                    run_id: run_id.clone(),
                    urgent,
                    state: ProbeDeliveryState::Queued,
                    detail: None,
                },
            );
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.entry(run_id).or_default();
        if urgent {
            queue.push_front(probe);
        } else {
            queue.push_back(probe);
        }
        probe_id
    }

    /// Push a pre-minted `PendingProbe` back onto the front of the queue for
    /// `run_id`. Used when `SendToPane` fails after the probe was already
    /// claimed — a later delivery opportunity retries, and the caller's
    /// `probe_id` stays stable across the retry. Delivery sites should call
    /// [`Self::release_probe_reservation`], which also frees the in-flight
    /// slot.
    pub(super) fn requeue_probe_front(&self, run_id: String, probe: PendingProbe) {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .entry(run_id)
            .or_default()
            .push_front(probe);
    }

    fn allocate_probe_id(&self) -> String {
        format!("probe-{}", self.next_probe_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Pop the next pending probe for `run_id`, if any, without claiming the
    /// run's in-flight slot.
    ///
    /// Test-only: every production delivery site goes through
    /// [`Self::try_reserve_probe_for_delivery`], which pops *and* claims
    /// atomically. A bare pop would reintroduce the window this rule exists to
    /// close, so it stays out of the shipped build rather than sitting there
    /// as the obvious-looking wrong choice.
    #[cfg(test)]
    pub(super) fn pop_pending_probe(&self, run_id: &str) -> Option<PendingProbe> {
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.get_mut(run_id)?;
        let probe = queue.pop_front();
        if queue.is_empty() {
            guard.remove(run_id);
        }
        probe
    }

    /// True when at least one probe is queued (not yet written into the
    /// pane) for `run_id`. Lets the `PostToolUse` dispatcher no-op on the
    /// overwhelming majority of tool boundaries without touching the
    /// registry, the driver table, or the DB.
    pub(super) fn has_pending_probe(&self, run_id: &str) -> bool {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .get(run_id)
            .is_some_and(|queue| !queue.is_empty())
    }

    /// True when a probe has been written into `run_id`'s pane (or claimed
    /// the run's delivery slot and is about to be) and its `ProbeReplied` is
    /// still outstanding.
    ///
    /// Cheap pre-check only — [`Self::try_reserve_probe_for_delivery`] is the
    /// one that decides, atomically. Every turn boundary clears the slot
    /// unconditionally (`take_in_flight_probe` in
    /// `dispatch_probe_reply_on_stop`), so a probe held back by this is
    /// deferred by at most one reply cycle rather than stalled.
    pub(super) fn has_in_flight_probe(&self, run_id: &str) -> bool {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .contains_key(run_id)
    }

    /// Claim the next queued probe for `run_id` **and** the run's single
    /// in-flight slot in one atomic step, or return `None` if either is
    /// unavailable.
    ///
    /// This is what makes "one probe in flight per run" a rule rather than a
    /// hope. There are three concurrent delivery sites — the `ProbeRun` call
    /// itself, every `PostToolUse`, and every `Stop` — and the pane write
    /// between claiming a probe and recording it takes a round trip plus a
    /// verification window. Checking `in_flight_probes` separately from
    /// popping would let two of those sites both observe an empty slot and
    /// both deliver, and since the slot holds one entry the first probe's
    /// pending `ProbeReplied` would be silently discarded.
    ///
    /// The caller snapshots `transcript_path`/`offset_bytes` *before* calling
    /// so the in-flight entry is complete from the moment it exists: any
    /// boundary that lands mid-write still finds a well-formed entry to
    /// extract a reply against, never a half-populated placeholder.
    ///
    /// Release the claim with [`Self::release_probe_reservation`] if the
    /// write cannot be completed.
    ///
    /// Lock order is `in_flight_probes` → `pending_probes`; no other path
    /// holds both, so there is nothing to deadlock against.
    pub(super) fn try_reserve_probe_for_delivery(
        &self,
        run_id: &str,
        transcript_path: Option<String>,
        offset_bytes: u64,
    ) -> Option<PendingProbe> {
        let mut in_flight = self.in_flight_probes.lock().expect("in_flight_probes mutex poisoned");
        if in_flight.contains_key(run_id) {
            return None;
        }
        let mut pending = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = pending.get_mut(run_id)?;
        let probe = queue.pop_front()?;
        if queue.is_empty() {
            pending.remove(run_id);
        }
        in_flight.insert(
            run_id.to_owned(),
            InFlightProbe {
                probe_id: probe.probe_id.clone(),
                transcript_path,
                offset_bytes,
            },
        );
        Some(probe)
    }

    /// Give back a claim taken by [`Self::try_reserve_probe_for_delivery`]
    /// when the pane write did not happen: the probe returns to the front of
    /// the queue with its id intact (callers waiting on the matching
    /// `ProbeReplied` must not see their id reissued) and the run's in-flight
    /// slot is freed for the next attempt.
    pub(super) fn release_probe_reservation(&self, run_id: &str, probe: PendingProbe) {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .remove(run_id);
        self.requeue_probe_front(run_id.to_owned(), probe);
    }

    /// Drop every not-yet-delivered probe queued for `run_id`. Used by
    /// the completion handler to discard a stale nudge (e.g. one
    /// requeued for retry after a failed `SendToPane`) once a Stop
    /// reveals the worker reported `[blocked]`/`[effort-escalation]` —
    /// otherwise `dispatch_probe_on_stop` would pop and deliver it
    /// regardless of that Stop's own (suppressed) completion outcome.
    /// Leaves any already-injected in-flight probe untouched.
    fn clear_pending_probes(&self, run_id: &str) {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .remove(run_id);
    }

    /// Take and return the in-flight probe for `run_id`, if any.
    /// Idempotent on the second pop: a duplicate Stop firing for
    /// the same run gets `None` and the engine emits no second
    /// `ProbeReplied` for the same probe id.
    pub(super) fn take_in_flight_probe(&self, run_id: &str) -> Option<InFlightProbe> {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .remove(run_id)
    }

    /// Record `probe_id`'s current delivery stage. Call sites drive
    /// every transition explicitly (see [`ProbeDeliveryState`]) —
    /// there's no automatic advancement based on other bookkeeping,
    /// so a probe id with no entry has never been queued in this
    /// process (or the engine restarted).
    ///
    /// A transition for an id that was never queued is dropped rather than
    /// inventing a record: `run_id`/`urgent` are only knowable at queue time,
    /// and a status answer that guessed them would be worse than "unknown
    /// probe id".
    pub(super) fn set_probe_lifecycle(&self, probe_id: &str, state: ProbeDeliveryState) {
        self.set_probe_lifecycle_detail(probe_id, state, None);
    }

    /// [`Self::set_probe_lifecycle`] plus an operator-facing note explaining
    /// how the probe reached `state` — surfaced verbatim by
    /// `bossctl probe-status`. Passing `None` clears any previous note, so a
    /// probe that recovers from `Unconfirmed` doesn't keep a stale
    /// explanation.
    pub(super) fn set_probe_lifecycle_detail(&self, probe_id: &str, state: ProbeDeliveryState, detail: Option<String>) {
        let mut guard = self.probe_lifecycle.lock().expect("probe_lifecycle mutex poisoned");
        match guard.get_mut(probe_id) {
            Some(record) => {
                record.state = state;
                record.detail = detail;
            }
            None => tracing::warn!(
                probe_id,
                state = state.as_str(),
                "probe lifecycle transition for an id that was never queued; dropping",
            ),
        }
    }

    /// Full record for `probe_id`, if this engine process queued it.
    /// Answers `FrontendRequest::ProbeStatus`.
    pub(super) fn probe_record(&self, probe_id: &str) -> Option<ProbeRecord> {
        self.probe_lifecycle
            .lock()
            .expect("probe_lifecycle mutex poisoned")
            .get(probe_id)
            .cloned()
    }

    /// Query the current lifecycle stage for `probe_id`, if any is
    /// tracked. Used by `dispatch_probe_reply_on_stop` to skip reply
    /// extraction for a probe the engine never actually dispatched,
    /// and by tests to assert the corrected no-auto-redelivery
    /// behavior without depending on internal queue contents.
    pub(super) fn probe_lifecycle_state(&self, probe_id: &str) -> Option<ProbeDeliveryState> {
        self.probe_lifecycle
            .lock()
            .expect("probe_lifecycle mutex poisoned")
            .get(probe_id)
            .map(|record| record.state)
    }
}
