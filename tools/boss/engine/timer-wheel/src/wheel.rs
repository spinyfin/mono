use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boss_event_bus::{Event, EventBus};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// A heap entry: a candidate `(deadline, id)` pair. The heap orders by
/// deadline (earliest first via `Reverse`); staleness — a superseded
/// reschedule or a cancellation — is detected lazily on pop by comparing
/// against `Shared::scheduled`, since `BinaryHeap` has no efficient
/// arbitrary-entry removal.
type HeapEntry = Reverse<(Instant, String)>;

struct Shared {
    /// Authoritative: id -> the deadline it is currently scheduled for.
    /// A heap entry is stale iff this map no longer maps its id to its
    /// exact deadline.
    scheduled: Mutex<HashMap<String, Instant>>,
    heap: Mutex<BinaryHeap<HeapEntry>>,
    /// Wakes the background loop when a new deadline is scheduled or an
    /// existing one is cancelled/rescheduled, so it can recompute what to
    /// wait for next instead of sleeping out a stale one.
    wake: Notify,
}

/// A shared timer-wheel: schedule deadlines by id, get an
/// `Event::Timer { deadline_id }` published onto the bus when each one
/// elapses.
///
/// Scheduling an id that is already pending replaces its deadline (the
/// earlier one is discarded without firing); [`cancel`](Self::cancel)
/// removes it entirely. The background loop is aborted when the
/// `TimerWheel` is dropped.
pub struct TimerWheel {
    shared: Arc<Shared>,
    loop_handle: JoinHandle<()>,
}

impl TimerWheel {
    /// Spawn the background loop that drives deadline delivery, publishing
    /// onto `bus` as deadlines elapse.
    pub fn spawn(bus: Arc<EventBus>) -> Self {
        let shared = Arc::new(Shared {
            scheduled: Mutex::new(HashMap::new()),
            heap: Mutex::new(BinaryHeap::new()),
            wake: Notify::new(),
        });
        let loop_shared = Arc::clone(&shared);
        let loop_handle = tokio::spawn(async move { run_loop(loop_shared, bus).await });
        Self { shared, loop_handle }
    }

    /// Schedule (or reschedule) `deadline_id` to fire at `deadline`.
    pub fn schedule_at(&self, deadline_id: impl Into<String>, deadline: Instant) {
        let deadline_id = deadline_id.into();
        self.shared
            .scheduled
            .lock()
            .expect("timer wheel scheduled-map lock poisoned")
            .insert(deadline_id.clone(), deadline);
        self.shared
            .heap
            .lock()
            .expect("timer wheel heap lock poisoned")
            .push(Reverse((deadline, deadline_id)));
        self.shared.wake.notify_one();
    }

    /// Schedule (or reschedule) `deadline_id` to fire `delay` from now.
    pub fn schedule_after(&self, deadline_id: impl Into<String>, delay: Duration) {
        self.schedule_at(deadline_id, Instant::now() + delay);
    }

    /// Cancel a pending deadline. A no-op if `deadline_id` isn't currently
    /// scheduled (already fired, already cancelled, or never scheduled).
    pub fn cancel(&self, deadline_id: &str) {
        self.shared
            .scheduled
            .lock()
            .expect("timer wheel scheduled-map lock poisoned")
            .remove(deadline_id);
        self.shared.wake.notify_one();
    }
}

impl Drop for TimerWheel {
    fn drop(&mut self) {
        self.loop_handle.abort();
    }
}

async fn run_loop(shared: Arc<Shared>, bus: Arc<EventBus>) {
    loop {
        match pop_next_valid(&shared) {
            None => shared.wake.notified().await,
            Some((deadline, id)) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.into()) => {
                        fire_if_still_current(&shared, &bus, deadline, id);
                    }
                    () = shared.wake.notified() => {
                        // Something changed (a closer deadline arrived, or
                        // this entry was rescheduled/cancelled). Put it back
                        // — `pop_next_valid` will discard it next time round
                        // if it's no longer current — and recompute.
                        shared
                            .heap
                            .lock()
                            .expect("timer wheel heap lock poisoned")
                            .push(Reverse((deadline, id)));
                    }
                }
            }
        }
    }
}

/// Publish `Event::Timer { deadline_id: id }` iff `id` is still scheduled for
/// exactly `deadline` — i.e. it wasn't cancelled or rescheduled between being
/// popped and its sleep completing.
fn fire_if_still_current(shared: &Shared, bus: &EventBus, deadline: Instant, id: String) {
    let mut scheduled = shared
        .scheduled
        .lock()
        .expect("timer wheel scheduled-map lock poisoned");
    if scheduled.get(&id) == Some(&deadline) {
        scheduled.remove(&id);
        drop(scheduled);
        bus.publish(Event::Timer { deadline_id: id });
    }
}

/// Pop heap entries until finding one whose `(deadline, id)` still matches
/// the authoritative `scheduled` map, discarding stale entries (superseded
/// by a reschedule, or cancelled) along the way.
fn pop_next_valid(shared: &Shared) -> Option<(Instant, String)> {
    let mut heap = shared.heap.lock().expect("timer wheel heap lock poisoned");
    let scheduled = shared
        .scheduled
        .lock()
        .expect("timer wheel scheduled-map lock poisoned");
    while let Some(Reverse((deadline, id))) = heap.pop() {
        if scheduled.get(&id) == Some(&deadline) {
            return Some((deadline, id));
        }
    }
    None
}
