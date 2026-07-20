//! Probe queueing and lifecycle tracking.
//!
//! A *probe* is a question the coordinator (or the completion handler)
//! injects into a live worker's pane so the worker answers it at its next
//! `Stop`/`PostToolUse` boundary. This module owns the per-run pending
//! queue, the single in-flight slot awaiting a reply, and the observable
//! [`ProbeLifecycleState`] each probe id moves through.
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;

/// Adapter so the completion handler can queue probes onto
/// `ServerState::pending_probes` without depending on `ServerState`
/// directly. Same late-bind dance as `ServerStatePaneReleaser` — the
/// completion handler is built before the `Arc<ServerState>` exists,
/// then `set_server_state` plumbs the upgrade target in. The next
/// `Stop` event for the run pops one queued entry and `SendToPane`s
/// it as if the user had typed it (`dispatch_probe_on_stop`).
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
    /// When `true`, dispatch at the next `PostToolUse` boundary
    /// rather than waiting for the next `Stop`. Urgent probes are
    /// always inserted at the front of the per-run queue.
    pub(super) urgent: bool,
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

/// Observable lifecycle of one probe, keyed by `probe_id`. Queried by
/// tests and by `dispatch_probe_reply_on_stop` (which asserts a probe
/// was at least `Injected` before it will report a reply for it).
///
/// This is the corrected-spec replacement for treating "no
/// `UserPromptSubmit` within the verification window" as a delivery
/// failure: rather than silently re-delivering (which duplicates the
/// probe if the CLI *did* enqueue it and the hook was merely slow or
/// absent), the engine now records `Unconfirmed` and leaves the
/// redelivery call to whoever is watching the probe topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeLifecycleState {
    /// Minted and sitting in `pending_probes`, not yet written to the pane.
    Queued,
    /// `SendToPane` returned Ok but delivery has not yet been confirmed
    /// or timed out.
    Injected,
    /// A `UserPromptSubmit` hook (or, failing that, a transcript scan)
    /// confirmed the CLI actually enqueued the injected text.
    Consumed,
    /// The verification window elapsed with no confirming signal. The
    /// write may still have landed — this state means "unproven", not
    /// "lost" — so the engine does not automatically re-deliver.
    Unconfirmed,
    /// `dispatch_probe_reply_on_stop` extracted and published the
    /// worker's reply.
    Replied,
}

impl ServerState {
    /// Push probe text onto the queue for `run_id`, mint a fresh
    /// `probe_id`, and return it so the caller can correlate the
    /// queued probe with the eventual `FrontendEvent::ProbeReplied`
    /// push. Non-urgent probes append to the back (FIFO); urgent
    /// probes push to the front so they fire before any queued
    /// non-urgent probes. The events-socket consumer delivers one
    /// probe per `Stop` event (non-urgent) or per `PostToolUse`
    /// event (urgent).
    pub fn queue_probe(&self, run_id: String, text: String, urgent: bool) -> String {
        let probe_id = self.allocate_probe_id();
        let probe = PendingProbe {
            probe_id: probe_id.clone(),
            text,
            urgent,
        };
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.entry(run_id).or_default();
        if urgent {
            queue.push_front(probe);
        } else {
            queue.push_back(probe);
        }
        drop(guard);
        self.set_probe_lifecycle(&probe_id, ProbeLifecycleState::Queued);
        probe_id
    }

    /// Push a pre-minted `PendingProbe` back onto the front of the
    /// queue for `run_id`. Used when `SendToPane` fails after we've
    /// already popped the probe — the next Stop will retry, and the
    /// caller's `probe_id` stays stable across the retry.
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

    /// Pop the next pending probe for `run_id`, if any. Called from
    /// the events-socket consumer when a `Stop` event arrives.
    pub(super) fn pop_pending_probe(&self, run_id: &str) -> Option<PendingProbe> {
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.get_mut(run_id)?;
        let probe = queue.pop_front();
        if queue.is_empty() {
            guard.remove(run_id);
        }
        probe
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

    /// Note that `probe_id` was just dispatched into the worker's
    /// pane for `run_id`. The next `Stop` boundary on this run will
    /// look for an in-flight entry, read the transcript bytes
    /// written after `offset_bytes`, and emit
    /// `FrontendEvent::ProbeReplied`. Any prior in-flight probe for
    /// the same run is overwritten — we only track one outstanding
    /// reply at a time per run, since dispatch is serialized on
    /// `Stop` events.
    pub(super) fn note_probe_dispatched(
        &self,
        run_id: String,
        probe_id: String,
        transcript_path: Option<String>,
        offset_bytes: u64,
    ) {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .insert(
                run_id,
                InFlightProbe {
                    probe_id,
                    transcript_path,
                    offset_bytes,
                },
            );
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

    /// Record `probe_id`'s current lifecycle stage. Call sites drive
    /// every transition explicitly (see [`ProbeLifecycleState`]) —
    /// there's no automatic advancement based on other bookkeeping,
    /// so a probe id with no entry has never been queued in this
    /// process (or the engine restarted).
    pub(super) fn set_probe_lifecycle(&self, probe_id: &str, state: ProbeLifecycleState) {
        self.probe_lifecycle
            .lock()
            .expect("probe_lifecycle mutex poisoned")
            .insert(probe_id.to_owned(), state);
    }

    /// Query the current lifecycle stage for `probe_id`, if any is
    /// tracked. Used by `dispatch_probe_reply_on_stop` to skip reply
    /// extraction for a probe the engine never actually dispatched,
    /// and by tests to assert the corrected no-auto-redelivery
    /// behavior without depending on internal queue contents.
    pub(super) fn probe_lifecycle_state(&self, probe_id: &str) -> Option<ProbeLifecycleState> {
        self.probe_lifecycle
            .lock()
            .expect("probe_lifecycle mutex poisoned")
            .get(probe_id)
            .copied()
    }
}
