use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::event::Event;
use crate::filter::TopicFilter;

/// Default bounded mailbox size for a subscriber that doesn't request one
/// explicitly. Matches the design doc's starting point; tunable per
/// subscriber via [`EventBus::subscribe_with_capacity`].
const DEFAULT_MAILBOX_CAPACITY: usize = 256;

struct Subscriber {
    filter: TopicFilter,
    sender: mpsc::Sender<Event>,
}

/// In-process, in-memory typed topic bus. `publish` fans an event out to
/// every matching subscriber's bounded mailbox; a full mailbox drops the
/// event rather than block the publisher — the bus is best-effort by
/// design, and every subscriber is expected to keep its own periodic
/// backstop reconcile for whatever the bus drops.
pub struct EventBus {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribe with the default mailbox capacity.
    pub fn subscribe(&self, filter: TopicFilter) -> Subscription {
        self.subscribe_with_capacity(filter, DEFAULT_MAILBOX_CAPACITY)
    }

    /// Subscribe with an explicit bounded mailbox capacity.
    pub fn subscribe_with_capacity(&self, filter: TopicFilter, capacity: usize) -> Subscription {
        let (sender, receiver) = mpsc::channel(capacity);
        self.subscribers
            .lock()
            .expect("event bus subscriber lock poisoned")
            .push(Subscriber { filter, sender });
        Subscription { receiver }
    }

    /// Fan `event` out to every matching subscriber. Non-blocking: never
    /// awaits, never blocks the caller on a slow or stalled subscriber.
    pub fn publish(&self, event: Event) {
        let subscribers = self.subscribers.lock().expect("event bus subscriber lock poisoned");
        for subscriber in subscribers.iter() {
            if subscriber.filter.matches(&event) {
                let _ = subscriber.sender.try_send(event.clone());
            }
        }
    }
}

/// A reconciler's handle onto the events it subscribed for.
pub struct Subscription {
    receiver: mpsc::Receiver<Event>,
}

impl Subscription {
    /// Await the next matching event. Returns `None` once the bus itself
    /// has been dropped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }
}
