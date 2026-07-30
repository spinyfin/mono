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
//! **The invariant this module owns:** `pending_probes` and `probe_lifecycle`
//! must never disagree. A probe id in `probe_lifecycle` at `Queued` is a live
//! promise that some dispatch path still holds the probe and intends to
//! deliver it. So a probe may only leave the pending queue by being
//! dispatched (which records what happened to it) or through
//! [`ServerState::drain_pending_probes`] (which settles it at an
//! undeliverable state and says why). A bare `pending_probes.remove(run_id)`
//! leaves the promise standing with nothing behind it: `probe_lifecycle`
//! keeps reading `Queued` even though nothing will ever deliver or drain the
//! probe again, so `bossctl probe-status` reports it as still on its way
//! indefinitely.

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

    fn clear_pending_probes(&self, run_id: &str, reason: &str) {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "probe queuer called before server state was bound");
            return;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "probe queuer: server state already dropped");
            return;
        };
        server.clear_pending_probes(run_id, reason);
    }
}

/// Why one call into a probe-dispatch path ended the way it did.
///
/// Returned by every `dispatch_probe_*` function and logged by that function
/// before it returns, so there is no exit from the dispatch path that leaves
/// no trace. The value exists on top of the log line because a test asserting
/// "the `None` branch was taken" should not have to parse `tracing` output.
///
/// The reason this is an explicit type rather than a bare `return`: a silent
/// exit (writing nothing to the trace) makes "the dispatcher never ran" and
/// "it ran and found an empty queue" indistinguishable from the outside —
/// which is exactly the difference between a bug being upstream or
/// downstream of this function. Every branch, including the routine
/// `NothingQueued` case, names itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeDispatchOutcome {
    /// The hook event was not a delivery boundary for this path (wrong event
    /// kind, or a driver whose `Stop` is not a turn boundary).
    NotADeliveryBoundary,
    /// The hook event carried no `run_id`, so no queue could be identified.
    NoRunId,
    /// Nothing was queued for the run. The overwhelmingly common outcome —
    /// most boundaries have no probe waiting.
    NothingQueued,
    /// Probes are queued but the run has no slot mapping, so there is no pane
    /// to write into. They stay queued for a later boundary.
    NoSlotMapping,
    /// Probes are queued but the `(activity, driver)` pair admits no write.
    /// They stay queued (fail closed — never write into a non-consuming
    /// foreground process).
    PostureRefused,
    /// A probe is already in flight for the run and still owes a
    /// `ProbeReplied`. The queue is left alone so the single in-flight slot
    /// is not overwritten; it drains after the next turn boundary.
    AlreadyInFlight,
    /// The queue was non-empty at the pre-pop peek and empty at the pop:
    /// another dispatch path won the race. Anomalous but not a loss — the
    /// winner owns the probe and records its outcome.
    RacedToEmpty,
    /// A probe was popped and dispatched; the value is the delivery state
    /// recorded for it.
    Dispatched(ProbeDeliveryState),
    /// A probe was popped, the write failed, and it was pushed back onto the
    /// front of the queue with its id intact for the next boundary.
    RequeuedAfterFailure,
}

impl ProbeDispatchOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::NotADeliveryBoundary => "not_a_delivery_boundary",
            Self::NoRunId => "no_run_id",
            Self::NothingQueued => "nothing_queued",
            Self::NoSlotMapping => "no_slot_mapping",
            Self::PostureRefused => "posture_refused",
            Self::AlreadyInFlight => "already_in_flight",
            Self::RacedToEmpty => "raced_to_empty",
            Self::Dispatched(_) => "dispatched",
            Self::RequeuedAfterFailure => "requeued_after_failure",
        }
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
    ///
    /// Refuses to re-queue against a run with no live slot mapping: nothing
    /// drains a run's queue after the run itself is gone (only
    /// [`Self::drain_pending_probes`] does, and its callers are teardown
    /// paths that already ran), so pushing a probe back on would leave it
    /// reading `queued` forever. In that case the probe is settled as
    /// [`ProbeDeliveryState::Abandoned`] here instead, and this returns
    /// `false` so the caller can report the outcome accurately.
    pub(super) fn requeue_probe_front(&self, run_id: String, probe: PendingProbe) -> bool {
        if self.worker_registry.slot_for_run(&run_id).is_none() {
            tracing::warn!(
                run_id,
                probe_id = %probe.probe_id,
                "pane write failed and the run's slot mapping is already gone; \
                 abandoning instead of re-queuing against a dead run",
            );
            self.set_probe_lifecycle_detail(
                &probe.probe_id,
                ProbeDeliveryState::Abandoned,
                Some("the pane write failed and the run was released before it could be retried".to_owned()),
            );
            return false;
        }
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .entry(run_id)
            .or_default()
            .push_front(probe);
        true
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

    /// True when a probe has been written into `run_id`'s pane (or claimed
    /// the run's delivery slot and is about to be) and its `ProbeReplied` is
    /// still outstanding.
    ///
    /// Cheap pre-check only — [`Self::try_reserve_probe_for_delivery`] is the
    /// one that decides, atomically. Every turn boundary clears the slot
    /// unconditionally (`take_in_flight_probe` in
    /// `dispatch_probe_reply_on_stop`), so a probe held back by this is
    /// deferred by at most one reply cycle rather than stalled.
    ///
    /// "Unconditionally" is what keeps that true for a driver that folds a
    /// mid-turn prompt into the running turn instead of starting a new one
    /// for it (Codex, measured). Such a prompt produces no boundary of its
    /// own, so a slot released only on "the probe's own turn boundary" would
    /// never be released and every later probe for the run would queue behind
    /// it forever. Releasing on the next boundary of any kind — which the
    /// running turn still reaches — is what makes the folded case a shorter
    /// wait rather than an infinite one.
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
    /// slot is freed for the next attempt — unless the run's slot mapping is
    /// already gone, in which case [`Self::requeue_probe_front`] settles the
    /// probe as `Abandoned` instead. Returns `true` when the probe was
    /// requeued, `false` when it was abandoned, so the caller can report the
    /// outcome it actually recorded rather than assuming a requeue.
    pub(super) fn release_probe_reservation(&self, run_id: &str, probe: PendingProbe) -> bool {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .remove(run_id);
        self.requeue_probe_front(run_id.to_owned(), probe)
    }

    /// True when at least one probe is queued (not yet written into the
    /// pane) for `run_id`. Lets a dispatcher no-op on the overwhelming
    /// majority of boundaries without touching the registry, the driver
    /// table, or the DB. Use [`Self::pending_probe_count`] instead when the
    /// exit being taken wants to report *how many* probes it left queued.
    pub(super) fn has_pending_probe(&self, run_id: &str) -> bool {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .get(run_id)
            .is_some_and(|queue| !queue.is_empty())
    }

    /// How many probes are queued (not yet dispatched) for `run_id`.
    ///
    /// Lets a dispatch path distinguish "nothing to do" from "something to do
    /// that I cannot do" *before* it decides whether an exit is routine or
    /// anomalous, without claiming anything — the claim must stay behind the
    /// fail-closed posture guard.
    pub(super) fn pending_probe_count(&self, run_id: &str) -> usize {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .get(run_id)
            .map(VecDeque::len)
            .unwrap_or(0)
    }

    /// Remove every not-yet-delivered probe queued for `run_id` and settle
    /// each one at the terminal `state`, with `detail` recorded as the
    /// operator-facing explanation. Returns the drained probe ids.
    ///
    /// **This is the only sanctioned way to remove a queued probe without
    /// delivering it.** Removing from `pending_probes` without a matching
    /// `probe_lifecycle` transition leaves the record reading `Queued` — a
    /// live promise with nothing behind it — and nothing revisits it, so
    /// `bossctl probe-status` would report the probe as still on its way
    /// indefinitely. Draining through here keeps the two tables in step and
    /// logs one line per probe, so the drop is both visible in the trace and
    /// answerable over the wire.
    ///
    /// Leaves any already-injected in-flight probe untouched: those are past
    /// the queue and their outcome is settled by the reply path.
    pub(super) fn drain_pending_probes(&self, run_id: &str, state: ProbeDeliveryState, detail: &str) -> Vec<String> {
        debug_assert!(
            state.is_undeliverable(),
            "draining a queued probe means it will never be delivered; \
             {} is not an undeliverable state",
            state.as_str(),
        );
        let drained = self
            .pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .remove(run_id)
            .unwrap_or_default();
        let mut ids = Vec::with_capacity(drained.len());
        for probe in drained {
            tracing::warn!(
                run_id,
                probe_id = %probe.probe_id,
                state = state.as_str(),
                detail,
                "queued probe drained undelivered; recording terminal delivery state",
            );
            self.set_probe_lifecycle_detail(&probe.probe_id, state, Some(detail.to_owned()));
            ids.push(probe.probe_id);
        }
        ids
    }

    /// Drop every not-yet-delivered probe queued for `run_id`. Used by
    /// the completion handler to discard a stale nudge (e.g. one
    /// requeued for retry after a failed `SendToPane`) once a Stop
    /// reveals the worker reported `[blocked]`/`[effort-escalation]` —
    /// otherwise `dispatch_probe_on_stop` would pop and deliver it
    /// regardless of that Stop's own (suppressed) completion outcome.
    ///
    /// `reason` is required and lands in the probe's status record: a
    /// deliberate discard is a legitimate outcome, but only if the caller who
    /// was told "queued" can find out that it happened and why.
    fn clear_pending_probes(&self, run_id: &str, reason: &str) {
        self.drain_pending_probes(run_id, ProbeDeliveryState::Dropped, reason);
    }

    /// Settle every probe still queued for `run_id` as [`ProbeDeliveryState::Abandoned`],
    /// because the run is going away: its pane is being released, so no
    /// delivery boundary will ever arrive.
    ///
    /// Called from [`ServerState::release_worker_pane`], which is the single
    /// choke point every teardown path funnels through (completion, `bossctl
    /// agents stop`, the stale/terminal/dead-pid sweeps, engine shutdown).
    /// Without this a probe accepted with a `next_turn_boundary` commitment
    /// outlives the run it was addressed to and reports `queued` forever.
    pub(super) fn abandon_pending_probes_for_terminated_run(&self, run_id: &str, cause: &str) {
        let ids = self.drain_pending_probes(
            run_id,
            ProbeDeliveryState::Abandoned,
            &format!("{cause}; the worker was gone before the probe's delivery boundary arrived"),
        );
        if !ids.is_empty() {
            tracing::warn!(
                run_id,
                probe_ids = %ids.join(","),
                cause,
                "run terminated with probes still queued; the accepted delivery commitment could \
                 not be honoured — recorded as abandoned rather than left queued",
            );
        }
    }

    /// Whether the worker process behind `run_id` probes *dead* right now.
    ///
    /// Used to keep [`ProbeDeliveryState::Consumed`] honest: a `SendToPane`
    /// that returns `Ok` only proves the app wrote bytes into the pty, and a
    /// pane whose foreground process has already exited will accept those
    /// bytes with nobody to read them (observed with a `codex` pane, where the
    /// probe was nonetheless recorded `consumed`). Reuses the shared
    /// [`crate::dead_pid_sweep::probe_pid`] liveness probe rather than a
    /// second `kill(pid, 0)` implementation.
    ///
    /// Deliberately conservative — this returns `true` only for a positive
    /// `ESRCH` verdict on a recorded pid. No recorded pid (`0`, i.e. the app
    /// has not reported one yet), `EPERM`, or any other errno all yield
    /// `false`, so an unverifiable worker keeps the pre-existing behaviour
    /// instead of being libelled as dead.
    pub(super) fn run_process_probes_dead(&self, run_id: &str) -> bool {
        let Some(shell_pid) = self.live_worker_states.shell_pid_for_run(run_id) else {
            return false;
        };
        if shell_pid <= 0 {
            return false;
        }
        matches!(
            crate::dead_pid_sweep::probe_pid(shell_pid),
            crate::dead_pid_sweep::PidStatus::Dead
        )
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
