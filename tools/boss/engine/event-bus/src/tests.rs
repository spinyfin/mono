use std::sync::Arc;
use std::time::Duration;

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

// `full_mailbox_drops_event_instead_of_blocking` (which used to publish
// two identical events into a capacity-1 mailbox) is removed: under the
// always-coalesce behaviour those two events coalesce rather than drop,
// so the case is now covered by `full_mailbox_drops_distinct_key_event_and_counts_it`
// below, which uses two distinct keys and also asserts the drop counter.

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
async fn pending_index_rebases_correctly_after_a_pop_with_multiple_keys_resident() {
    // Exercises `Mailbox::pop_front_locked`'s index bookkeeping with more
    // than one event resident at once -- a wrong-index bug here would
    // otherwise slip through every other test, none of which has more
    // than one key in the mailbox simultaneously.
    let registry = Arc::new(Registry::new());
    let bus = EventBus::with_metrics(registry.clone());
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::TaskTerminal), 3);

    // Fill all three slots: A, B, C (in that order).
    bus.publish(Event::TaskTerminal {
        task_id: "A".to_string(),
        project_id: "p_A1".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "B".to_string(),
        project_id: "p_B1".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "C".to_string(),
        project_id: "p_C1".to_string(),
    });

    // Drain A, freeing its slot and shifting B and C's front-relative
    // positions down by one.
    assert_eq!(
        sub.recv().await,
        Some(Event::TaskTerminal {
            task_id: "A".to_string(),
            project_id: "p_A1".to_string(),
        })
    );

    // A fresh event for A gets a brand-new slot (queued behind B and C);
    // a newer event for C coalesces into C's shifted slot in place.
    bus.publish(Event::TaskTerminal {
        task_id: "A".to_string(),
        project_id: "p_A2".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "C".to_string(),
        project_id: "p_C2".to_string(),
    });

    assert_eq!(
        sub.recv().await,
        Some(Event::TaskTerminal {
            task_id: "B".to_string(),
            project_id: "p_B1".to_string(),
        })
    );
    assert_eq!(
        sub.recv().await,
        Some(Event::TaskTerminal {
            task_id: "C".to_string(),
            project_id: "p_C2".to_string(),
        }),
        "C's slot must hold the newer coalesced event"
    );
    assert_eq!(
        sub.recv().await,
        Some(Event::TaskTerminal {
            task_id: "A".to_string(),
            project_id: "p_A2".to_string(),
        }),
        "A must have gotten a fresh slot after its first event was drained"
    );
    assert_eq!(
        registry.counter_value("bus_events_dropped_total.task_terminal"),
        None,
        "every event here either queued into a free slot or coalesced -- nothing was dropped"
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
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::TaskTerminal), 1);

    // Capacity 1, filled with task_1's pending event. task_2 is a
    // distinct key with no pending slot to coalesce into, so it is
    // dropped -- without a registry wired up, that must not panic.
    bus.publish(Event::TaskTerminal {
        task_id: "task_1".to_string(),
        project_id: "proj_1".to_string(),
    });
    bus.publish(Event::TaskTerminal {
        task_id: "task_2".to_string(),
        project_id: "proj_2".to_string(),
    });

    drop(bus);
    assert_eq!(
        sub.recv().await,
        Some(Event::TaskTerminal {
            task_id: "task_1".to_string(),
            project_id: "proj_1".to_string(),
        })
    );
    assert_eq!(sub.recv().await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn recv_parked_before_drop_still_wakes_up_with_none() {
    // Unlike `subscription_recv_returns_none_after_bus_dropped_and_drained`
    // (which drops the bus before `recv` is ever polled), this spawns
    // `recv` on its own worker thread and gives it real wall-clock time
    // to register with `Notify` and genuinely park -- queue empty, bus
    // not yet closed -- before the bus is dropped from the main task.
    // `Mailbox::close` must still wake it, or this hangs and the test
    // times out. Note this cannot deterministically hit the specific
    // lock-release-then-register interleaving that made the lost-wakeup
    // bug possible -- that window is far too narrow for a wall-clock
    // sleep to land in reliably -- so it does not by itself prove that
    // bug fixed; `Mailbox::close`'s use of `notify_one` (which stores a
    // permit even for a not-yet-registered waiter, unlike
    // `notify_waiters`) is what closes that window.
    let bus = EventBus::new();
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::DispatchReady));

    let recv_task = tokio::spawn(async move { sub.recv().await });

    // Give the spawned task a real chance to run past its empty-queue
    // check and start awaiting `notified()` before we drop the bus.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(bus);

    let result = tokio::time::timeout(Duration::from_secs(5), recv_task)
        .await
        .expect("recv() must wake up after the bus is dropped, not hang forever")
        .expect("recv task must not panic");
    assert_eq!(result, None);
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
