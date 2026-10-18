//! Engine-wide single-flight for the database load behind `GetWorkTree`.
//!
//! The board refresh is the busiest read, and a burst of them for one
//! product costs one database load per request. While a load is blocked on a
//! SQLite lock its snapshot is not taken yet, so a refresh that arrives
//! meanwhile — on any session — waits for that load instead of starting its
//! own, and every waiter still gets a tree no older than its own arrival (see
//! `read_coalesce` for why a lock wait keeps a read joinable).
//!
//! Only the load is shared. Each waiter encodes its own response against its
//! own session's delta cache and freshness tracking, so deltas, sequence ids
//! and worker-tier sanitisation stay per session and per request.
//!
//! `load` blocks the calling thread; `GetWorkTree` runs on the blocking pool
//! (it is a bulk read), where waiting costs nothing the load itself would not.
use crate::work::contention::{READ_WAIT, ReadWait};
use boss_protocol::WorkTree;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
pub(super) struct WorkTreeFlights {
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    loads: AtomicU64,
    shared: AtomicU64,
}

#[cfg(test)]
impl WorkTreeFlights {
    /// Loads that ran against the database.
    pub(super) fn loads(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }

    /// Requests answered from another request's load.
    pub(super) fn shared(&self) -> u64 {
        self.shared.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Flight {
    wait: Arc<ReadWait>,
    /// Requests that joined this load. Counted under the flights lock, which
    /// also retires the flight, so the leader's count is final once retired.
    followers: AtomicUsize,
    result: Mutex<Option<Result<Arc<WorkTree>, String>>>,
    done: Condvar,
}

/// Publishes the leader's outcome and retires the flight, also when the load
/// panics, so waiters are never left blocked.
struct Leader<'a> {
    flights: &'a WorkTreeFlights,
    product_id: &'a str,
    flight: Arc<Flight>,
}

impl Leader<'_> {
    /// Stop new requests joining and return how many already did.
    fn retire(&self) -> usize {
        let mut flights = self.flights.flights.lock().expect("work tree flights poisoned");
        if flights
            .get(self.product_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            flights.remove(self.product_id);
        }
        self.flight.followers.load(Ordering::Relaxed)
    }

    fn publish(&self, result: Result<Arc<WorkTree>, String>) {
        let mut slot = self.flight.result.lock().expect("work tree flight poisoned");
        if slot.is_none() {
            *slot = Some(result);
        }
        self.flight.done.notify_all();
    }
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        self.retire();
        self.publish(Err("work tree load aborted".to_owned()));
    }
}

impl WorkTreeFlights {
    /// The work tree for `product_id`: loaded by `load`, or taken from an
    /// in-flight load that is still waiting for its SQLite lock.
    pub(super) fn load(
        &self,
        product_id: &str,
        load: impl FnOnce() -> anyhow::Result<WorkTree>,
    ) -> anyhow::Result<WorkTree> {
        let flight = {
            let mut flights = self.flights.lock().expect("work tree flights poisoned");
            match flights.get(product_id) {
                Some(flight) if flight.wait.is_waiting() => {
                    flight.followers.fetch_add(1, Ordering::Relaxed);
                    let flight = flight.clone();
                    drop(flights);
                    self.shared.fetch_add(1, Ordering::Relaxed);
                    return Self::await_result(&flight);
                }
                _ => {
                    let flight = Arc::new(Flight::default());
                    flights.insert(product_id.to_owned(), flight.clone());
                    flight
                }
            }
        };
        self.loads.fetch_add(1, Ordering::Relaxed);
        let leader = Leader {
            flights: self,
            product_id,
            flight,
        };
        let outcome = READ_WAIT.sync_scope(leader.flight.wait.clone(), load);
        let followers = leader.retire();
        match outcome {
            Ok(tree) => {
                // Copy the tree only when someone is waiting for it.
                if followers > 0 {
                    leader.publish(Ok(Arc::new(tree.clone())));
                }
                Ok(tree)
            }
            Err(err) => {
                leader.publish(Err(format!("{err:#}")));
                Err(err)
            }
        }
    }

    fn await_result(flight: &Flight) -> anyhow::Result<WorkTree> {
        let mut slot = flight.result.lock().expect("work tree flight poisoned");
        while slot.is_none() {
            slot = flight.done.wait(slot).expect("work tree flight poisoned");
        }
        match slot.as_ref().expect("result published") {
            Ok(tree) => Ok(WorkTree::clone(tree)),
            Err(message) => Err(anyhow::anyhow!("{message}")),
        }
    }
}

#[cfg(test)]
#[path = "work_tree_flight_tests.rs"]
mod tests;
