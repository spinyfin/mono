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
    /// Queued (coalesce key, event) pairs, oldest first. The key travels
    /// with its event so a pop can evict the matching `pending` entry in
    /// O(1) instead of rebuilding the whole map.
    queue: VecDeque<((EventKind, String), Event)>,
    /// For each (kind, coalesce key) with a pending event, its absolute
    /// sequence index (see `base`). At most one pending entry per key at
    /// any time: a later event for the same key always overwrites the
    /// pending one in place, regardless of how full the mailbox is —
    /// only a key with no pending slot can be dropped, and only when the
    /// mailbox is full. Mirrors `session_queue`'s `pending_topics`
    /// newest-wins model.
    pending: HashMap<(EventKind, String), usize>,
    /// Absolute sequence index of `queue`'s front element. Incremented on
    /// every pop so `pending` indices never need rebasing.
    base: usize,
    closed: bool,
}

/// A bounded, coalescing per-subscriber mailbox. `try_send` never blocks:
/// a later event for a (topic, key) that already has a pending event
/// always overwrites it in place — newest wins, regardless of occupancy.
/// Only when the mailbox is full and the incoming event's key has no
/// pending slot to overwrite is the new event dropped.
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
                base: 0,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    fn try_send(&self, event: Event) -> SendOutcome {
        let mut state = self.state.lock().expect("event bus mailbox lock poisoned");
        let coalesce_key = (event.kind(), event.coalesce_key());

        if let Some(&abs_idx) = state.pending.get(&coalesce_key) {
            let idx = abs_idx - state.base;
            debug_assert!(idx < state.queue.len(), "pending index must stay in range");
            state.queue[idx].1 = event;
            drop(state);
            self.notify.notify_one();
            return SendOutcome::Coalesced;
        }

        if state.queue.len() >= self.capacity {
            return SendOutcome::Dropped;
        }

        let abs_idx = state.base + state.queue.len();
        state.queue.push_back((coalesce_key.clone(), event));
        state.pending.insert(coalesce_key, abs_idx);
        drop(state);
        self.notify.notify_one();
        SendOutcome::Sent
    }

    /// Mark the mailbox closed so pending `recv` calls resolve to `None`
    /// once drained, rather than waiting forever with no producer left.
    /// Uses `notify_one` rather than `notify_waiters`: a mailbox has
    /// exactly one consumer, and unlike `notify_waiters`, `notify_one`
    /// stores a permit even when no task is currently registered as a
    /// waiter. Without that, a `recv` that has just released the state
    /// lock but not yet called `notified()` would miss this wakeup and
    /// hang forever with no producer left to notify it again.
    fn close(&self) {
        let mut state = self.state.lock().expect("event bus mailbox lock poisoned");
        state.closed = true;
        drop(state);
        self.notify.notify_one();
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

    /// Pop the oldest queued event (if any) and evict its `pending`
    /// entry, if still present, in O(1) — no rebuild of `pending` needed
    /// since indices are absolute and `base` tracks the front offset.
    fn pop_front_locked(state: &mut MailboxState) -> Option<Event> {
        let (key, event) = state.queue.pop_front()?;
        state.pending.remove(&key);
        state.base += 1;
        Some(event)
    }
}

struct Subscriber {
    filter: TopicFilter,
    mailbox: Arc<Mailbox>,
}

/// In-process, in-memory typed topic bus. `publish` fans an event out to
/// every matching subscriber's bounded, coalescing mailbox: a later event
/// for a (topic, key) that already has a pending event always overwrites
/// it in place, and only a full mailbox whose incoming event has no
/// pending slot to overwrite drops the new one rather than block the
/// publisher — the bus is best-effort by design, and every subscriber is
/// expected to keep its own periodic backstop reconcile for whatever the
/// bus drops.
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

    /// Subscribe with an explicit bounded mailbox capacity. `capacity`
    /// must be at least 1 — a zero-capacity mailbox can never queue an
    /// event, so every publish would silently drop and the subscriber
    /// would never receive anything despite looking live. Clamped to 1
    /// in release builds; debug builds assert instead so the mistake is
    /// caught at the call site.
    pub fn subscribe_with_capacity(&self, filter: TopicFilter, capacity: usize) -> Subscription {
        debug_assert!(capacity > 0, "mailbox capacity must be at least 1");
        let mailbox = Arc::new(Mailbox::new(capacity.max(1)));
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
        let mut subscribers = self.subscribers.lock().expect("event bus subscriber lock poisoned");
        // A dropped `Subscription` leaves its `Mailbox` referenced only by
        // this `Subscriber` entry (strong count 1): nothing will ever
        // drain it again, so every future matching publish would just
        // fill it up and then count as a drop forever. Prune those before
        // fanning out so the drop counter stays a signal of real
        // backpressure, not orphaned subscribers.
        subscribers.retain(|subscriber| Arc::strong_count(&subscriber.mailbox) > 1);
        // Tally drops locally instead of touching the registry (a heap
        // allocation plus its counters write lock) on every dropped
        // event while still holding the subscribers lock -- under a
        // pathological burst that would serialize every other publisher
        // behind both locks. Every subscriber in one `publish` call sees
        // the same `event`, so a single count-and-record after the
        // subscribers guard is released covers the whole fan-out.
        let mut dropped = 0u64;
        for subscriber in subscribers.iter() {
            if !subscriber.filter.matches(&event) {
                continue;
            }
            if let SendOutcome::Dropped = subscriber.mailbox.try_send(event.clone()) {
                dropped += 1;
            }
        }
        drop(subscribers);
        if dropped > 0 {
            self.record_drop(event.kind(), dropped);
        }
    }

    fn record_drop(&self, kind: EventKind, count: u64) {
        let Some(registry) = &self.metrics else {
            return;
        };
        let name = format!("{DROPPED_COUNTER_PREFIX}.{}", kind.topic_name());
        registry.counter_inc_by_dynamic(&name, DROPPED_COUNTER_DESCRIPTION, count);
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
