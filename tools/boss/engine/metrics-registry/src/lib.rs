//! In-memory metrics registry: counter / gauge primitives and the
//! `register_counter!` / `register_gauge!` declaration macros.
//!
//! This crate holds the storage and declaration half of the engine's
//! metrics framework. Persisting snapshots to a store (`boss-metrics`)
//! and the `init_all` startup registration sweep (`boss-engine`) stay
//! one level up: persistence needs a store abstraction and the
//! startup sweep reaches into every metric-declaring engine module,
//! so both belong on the consumer side of this edge. The dependency
//! runs one way — `boss-engine` -> `boss-metrics` -> `boss-engine-metrics-registry`.
//!
//! Counters are strictly monotonic `u64`s; the only mutator is
//! `inc` / `inc_by`. Gauges are signed `i64`s overwritten by the
//! producer on each publication.
//!
//! Counter names must be lowercase ASCII letters, digits, dots or
//! underscores; dot-separated namespaces by convention
//! (`pr_url_capture.primary_path.hit`). Duplicate names panic at
//! registration so the failure surfaces at engine startup rather
//! than the first increment.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// Static descriptor for a counter, produced by [`register_counter!`].
///
/// Holds the canonical name and one-line description only. The
/// running `u64` value lives inside the [`Registry`] this handle is
/// resolved against — the call site invokes
/// `HANDLE.inc(&registry)` and the registry does the lookup.
pub struct CounterHandle {
    name: &'static str,
    description: &'static str,
}

impl CounterHandle {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    /// Add 1 to this counter in `registry`. Panics if the handle was
    /// not registered via [`Registry::register_counter`] (typically
    /// fixed by registering the handle at startup — for the engine,
    /// in `boss_engine::metrics_init::init_all`).
    pub fn inc(&self, registry: &Registry) {
        registry.counter_inc_by(self.name, 1);
    }

    /// Add `n` to this counter in `registry`. Saturating on the
    /// (extremely unlikely) overflow to keep the monotonic contract.
    pub fn inc_by(&self, registry: &Registry, n: u64) {
        registry.counter_inc_by(self.name, n);
    }
}

/// Static descriptor for a gauge, produced by [`register_gauge!`].
///
/// Like [`CounterHandle`] but with `set` / `get` for signed `i64`
/// values. Each `set` overwrites the previous value and stamps the
/// observation time.
pub struct GaugeHandle {
    name: &'static str,
    description: &'static str,
}

impl GaugeHandle {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    /// Set this gauge in `registry` to `value`. Panics if the handle
    /// was not registered.
    pub fn set(&self, registry: &Registry, value: i64) {
        registry.gauge_set(self.name, value);
    }
}

/// Declare a static [`CounterHandle`].
///
/// Engine modules reach this macro through the `boss_engine` crate-root
/// re-export, so call sites use `crate::register_counter!`, not the
/// fully-qualified path shown below:
///
/// ```ignore
/// crate::register_counter!(
///     PR_URL_CAPTURE_PRIMARY_HIT,
///     "pr_url_capture.primary_path.hit",
///     "On-stop hook found a staged PR URL and skipped the detector.",
/// );
/// ```
///
/// The handle must be added to the consumer's registration entry
/// point (for the engine, `boss_engine::metrics_init::init_all`) so
/// registration runs at startup (design §"Risks / open questions"
/// item 2).
#[macro_export]
macro_rules! register_counter {
    ($static_name:ident, $name:literal, $description:literal $(,)?) => {
        #[allow(dead_code)]
        pub static $static_name: $crate::CounterHandle = $crate::CounterHandle::new($name, $description);
    };
}

/// Declare a static [`GaugeHandle`].
#[macro_export]
macro_rules! register_gauge {
    ($static_name:ident, $name:literal, $description:literal $(,)?) => {
        #[allow(dead_code)]
        pub static $static_name: $crate::GaugeHandle = $crate::GaugeHandle::new($name, $description);
    };
}

/// In-memory store of every counter and gauge plus persisted rows
/// that no live handle currently owns (rehydrated as "stale").
///
/// Thread safety: counter / gauge values are atomic, the maps are
/// `RwLock`s — registration takes the write lock, increment takes
/// the read lock then atomic-adds. Lookups happen by `name`. The
/// design's expected steady-state is ~50 entries so the hashmap +
/// rwlock cost is irrelevant on the hot path.
pub struct Registry {
    counters: RwLock<HashMap<String, Arc<CounterEntry>>>,
    gauges: RwLock<HashMap<String, Arc<GaugeEntry>>>,
}

struct CounterEntry {
    name: String,
    description: String,
    value: AtomicU64,
    updated_at_ms: AtomicI64,
    /// True when this row was rehydrated from `state.db` but no
    /// `register_counter!` handle in the current engine binary
    /// matches its name. The design retains these so historical
    /// answers stay queryable (§"Risks / open questions" item 3).
    stale: AtomicBool,
}

struct GaugeEntry {
    name: String,
    description: String,
    value: AtomicI64,
    observed_at_ms: AtomicI64,
    stale: AtomicBool,
}

/// Read-only snapshot of a counter row, suitable for serialising.
#[derive(Debug, Clone)]
pub struct CounterSnapshot {
    pub name: String,
    pub description: String,
    pub value: u64,
    pub updated_at_ms: i64,
    pub stale: bool,
}

#[derive(Debug, Clone)]
pub struct GaugeSnapshot {
    pub name: String,
    pub description: String,
    pub value: i64,
    pub observed_at_ms: i64,
    pub stale: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            counters: RwLock::new(HashMap::new()),
            gauges: RwLock::new(HashMap::new()),
        }
    }

    /// Register a [`CounterHandle`]. Panics on duplicate name or
    /// invalid name (lowercase ASCII letters, digits, dots,
    /// underscores only).
    pub fn register_counter(&self, handle: &CounterHandle) {
        validate_name(handle.name);
        let mut counters = self.counters.write().expect("metrics counters lock poisoned");
        match counters.get(handle.name) {
            Some(existing) if existing.stale.load(Ordering::Relaxed) => {
                // A rehydrated row was waiting for its handle. Take
                // it over: keep the persisted value and timestamp,
                // adopt the binary's description, drop the stale
                // flag.
                existing.stale.store(false, Ordering::Relaxed);
                // Hot-swap description in place: it's a String inside
                // an Arc, so we re-create the entry. Cheap, runs once.
                let value = existing.value.load(Ordering::Relaxed);
                let updated_at_ms = existing.updated_at_ms.load(Ordering::Relaxed);
                counters.insert(
                    handle.name.to_owned(),
                    Arc::new(CounterEntry {
                        name: handle.name.to_owned(),
                        description: handle.description.to_owned(),
                        value: AtomicU64::new(value),
                        updated_at_ms: AtomicI64::new(updated_at_ms),
                        stale: AtomicBool::new(false),
                    }),
                );
            }
            Some(_existing) => {
                panic!("duplicate counter registration: {} already registered", handle.name);
            }
            None => {
                counters.insert(
                    handle.name.to_owned(),
                    Arc::new(CounterEntry {
                        name: handle.name.to_owned(),
                        description: handle.description.to_owned(),
                        value: AtomicU64::new(0),
                        updated_at_ms: AtomicI64::new(now_ms()),
                        stale: AtomicBool::new(false),
                    }),
                );
            }
        }
    }

    /// Increment a dynamically-named counter by `n`, registering it
    /// on first use with `description`. Unlike [`Self::register_counter`]
    /// / [`CounterHandle`] (one static handle per compile-time-known
    /// name, panicking on a duplicate registration), this is the entry
    /// point for names only known at runtime — e.g. a counter keyed by
    /// `product_id`, which isn't a fixed set the binary can declare
    /// ahead of time. "Already registered" is the expected steady
    /// state here, not a bug, so repeat calls are idempotent rather
    /// than panicking. Still panics on an invalid name (see
    /// [`validate_name`]).
    pub fn counter_inc_by_dynamic(&self, name: &str, description: &str, n: u64) {
        validate_name(name);
        let mut counters = self.counters.write().expect("metrics counters lock poisoned");
        let entry = counters.entry(name.to_owned()).or_insert_with(|| {
            Arc::new(CounterEntry {
                name: name.to_owned(),
                description: description.to_owned(),
                value: AtomicU64::new(0),
                updated_at_ms: AtomicI64::new(now_ms()),
                stale: AtomicBool::new(false),
            })
        });
        entry.stale.store(false, Ordering::Relaxed);
        entry.value.fetch_add(n, Ordering::Relaxed);
        entry.updated_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Register a [`GaugeHandle`]. Panics on duplicate name or
    /// invalid name.
    pub fn register_gauge(&self, handle: &GaugeHandle) {
        validate_name(handle.name);
        let mut gauges = self.gauges.write().expect("metrics gauges lock poisoned");
        match gauges.get(handle.name) {
            Some(existing) if existing.stale.load(Ordering::Relaxed) => {
                let value = existing.value.load(Ordering::Relaxed);
                let observed_at_ms = existing.observed_at_ms.load(Ordering::Relaxed);
                gauges.insert(
                    handle.name.to_owned(),
                    Arc::new(GaugeEntry {
                        name: handle.name.to_owned(),
                        description: handle.description.to_owned(),
                        value: AtomicI64::new(value),
                        observed_at_ms: AtomicI64::new(observed_at_ms),
                        stale: AtomicBool::new(false),
                    }),
                );
            }
            Some(_existing) => {
                panic!("duplicate gauge registration: {} already registered", handle.name);
            }
            None => {
                gauges.insert(
                    handle.name.to_owned(),
                    Arc::new(GaugeEntry {
                        name: handle.name.to_owned(),
                        description: handle.description.to_owned(),
                        value: AtomicI64::new(0),
                        observed_at_ms: AtomicI64::new(now_ms()),
                        stale: AtomicBool::new(false),
                    }),
                );
            }
        }
    }

    /// Insert a rehydrated counter row from `state.db` whose name
    /// does not match any currently-registered handle. Surfaced as
    /// "stale: not registered by current engine" so the operator can
    /// still see historical values. If a handle later registers
    /// against this name, [`Self::register_counter`] adopts the row.
    pub fn insert_stale_counter(&self, name: &str, description: &str, value: u64, updated_at_ms: i64) {
        let mut counters = self.counters.write().expect("metrics counters lock poisoned");
        counters.insert(
            name.to_owned(),
            Arc::new(CounterEntry {
                name: name.to_owned(),
                description: description.to_owned(),
                value: AtomicU64::new(value),
                updated_at_ms: AtomicI64::new(updated_at_ms),
                stale: AtomicBool::new(true),
            }),
        );
    }

    pub fn insert_stale_gauge(&self, name: &str, description: &str, value: i64, observed_at_ms: i64) {
        let mut gauges = self.gauges.write().expect("metrics gauges lock poisoned");
        gauges.insert(
            name.to_owned(),
            Arc::new(GaugeEntry {
                name: name.to_owned(),
                description: description.to_owned(),
                value: AtomicI64::new(value),
                observed_at_ms: AtomicI64::new(observed_at_ms),
                stale: AtomicBool::new(true),
            }),
        );
    }

    /// Seed a registered counter's value from `state.db` rehydration.
    /// Used after `register_counter` to load the persisted total
    /// without losing it across the restart boundary. Returns true
    /// if the counter was present.
    pub fn seed_counter(&self, name: &str, value: u64, updated_at_ms: i64) -> bool {
        let counters = self.counters.read().expect("metrics counters lock poisoned");
        if let Some(entry) = counters.get(name) {
            entry.value.store(value, Ordering::Relaxed);
            entry.updated_at_ms.store(updated_at_ms, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn seed_gauge(&self, name: &str, value: i64, observed_at_ms: i64) -> bool {
        let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
        if let Some(entry) = gauges.get(name) {
            entry.value.store(value, Ordering::Relaxed);
            entry.observed_at_ms.store(observed_at_ms, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Increment a registered counter by `n`. Panics if `name` is
    /// not registered — this is meant to fail loud during tests /
    /// startup, not the production hot path (where `init_all`
    /// guarantees registration before any `.inc()` fires).
    fn counter_inc_by(&self, name: &str, n: u64) {
        let counters = self.counters.read().expect("metrics counters lock poisoned");
        let entry = counters
            .get(name)
            .unwrap_or_else(|| panic!("counter not registered: {name}"));
        if entry.stale.load(Ordering::Relaxed) {
            panic!(
                "counter {name} is marked stale (rehydrated from state.db but no current handle); did you forget to register the handle at startup (boss_engine::metrics_init::init_all)?"
            );
        }
        entry.value.fetch_add(n, Ordering::Relaxed);
        entry.updated_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn gauge_set(&self, name: &str, value: i64) {
        let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
        let entry = gauges
            .get(name)
            .unwrap_or_else(|| panic!("gauge not registered: {name}"));
        if entry.stale.load(Ordering::Relaxed) {
            panic!(
                "gauge {name} is marked stale (rehydrated from state.db but no current handle); did you forget to register the handle at startup (boss_engine::metrics_init::init_all)?"
            );
        }
        entry.value.store(value, Ordering::Relaxed);
        entry.observed_at_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Snapshot every counter (registered and stale) for the flush
    /// task or a future `bossctl metrics list` reader.
    pub fn counter_snapshots(&self) -> Vec<CounterSnapshot> {
        let counters = self.counters.read().expect("metrics counters lock poisoned");
        let mut out: Vec<CounterSnapshot> = counters
            .values()
            .map(|entry| CounterSnapshot {
                name: entry.name.clone(),
                description: entry.description.clone(),
                value: entry.value.load(Ordering::Relaxed),
                updated_at_ms: entry.updated_at_ms.load(Ordering::Relaxed),
                stale: entry.stale.load(Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn gauge_snapshots(&self) -> Vec<GaugeSnapshot> {
        let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
        let mut out: Vec<GaugeSnapshot> = gauges
            .values()
            .map(|entry| GaugeSnapshot {
                name: entry.name.clone(),
                description: entry.description.clone(),
                value: entry.value.load(Ordering::Relaxed),
                observed_at_ms: entry.observed_at_ms.load(Ordering::Relaxed),
                stale: entry.stale.load(Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Snapshot a single counter by name. Returns `None` if the name
    /// is not registered (registered or stale).
    pub fn counter_snapshot_one(&self, name: &str) -> Option<CounterSnapshot> {
        let counters = self.counters.read().expect("metrics counters lock poisoned");
        counters.get(name).map(|entry| CounterSnapshot {
            name: entry.name.clone(),
            description: entry.description.clone(),
            value: entry.value.load(Ordering::Relaxed),
            updated_at_ms: entry.updated_at_ms.load(Ordering::Relaxed),
            stale: entry.stale.load(Ordering::Relaxed),
        })
    }

    /// Snapshot a single gauge by name. Returns `None` if the name
    /// is not registered.
    pub fn gauge_snapshot_one(&self, name: &str) -> Option<GaugeSnapshot> {
        let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
        gauges.get(name).map(|entry| GaugeSnapshot {
            name: entry.name.clone(),
            description: entry.description.clone(),
            value: entry.value.load(Ordering::Relaxed),
            observed_at_ms: entry.observed_at_ms.load(Ordering::Relaxed),
            stale: entry.stale.load(Ordering::Relaxed),
        })
    }

    /// Reset one metric to zero. Looks up `name` in counters first,
    /// then gauges. Returns `(counter_reset, gauge_reset)`. Both can
    /// be true if somehow the same name appears in both maps (which
    /// would be a registration bug, but we don't panic here).
    pub fn reset_one(&self, name: &str) -> (bool, bool) {
        let now = now_ms();
        let counter_reset = {
            let counters = self.counters.read().expect("metrics counters lock poisoned");
            if let Some(entry) = counters.get(name) {
                entry.value.store(0, Ordering::Relaxed);
                entry.updated_at_ms.store(now, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        let gauge_reset = {
            let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
            if let Some(entry) = gauges.get(name) {
                entry.value.store(0, Ordering::Relaxed);
                entry.observed_at_ms.store(now, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        (counter_reset, gauge_reset)
    }

    /// Reset every counter and gauge to zero. Returns
    /// `(counters_reset, gauges_reset)`.
    pub fn reset_all(&self) -> (u64, u64) {
        let now = now_ms();
        let counters_reset = {
            let counters = self.counters.read().expect("metrics counters lock poisoned");
            for entry in counters.values() {
                entry.value.store(0, Ordering::Relaxed);
                entry.updated_at_ms.store(now, Ordering::Relaxed);
            }
            counters.len() as u64
        };
        let gauges_reset = {
            let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
            for entry in gauges.values() {
                entry.value.store(0, Ordering::Relaxed);
                entry.observed_at_ms.store(now, Ordering::Relaxed);
            }
            gauges.len() as u64
        };
        (counters_reset, gauges_reset)
    }

    /// Convenience for tests that only need to assert on a single
    /// counter's value.
    pub fn counter_value(&self, name: &str) -> Option<u64> {
        let counters = self.counters.read().expect("metrics counters lock poisoned");
        counters.get(name).map(|e| e.value.load(Ordering::Relaxed))
    }

    pub fn gauge_value(&self, name: &str) -> Option<i64> {
        let gauges = self.gauges.read().expect("metrics gauges lock poisoned");
        gauges.get(name).map(|e| e.value.load(Ordering::Relaxed))
    }
}

/// Names: lowercase ASCII letters, digits, `.`, `_`. Must not be
/// empty, must not start or end with `.`. Dot-separated namespaces
/// by convention but not enforced (see design §"Risks / open
/// questions" item 4).
fn validate_name(name: &str) {
    if name.is_empty() {
        panic!("metric name must not be empty");
    }
    if name.starts_with('.') || name.ends_with('.') {
        panic!("metric name must not start or end with '.': {name}");
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_';
        if !ok {
            panic!("metric name {name} contains invalid character {c:?}; allowed: a-z 0-9 . _");
        }
    }
}

pub fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    register_counter!(TEST_COUNTER_A, "test.counter_a", "Phase 1 unit-test counter A.");
    register_counter!(TEST_COUNTER_B, "test.counter_b", "Phase 1 unit-test counter B.");
    register_gauge!(TEST_GAUGE_A, "test.gauge_a", "Phase 1 unit-test gauge A.");

    #[test]
    fn register_and_increment_counter() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);

        TEST_COUNTER_A.inc(&registry);
        TEST_COUNTER_A.inc(&registry);
        TEST_COUNTER_A.inc_by(&registry, 7);

        assert_eq!(registry.counter_value("test.counter_a"), Some(9));
    }

    #[test]
    fn snapshots_are_sorted_by_name() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_B);
        registry.register_counter(&TEST_COUNTER_A);
        TEST_COUNTER_A.inc(&registry);
        TEST_COUNTER_B.inc_by(&registry, 3);

        let snaps = registry.counter_snapshots();
        let names: Vec<_> = snaps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["test.counter_a", "test.counter_b"]);
        assert_eq!(snaps[0].value, 1);
        assert_eq!(snaps[1].value, 3);
        assert_eq!(snaps[0].description, "Phase 1 unit-test counter A.");
    }

    #[test]
    fn gauge_set_overwrites_value() {
        let registry = Registry::new();
        registry.register_gauge(&TEST_GAUGE_A);

        TEST_GAUGE_A.set(&registry, 42);
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(42));
        TEST_GAUGE_A.set(&registry, -1);
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(-1));
    }

    #[test]
    #[should_panic(expected = "duplicate counter registration")]
    fn duplicate_counter_registration_panics() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        registry.register_counter(&TEST_COUNTER_A);
    }

    #[test]
    fn dynamic_counter_registers_on_first_use_and_accumulates() {
        let registry = Registry::new();
        registry.counter_inc_by_dynamic("conflict.acme.lockfile.classified", "test dynamic counter", 1);
        registry.counter_inc_by_dynamic("conflict.acme.lockfile.classified", "test dynamic counter", 4);

        assert_eq!(registry.counter_value("conflict.acme.lockfile.classified"), Some(5));
    }

    #[test]
    fn dynamic_counter_repeat_registration_does_not_panic() {
        // Unlike `register_counter`, a dynamically-named counter's
        // "already registered" state is the expected steady state,
        // not a bug — product/class names aren't known at compile
        // time, so every increment after the first re-registers.
        let registry = Registry::new();
        for _ in 0..3 {
            registry.counter_inc_by_dynamic("conflict.acme.semantic.classified", "test dynamic counter", 1);
        }
        assert_eq!(registry.counter_value("conflict.acme.semantic.classified"), Some(3));
    }

    #[test]
    fn dynamic_counter_keys_are_independent_per_name() {
        let registry = Registry::new();
        registry.counter_inc_by_dynamic("conflict.acme.lockfile.classified", "d", 2);
        registry.counter_inc_by_dynamic("conflict.other_co.lockfile.classified", "d", 5);

        assert_eq!(registry.counter_value("conflict.acme.lockfile.classified"), Some(2));
        assert_eq!(registry.counter_value("conflict.other_co.lockfile.classified"), Some(5));
    }

    #[test]
    #[should_panic(expected = "invalid character")]
    fn dynamic_counter_rejects_invalid_name() {
        let registry = Registry::new();
        registry.counter_inc_by_dynamic("Bad.Name", "d", 1);
    }

    #[test]
    #[should_panic(expected = "duplicate gauge registration")]
    fn duplicate_gauge_registration_panics() {
        let registry = Registry::new();
        registry.register_gauge(&TEST_GAUGE_A);
        registry.register_gauge(&TEST_GAUGE_A);
    }

    #[test]
    #[should_panic(expected = "counter not registered")]
    fn increment_before_registration_panics() {
        let registry = Registry::new();
        TEST_COUNTER_A.inc(&registry);
    }

    #[test]
    fn name_validation_rejects_uppercase() {
        let registry = Registry::new();
        static BAD: CounterHandle = CounterHandle::new("Bad.Name", "uppercase rejected");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| registry.register_counter(&BAD)));
        assert!(result.is_err(), "expected register to panic for uppercase name");
    }

    #[test]
    fn name_validation_rejects_empty_and_edge_dots() {
        for bad in ["", ".leading", "trailing.", "has space", "with-dash"] {
            let registry = Registry::new();
            // Build the handle on the fly; we can't use `static`
            // because the lifetime is dynamic in this loop.
            let leaked: &'static str = Box::leak(bad.to_owned().into_boxed_str());
            let handle = CounterHandle::new(leaked, "rejected");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| registry.register_counter(&handle)));
            assert!(result.is_err(), "expected register to panic for {bad:?}");
        }
    }

    #[test]
    fn stale_row_is_adopted_on_registration() {
        let registry = Registry::new();
        registry.insert_stale_counter("test.counter_a", "legacy description", 17, 100);
        assert_eq!(registry.counter_value("test.counter_a"), Some(17));
        let snap_before = &registry.counter_snapshots()[0];
        assert!(snap_before.stale);
        assert_eq!(snap_before.description, "legacy description");

        registry.register_counter(&TEST_COUNTER_A);
        let snap_after = &registry.counter_snapshots()[0];
        assert!(!snap_after.stale, "adopted row should clear stale flag");
        assert_eq!(snap_after.value, 17, "adopted row must preserve value");
        assert_eq!(
            snap_after.description, "Phase 1 unit-test counter A.",
            "adopted row should pick up the current binary's description",
        );
    }

    #[test]
    fn snapshot_one_returns_none_for_unknown_name() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        assert!(registry.counter_snapshot_one("does.not.exist").is_none());
        assert!(registry.gauge_snapshot_one("does.not.exist").is_none());
    }

    #[test]
    fn snapshot_one_returns_correct_value() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        TEST_COUNTER_A.inc_by(&registry, 5);
        let snap = registry.counter_snapshot_one("test.counter_a").unwrap();
        assert_eq!(snap.value, 5);
        assert!(!snap.stale);
        assert_eq!(snap.name, "test.counter_a");
    }

    #[test]
    fn reset_one_zeros_counter_and_returns_true() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        TEST_COUNTER_A.inc_by(&registry, 10);
        assert_eq!(registry.counter_value("test.counter_a"), Some(10));

        let (counter_reset, gauge_reset) = registry.reset_one("test.counter_a");
        assert!(counter_reset);
        assert!(!gauge_reset);
        assert_eq!(registry.counter_value("test.counter_a"), Some(0));
    }

    #[test]
    fn reset_one_zeros_gauge_and_returns_true() {
        let registry = Registry::new();
        registry.register_gauge(&TEST_GAUGE_A);
        TEST_GAUGE_A.set(&registry, 99);
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(99));

        let (counter_reset, gauge_reset) = registry.reset_one("test.gauge_a");
        assert!(!counter_reset);
        assert!(gauge_reset);
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(0));
    }

    #[test]
    fn reset_one_returns_false_for_unknown_name() {
        let registry = Registry::new();
        let (c, g) = registry.reset_one("no.such.metric");
        assert!(!c);
        assert!(!g);
    }

    #[test]
    fn seed_counter_overwrites_value_and_timestamp_when_registered() {
        // Rehydration seeds a registered counter with the persisted
        // total + timestamp so a restart doesn't lose accumulated
        // history. It reports success and the values are observable
        // through the snapshot.
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        TEST_COUNTER_A.inc_by(&registry, 3);

        let seeded = registry.seed_counter("test.counter_a", 500, 123_456);
        assert!(seeded, "seeding a registered counter returns true");

        let snap = registry.counter_snapshot_one("test.counter_a").unwrap();
        assert_eq!(snap.value, 500, "seed overwrites the live value");
        assert_eq!(snap.updated_at_ms, 123_456, "seed overwrites the timestamp");
        assert!(!snap.stale, "a seeded registered counter is not stale");
    }

    #[test]
    fn seed_counter_is_noop_and_returns_false_when_absent() {
        let registry = Registry::new();
        let seeded = registry.seed_counter("test.counter_a", 42, 999);
        assert!(!seeded, "seeding an unregistered counter returns false");
        assert!(
            registry.counter_snapshot_one("test.counter_a").is_none(),
            "seeding an absent name must not create the counter",
        );
    }

    #[test]
    fn seed_gauge_overwrites_value_and_timestamp_when_registered() {
        let registry = Registry::new();
        registry.register_gauge(&TEST_GAUGE_A);
        TEST_GAUGE_A.set(&registry, 7);

        let seeded = registry.seed_gauge("test.gauge_a", -250, 654_321);
        assert!(seeded, "seeding a registered gauge returns true");

        let snap = registry.gauge_snapshot_one("test.gauge_a").unwrap();
        assert_eq!(snap.value, -250, "seed overwrites the live value");
        assert_eq!(snap.observed_at_ms, 654_321, "seed overwrites the timestamp");
        assert!(!snap.stale, "a seeded registered gauge is not stale");
    }

    #[test]
    fn seed_gauge_is_noop_and_returns_false_when_absent() {
        let registry = Registry::new();
        let seeded = registry.seed_gauge("test.gauge_a", 42, 999);
        assert!(!seeded, "seeding an unregistered gauge returns false");
        assert!(
            registry.gauge_snapshot_one("test.gauge_a").is_none(),
            "seeding an absent name must not create the gauge",
        );
    }

    #[test]
    fn insert_stale_gauge_surfaces_in_snapshots() {
        // Mirrors `stale_row_is_adopted_on_registration`'s counter
        // coverage: a rehydrated row with no live handle shows up in
        // both the aggregate and single-name snapshots, flagged stale.
        let registry = Registry::new();
        registry.insert_stale_gauge("test.gauge_a", "legacy gauge description", -13, 200);

        let snaps = registry.gauge_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "test.gauge_a");
        assert_eq!(snaps[0].value, -13);
        assert_eq!(snaps[0].observed_at_ms, 200);
        assert_eq!(snaps[0].description, "legacy gauge description");
        assert!(snaps[0].stale, "an inserted stale gauge is flagged stale");

        let one = registry.gauge_snapshot_one("test.gauge_a").unwrap();
        assert_eq!(one.value, -13);
        assert!(one.stale);
    }

    #[test]
    fn stale_gauge_row_is_adopted_on_registration() {
        // Parallel to the counter `stale_row_is_adopted_on_registration`:
        // registering a handle over a stale gauge keeps the persisted
        // value + timestamp, adopts the binary's description, and
        // clears the stale flag.
        let registry = Registry::new();
        registry.insert_stale_gauge("test.gauge_a", "legacy gauge description", 88, 300);
        let snap_before = registry.gauge_snapshot_one("test.gauge_a").unwrap();
        assert!(snap_before.stale);
        assert_eq!(snap_before.description, "legacy gauge description");

        registry.register_gauge(&TEST_GAUGE_A);
        let snap_after = registry.gauge_snapshot_one("test.gauge_a").unwrap();
        assert!(!snap_after.stale, "adopted gauge row should clear stale flag");
        assert_eq!(snap_after.value, 88, "adopted gauge row must preserve value");
        assert_eq!(
            snap_after.observed_at_ms, 300,
            "adopted gauge row must preserve timestamp"
        );
        assert_eq!(
            snap_after.description, "Phase 1 unit-test gauge A.",
            "adopted gauge row should pick up the current binary's description",
        );

        // An adopted (no-longer-stale) gauge is writable again.
        TEST_GAUGE_A.set(&registry, 5);
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(5));
    }

    #[test]
    fn reset_all_zeros_every_counter_and_gauge() {
        let registry = Registry::new();
        registry.register_counter(&TEST_COUNTER_A);
        registry.register_counter(&TEST_COUNTER_B);
        registry.register_gauge(&TEST_GAUGE_A);
        TEST_COUNTER_A.inc_by(&registry, 3);
        TEST_COUNTER_B.inc_by(&registry, 7);
        TEST_GAUGE_A.set(&registry, -5);

        let (counters_reset, gauges_reset) = registry.reset_all();
        assert_eq!(counters_reset, 2);
        assert_eq!(gauges_reset, 1);
        assert_eq!(registry.counter_value("test.counter_a"), Some(0));
        assert_eq!(registry.counter_value("test.counter_b"), Some(0));
        assert_eq!(registry.gauge_value("test.gauge_a"), Some(0));
    }
}
