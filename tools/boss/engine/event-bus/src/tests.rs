use crate::{Event, EventBus, EventKind, TopicFilter};

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
