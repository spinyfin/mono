use std::sync::Arc;

use crate::{Event, EventBus, EventKind, Registry, TopicFilter};

#[tokio::test]
async fn subscriber_receives_matching_event() {
    let bus = EventBus::new();
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::ProjectImplDrained));

    bus.publish(Event::ProjectImplDrained {
        project_id: "proj_1".to_string(),
    });

    let event = sub.recv().await.expect("expected an event");
    assert_eq!(
        event,
        Event::ProjectImplDrained {
            project_id: "proj_1".to_string(),
        }
    );
}

#[tokio::test]
async fn subscriber_does_not_receive_non_matching_event() {
    let bus = EventBus::new();
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::ProjectImplDrained));

    bus.publish(Event::HostDisabled {
        host_id: "host_1".to_string(),
    });
    bus.publish(Event::ProjectImplDrained {
        project_id: "proj_1".to_string(),
    });

    // The HostDisabled event above must not show up here.
    let event = sub.recv().await.expect("expected an event");
    assert_eq!(
        event,
        Event::ProjectImplDrained {
            project_id: "proj_1".to_string(),
        }
    );
}

#[tokio::test]
async fn fans_out_to_multiple_subscribers() {
    let bus = EventBus::new();
    let mut sub_a = bus.subscribe(TopicFilter::kind(EventKind::DispatchReady));
    let mut sub_b = bus.subscribe(TopicFilter::kind(EventKind::DispatchReady));

    bus.publish(Event::DispatchReady);

    assert_eq!(sub_a.recv().await, Some(Event::DispatchReady));
    assert_eq!(sub_b.recv().await, Some(Event::DispatchReady));
}

#[tokio::test]
async fn publish_with_no_subscribers_does_not_panic() {
    let bus = EventBus::new();
    bus.publish(Event::DispatchReady);
}

#[tokio::test]
async fn topic_filter_with_multiple_kinds_matches_any() {
    let bus = EventBus::new();
    let mut sub = bus.subscribe(TopicFilter::kinds([
        EventKind::PrMerged,
        EventKind::PrReconcileRequested,
    ]));

    bus.publish(Event::PrReconcileRequested {
        pr_url: "https://example.invalid/pr/1".to_string(),
    });
    bus.publish(Event::PrMerged {
        pr_url: "https://example.invalid/pr/1".to_string(),
        task_id: "task_1".to_string(),
    });

    assert_eq!(
        sub.recv().await,
        Some(Event::PrReconcileRequested {
            pr_url: "https://example.invalid/pr/1".to_string(),
        })
    );
    assert_eq!(
        sub.recv().await,
        Some(Event::PrMerged {
            pr_url: "https://example.invalid/pr/1".to_string(),
            task_id: "task_1".to_string(),
        })
    );
}

#[tokio::test]
async fn full_mailbox_drops_event_instead_of_blocking() {
    let bus = EventBus::new();
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::DispatchReady), 1);

    // Mailbox capacity is 1: the first publish fills it, the second is
    // dropped rather than blocking the publisher.
    bus.publish(Event::DispatchReady);
    bus.publish(Event::DispatchReady);

    assert_eq!(sub.recv().await, Some(Event::DispatchReady));
}

#[tokio::test]
async fn full_mailbox_coalesces_same_key_event_instead_of_dropping() {
    // Capacity 1, filled with a pending event for task_1. A second event
    // for the *same* task overwrites the pending one (newest wins) rather
    // than being dropped -- both events describe the same entity, so the
    // subscriber loses nothing by only ever seeing the latest.
    let registry = Arc::new(Registry::new());
    let bus = EventBus::with_metrics(registry.clone());
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::TaskTerminal), 1);

    bus.publish(Event::TaskTerminal {
        task_id: "task_1".to_string(),
        project_id: "proj_1".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "task_1".to_string(),
        project_id: "proj_2".to_string(),
    });

    let event = sub.recv().await.expect("expected the coalesced event");
    assert_eq!(
        event,
        Event::TaskTerminal {
            task_id: "task_1".to_string(),
            project_id: "proj_2".to_string(),
        },
        "newest event for the coalesce key must win"
    );
    assert_eq!(
        registry.counter_value("bus_events_dropped_total.task_terminal"),
        None,
        "a successful coalesce must not count as a drop"
    );
}

#[tokio::test]
async fn full_mailbox_drops_distinct_key_event_and_counts_it() {
    // Capacity 1, filled with a pending event for task_1. A distinct
    // task_2 event has no pending slot to coalesce into, so it is
    // dropped and the per-topic drop counter increments.
    let registry = Arc::new(Registry::new());
    let bus = EventBus::with_metrics(registry.clone());
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::TaskTerminal), 1);

    bus.publish(Event::TaskTerminal {
        task_id: "task_1".to_string(),
        project_id: "proj_1".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "task_2".to_string(),
        project_id: "proj_2".to_string(),
    });

    let event = sub.recv().await.expect("expected the surviving event");
    assert_eq!(
        event,
        Event::TaskTerminal {
            task_id: "task_1".to_string(),
            project_id: "proj_1".to_string(),
        },
        "the pending event must survive; the distinct-key one was dropped"
    );
    assert_eq!(
        registry.counter_value("bus_events_dropped_total.task_terminal"),
        Some(1)
    );
}

#[tokio::test]
async fn coalescing_keeps_up_under_a_pressure_burst() {
    // A burst of same-key events into a capacity-1 mailbox never grows
    // the queue and never drops: every publish either fills the empty
    // slot or coalesces into it.
    let registry = Arc::new(Registry::new());
    let bus = EventBus::with_metrics(registry.clone());
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::ExecutionTerminal), 1);

    for i in 0..50 {
        bus.publish(Event::ExecutionTerminal {
            execution_id: "exec_1".to_string(),
            task_id: format!("task_{i}"),
            host_id: "host_1".to_string(),
            pool_claim: None,
        });
    }

    let event = sub.recv().await.expect("expected the last coalesced event");
    assert_eq!(
        event,
        Event::ExecutionTerminal {
            execution_id: "exec_1".to_string(),
            task_id: "task_49".to_string(),
            host_id: "host_1".to_string(),
            pool_claim: None,
        }
    );
    assert_eq!(
        registry.counter_value("bus_events_dropped_total.execution_terminal"),
        None,
        "a same-key burst must coalesce, never drop"
    );
}

#[tokio::test]
async fn drop_counter_is_independent_per_topic() {
    let registry = Arc::new(Registry::new());
    let bus = EventBus::with_metrics(registry.clone());
    let mut task_sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::TaskTerminal), 1);
    let mut host_sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::HostDisabled), 1);

    for task_id in ["task_1", "task_2", "task_3"] {
        bus.publish(Event::TaskTerminal {
            task_id: task_id.to_string(),
            project_id: "proj_1".to_string(),
        });
    }
    bus.publish(Event::HostDisabled {
        host_id: "host_1".to_string(),
    });
    bus.publish(Event::HostDisabled {
        host_id: "host_2".to_string(),
    });

    assert_eq!(
        registry.counter_value("bus_events_dropped_total.task_terminal"),
        Some(2)
    );
    assert_eq!(
        registry.counter_value("bus_events_dropped_total.host_disabled"),
        Some(1)
    );

    // Drain so the subscriptions don't get flagged as unused.
    assert!(task_sub.recv().await.is_some());
    assert!(host_sub.recv().await.is_some());
}

#[tokio::test]
async fn without_metrics_dropped_events_do_not_panic() {
    // `EventBus::new` (no registry) must tolerate drops silently -- unit
    // tests and any caller that doesn't care about metrics shouldn't be
    // forced to wire one up.
    let bus = EventBus::new();
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::DispatchReady), 0);

    bus.publish(Event::DispatchReady);

    // Capacity 0: nothing was ever queued.
    drop(bus);
    assert_eq!(sub.recv().await, None);
}

#[tokio::test]
async fn subscription_recv_returns_none_after_bus_dropped_and_drained() {
    let bus = EventBus::new();
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::DispatchReady));

    bus.publish(Event::DispatchReady);
    drop(bus);

    // The queued event is still delivered...
    assert_eq!(sub.recv().await, Some(Event::DispatchReady));
    // ...but once drained, a dropped bus resolves further recv()s to None
    // instead of hanging forever with no producer left.
    assert_eq!(sub.recv().await, None);
}
