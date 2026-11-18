// Unit tests for `TopicBroker`'s subscription-routing contract:
// `subscribe` / `unsubscribe` / `remove_session` / `publish`.
//
// The two publish-overflow paths are covered in `session_sink_queue.rs`
// (`broker_publish_disconnects_slow_subscriber`,
// `broker_publish_degrades_bursty_subscriber_without_disconnecting`); the
// routing itself was only exercised incidentally by integration tests.
//
// These assert on the observable contract only — the values `subscribe` /
// `unsubscribe` return, and which envelopes actually land in each session's
// sink. They deliberately do not inspect `TopicBrokerInner`'s
// `sinks` / `topics_by_session` / `sessions_by_topic` maps: those are the
// implementation of the routing, not the behaviour it owes callers, and
// pinning them would make every future re-indexing a test change.

use super::*;

/// Drain everything queued on `sink`, returning the `(topic, revision)` of
/// each `TopicEvent` in delivery order. Closes the sink first so the drain
/// terminates deterministically (`SessionSink::next` yields `None` once a
/// closed sink is empty) rather than relying on a timeout. Call this last in
/// a test: a closed sink accepts no further enqueues.
async fn drain_topic_events(sink: &SessionSink) -> Vec<(String, u64)> {
    sink.close();
    let mut delivered = Vec::new();
    while let Some(env) = sink.next().await {
        if let FrontendEvent::TopicEvent { topic, revision, .. } = &env.payload {
            delivered.push((topic.clone(), *revision));
        }
    }
    delivered
}

fn topics(topics: &[&str]) -> Vec<String> {
    topics.iter().map(|t| (*t).to_owned()).collect()
}

#[tokio::test]
async fn broker_subscribe_returns_only_newly_added_topics() {
    let broker = TopicBroker::default();
    broker.register_session("session-1", make_session_sink()).await;

    let first = broker
        .subscribe("session-1", &topics(&["work.products", "work.tasks"]))
        .await;
    assert_eq!(
        first,
        topics(&["work.products", "work.tasks"]),
        "a fresh batch is all new"
    );

    let repeat = broker.subscribe("session-1", &topics(&["work.products"])).await;
    assert!(
        repeat.is_empty(),
        "re-subscribing an already-held topic adds nothing: {repeat:?}"
    );

    let mixed = broker
        .subscribe("session-1", &topics(&["work.products", "work.projects"]))
        .await;
    assert_eq!(
        mixed,
        topics(&["work.projects"]),
        "a mixed batch reports only the topics it actually added"
    );
}

/// Subscribing is per-session: one session already holding a topic must not
/// make another session's subscribe to the same topic look like a no-op.
#[tokio::test]
async fn broker_subscribe_reports_new_topics_per_session() {
    let broker = TopicBroker::default();
    broker.register_session("session-1", make_session_sink()).await;
    broker.register_session("session-2", make_session_sink()).await;

    assert_eq!(
        broker.subscribe("session-1", &topics(&["work.products"])).await,
        topics(&["work.products"])
    );
    assert_eq!(
        broker.subscribe("session-2", &topics(&["work.products"])).await,
        topics(&["work.products"]),
        "a second session subscribing to a held topic is new *for that session*"
    );
}

#[tokio::test]
async fn broker_subscribe_ignores_blank_topics_and_trims_whitespace() {
    let broker = TopicBroker::default();
    let sink = make_session_sink();
    broker.register_session("session-1", sink.clone()).await;

    let added = broker
        .subscribe("session-1", &topics(&["", "   ", "\t\n", "  work.products  "]))
        .await;
    assert_eq!(
        added,
        topics(&["work.products"]),
        "empty and whitespace-only topics are dropped; the padded one is trimmed"
    );

    // Trimming is normalization, not just cosmetic on the return value: the
    // padded topic is stored under its trimmed name, so it both collides with
    // a plain re-subscribe and receives publishes addressed to the plain name.
    assert!(
        broker
            .subscribe("session-1", &topics(&["work.products"]))
            .await
            .is_empty(),
        "the trimmed topic is what got stored"
    );
    broker
        .publish("work.products", topic_envelope("work.products", 7))
        .await;

    assert_eq!(
        drain_topic_events(&sink).await,
        vec![("work.products".to_owned(), 7)],
        "a topic subscribed with surrounding whitespace still receives publishes"
    );
}

#[tokio::test]
async fn broker_unsubscribe_returns_only_topics_actually_removed() {
    let broker = TopicBroker::default();
    broker.register_session("session-1", make_session_sink()).await;
    broker
        .subscribe("session-1", &topics(&["work.products", "work.tasks"]))
        .await;

    let removed = broker
        .unsubscribe("session-1", &topics(&["work.products", "work.never-held"]))
        .await;
    assert_eq!(
        removed,
        topics(&["work.products"]),
        "only the held topic is reported removed; one never subscribed is skipped"
    );

    let again = broker.unsubscribe("session-1", &topics(&["work.products"])).await;
    assert!(again.is_empty(), "unsubscribing twice removes nothing: {again:?}");

    let unknown_session = broker.unsubscribe("session-unknown", &topics(&["work.tasks"])).await;
    assert!(
        unknown_session.is_empty(),
        "a session that holds no topics removes nothing: {unknown_session:?}"
    );
}

#[tokio::test]
async fn broker_unsubscribe_stops_delivery_on_that_topic_only() {
    let broker = TopicBroker::default();
    let sink = make_session_sink();
    broker.register_session("session-1", sink.clone()).await;
    broker
        .subscribe("session-1", &topics(&["work.products", "work.tasks"]))
        .await;

    broker.unsubscribe("session-1", &topics(&["work.products"])).await;
    broker
        .publish("work.products", topic_envelope("work.products", 1))
        .await;
    broker.publish("work.tasks", topic_envelope("work.tasks", 2)).await;

    assert_eq!(
        drain_topic_events(&sink).await,
        vec![("work.tasks".to_owned(), 2)],
        "the unsubscribed topic no longer lands; the retained subscription still delivers"
    );
}

#[tokio::test]
async fn broker_remove_session_stops_delivery_without_disturbing_other_sessions() {
    let broker = TopicBroker::default();
    let removed_sink = make_session_sink();
    let kept_sink = make_session_sink();
    broker.register_session("session-1", removed_sink.clone()).await;
    broker.register_session("session-2", kept_sink.clone()).await;

    broker
        .subscribe("session-1", &topics(&["work.shared", "work.solo"]))
        .await;
    broker.subscribe("session-2", &topics(&["work.shared"])).await;

    broker.remove_session("session-1").await;

    broker.publish("work.shared", topic_envelope("work.shared", 1)).await;
    broker.publish("work.solo", topic_envelope("work.solo", 2)).await;

    assert!(
        drain_topic_events(&removed_sink).await.is_empty(),
        "a removed session receives nothing on any topic it held"
    );
    assert_eq!(
        drain_topic_events(&kept_sink).await,
        vec![("work.shared".to_owned(), 1)],
        "another session on the same topic keeps its subscription"
    );
}

#[tokio::test]
async fn broker_publish_fans_out_to_every_subscriber_and_no_one_else() {
    let broker = TopicBroker::default();
    let first = make_session_sink();
    let second = make_session_sink();
    let bystander = make_session_sink();
    broker.register_session("session-1", first.clone()).await;
    broker.register_session("session-2", second.clone()).await;
    broker.register_session("session-3", bystander.clone()).await;

    broker.subscribe("session-1", &topics(&["work.products"])).await;
    broker.subscribe("session-2", &topics(&["work.products"])).await;
    broker.subscribe("session-3", &topics(&["work.tasks"])).await;

    broker
        .publish("work.products", topic_envelope("work.products", 5))
        .await;

    let expected = vec![("work.products".to_owned(), 5)];
    assert_eq!(
        drain_topic_events(&first).await,
        expected,
        "first subscriber gets the event"
    );
    assert_eq!(
        drain_topic_events(&second).await,
        expected,
        "one envelope fans out to every subscriber"
    );
    assert!(
        drain_topic_events(&bystander).await.is_empty(),
        "a session subscribed only to another topic receives nothing"
    );
}

/// A registered session with no subscriptions, and a topic nobody holds, are
/// both silent no-ops rather than errors.
#[tokio::test]
async fn broker_publish_to_unsubscribed_topic_delivers_nothing() {
    let broker = TopicBroker::default();
    let sink = make_session_sink();
    broker.register_session("session-1", sink.clone()).await;

    broker
        .publish("work.nobody-listening", topic_envelope("work.nobody-listening", 1))
        .await;

    assert!(
        drain_topic_events(&sink).await.is_empty(),
        "publishing to a topic with no subscribers reaches no session"
    );
}
