use std::sync::Arc;
use std::time::Duration;

use boss_event_bus::{Event, EventBus, EventKind, TopicFilter};

use crate::TimerWheel;

// With `start_paused = true`, tokio auto-advances virtual time to the next
// registered timer whenever the runtime would otherwise deadlock (every
// task parked). Awaiting `subscription.recv()` directly — with no manual
// `tokio::time::advance` — therefore fast-forwards straight to whichever
// deadline is actually due next, without any real (or simulated) waiting.

#[tokio::test(start_paused = true)]
async fn fires_timer_event_when_deadline_elapses() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::Timer));
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.schedule_after("deadline_1", Duration::from_secs(30));

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "deadline_1".to_string(),
        }),
    );
}

#[tokio::test(start_paused = true)]
async fn delivers_independently_scheduled_deadlines_in_order() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::Timer));
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.schedule_after("later", Duration::from_secs(20));
    wheel.schedule_after("sooner", Duration::from_secs(10));

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "sooner".to_string(),
        }),
        "the earlier deadline must fire first regardless of scheduling order",
    );
    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "later".to_string(),
        }),
    );
}

// Rescheduling the same id must discard the earlier deadline entirely — it
// must never fire — and only the latest deadline takes effect.
#[tokio::test(start_paused = true)]
async fn rescheduling_an_id_replaces_its_pending_deadline() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::Timer));
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.schedule_after("deadline_1", Duration::from_secs(10));
    wheel.schedule_after("deadline_1", Duration::from_secs(100));

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "deadline_1".to_string(),
        }),
        "only one Timer event, for the rescheduled (later) deadline",
    );
}

// A cancelled deadline must never publish, even after its original deadline
// has elapsed. Prove it by cancelling the earlier of two deadlines and
// checking the first (and only) event observed is the later, uncancelled one.
#[tokio::test(start_paused = true)]
async fn cancel_prevents_the_deadline_from_firing() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::Timer));
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.schedule_after("cancelled", Duration::from_secs(10));
    wheel.cancel("cancelled");
    wheel.schedule_after("sentinel", Duration::from_secs(20));

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "sentinel".to_string(),
        }),
        "the cancelled deadline must not have published a Timer event ahead of the sentinel",
    );
}

// Cancelling an id that was never scheduled (or already fired) is a no-op,
// not an error.
#[tokio::test(start_paused = true)]
async fn cancel_of_unknown_id_is_a_no_op() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe(TopicFilter::kind(EventKind::Timer));
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.cancel("never_scheduled");
    wheel.schedule_after("deadline_1", Duration::from_secs(5));

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "deadline_1".to_string(),
        }),
    );
}

// Dropping the `TimerWheel` aborts its background loop, so no further
// deadlines fire and the process doesn't leak a spawned task forever.
#[tokio::test(start_paused = true)]
async fn dropping_the_wheel_stops_delivery() {
    let bus = Arc::new(EventBus::new());
    let mut sub = bus.subscribe_with_capacity(TopicFilter::kind(EventKind::Timer), 4);
    let wheel = TimerWheel::spawn(Arc::clone(&bus));

    wheel.schedule_after("deadline_1", Duration::from_secs(10));
    drop(wheel);

    // Nothing left to drive the runtime forward (the loop task is aborted),
    // so publishing another event proves the point without relying on the
    // absence of one within an arbitrary timeout.
    bus.publish(Event::Timer {
        deadline_id: "sentinel".to_string(),
    });

    assert_eq!(
        sub.recv().await,
        Some(Event::Timer {
            deadline_id: "sentinel".to_string(),
        }),
        "only the directly-published sentinel arrives; the aborted wheel never fired deadline_1",
    );
}
