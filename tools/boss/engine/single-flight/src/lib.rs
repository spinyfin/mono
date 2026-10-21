//! Blocking single-flight loads with a freshness fence and one pending batch.
//!
//! Call from an owned blocking job: dropping an async waiter must not cancel
//! that job. No completed values are cached. Locks cover bookkeeping only;
//! different keys can load concurrently. A freshness probe must be cheap and
//! must not acquire the data writer's lock.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};

/// Result order is assigned at load start, never at request registration.
pub struct Loaded<T> {
    pub sequence: u64,
    pub value: Result<T, String>,
}

struct Flight<T> {
    result: Mutex<Option<Arc<Loaded<T>>>>,
    ready: Condvar,
}

impl<T> Default for Flight<T> {
    fn default() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

impl<T> Flight<T> {
    fn wait(&self) -> Arc<Loaded<T>> {
        let result = self.result.lock().expect("flight result poisoned");
        self.ready
            .wait_while(result, |value| value.is_none())
            .expect("flight result poisoned")
            .as_ref()
            .unwrap()
            .clone()
    }

    fn publish(&self, value: Arc<Loaded<T>>) {
        *self.result.lock().expect("flight result poisoned") = Some(value);
        self.ready.notify_all();
    }
}

struct Slot<S, T> {
    stamp: Option<S>,
    current: Arc<Flight<T>>,
    next: Option<Arc<Flight<T>>>,
}

struct State<K, S, T> {
    sequence: u64,
    slots: HashMap<K, Slot<S, T>>,
}

pub struct SingleFlight<K, S, T> {
    state: Mutex<State<K, S, T>>,
}

impl<K, S, T> Default for SingleFlight<K, S, T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                sequence: 0,
                slots: HashMap::new(),
            }),
        }
    }
}

impl<K: Eq + Hash + Clone, S: Eq, T> SingleFlight<K, S, T> {
    /// A request joins only a not-yet-started or still-current load. Otherwise
    /// it joins the single pending batch. The first caller in each batch owns
    /// its load; all callers (including that owner) receive the same result.
    pub fn run(
        &self,
        key: K,
        freshness: impl Fn() -> Result<S, String>,
        load: impl FnOnce() -> Result<T, String>,
    ) -> Result<Arc<Loaded<T>>, String> {
        let (flight, previous, owner) = {
            let mut state = self.state.lock().expect("single flight poisoned");
            let stamp = freshness()?;
            match state.slots.get_mut(&key) {
                Some(slot) if slot.stamp.as_ref().is_none_or(|started| *started == stamp) => {
                    (slot.current.clone(), None, false)
                }
                Some(slot) => match &slot.next {
                    Some(next) => (next.clone(), None, false),
                    None => {
                        let next = Arc::new(Flight::default());
                        slot.next = Some(next.clone());
                        (next, Some(slot.current.clone()), true)
                    }
                },
                None => {
                    let flight = Arc::new(Flight::default());
                    state.slots.insert(
                        key.clone(),
                        Slot {
                            stamp: None,
                            current: flight.clone(),
                            next: None,
                        },
                    );
                    (flight, None, true)
                }
            }
        };
        if !owner {
            return Ok(flight.wait());
        }
        if let Some(previous) = previous {
            previous.wait();
        }
        // Refresh at the actual start: all invalidations while the previous
        // load was running are covered by this one follow-up scan.
        let (sequence, stamp) = {
            let mut state = self.state.lock().expect("single flight poisoned");
            state.sequence += 1;
            let stamp = freshness();
            let ready = stamp.map(|stamp| {
                state.slots.get_mut(&key).unwrap().stamp = Some(stamp);
            });
            (state.sequence, ready)
        };
        let value = match stamp {
            Ok(()) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(load))
                .unwrap_or_else(|_| Err("shared load panicked".into())),
            Err(error) => Err(error),
        };
        let result = Arc::new(Loaded { sequence, value });
        let mut state = self.state.lock().expect("single flight poisoned");
        let slot = state.slots.get_mut(&key).unwrap();
        if let Some(next) = slot.next.take() {
            slot.current = next;
            slot.stamp = None;
        } else {
            state.slots.remove(&key);
        }
        flight.publish(result.clone());
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
