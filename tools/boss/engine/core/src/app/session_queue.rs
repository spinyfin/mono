//! Per-session outbound event queue and topic fan-out broker.
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.
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

/// How long the head-of-line bulk envelope must have waited before we treat
/// a full queue as a genuinely stuck client rather than a transient burst.
/// Incident 2026-07-14: sessions were disconnected with `oldest_age_ms` of
/// only ~1.3-1.8s — the client was actively draining, just slower than a
/// merge-poller sweep's publish rate for a couple seconds. A real wedge (app
/// not reading its socket at all) blows well past this within one sweep
/// interval, so 5s comfortably separates "bursty but alive" from "stuck"
/// without meaningfully delaying detection of an actually-dead client.
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
    /// The bulk lane is full and the client is genuinely stuck (the
    /// head-of-line envelope has waited past [`STUCK_CLIENT_AGE_MS`]).
    /// Callers disconnect the session on this outcome.
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

pub(super) struct SessionQueue {
    /// Priority lane: small engine→app control pushes (`EngineRequest`)
    /// only, drained ahead of everything in `items`. Each entry is
    /// `(enqueued_at, envelope)`. Not coalesced — each `EngineRequest` is a
    /// distinct request awaiting its own reply — and bounded independently
    /// by [`MAX_PRIORITY_QUEUE`], so a saturated bulk lane never blocks (or
    /// wedges) a reveal / pane-release. See [`is_priority_event`].
    pub(super) priority: VecDeque<(Instant, FrontendEventEnvelope)>,
    /// Bulk lane: everything that isn't a priority control push — `WorkTree`
    /// snapshot responses, `TopicEvent` invalidations, list/result replies,
    /// the `Hello` handshake. Each entry is `(enqueued_at, envelope)`. The
    /// instant is stamped at enqueue time (preserved across coalesce) so
    /// [`SessionQueue::stats`] can report how long the head-of-line envelope
    /// has been waiting.
    pub(super) items: VecDeque<(Instant, FrontendEventEnvelope)>,
    /// For each topic with a pending unsent TopicEvent, the index of that
    /// envelope in `items` (front-relative; decremented on pop). Lets us
    /// overwrite stale invalidations instead of growing the queue.
    pub(super) pending_topics: HashMap<String, usize>,
    pub(super) closed: bool,
    /// One-shot backpressure latch. Set when an enqueue overflows
    /// `MAX_SESSION_QUEUE`; while set, further enqueues report `Slow`
    /// without growing memory. Recoverable: [`SessionQueue::pop_front`]
    /// clears it once the queue drains to empty, so a session that
    /// briefly overflowed but then caught up accepts events again instead
    /// of silently dropping every subsequent enqueue forever.
    pub(super) slow: bool,
}

impl SessionQueue {
    pub(super) fn new() -> Self {
        Self {
            priority: VecDeque::new(),
            items: VecDeque::new(),
            pending_topics: HashMap::new(),
            closed: false,
            slow: false,
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

        if let Some(topic) = topic_event_topic(&env.payload) {
            if let Some(&idx) = self.pending_topics.get(&topic) {
                debug_assert!(idx < self.items.len());
                // Overwrite the stale invalidation in place, keeping the
                // original enqueue instant so `oldest_age_ms` still reflects
                // how long this queue slot has been waiting to flush.
                self.items[idx].1 = env;
                return EnqueueOutcome::Coalesced;
            }
            if self.items.len() >= MAX_SESSION_QUEUE {
                return self.admit_under_pressure(env, Some(topic));
            }
            let idx = self.items.len();
            self.items.push_back((Instant::now(), env));
            self.pending_topics.insert(topic, idx);
            return EnqueueOutcome::Enqueued;
        }

        if self.items.len() >= MAX_SESSION_QUEUE {
            return self.admit_under_pressure(env, None);
        }
        self.items.push_back((Instant::now(), env));
        EnqueueOutcome::Enqueued
    }

    /// Called once the bulk lane is at [`MAX_SESSION_QUEUE`] and a new
    /// envelope needs a slot. Distinguishes a genuinely stuck client from a
    /// transient publish burst by the head-of-line envelope's age (incident
    /// 2026-07-14: sessions were torn down mid-burst with `oldest_age_ms`
    /// of only ~1.3-1.8s, i.e. the client was draining fine, just not fast
    /// enough for an instant). A stuck client (past [`STUCK_CLIENT_AGE_MS`])
    /// still gets `Slow`, which callers turn into a disconnect. A bursty-but-
    /// alive client instead has its oldest pending entry dropped to make
    /// room — bounded, O(1), never grows the queue past `MAX_SESSION_QUEUE`
    /// — and a [`RESYNC_TOPIC`] marker is admitted alongside it (dropping a
    /// second entry to make room if one isn't already pending) so the
    /// client knows to refetch rather than silently miss the dropped
    /// topic(s). The marker coalesces like any other topic (at most one
    /// pending at a time), so a sustained burst that keeps degrading before
    /// the marker is ever delivered doesn't queue more than one.
    fn admit_under_pressure(&mut self, env: FrontendEventEnvelope, topic: Option<String>) -> EnqueueOutcome {
        let oldest_age_ms = self
            .items
            .front()
            .map(|(enqueued_at, _)| Instant::now().saturating_duration_since(*enqueued_at).as_millis() as u64)
            .unwrap_or(0);
        if oldest_age_ms >= STUCK_CLIENT_AGE_MS {
            self.slow = true;
            return EnqueueOutcome::Slow;
        }

        self.evict_oldest_bulk();
        if !self.pending_topics.contains_key(RESYNC_TOPIC) {
            self.evict_oldest_bulk();
            let idx = self.items.len();
            self.items.push_back((Instant::now(), resync_envelope()));
            self.pending_topics.insert(RESYNC_TOPIC.to_owned(), idx);
        }

        let idx = self.items.len();
        self.items.push_back((Instant::now(), env));
        if let Some(topic) = topic {
            self.pending_topics.insert(topic, idx);
        }
        EnqueueOutcome::Degraded
    }

    /// Pop the oldest bulk-lane entry (if any), keeping `pending_topics`
    /// indices front-relative. Shared by [`SessionQueue::pop_front`]'s
    /// normal drain and [`SessionQueue::admit_under_pressure`]'s
    /// drop-oldest burst handling.
    fn evict_oldest_bulk(&mut self) -> Option<(Instant, FrontendEventEnvelope)> {
        let popped = self.items.pop_front()?;
        let mut next = HashMap::with_capacity(self.pending_topics.len());
        for (topic, idx) in self.pending_topics.drain() {
            if idx == 0 {
                continue;
            }
            next.insert(topic, idx - 1);
        }
        self.pending_topics = next;
        Some(popped)
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

pub(super) fn topic_event_topic(payload: &FrontendEvent) -> Option<String> {
    match payload {
        FrontendEvent::TopicEvent { topic, .. } => Some(topic.clone()),
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

/// Outbound side of one connected session: a bounded coalescing queue plus
/// the shutdown trigger the reader loop selects on. The broker fans
/// invalidations out by calling `enqueue`; the writer task drains via
/// `next`; if either side decides the session is slow or finished, it
/// `close`s the sink and `trigger_shutdown` stops the reader.
pub(super) struct SessionSink {
    pub(super) queue: StdMutex<SessionQueue>,
    notify: Notify,
    shutdown: StdMutex<Option<oneshot::Sender<()>>>,
    /// In-flight population-timing traces for this session, keyed by the
    /// envelope `request_id`. The `get_work_tree` handler stashes a partial
    /// trace here (decode + DB + assemble segments) right before enqueueing
    /// its response; the writer task removes it when it pops that response,
    /// appends the `serialize` / `socket_write` / `total` segments, and
    /// flushes. Empty for every non-population request. See
    /// [`crate::population_timing`].
    pop_traces: StdMutex<HashMap<String, crate::population_timing::PopulationTrace>>,
}

impl SessionSink {
    pub(super) fn new(shutdown_tx: oneshot::Sender<()>) -> Self {
        Self {
            queue: StdMutex::new(SessionQueue::new()),
            notify: Notify::new(),
            shutdown: StdMutex::new(Some(shutdown_tx)),
            pop_traces: StdMutex::new(HashMap::new()),
        }
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

    pub(super) fn enqueue(&self, env: FrontendEventEnvelope) -> EnqueueOutcome {
        let outcome = {
            let mut q = self.queue.lock().expect("session queue lock poisoned");
            q.enqueue(env)
        };
        match outcome {
            EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced | EnqueueOutcome::Degraded => self.notify.notify_one(),
            EnqueueOutcome::Closed | EnqueueOutcome::Slow => {}
        }
        outcome
    }

    /// Snapshot this session's outbound queue depth, head-of-line age, and
    /// backpressure/closed flags for diagnostics.
    pub(super) fn queue_stats(&self) -> QueueStats {
        self.queue.lock().expect("session queue lock poisoned").stats()
    }

    pub(super) fn close(&self) {
        {
            let mut q = self.queue.lock().expect("session queue lock poisoned");
            q.closed = true;
        }
        self.notify.notify_one();
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
            let notified = self.notify.notified();
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
                Some(env_opt) => return env_opt,
                None => notified.await,
            }
        }
    }
}

#[derive(Default)]
pub(super) struct TopicBroker {
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
        // push" report from forensics into a grep (see T2764: a
        // `CiRemediationStarted` push vanished during an unsubscribed
        // window and stranded a stale badge for up to 24h).
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
            let stats = sink.queue_stats();
            tracing::warn!(
                session_id = %session_id,
                topic,
                queue_depth = stats.depth,
                priority_depth = stats.priority_depth,
                oldest_age_ms = stats.oldest_age_ms,
                "slow subscriber: outbound queue full, disconnecting"
            );
            sink.close();
            sink.trigger_shutdown();
            self.remove_session(&session_id).await;
        }
    }
}
