//! Bounded, epoch-validated cache of per-item board runtimes.
//!
//! Board assembly needs a `TaskRuntime` for every card, and computing one costs
//! 1 to 4 point queries (about 22k statements on the scale fixture). A runtime
//! changes only when its own executions or runs change, so this cache keeps the
//! last computed runtime per work item and reuses it while the item's
//! `board_runtime_epoch` is unchanged.
//!
//! Invalidation is data-driven, not call-site driven: SQLite triggers on
//! `work_executions` and `work_runs` bump the owning item's epoch inside the
//! same transaction as the write (see `schema_init`'s schema 49), so a write
//! from any connection or process is seen, and an assembly reads the epochs in
//! the same read snapshot as the data it would have computed from. An entry is
//! reusable only on an exact epoch match.
//!
//! Two inputs are not epoch-covered and are handled here instead:
//! - Wall-clock time: a future `dispatch_not_before` flips the runtime when it
//!   passes, so such an entry carries `expires_at` and is dropped at that time.
//! - Other items' state (dependency gating and chain-hold blockers): runtimes
//!   that consulted another item are never cached; the caller recomputes them.
//!
//! Lifecycle: every assembly is a pass over one product. Entries a pass does
//! not touch (item archived, deleted or moved out) are pruned when it
//! completes, and a global capacity evicts the least recently touched passes.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use boss_protocol::TaskRuntime;

/// Entries kept across all products. A runtime is a few hundred bytes, so this
/// bounds the cache to a few tens of MiB while covering far more cards than the
/// largest measured product (about 8k).
pub const DEFAULT_CAPACITY: usize = 65_536;

pub struct BoardRuntimeCache {
    capacity: usize,
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    next_pass: u64,
}

struct Entry {
    epoch: i64,
    expires_at: Option<i64>,
    product_id: String,
    pass: u64,
    runtime: TaskRuntime,
}

/// A freshly computed runtime offered to the cache.
pub struct CachedRuntime {
    /// The item's epoch read in the same snapshot the runtime was computed from.
    pub epoch: i64,
    /// Epoch seconds at which the runtime stops being valid, if time-dependent.
    pub expires_at: Option<i64>,
    pub runtime: TaskRuntime,
}

/// Outcome of probing a pass's items: `hits[i]` answers `items[i]`.
pub struct Lookup {
    pub pass: u64,
    pub hits: Vec<Option<TaskRuntime>>,
    pub expiries: Vec<Option<i64>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub approx_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

impl Default for BoardRuntimeCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl BoardRuntimeCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::default(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Start a pass over `product_id`. `items` pairs each work item id with its
    /// current epoch. A hit requires an exact epoch match and an unexpired
    /// entry; every hit is stamped into this pass so completion keeps it.
    pub fn lookup(&self, product_id: &str, items: &[(&str, i64)], now: i64) -> Lookup {
        let mut state = self.state.lock().expect("board runtime cache poisoned");
        state.next_pass += 1;
        let pass = state.next_pass;
        let mut hits = Vec::with_capacity(items.len());
        let mut expiries = Vec::with_capacity(items.len());
        for (id, epoch) in items {
            let hit = state.entries.get_mut(*id).and_then(|entry| {
                let fresh = entry.epoch == *epoch && entry.expires_at.is_none_or(|expires| now < expires);
                fresh.then(|| {
                    entry.pass = pass;
                    if entry.product_id != product_id {
                        entry.product_id = product_id.to_owned();
                    }
                    entry.runtime.clone()
                })
            });
            expiries.push(state.entries.get(*id).and_then(|entry| entry.expires_at));
            hits.push(hit);
        }
        let found = hits.iter().filter(|hit| hit.is_some()).count() as u64;
        self.hits.fetch_add(found, Ordering::Relaxed);
        self.misses.fetch_add(items.len() as u64 - found, Ordering::Relaxed);
        Lookup { pass, hits, expiries }
    }

    /// Finish the pass: store `fresh` runtimes, prune this product's entries no
    /// pass at or after `pass` touched, and enforce the global capacity.
    pub fn complete(&self, product_id: &str, pass: u64, fresh: Vec<(String, CachedRuntime)>) {
        let mut state = self.state.lock().expect("board runtime cache poisoned");
        for (id, cached) in fresh {
            // A concurrent assembly on a newer snapshot may already have stored
            // a later epoch; never move an entry backwards.
            if state.entries.get(&id).is_some_and(|entry| entry.epoch > cached.epoch) {
                continue;
            }
            state.entries.insert(
                id,
                Entry {
                    epoch: cached.epoch,
                    expires_at: cached.expires_at,
                    product_id: product_id.to_owned(),
                    pass,
                    runtime: cached.runtime,
                },
            );
        }
        state
            .entries
            .retain(|_, entry| entry.product_id != product_id || entry.pass >= pass);
        let excess = state.entries.len().saturating_sub(self.capacity);
        if excess > 0 {
            let mut by_age: Vec<(u64, String)> = state
                .entries
                .iter()
                .map(|(id, entry)| (entry.pass, id.clone()))
                .collect();
            by_age.sort_unstable();
            for (_, id) in by_age.into_iter().take(excess) {
                state.entries.remove(&id);
            }
            self.evictions.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub fn stats(&self) -> CacheStats {
        let state = self.state.lock().expect("board runtime cache poisoned");
        let approx_bytes = state
            .entries
            .iter()
            .map(|(id, entry)| {
                std::mem::size_of::<Entry>() + id.len() + entry.product_id.len() + runtime_bytes(&entry.runtime)
            })
            .sum();
        CacheStats {
            entries: state.entries.len(),
            approx_bytes,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
fn runtime_bytes(runtime: &TaskRuntime) -> usize {
    let opt = |value: &Option<String>| value.as_ref().map_or(0, String::len);
    runtime.work_item_id.len()
        + opt(&runtime.current_run_id)
        + opt(&runtime.execution_id)
        + opt(&runtime.run_status)
        + opt(&runtime.dispatch_retry_at)
        + opt(&runtime.dispatch_wait_reason)
        + opt(&runtime.dispatch_wait_since)
        + opt(&runtime.dispatch_not_before)
        + runtime.dispatch_wait_blocker.as_ref().map_or(0, |blocker| {
            blocker.work_item_id.len()
                + blocker.product_id.len()
                + blocker.short_id.map_or(0, |_| std::mem::size_of::<i64>())
                + blocker.kind.len()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(id: &str) -> TaskRuntime {
        TaskRuntime::builder().work_item_id(id).build()
    }

    fn fresh(id: &str, epoch: i64, expires_at: Option<i64>) -> (String, CachedRuntime) {
        (
            id.to_owned(),
            CachedRuntime {
                epoch,
                expires_at,
                runtime: runtime(id),
            },
        )
    }

    fn pass(cache: &BoardRuntimeCache, product: &str, items: &[(&str, i64)], now: i64) -> Vec<bool> {
        let lookup = cache.lookup(product, items, now);
        let misses: Vec<_> = items
            .iter()
            .zip(&lookup.hits)
            .filter(|(_, hit)| hit.is_none())
            .map(|((id, epoch), _)| fresh(id, *epoch, None))
            .collect();
        cache.complete(product, lookup.pass, misses);
        lookup.hits.iter().map(Option::is_some).collect()
    }

    #[test]
    fn hits_only_on_an_exact_epoch_match() {
        let cache = BoardRuntimeCache::default();
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)], 10), [false, false]);
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)], 10), [true, true]);
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 4)], 10), [true, false]);
        // An epoch can only grow; a lower one is a different snapshot, not a hit.
        assert_eq!(pass(&cache, "p", &[("a", 0), ("b", 3)], 10), [true, false]);
    }

    #[test]
    fn expiry_is_exclusive_of_the_deadline() {
        let cache = BoardRuntimeCache::default();
        let lookup = cache.lookup("p", &[("a", 1)], 100);
        cache.complete("p", lookup.pass, vec![fresh("a", 1, Some(150))]);
        for (now, expected) in [(100, true), (149, true), (150, false), (151, false)] {
            let hit = cache.lookup("p", &[("a", 1)], now).hits[0].is_some();
            assert_eq!(hit, expected, "now={now}");
        }
    }

    #[test]
    fn a_pass_prunes_items_it_no_longer_sees() {
        let cache = BoardRuntimeCache::default();
        pass(&cache, "p", &[("a", 0), ("b", 0)], 0);
        pass(&cache, "q", &[("c", 0)], 0);
        assert_eq!(cache.stats().entries, 3);
        // `b` was archived or deleted; the other product is untouched.
        assert_eq!(pass(&cache, "p", &[("a", 0)], 0), [true]);
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(pass(&cache, "q", &[("c", 0)], 0), [true]);
    }

    #[test]
    fn a_moved_item_survives_its_old_products_pass() {
        let cache = BoardRuntimeCache::default();
        pass(&cache, "p", &[("a", 0)], 0);
        assert_eq!(pass(&cache, "q", &[("a", 0)], 0), [true]);
        pass(&cache, "p", &[], 0);
        assert_eq!(pass(&cache, "q", &[("a", 0)], 0), [true]);
    }

    #[test]
    fn capacity_evicts_the_least_recently_touched_pass() {
        let cache = BoardRuntimeCache::with_capacity(3);
        pass(&cache, "p", &[("a", 0), ("b", 0)], 0);
        pass(&cache, "q", &[("c", 0), ("d", 0)], 0);
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.evictions), (3, 1));
        // The older product lost one entry; the newest pass is intact.
        assert_eq!(pass(&cache, "q", &[("c", 0), ("d", 0)], 0), [true, true]);
    }

    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_entry() {
        let cache = BoardRuntimeCache::default();
        let slow = cache.lookup("p", &[("a", 1)], 0);
        let quick = cache.lookup("p", &[("a", 2)], 0);
        cache.complete("p", quick.pass, vec![fresh("a", 2, None)]);
        cache.complete("p", slow.pass, vec![fresh("a", 1, None)]);
        assert!(cache.lookup("p", &[("a", 2)], 0).hits[0].is_some());
        assert!(cache.lookup("p", &[("a", 1)], 0).hits[0].is_none());
    }

    #[test]
    fn stats_count_hits_misses_and_account_bytes() {
        let cache = BoardRuntimeCache::default();
        pass(&cache, "p", &[("a", 0), ("b", 0)], 0);
        pass(&cache, "p", &[("a", 0), ("b", 0)], 0);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (2, 2));
        assert!(stats.approx_bytes >= 2 * std::mem::size_of::<Entry>());
    }
}
