//! Per-session outbound event queue and topic fan-out broker.
//!
//! Everything here is `pub(super)` (visible to `app` and, transitively, the
//! rest of its module tree — `app::tests`, `app::tests::session_sink_queue`, and sibling
//! handler modules) rather than `pub`, matching the encapsulation this code
//! had while it lived directly in `app.rs`.

use super::*;

/// Maximum events that can be queued in one session's **bulk lane** before we
/// start shedding load. Sized for typical work-invalidation traffic: each
/// mutation emits at most a couple of envelopes, and same-topic
/// invalidations are coalesced, so 256 absorbs bursts while bounding
/// memory. Hitting this cap no longer disconnects on its own — see
/// [`STUCK_CLIENT_AGE_MS`] — because a fast-draining client can legitimately
/// see >256 *distinct* topics invalidated within a couple seconds during a
/// merge-poller sweep across many live workers/products; disconnecting that
/// client only recreates the burst on reconnect (full resubscribe + cold
/// refetch) without the client ever having been the problem.
pub(super) const MAX_SESSION_QUEUE: usize = 256;

/// Maximum time without socket write progress before a full bulk queue is
/// considered stuck. Queue age alone is insufficient: a large response can
/// occupy the writer for much longer while the client keeps consuming bytes.
pub(super) const STUCK_CLIENT_AGE_MS: u64 = 5_000;

/// Reserved synthetic topic for the resync marker [`SessionQueue`] injects
/// after it has had to drop pending bulk invalidations to survive a burst
/// (see [`SessionQueue::evict_oldest_bulk`]). Never subscribed to via the
/// normal topic-broker path — it exists only so the marker participates in
/// the same per-topic coalescing as real topics (at most one pending marker
/// per session) and so [`TopicEventPayload::ResyncRequired`] has some topic
/// string to carry.
pub(super) const RESYNC_TOPIC: &str = "__resync__";

/// Cap on the **priority lane**, which carries only small engine→app control
/// pushes (`EngineRequest`: reveal, pane-release, spawn, focus, send-input,
/// interrupt). These are point-to-point requests the engine awaits a reply
/// to; only a handful are ever in flight at once (spawns are serialized by
/// the spawn-pane lock). A backlog this deep means the app isn't draining
/// even tiny control frames — a genuine wedge — so a priority overflow
/// reports `Slow` (→ [`SendToAppError::SessionWedged`]) rather than growing
/// memory. Much smaller than [`MAX_SESSION_QUEUE`] because the priority lane
/// should never hold more than a few entries in healthy operation.
pub(super) const MAX_PRIORITY_QUEUE: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnqueueOutcome {
    Enqueued,
    Coalesced,
    Closed,
    /// A lane is stuck without socket write progress, or admitting an
    /// envelope would evict a correlated reply. The sink disconnects.
    Slow,
    /// The bulk lane was full but the client is actively draining (just
    /// slower than a burst's publish rate): the oldest pending entry was
    /// dropped to admit this one instead of disconnecting, and the session
    /// now owes a resync marker. Not an error — callers should log at most
    /// at debug level.
    Degraded,
}

/// Point-in-time view of one session's outbound queue. Emitted on every
/// engine→app send timeout and periodically by the queue-depth logger so
/// a saturated push channel (the `reveal_work_item` `Send(Timeout)`
/// failure mode) is diagnosable from the engine logs instead of inferred
/// from per-call WARNs. `oldest_age_ms` is how long the head-of-line
/// envelope has waited — the head-of-line-blocking signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueueStats {
    /// Combined depth across both lanes (priority + bulk).
    pub(super) depth: usize,
    /// Depth of the priority lane alone. Small engine→app control pushes
    /// drain ahead of bulk snapshot/invalidation traffic, so a healthy
    /// channel keeps this near zero even while `depth` climbs under bulk
    /// load — evidence the priority lane is doing its job. If it *does*
    /// climb, the app is wedged on control frames too.
    pub(super) priority_depth: usize,
    pub(super) oldest_age_ms: u64,
    pub(super) slow: bool,
    pub(super) closed: bool,
}

#[derive(bon::Builder)]
pub(super) struct SessionQueue {
    /// Priority lane: small engine→app control pushes (`EngineRequest`)
    /// only, drained ahead of everything in `items`. Each entry is
    /// `(enqueued_at, envelope)`. Not coalesced — each `EngineRequest` is a
    /// distinct request awaiting its own reply — and bounded independently
    /// by [`MAX_PRIORITY_QUEUE`], so a saturated bulk lane never blocks (or
    /// wedges) a reveal / pane-release. See [`is_priority_event`].
    #[builder(default)]
    pub(super) priority: VecDeque<(Instant, FrontendEventEnvelope)>,
    /// Bulk lane: everything that isn't a priority control push — `WorkTree`
    /// snapshot responses, `TopicEvent` invalidations, list/result replies,
    /// the `Hello` handshake. Each entry is `(enqueued_at, envelope)`. The
    /// instant is stamped at enqueue time (preserved across coalesce) so
    /// [`SessionQueue::stats`] can report how long the head-of-line envelope
    /// has been waiting.
    #[builder(default)]
    pub(super) items: VecDeque<(Instant, FrontendEventEnvelope)>,
    /// For each topic with a pending unsent TopicEvent, the index of that
    /// envelope in `items` (front-relative; decremented on pop). Lets us
    /// overwrite stale invalidations instead of growing the queue.
    #[builder(default)]
    pub(super) pending_topics: HashMap<String, usize>,
    /// Full-state pushes indexed by kind, with the same lifetime as topic slots.
    #[builder(default)]
    pending_snapshots: HashMap<&'static str, usize>,
    #[builder(default = false)]
    pub(super) closed: bool,
    /// One-shot backpressure latch. Set when an enqueue overflows
    /// `MAX_SESSION_QUEUE`; while set, further enqueues report `Slow`
    /// without growing memory. Recoverable: [`SessionQueue::pop_front`]
    /// clears it once the queue drains to empty, so a session that
    /// briefly overflowed but then caught up accepts events again instead
    /// of silently dropping every subsequent enqueue forever.
    #[builder(default = false)]
    pub(super) slow: bool,
    /// Updated only after the socket accepts bytes, never on enqueue/eviction.
    pub(super) last_write_progress: Option<tokio::time::Instant>,
    pub(super) in_flight: Option<super::session_write::EnvelopeSummary>,
}

impl SessionQueue {
    pub(super) fn new() -> Self {
        Self {
            priority: VecDeque::new(),
            items: VecDeque::new(),
            pending_topics: HashMap::new(),
            pending_snapshots: HashMap::new(),
            closed: false,
            slow: false,
            last_write_progress: None,
            in_flight: None,
        }
    }

    pub(super) fn enqueue(&mut self, env: FrontendEventEnvelope) -> EnqueueOutcome {
        if self.closed {
            return EnqueueOutcome::Closed;
        }

        // Priority lane: a small engine→app control push jumps ahead of bulk
        // snapshot/invalidation traffic and is admitted even when the bulk
        // lane has latched `slow`, so a reveal / pane-release never waits out
        // — or fails behind — a ~2,000-item `WorkTree` drain. This is the
        // root-cause fix for the `reveal_work_item` `Send(Timeout)` incident.
        if is_priority_event(&env.payload) {
            if self.priority.len() >= MAX_PRIORITY_QUEUE {
                // The app isn't draining even tiny control frames — a genuine
                // wedge. Report `Slow` so `send_to_app` fails fast as
                // `SessionWedged` and tears the session down. No latch needed:
                // the lane doesn't coalesce, so the length check alone bounds
                // memory and the lane recovers naturally as entries drain.
                return EnqueueOutcome::Slow;
            }
            self.priority.push_back((Instant::now(), env));
            return EnqueueOutcome::Enqueued;
        }

        // Bulk lane. The `slow` latch gates only this lane — it must never
        // reject a priority control push (handled above).
        if self.slow {
            return EnqueueOutcome::Slow;
        }

        if env.request_id.is_none() {
            if let Some(idx) = topic_event_topic(&env.payload)
                .and_then(|topic| self.pending_topics.get(&topic))
                .copied()
            {
                debug_assert!(idx < self.items.len());
                // Preserve the original instant and position. Topic invalidations
                // merge their changes into the pending envelope.
                coalesce_topic_event(&mut self.items[idx].1, env);
                return EnqueueOutcome::Coalesced;
            }
            if let Some(idx) = snapshot_event_kind(&env.payload)
                .and_then(|kind| self.pending_snapshots.get(kind))
                .copied()
            {
                debug_assert!(idx < self.items.len());
                // Preserve the original instant and position. Full snapshots
                // replace the stale payload in place.
                replace_pending_snapshot(&mut self.items[idx].1, env);
                return EnqueueOutcome::Coalesced;
            }
        }

        if self.items.len() >= MAX_SESSION_QUEUE {
            return self.admit_under_pressure(env);
        }
        self.push_bulk(env);
        EnqueueOutcome::Enqueued
    }

    /// Register coalescing slots on every admission path, including pressure.
    fn push_bulk(&mut self, env: FrontendEventEnvelope) {
        let idx = self.items.len();
        if env.request_id.is_none() {
            if let Some(topic) = topic_event_topic(&env.payload) {
                self.pending_topics.insert(topic, idx);
            }
            if let Some(kind) = snapshot_event_kind(&env.payload) {
                self.pending_snapshots.insert(kind, idx);
            }
        } else if let Some(kind) = snapshot_event_kind(&env.payload) {
            // A correlated snapshot reply is a barrier: later uncorrelated
            // pushes of this kind must append after it rather than replace
            // an earlier pending snapshot sitting before the reply.
            self.pending_snapshots.remove(kind);
        }
        self.items.push_back((Instant::now(), env));
    }

    /// At capacity, disconnect when both the backlog and the last successful
    /// socket write are old, or too few uncorrelated pushes can be evicted.
    /// Otherwise shed the oldest uncorrelated pushes and retain one resync
    /// marker, preserving correlated replies and the queue bound. Eviction
    /// is not write progress; the writer also times out a stalled frame
    /// independently of further publishes.
    fn admit_under_pressure(&mut self, env: FrontendEventEnvelope) -> EnqueueOutcome {
        let oldest_age_ms = self
            .items
            .front()
            .map(|(enqueued_at, _)| Instant::now().saturating_duration_since(*enqueued_at).as_millis() as u64)
            .unwrap_or(0);
        let write_idle_ms = self.last_write_progress.map(|at| at.elapsed().as_millis() as u64);
        if oldest_age_ms >= STUCK_CLIENT_AGE_MS && write_idle_ms.is_none_or(|age| age >= STUCK_CLIENT_AGE_MS) {
            self.slow = true;
            return EnqueueOutcome::Slow;
        }
        // Never evict correlated replies or the resync marker: drop the
        // oldest uncorrelated pushes instead, wherever they sit in the lane.
        // Only when too few evictable pushes remain (a lane of replies) is
        // the client treated as unable to keep up; the sink then disconnects.
        let marker_idx = self.pending_topics.get(RESYNC_TOPIC).copied();
        let needed = if marker_idx.is_some() { 1 } else { 2 };
        let victims: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(i, (_, item))| item.request_id.is_none() && Some(*i) != marker_idx)
            .map(|(i, _)| i)
            .take(needed)
            .collect();
        if victims.len() < needed {
            self.slow = true;
            return EnqueueOutcome::Slow;
        }
        // Remove highest index first so earlier indices stay valid.
        for &i in victims.iter().rev() {
            self.evict_at(i);
        }
        if marker_idx.is_none() {
            self.push_bulk(resync_envelope());
        }

        self.push_bulk(env);
        EnqueueOutcome::Degraded
    }

    /// Pop the oldest bulk-lane entry (if any), keeping `pending_topics`
    /// indices front-relative. Shared by [`SessionQueue::pop_front`]'s
    /// normal drain.
    fn evict_oldest_bulk(&mut self) -> Option<(Instant, FrontendEventEnvelope)> {
        self.evict_at(0)
    }

    /// Remove a bulk entry and shift both topic and snapshot slot indices.
    fn evict_at(&mut self, index: usize) -> Option<(Instant, FrontendEventEnvelope)> {
        let removed = self.items.remove(index)?;
        self.pending_topics.retain(|_, idx| *idx != index);
        self.pending_snapshots.retain(|_, idx| *idx != index);
        for idx in self
            .pending_topics
            .values_mut()
            .chain(self.pending_snapshots.values_mut())
        {
            if *idx > index {
                *idx -= 1;
            }
        }
        Some(removed)
    }

    pub(super) fn pop_front(&mut self) -> Option<FrontendEventEnvelope> {
        // Drain the priority lane first: small control pushes always leave
        // before bulk snapshot/invalidation traffic. The priority lane does
        // not participate in topic coalescing, so popping from it leaves the
        // bulk lane's `pending_topics` indices (and the `slow` latch) alone.
        if let Some((_enqueued_at, env)) = self.priority.pop_front() {
            return Some(env);
        }

        let (_enqueued_at, env) = self.evict_oldest_bulk()?;
        // Recover the backpressure latch once the backlog is fully drained:
        // a session that caught up is no longer slow and must accept new
        // events rather than reject them permanently.
        if self.items.is_empty() {
            self.slow = false;
        }
        Some(env)
    }

    /// Snapshot depth, head-of-line age, and the backpressure/closed
    /// flags. Cheap and lock-local — the caller already holds the queue
    /// mutex via [`SessionSink::queue_stats`].
    pub(super) fn stats(&self) -> QueueStats {
        let now = Instant::now();
        let front_age = |lane: &VecDeque<(Instant, FrontendEventEnvelope)>| {
            lane.front()
                .map(|(enqueued_at, _)| now.saturating_duration_since(*enqueued_at).as_millis() as u64)
                .unwrap_or(0)
        };
        // The oldest still-waiting envelope across both lanes — the true
        // head-of-line-blocking signal.
        let oldest_age_ms = front_age(&self.priority).max(front_age(&self.items));
        QueueStats {
            depth: self.priority.len() + self.items.len(),
            priority_depth: self.priority.len(),
            oldest_age_ms,
            slow: self.slow,
            closed: self.closed,
        }
    }

    /// Test-only: back-date the bulk lane's head-of-line entry so
    /// [`SessionQueue::admit_under_pressure`] sees it as older than
    /// [`STUCK_CLIENT_AGE_MS`], simulating a genuinely stuck client without
    /// an actual multi-second sleep in the test.
    #[cfg(test)]
    pub(super) fn backdate_oldest_bulk_entry(&mut self, age_ms: u64) {
        if let Some((enqueued_at, _)) = self.items.front_mut() {
            *enqueued_at = Instant::now() - std::time::Duration::from_millis(age_ms);
        }
    }
}

/// Replace a pending unsent snapshot in place with a newer uncorrelated push
/// of the same kind. Snapshots are complete state, so the newest payload is
/// sufficient; the original enqueue instant and queue position stay put.
/// Snapshot replacement stays off the topic-merge path so WorkInvalidated
/// unioning cannot change snapshot semantics.
fn replace_pending_snapshot(pending: &mut FrontendEventEnvelope, next: FrontendEventEnvelope) {
    *pending = next;
}

/// Replace the unsent push `pending` with the newer `next` for the same topic
/// without losing what `pending` told the app.
///
/// Used only for topic invalidations. Session-wide snapshots replace in place
/// via [`replace_pending_snapshot`].
///
/// A [`TopicEventPayload::WorkInvalidated`] carries the class of the change and
/// the cards it names, and the app chooses its refetch from both. Replacing one
/// with the other would corrupt that choice: a structural push followed by a
/// runtime one would arrive as runtime only and leave the board stale, and
/// runtime pushes for different cards would lose every card but the last. So
/// two invalidations merge into one with the stronger class and the union of
/// the cards, and everything else (reason, revision, origin) from the newer
/// push. A union that names more cards than a patch is cheaper than is promoted
/// to structural, as is a pair for different products. Any other pairing keeps
/// the replace-with-newest behavior.
fn coalesce_topic_event(pending: &mut FrontendEventEnvelope, mut next: FrontendEventEnvelope) {
    let merged = match (&pending.payload, &next.payload) {
        (
            FrontendEvent::TopicEvent {
                event:
                    TopicEventPayload::WorkInvalidated {
                        product_id: pending_product,
                        item_ids: pending_ids,
                        class: pending_class,
                        ..
                    },
                ..
            },
            FrontendEvent::TopicEvent {
                event:
                    TopicEventPayload::WorkInvalidated {
                        product_id: next_product,
                        item_ids: next_ids,
                        class: next_class,
                        ..
                    },
                ..
            },
        ) => {
            let known: std::collections::HashSet<&str> = pending_ids.iter().map(String::as_str).collect();
            let mut item_ids = pending_ids.clone();
            item_ids.extend(next_ids.iter().filter(|id| !known.contains(id.as_str())).cloned());
            let mut class = pending_class.merge(*next_class);
            if pending_product != next_product || item_ids.len() > crate::protocol::MAX_RUNTIME_PATCH_ITEMS {
                class = InvalidationClass::Structural;
            }
            Some((class, item_ids))
        }
        _ => None,
    };
    if let Some((merged_class, merged_ids)) = merged
        && let FrontendEvent::TopicEvent {
            event: TopicEventPayload::WorkInvalidated { class, item_ids, .. },
            ..
        } = &mut next.payload
    {
        *class = merged_class;
        *item_ids = merged_ids;
    }
    *pending = next;
}

pub(super) fn topic_event_topic(payload: &FrontendEvent) -> Option<String> {
    match payload {
        FrontendEvent::TopicEvent { topic, .. } => Some(topic.clone()),
        FrontendEvent::OperatorDecisionSnapshot { .. } => Some(super::decision_notifications::TOPIC.to_owned()),
        _ => None,
    }
}

/// Explicit allowlist of session-wide snapshots whose newest payload fully
/// replaces the previous one. Callers must exclude correlated replies. Scoped
/// lists/document results, row deltas, probes and control requests are not
/// interchangeable by kind and must retain every pending envelope.
fn snapshot_event_kind(payload: &FrontendEvent) -> Option<&'static str> {
    match payload {
        // Complete current OAuth state, including any pending device-flow UI.
        FrontendEvent::GitHubAuthState { .. } => Some("git_hub_auth_state"),
        // Every allocated worker slot's live state, not a changed-slot delta.
        FrontendEvent::WorkerLiveStatesList { .. } => Some("worker_live_states_list"),
        // Complete health report, including all current issues and pause flags.
        FrontendEvent::EngineHealthResult { .. } => Some("engine_health_result"),
        // All pool capacities and the coordinator model for this engine.
        FrontendEvent::EnginePoolConfig { .. } => Some("engine_pool_config"),
        // Entire feature registry with effective values; current RPC replies stay separate.
        FrontendEvent::FeatureFlagsList { .. } => Some("feature_flags_list"),
        // Quota refresh includes every driver, including explicit failure entries.
        FrontendEvent::DriverQuotaUsageResult { .. } => Some("driver_quota_usage_result"),
        _ => None,
    }
}

/// Build the marker envelope [`SessionQueue::admit_under_pressure`] injects
/// once it has dropped a pending bulk entry to survive a burst. Carries no
/// meaningful `revision` (the app doesn't use `TopicEvent::revision` for
/// gap detection) — its only job is to tell the app "you may have missed
/// an invalidation; refetch" without tearing down the connection the way a
/// disconnect-and-reconnect would.
fn resync_envelope() -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::TopicEvent {
        topic: RESYNC_TOPIC.to_owned(),
        revision: 0,
        origin_session_id: String::new(),
        origin_request_id: None,
        event: TopicEventPayload::ResyncRequired,
    })
}

/// Whether an outbound envelope belongs in the priority lane. Only small,
/// latency-sensitive engine→app *control* pushes qualify: the `EngineRequest`
/// frames the engine issues via [`ServerState::send_to_app`] (reveal,
/// pane-release, spawn, focus, send-input, interrupt) and then blocks on with
/// a short (~5s) timeout. Everything else — bulk `WorkTree` snapshot
/// responses, `TopicEvent` invalidations, list/result replies, `Hello` —
/// stays in the bulk lane. The predicate is deliberately narrow: the priority
/// lane only helps if it stays nearly empty, so it must not admit the very
/// snapshot/invalidation traffic it lets control pushes jump ahead of.
fn is_priority_event(payload: &FrontendEvent) -> bool {
    matches!(payload, FrontendEvent::EngineRequest { .. })
}

/// The two wakeups a session's queue needs: `writer` signals new work to the
/// writer task; `drained` signals the request reader (paused in
/// [`SessionSink::wait_for_response_headroom`]) that the writer popped an
/// envelope.
#[derive(Default)]
struct SinkWakers {
    writer: Notify,
    drained: Notify,
}

/// Outbound side of one connected session: a bounded coalescing queue plus
/// the shutdown trigger the reader loop selects on. The broker fans
/// invalidations out by calling `enqueue`; the writer task drains via
/// `next`; if either side decides the session is slow or finished, it
/// `close`s the sink and `trigger_shutdown` stops the reader.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct SessionSink {
    metrics: Arc<boss_metrics::Registry>,
    #[builder(default = StdMutex::new(SessionQueue::new()))]
    pub(super) queue: StdMutex<SessionQueue>,
    #[builder(skip)]
    wakers: SinkWakers,
    shutdown: StdMutex<Option<oneshot::Sender<()>>>,
    /// In-flight population-timing traces for this session, keyed by the
    /// envelope `request_id`. The `get_work_tree` handler stashes a partial
    /// trace here (decode + DB + assemble segments) right before enqueueing
    /// its response; the writer task removes it when it pops that response,
    /// appends the `serialize` / `socket_write` / `total` segments, and
    /// flushes. Empty for every non-population request. See
    /// [`crate::population_timing`].
    #[builder(default)]
    pop_traces: StdMutex<HashMap<String, crate::population_timing::PopulationTrace>>,
    #[builder(default)]
    pub(super) board_reads: StdMutex<super::read_freshness::ReadFreshness<(u64, boss_protocol::WorkTree)>>,
    #[builder(default)]
    pub(super) view_reads: StdMutex<super::read_freshness::ReadFreshness<FrontendEventEnvelope>>,
    #[builder(default)]
    pub(super) work_tree_cache: StdMutex<boss_protocol::work_tree_delta::WorkTreeCache>,
    /// One-shot acknowledgements for responses whose producer must know that
    /// the writer has flushed the frame before it can safely tear down the
    /// caller's process tree.
    #[builder(default)]
    delivery_waiters: StdMutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl SessionSink {
    pub(super) fn new(shutdown_tx: oneshot::Sender<()>, metrics: Arc<boss_metrics::Registry>) -> Self {
        Self::builder()
            .shutdown(StdMutex::new(Some(shutdown_tx)))
            .metrics(metrics)
            .build()
    }

    /// Stash a partial population-timing trace to be completed by the writer
    /// task when it sends the response with this `request_id`.
    pub(super) fn stash_population_trace(&self, request_id: &str, trace: crate::population_timing::PopulationTrace) {
        self.pop_traces
            .lock()
            .expect("pop_traces lock poisoned")
            .insert(request_id.to_owned(), trace);
    }

    /// Remove the population-timing trace for `request_id`, if any. Called
    /// by the writer task as it serializes the response.
    pub(super) fn take_population_trace(&self, request_id: &str) -> Option<crate::population_timing::PopulationTrace> {
        self.pop_traces
            .lock()
            .expect("pop_traces lock poisoned")
            .remove(request_id)
    }

    pub(super) fn enqueue(&self, mut env: FrontendEventEnvelope) -> EnqueueOutcome {
        let push_kind = env
            .request_id
            .is_none()
            .then(|| super::frontend_event_kind::kind(&env.payload));
        let mut reads = self.view_reads.lock().expect("view reads poisoned");
        if let Some(id) = env.request_id.clone() {
            if env.payload.is_failed_read_reply() {
                reads.cancel(&id);
            } else {
                env = reads.finish(&id, env);
            }
            env.request_id = Some(id);
        }
        let outcome = self.queue.lock().expect("session queue lock poisoned").enqueue(env);
        if let Some(kind) = push_kind {
            super::frontend_metrics::record_push(&self.metrics, kind, &outcome);
        }
        match outcome {
            EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced | EnqueueOutcome::Degraded => {
                self.wakers.writer.notify_one()
            }
            EnqueueOutcome::Slow => {
                self.close();
                self.trigger_shutdown();
            }
            EnqueueOutcome::Closed => {}
        }
        outcome
    }

    /// Enqueue a response and return a receiver completed by the writer after
    /// the response frame is flushed. `false` means the response could not be
    /// queued or written.
    pub(super) fn enqueue_response_awaiting_delivery(&self, env: FrontendEventEnvelope) -> oneshot::Receiver<bool> {
        let request_id = env.request_id.clone().expect("response envelope has request id");
        let (tx, rx) = oneshot::channel();
        self.delivery_waiters
            .lock()
            .expect("delivery_waiters lock poisoned")
            .insert(request_id.clone(), tx);
        match self.enqueue(env) {
            EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced | EnqueueOutcome::Degraded => {}
            EnqueueOutcome::Closed | EnqueueOutcome::Slow => self.complete_response_delivery(Some(&request_id), false),
        }
        rx
    }

    /// Complete a response-delivery acknowledgement after the writer has
    /// either flushed its frame or encountered a socket failure.
    pub(super) fn complete_response_delivery(&self, request_id: Option<&str>, delivered: bool) {
        if let Some(request_id) = request_id
            && let Some(tx) = self
                .delivery_waiters
                .lock()
                .expect("delivery_waiters lock poisoned")
                .remove(request_id)
        {
            let _ = tx.send(delivered);
        }
    }

    /// Snapshot this session's outbound queue depth, head-of-line age, and
    /// backpressure/closed flags for diagnostics.
    pub(super) fn queue_stats(&self) -> QueueStats {
        self.queue.lock().expect("session queue lock poisoned").stats()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.queue.lock().expect("session queue lock poisoned").closed
    }

    pub(super) fn close(&self) {
        {
            let mut q = self.queue.lock().expect("session queue lock poisoned");
            q.closed = true;
        }
        self.wakers.writer.notify_one();
    }

    pub(super) fn trigger_shutdown(&self) {
        if let Some(tx) = self.shutdown.lock().expect("shutdown lock poisoned").take() {
            let _ = tx.send(());
        }
    }

    /// Wait for the next envelope. Returns `None` once the sink is closed
    /// and the queue is drained.
    pub(super) async fn next(&self) -> Option<FrontendEventEnvelope> {
        loop {
            // Register interest first so a `notify_one` between our queue
            // peek and the await still wakes us.
            let notified = self.wakers.writer.notified();
            let snapshot = {
                let mut q = self.queue.lock().expect("session queue lock poisoned");
                if let Some(env) = q.pop_front() {
                    Some(Some(env))
                } else if q.closed {
                    Some(None)
                } else {
                    None
                }
            };
            match snapshot {
                Some(env_opt) => {
                    self.wakers.drained.notify_one();
                    return env_opt;
                }
                None => notified.await,
            }
        }
    }

    /// Backpressure for the request reader: resolves once the bulk lane has
    /// room for the replies of requests about to be accepted (or the sink is
    /// closed). The caller includes configured active and pending bulk/live
    /// budgets plus the next request. This reduces pressure; enqueue still
    /// enforces reply-or-disconnect when concurrent producers fill the lane.
    pub(super) async fn wait_for_response_headroom(&self, outstanding: usize) {
        loop {
            let drained = self.wakers.drained.notified();
            {
                let q = self.queue.lock().expect("session queue lock poisoned");
                if q.closed || q.items.len() < MAX_SESSION_QUEUE.saturating_sub(outstanding).max(1) {
                    return;
                }
            }
            drained.await;
        }
    }
}

#[derive(Default)]
pub(super) struct TopicBroker {
    pub(super) decision_snapshot: Mutex<Option<Vec<boss_protocol::OperatorDecisionNotification>>>,
    pub(super) inner: Mutex<TopicBrokerInner>,
}

#[derive(Default)]
pub(super) struct TopicBrokerInner {
    pub(super) sinks: HashMap<String, Arc<SessionSink>>,
    pub(super) topics_by_session: HashMap<String, HashSet<String>>,
    pub(super) sessions_by_topic: HashMap<String, HashSet<String>>,
}

impl TopicBroker {
    pub(super) async fn register_session(&self, session_id: &str, sink: Arc<SessionSink>) {
        let mut inner = self.inner.lock().await;
        inner.sinks.insert(session_id.to_owned(), sink);
    }

    pub(super) async fn remove_session(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.sinks.remove(session_id);
        if let Some(topics) = inner.topics_by_session.remove(session_id) {
            for topic in topics {
                if let Some(sessions) = inner.sessions_by_topic.get_mut(&topic) {
                    sessions.remove(session_id);
                    if sessions.is_empty() {
                        inner.sessions_by_topic.remove(&topic);
                    }
                }
            }
        }
    }

    pub(super) async fn subscribe(&self, session_id: &str, topics: &[String]) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let mut added = Vec::new();
        for topic in topics {
            let topic = topic.trim();
            if topic.is_empty() {
                continue;
            }
            let inserted = inner
                .topics_by_session
                .entry(session_id.to_owned())
                .or_default()
                .insert(topic.to_owned());
            inner
                .sessions_by_topic
                .entry(topic.to_owned())
                .or_default()
                .insert(session_id.to_owned());
            if inserted {
                added.push(topic.to_owned());
            }
        }
        if !added.is_empty() {
            tracing::debug!(session_id, topics = ?added, "topic broker: session subscribed");
        }
        added
    }

    pub(super) async fn unsubscribe(&self, session_id: &str, topics: &[String]) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let mut removed = Vec::new();
        for topic in topics {
            let topic = topic.trim();
            if topic.is_empty() {
                continue;
            }
            let session_removed = inner
                .topics_by_session
                .get_mut(session_id)
                .map(|session_topics| session_topics.remove(topic))
                .unwrap_or(false);
            if !session_removed {
                continue;
            }
            if let Some(sessions) = inner.sessions_by_topic.get_mut(topic) {
                sessions.remove(session_id);
                if sessions.is_empty() {
                    inner.sessions_by_topic.remove(topic);
                }
            }
            removed.push(topic.to_owned());
        }

        if matches!(
            inner.topics_by_session.get(session_id),
            Some(topics) if topics.is_empty()
        ) {
            inner.topics_by_session.remove(session_id);
        }

        if !removed.is_empty() {
            tracing::debug!(session_id, topics = ?removed, "topic broker: session unsubscribed");
        }
        removed
    }

    /// Fan an envelope out to every session subscribed to `topic`. A session
    /// whose queue overflows is either gracefully degraded (oldest pending
    /// entry dropped, a resync marker queued) if it's actively draining, or
    /// evicted from the broker and disconnected if it's genuinely stuck —
    /// see [`SessionQueue::admit_under_pressure`] for the distinction.
    /// Invalidations are cheap to replay by resubscribing, so a real wedge
    /// still gets disconnected rather than allowed to balloon engine memory.
    pub(super) async fn publish(&self, topic: &str, envelope: FrontendEventEnvelope) {
        let sinks = {
            let inner = self.inner.lock().await;
            inner
                .sessions_by_topic
                .get(topic)
                .into_iter()
                .flat_map(|sessions| sessions.iter())
                .filter_map(|session_id| {
                    inner
                        .sinks
                        .get(session_id)
                        .map(|sink| (session_id.clone(), sink.clone()))
                })
                .collect::<Vec<_>>()
        };

        // A push with zero recipients means the topic currently has no
        // subscribed session — the event is silently dropped rather than
        // queued, so this is the one line that turns a "missed frontend
        // push" report from forensics into a grep: a
        // `CiRemediationStarted` push vanished during an unsubscribed
        // window and stranded a stale badge for up to 24h.
        if sinks.is_empty() {
            tracing::debug!(topic, "topic broker: publish had no subscribed sessions");
        }

        let mut enqueued_count = 0usize;
        let mut closed_count = 0usize;
        let mut slow = Vec::new();
        for (session_id, sink) in sinks {
            match sink.enqueue(envelope.clone()) {
                EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced => enqueued_count += 1,
                EnqueueOutcome::Closed => closed_count += 1,
                EnqueueOutcome::Degraded => {
                    enqueued_count += 1;
                    let stats = sink.queue_stats();
                    tracing::debug!(
                        session_id = %session_id,
                        topic,
                        queue_depth = stats.depth,
                        oldest_age_ms = stats.oldest_age_ms,
                        "outbound queue full during burst: dropped oldest pending entry, session will resync",
                    );
                }
                EnqueueOutcome::Slow => slow.push((session_id, sink)),
            }
        }

        // Logged after the enqueue loop (not against `sinks.len()`) so the
        // count reflects sessions the event actually reached — a push to a
        // sink whose session already closed no longer inflates this to look
        // like a real delivery (see the finding on PR #2068).
        if enqueued_count > 0 || closed_count > 0 {
            tracing::debug!(topic, enqueued_count, closed_count, "topic broker: publish delivered");
        }

        for (session_id, sink) in slow {
            sink.log_stuck(&session_id, "outbound queue full", Some(topic));
            sink.close();
            sink.trigger_shutdown();
            self.remove_session(&session_id).await;
        }
    }
}
