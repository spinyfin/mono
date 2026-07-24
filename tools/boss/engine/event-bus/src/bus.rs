use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use boss_engine_metrics_registry::Registry;
use tokio::sync::Notify;

use crate::event::{Event, EventKind};
use crate::filter::TopicFilter;

/// Default bounded mailbox size for a subscriber that doesn't request one
/// explicitly. Matches the design doc's starting point; tunable per
/// subscriber via [`EventBus::subscribe_with_capacity`].
const DEFAULT_MAILBOX_CAPACITY: usize = 256;

/// Dot-namespace prefix for the per-topic drop counter: dynamically
/// registered against the engine's [`Registry`] the first time a topic
/// drops an event, named `bus_events_dropped_total.<topic>` per the
/// design (`bus_events_dropped_total{topic}`).
const DROPPED_COUNTER_PREFIX: &str = "bus_events_dropped_total";
const DROPPED_COUNTER_DESCRIPTION: &str =
    "Events dropped from a full event-bus subscriber mailbox that could not be coalesced.";

/// One pending outcome of [`Mailbox::try_send`].
enum SendOutcome {
    /// Queued in a previously-empty slot.
    Sent,
    /// Overwrote an already-pending event with the same (kind, key).
    Coalesced,
    /// Mailbox was full and no matching pending event could be
    /// overwritten; the caller should count this as a drop.
    Dropped,
}

struct MailboxState {
    queue: VecDeque<Event>,
    /// For each (kind, coalesce key) with a pending event, its
    /// front-relative index in `queue`. Lets a full mailbox overwrite a
    /// stale pending event in place instead of growing or blocking,
    /// mirroring `session_queue`'s `pending_topics` newest-wins model.
    pending: HashMap<(EventKind, String), usize>,
    closed: bool,
}

/// A bounded, coalescing per-subscriber mailbox. `try_send` never blocks:
/// a full mailbox either overwrites an already-pending event for the same
/// (topic, key) — newest wins — or, if no such event exists, drops the
/// new one.
struct Mailbox {
    capacity: usize,
    state: Mutex<MailboxState>,
    notify: Notify,
}

impl Mailbox {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(MailboxState {
                queue: VecDeque::new(),
                pending: HashMap::new(),
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    fn try_send(&self, event: Event) -> SendOutcome {
        let mut state = self.state.lock().expect("event bus mailbox lock poisoned");
        let coalesce_key = (event.kind(), event.coalesce_key());

        if let Some(&idx) = state.pending.get(&coalesce_key) {
            debug_assert!(idx < state.queue.len(), "pending index must stay in range");
            state.queue[idx] = event;
            drop(state);
            self.notify.notify_one();
            return SendOutcome::Coalesced;
        }

        if state.queue.len() >= self.capacity {
            return SendOutcome::Dropped;
        }

        let idx = state.queue.len();
        state.queue.push_back(event);
        state.pending.insert(coalesce_key, idx);
        drop(state);
        self.notify.notify_one();
        SendOutcome::Sent
    }

    /// Mark the mailbox closed so pending `recv` calls resolve to `None`
    /// once drained, rather than waiting forever with no producer left.
    fn close(&self) {
        let mut state = self.state.lock().expect("event bus mailbox lock poisoned");
        state.closed = true;
        drop(state);
        self.notify.notify_waiters();
    }

    async fn recv(&self) -> Option<Event> {
        loop {
            {
                let mut state = self.state.lock().expect("event bus mailbox lock poisoned");
                if let Some(event) = Self::pop_front_locked(&mut state) {
                    return Some(event);
                }
                if state.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// Pop the oldest queued event (if any), keeping `pending` indices
    /// front-relative.
    fn pop_front_locked(state: &mut MailboxState) -> Option<Event> {
        let popped = state.queue.pop_front()?;
        let mut next = HashMap::with_capacity(state.pending.len());
        for (key, idx) in state.pending.drain() {
            if idx == 0 {
                continue;
            }
            next.insert(key, idx - 1);
        }
        state.pending = next;
        Some(popped)
    }
}

struct Subscriber {
    filter: TopicFilter,
    mailbox: Arc<Mailbox>,
}

/// In-process, in-memory typed topic bus. `publish` fans an event out to
/// every matching subscriber's bounded, coalescing mailbox; a full
/// mailbox with no matching pending event drops the new one rather than
/// block the publisher — the bus is best-effort by design, and every
/// subscriber is expected to keep its own periodic backstop reconcile
/// for whatever the bus drops.
pub struct EventBus {
    subscribers: Mutex<Vec<Subscriber>>,
    /// When set, every dropped event increments
    /// `bus_events_dropped_total.<topic>` here. `None` (the default via
    /// [`EventBus::new`]) skips metrics entirely, e.g. for unit tests
    /// that don't wire up a [`Registry`].
    metrics: Option<Arc<Registry>>,
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
            metrics: None,
        }
    }

    /// Like [`EventBus::new`], but wires the drop counter to `registry`.
    pub fn with_metrics(registry: Arc<Registry>) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            metrics: Some(registry),
        }
    }

    /// Subscribe with the default mailbox capacity.
    pub fn subscribe(&self, filter: TopicFilter) -> Subscription {
        self.subscribe_with_capacity(filter, DEFAULT_MAILBOX_CAPACITY)
    }

    /// Subscribe with an explicit bounded mailbox capacity.
    pub fn subscribe_with_capacity(&self, filter: TopicFilter, capacity: usize) -> Subscription {
        let mailbox = Arc::new(Mailbox::new(capacity));
        self.subscribers
            .lock()
            .expect("event bus subscriber lock poisoned")
            .push(Subscriber {
                filter,
                mailbox: mailbox.clone(),
            });
        Subscription { mailbox }
    }

    /// Fan `event` out to every matching subscriber. Non-blocking: never
    /// awaits, never blocks the caller on a slow or stalled subscriber.
    pub fn publish(&self, event: Event) {
        let subscribers = self.subscribers.lock().expect("event bus subscriber lock poisoned");
        for subscriber in subscribers.iter() {
            if !subscriber.filter.matches(&event) {
                continue;
            }
            if let SendOutcome::Dropped = subscriber.mailbox.try_send(event.clone()) {
                self.record_drop(event.kind());
            }
        }
    }

    fn record_drop(&self, kind: EventKind) {
        let Some(registry) = &self.metrics else {
            return;
        };
        let name = format!("{DROPPED_COUNTER_PREFIX}.{}", kind.topic_name());
        registry.counter_inc_by_dynamic(&name, DROPPED_COUNTER_DESCRIPTION, 1);
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        let subscribers = self.subscribers.lock().expect("event bus subscriber lock poisoned");
        for subscriber in subscribers.iter() {
            subscriber.mailbox.close();
        }
    }
}

/// A reconciler's handle onto the events it subscribed for.
pub struct Subscription {
    mailbox: Arc<Mailbox>,
}

impl Subscription {
    /// Await the next matching event. Returns `None` once the bus itself
    /// has been dropped and this subscription's mailbox has drained.
    pub async fn recv(&mut self) -> Option<Event> {
        self.mailbox.recv().await
    }
}
