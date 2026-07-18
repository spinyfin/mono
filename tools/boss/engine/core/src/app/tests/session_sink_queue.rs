use super::*;

#[test]
fn coalesces_same_topic_into_a_single_pending_envelope() {
    let mut q = SessionQueue::new();
    assert_eq!(q.enqueue(topic_envelope("work.products", 1)), EnqueueOutcome::Enqueued);
    assert_eq!(q.enqueue(topic_envelope("work.products", 2)), EnqueueOutcome::Coalesced);
    assert_eq!(q.enqueue(topic_envelope("work.products", 3)), EnqueueOutcome::Coalesced);
    assert_eq!(q.items.len(), 1);
    let env = q.pop_front().unwrap();
    assert_eq!(env.revision, Some(3));
    assert!(q.pop_front().is_none());
}

#[test]
fn does_not_coalesce_across_topics() {
    let mut q = SessionQueue::new();
    q.enqueue(topic_envelope("work.products", 1));
    q.enqueue(topic_envelope("work.product.p1", 2));
    q.enqueue(topic_envelope("work.products", 3));
    assert_eq!(q.items.len(), 2);

    let first = q.pop_front().unwrap();
    let second = q.pop_front().unwrap();
    assert_eq!(topic_of(&first).as_deref(), Some("work.products"));
    assert_eq!(first.revision, Some(3));
    assert_eq!(topic_of(&second).as_deref(), Some("work.product.p1"));
    assert_eq!(second.revision, Some(2));
}

#[test]
fn coalescing_indices_survive_pops_of_other_topics() {
    let mut q = SessionQueue::new();
    q.enqueue(topic_envelope("a", 1));
    q.enqueue(topic_envelope("b", 2));
    // Pop topic "a", then a new "b" event should still coalesce with
    // the earlier "b" sitting at the (now-front) of the queue.
    let popped = q.pop_front().unwrap();
    assert_eq!(topic_of(&popped).as_deref(), Some("a"));
    assert_eq!(q.enqueue(topic_envelope("b", 3)), EnqueueOutcome::Coalesced);
    assert_eq!(q.items.len(), 1);
    assert_eq!(q.pop_front().unwrap().revision, Some(3));
}

/// A client that's actively draining (the head-of-line entry is young) must
/// not be disconnected just because a burst outran the cap for an instant —
/// incident 2026-07-14, where sessions were torn down with `oldest_age_ms`
/// of only ~1.3-1.8s. Overflow instead drops the oldest pending entry (plus
/// a second, to make room for a resync marker admitted alongside it — see
/// `admit_under_pressure`), admits the new envelope, and leaves the client
/// connected.
#[test]
fn enqueue_degrades_gracefully_when_queue_is_full_but_client_is_draining() {
    let mut q = SessionQueue::new();
    // Fill with non-coalescing responses up to the cap. Enqueued back to
    // back like this, the head-of-line entry is always well under
    // `STUCK_CLIENT_AGE_MS`.
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("r-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    assert_eq!(q.enqueue(response_envelope("overflow")), EnqueueOutcome::Degraded);
    assert!(!q.slow, "a draining client must not latch the disconnect flag");
    assert_eq!(q.items.len(), MAX_SESSION_QUEUE, "depth stays bounded at the cap");
    assert!(
        q.pending_topics.contains_key(RESYNC_TOPIC),
        "a resync marker must be pending"
    );
    // r-0 was dropped to make room for the new envelope, and r-1 was
    // dropped to make room for the resync marker admitted alongside it.
    assert_eq!(q.items.front().unwrap().1.request_id.as_deref(), Some("r-2"));
    assert_eq!(q.items.back().unwrap().1.request_id.as_deref(), Some("overflow"));
    // Subsequent enqueues keep degrading rather than disconnecting. The
    // resync marker is already pending, so this overflow drops only one
    // more entry.
    assert_eq!(q.enqueue(response_envelope("after-overflow")), EnqueueOutcome::Degraded);
    assert_eq!(q.items.front().unwrap().1.request_id.as_deref(), Some("r-3"));
}

/// The resync marker must actually reach the client — dropping entries
/// silently would violate "no event loss without a resync".
#[test]
fn degraded_admission_delivers_a_resync_marker() {
    let mut q = SessionQueue::new();
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("r-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    assert_eq!(q.enqueue(response_envelope("overflow")), EnqueueOutcome::Degraded);

    let mut saw_marker = false;
    while let Some(env) = q.pop_front() {
        if let FrontendEvent::TopicEvent { topic, event, .. } = &env.payload
            && topic == RESYNC_TOPIC
        {
            assert!(matches!(event, TopicEventPayload::ResyncRequired));
            saw_marker = true;
        }
    }
    assert!(
        saw_marker,
        "resync marker must actually be delivered, not just tracked internally"
    );
}

/// The mirror case: a head-of-line entry that's genuinely old (the client
/// isn't draining at all, not just momentarily behind a burst) still
/// disconnects — dropping entries indefinitely for a truly wedged client
/// would balloon engine memory forever.
#[test]
fn enqueue_marks_slow_when_client_is_genuinely_stuck() {
    let mut q = SessionQueue::new();
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("r-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
    assert_eq!(q.enqueue(response_envelope("overflow")), EnqueueOutcome::Slow);
    assert!(q.slow);
    // Subsequent enqueues continue to report Slow.
    assert_eq!(q.enqueue(response_envelope("after-overflow")), EnqueueOutcome::Slow);
}

#[test]
fn enqueue_returns_closed_after_close() {
    let mut q = SessionQueue::new();
    q.closed = true;
    assert_eq!(q.enqueue(response_envelope("r-1")), EnqueueOutcome::Closed);
}

/// The `slow` backpressure latch must be recoverable: once a session that
/// briefly overflowed drains its backlog to empty, further enqueues have to
/// succeed again. The old one-way latch permanently dropped every
/// subsequent enqueue with no recovery — the silent-wedge failure mode.
#[test]
fn enqueue_recovers_from_slow_after_draining_to_empty() {
    let mut q = SessionQueue::new();
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("r-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    // Back-date the head-of-line entry to simulate a genuinely stuck
    // client — a merely bursty one degrades gracefully instead of
    // latching `slow` (see `enqueue_degrades_gracefully_...`).
    q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
    // One past the cap latches the slow flag.
    assert_eq!(q.enqueue(response_envelope("overflow")), EnqueueOutcome::Slow);
    assert!(q.slow);

    // Drain all but the last item — the latch persists while a backlog remains.
    for _ in 0..MAX_SESSION_QUEUE - 1 {
        assert!(q.pop_front().is_some());
    }
    assert!(q.slow, "latch must persist while the queue is still draining");

    // Popping the final item empties the queue and clears the latch.
    assert!(q.pop_front().is_some());
    assert!(q.pop_front().is_none());
    assert!(!q.slow, "slow latch must clear once the queue drains to empty");

    // And enqueue works again.
    assert_eq!(q.enqueue(response_envelope("after-recover")), EnqueueOutcome::Enqueued);
}

/// `stats()` reports depth, the backpressure latch, and the closed flag —
/// the fields logged on every send timeout and by the periodic sampler.
#[test]
fn queue_stats_reports_depth_and_backpressure_flags() {
    let mut q = SessionQueue::new();
    let empty = q.stats();
    assert_eq!(empty.depth, 0);
    assert_eq!(empty.oldest_age_ms, 0);
    assert!(!empty.slow && !empty.closed);

    q.enqueue(response_envelope("a"));
    q.enqueue(response_envelope("b"));
    let s = q.stats();
    assert_eq!(s.depth, 2);
    assert!(!s.slow);

    // Fill to exactly the cap, then back-date the head-of-line entry and
    // push one more to latch `slow` (a merely bursty client would instead
    // degrade gracefully — see `enqueue_degrades_gracefully_...`). Depth
    // saturates at the cap either way.
    for i in 0..(MAX_SESSION_QUEUE - 2) {
        q.enqueue(response_envelope(&format!("f-{i}")));
    }
    q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
    q.enqueue(response_envelope("overflow"));
    let full = q.stats();
    assert!(full.slow);
    assert_eq!(full.depth, MAX_SESSION_QUEUE);

    q.closed = true;
    assert!(q.stats().closed);
}

/// The priority lane is the root-cause fix for the `reveal_work_item` /
/// `release_worker_pane` Send(Timeout) incident: a small engine→app control
/// push must be admitted even when the bulk lane is full **and** latched
/// `slow`, and must drain ahead of the bulk backlog.
#[test]
fn priority_event_jumps_ahead_of_saturated_bulk_lane() {
    let mut q = SessionQueue::new();
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("bulk-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    // Simulate a genuinely stuck client so the bulk lane latches `slow`
    // (a merely bursty-but-draining client would instead degrade
    // gracefully without ever latching — see
    // `enqueue_degrades_gracefully_...`).
    q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
    // The bulk lane is full and latched slow — a further *bulk* enqueue is
    // rejected...
    assert_eq!(q.enqueue(response_envelope("bulk-overflow")), EnqueueOutcome::Slow);
    assert!(q.slow);
    // ...but a priority control push is still admitted.
    assert_eq!(q.enqueue(engine_request_envelope("reveal-1")), EnqueueOutcome::Enqueued);

    // And it drains first, ahead of every queued bulk item.
    let first = q.pop_front().expect("priority item drains first");
    assert!(
        matches!(first.payload, FrontendEvent::EngineRequest { .. }),
        "priority EngineRequest must leave before any bulk envelope",
    );
    // The next envelope out is the head of the bulk lane.
    let second = q.pop_front().expect("bulk item follows");
    assert_eq!(second.request_id.as_deref(), Some("bulk-0"));
}

/// A saturated *priority* lane is the genuine wedge — the app isn't draining
/// even tiny control frames. It reports `Slow` (→ `SessionWedged`) without a
/// coalescing latch, and recovers as soon as one entry drains.
#[test]
fn priority_lane_reports_slow_when_full_and_recovers_on_drain() {
    let mut q = SessionQueue::new();
    for i in 0..MAX_PRIORITY_QUEUE {
        assert_eq!(
            q.enqueue(engine_request_envelope(&format!("p-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    // One past the cap reports Slow.
    assert_eq!(q.enqueue(engine_request_envelope("overflow")), EnqueueOutcome::Slow);
    // Draining one entry frees a slot — the lane recovers with no latch to clear.
    assert!(q.pop_front().is_some());
    assert_eq!(
        q.enqueue(engine_request_envelope("after-drain")),
        EnqueueOutcome::Enqueued
    );
}

/// `stats()` reports priority-lane depth separately from the combined depth so
/// an operator can watch the priority lane stay shallow while the bulk lane
/// backs up (the fix working) — or climb (the app wedged on control frames
/// too).
#[test]
fn queue_stats_reports_priority_depth_separately() {
    let mut q = SessionQueue::new();
    q.enqueue(engine_request_envelope("p-1"));
    q.enqueue(engine_request_envelope("p-2"));
    q.enqueue(response_envelope("b-1"));
    let s = q.stats();
    assert_eq!(s.priority_depth, 2, "priority lane depth reported on its own");
    assert_eq!(s.depth, 3, "combined depth spans both lanes");
}

#[tokio::test]
async fn sink_next_drains_queue_and_returns_none_when_closed() {
    let (tx, _rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));
    sink.enqueue(response_envelope("r-1"));
    sink.enqueue(response_envelope("r-2"));
    sink.close();

    let first = sink.next().await.expect("first envelope");
    assert_eq!(first.request_id.as_deref(), Some("r-1"));
    let second = sink.next().await.expect("second envelope");
    assert_eq!(second.request_id.as_deref(), Some("r-2"));
    assert!(sink.next().await.is_none());
}

#[tokio::test]
async fn sink_close_wakes_pending_next_call() {
    let (tx, _rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));
    let waiter = sink.clone();
    let join = tokio::spawn(async move { waiter.next().await });
    // Give the spawned task time to enter notified().await.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    sink.close();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), join)
        .await
        .expect("close should wake next()");
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn broker_publish_disconnects_slow_subscriber() {
    let (tx, mut rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));

    // Pre-fill the sink past capacity by injecting non-coalescing entries
    // (responses are not coalesced) without ever draining, then back-date
    // the head-of-line entry so the next overflow reads as a genuinely
    // stuck client rather than a graceful degrade (see
    // `enqueue_degrades_gracefully_...`).
    {
        let mut q = sink.queue.lock().unwrap();
        for i in 0..MAX_SESSION_QUEUE {
            let outcome = q.enqueue(response_envelope(&format!("r-{i}")));
            assert_eq!(outcome, EnqueueOutcome::Enqueued);
        }
        q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
    }

    let broker = TopicBroker::default();
    broker.register_session("session-1", sink.clone()).await;
    broker.subscribe("session-1", &["work.products".to_owned()]).await;

    // Publishing one more event should overflow and trigger shutdown.
    broker
        .publish("work.products", topic_envelope("work.products", 99))
        .await;

    let shutdown = tokio::time::timeout(std::time::Duration::from_secs(1), &mut rx)
        .await
        .expect("shutdown should fire");
    assert!(shutdown.is_ok());

    // Broker should also have evicted the session.
    let inner = broker.inner.lock().await;
    assert!(!inner.sinks.contains_key("session-1"));
    assert!(!inner.sessions_by_topic.contains_key("work.products"));
}

/// The 2026-07-14 incident: a session whose queue is full but whose
/// head-of-line entry is fresh (a burst, not a wedge) must stay connected.
/// `TopicBroker::publish` is the exact call site that used to disconnect it.
#[tokio::test]
async fn broker_publish_degrades_bursty_subscriber_without_disconnecting() {
    let (tx, mut rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));

    // Pre-fill the sink past capacity with fresh, back-to-back entries —
    // the "bursty but draining" case, deliberately not back-dated.
    {
        let mut q = sink.queue.lock().unwrap();
        for i in 0..MAX_SESSION_QUEUE {
            let outcome = q.enqueue(response_envelope(&format!("r-{i}")));
            assert_eq!(outcome, EnqueueOutcome::Enqueued);
        }
    }

    let broker = TopicBroker::default();
    broker.register_session("session-1", sink.clone()).await;
    broker.subscribe("session-1", &["work.products".to_owned()]).await;

    broker
        .publish("work.products", topic_envelope("work.products", 99))
        .await;

    // No shutdown fires — the session stays connected and registered.
    let shutdown = tokio::time::timeout(std::time::Duration::from_millis(200), &mut rx).await;
    assert!(shutdown.is_err(), "a draining session must not be disconnected");
    let inner = broker.inner.lock().await;
    assert!(inner.sinks.contains_key("session-1"));
}
