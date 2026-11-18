//! Startup contention policy, independent of drivers, transport and the engine.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// The slowest measured solo Codex startup (2026-09-13). While startup
/// remains unverified, resume admits at most one more worker per interval.
/// Waiting for proof without this escape would space dead-driver reaps
/// 300s apart, outside the breaker's three-failures-in-300s window.
pub const RESUME_STARTUP_INTERVAL: Duration = Duration::from_secs(84);

/// Pending discoveries share a high-water mark of overlapping startups.
/// The mark is driver-agnostic: Claude, grok, and Codex startups all raise
/// it, because the contention being modelled is host CPU and IO during
/// process startup, not JSONL file discovery. Completed, cancelled and
/// failed discoveries release their budget on drop; established sessions
/// never count as startup contention.
#[derive(Default)]
pub struct DiscoveryLoad {
    pending: Mutex<Vec<Weak<AtomicUsize>>>,
    /// Snapshot refreshed by live-slot registration, release, driver signals,
    /// registry attachment, and the 60s spawn-ack/driver-start sweep.
    /// Counts live slots still inside their driver-start window with no driver
    /// signal. JSONL `begin` is often armed before the pane registers, so
    /// this count plus one is the overlapping-startup floor.
    in_flight: AtomicUsize,
}

impl DiscoveryLoad {
    /// Raise every pending discovery's peak to at least `count` overlapping
    /// in-flight startups. `count` is the live-registry total (every driver),
    /// not the JSONL-only pending set.
    pub fn observe_in_flight(&self, count: usize) {
        self.in_flight.store(count, Ordering::Relaxed);
        let mut pending = self.pending.lock().expect("discovery load mutex poisoned");
        pending.retain(|entry| entry.strong_count() > 0);
        for entry in pending.iter().filter_map(Weak::upgrade) {
            entry.fetch_max(count, Ordering::Relaxed);
        }
    }

    pub fn begin(&self) -> DiscoveryBudget {
        let mut pending = self.pending.lock().expect("discovery load mutex poisoned");
        pending.retain(|entry| entry.strong_count() > 0);
        // `begin` usually runs before this worker is a live slot, so add one
        // to the registry count. JSONL-only tests never call observe, so
        // `pending.len() + 1` remains the overlapping-discovery count.
        let from_discoveries = pending.len() + 1;
        let from_slots = self.in_flight.load(Ordering::Relaxed).saturating_add(1);
        let count = from_discoveries.max(from_slots);
        for entry in pending.iter().filter_map(Weak::upgrade) {
            entry.fetch_max(count, Ordering::Relaxed);
        }
        let peak = Arc::new(AtomicUsize::new(count));
        pending.push(Arc::downgrade(&peak));
        DiscoveryBudget { peak }
    }
}

/// A deadline allowance measured from discovery start, never from the latest
/// peer's arrival. Later peers extend earlier workers too; peers finishing
/// cannot retract time already granted for contention they caused.
pub struct DiscoveryBudget {
    peak: Arc<AtomicUsize>,
}

impl DiscoveryBudget {
    pub fn timeout(&self) -> Duration {
        // 2026-09-13 Codex measurements: 78–84s solo, 119–120s at six-way
        // startup. The conservative slowdown is (120 - 78) / 5 = 8.4s per
        // additional startup, rounded up to 9s. Preserve the existing 120s
        // solo allowance: six overlapping discoveries get 120 + 5*9 = 165s,
        // ~45s above the measured 119–120s six-way startup.
        // This is a contention allowance, not a throughput prediction beyond
        // the measured six. Cap at twice the measured burst (240s), leaving
        // 60s for provisioning and signal delivery inside the separate 300s
        // driver-start grace.
        let peers = self.peak.load(Ordering::Relaxed).saturating_sub(1);
        Duration::from_secs(120 + peers.saturating_mul(9).min(120) as u64)
    }
}

/// Only the ready backlog captured on resume is paced. New arrivals keep
/// normal admission, so a dependency-held backlog row cannot permanently
/// throttle steady-state work. The scheduler supplies time and retries;
/// this policy never sleeps and needs no manual step.
#[derive(Default)]
pub struct ResumeAdmission {
    capture_pending: bool,
    backlog: HashSet<String>,
    last_admission: Option<Instant>,
}

impl ResumeAdmission {
    pub fn resume(&mut self, now: Instant) {
        self.capture_pending = true;
        self.backlog.clear();
        self.last_admission = Some(now);
    }

    pub fn observe_ready(&mut self, ready: impl Iterator<Item = String>) {
        if !self.capture_pending && self.backlog.is_empty() {
            return;
        }
        let ready: HashSet<_> = ready.collect();
        if self.capture_pending {
            self.backlog = ready;
            self.capture_pending = false;
        } else {
            self.backlog.retain(|id| ready.contains(id));
        }
    }

    pub fn admitted(&mut self, execution_id: &str, now: Instant) {
        if self.backlog.contains(execution_id) {
            self.last_admission = Some(now);
        }
    }

    pub fn holds(&self, execution_id: &str, startup_pending: bool, now: Instant) -> bool {
        // Solo Codex startup was measured at 78–84s; six concurrent starts
        // reached 119–120s and exhausted discovery. Drain the resumed cohort
        // on driver proof, with a bounded no-signal fallback so dead spawns
        // still accumulate inside the unchanged circuit breaker's window.
        startup_pending
            && self.backlog.contains(execution_id)
            && self
                .last_admission
                .is_some_and(|last| now.saturating_duration_since(last) < RESUME_STARTUP_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_startups_from_other_drivers_raise_jsonl_deadline() {
        let load = DiscoveryLoad::default();
        let first = load.begin();
        assert_eq!(first.timeout(), Duration::from_secs(120));
        // Three Claude/grok live slots raise this already-armed Codex discovery:
        // peak 3 → 120 + 2*9 = 138s.
        load.observe_in_flight(3);
        assert_eq!(first.timeout(), Duration::from_secs(138));
        load.observe_in_flight(5);
        assert_eq!(first.timeout(), Duration::from_secs(156));
        load.observe_in_flight(6);
        assert_eq!(first.timeout(), Duration::from_secs(165));
        load.observe_in_flight(1);
        assert_eq!(
            first.timeout(),
            Duration::from_secs(165),
            "a peer finishing cannot retract time already granted"
        );
    }

    #[test]
    fn begin_sees_in_flight_slots_already_observed() {
        let load = DiscoveryLoad::default();
        load.observe_in_flight(5);
        let first = load.begin();
        // Five live slots plus this discovery that has not registered yet.
        assert_eq!(first.timeout(), Duration::from_secs(165));
    }

    #[test]
    fn burst_extends_first_worker_and_retains_contention_after_peers_finish() {
        let load = DiscoveryLoad::default();
        let first = load.begin();
        assert_eq!(first.timeout(), Duration::from_secs(120));
        let peers: Vec<_> = (0..5).map(|_| load.begin()).collect();
        assert_eq!(first.timeout(), Duration::from_secs(165));
        assert!(peers.iter().all(|peer| peer.timeout() == first.timeout()));
        drop(peers);
        assert_eq!(first.timeout(), Duration::from_secs(165));
        drop(first);
        assert_eq!(load.begin().timeout(), Duration::from_secs(120));
    }

    #[test]
    fn repeated_arrivals_cannot_extend_a_dead_spawn_forever() {
        let load = DiscoveryLoad::default();
        let first = load.begin();
        let peers: Vec<_> = (0..100).map(|_| load.begin()).collect();
        assert_eq!(first.timeout(), Duration::from_secs(240));
        drop(peers);
        for _ in 0..100 {
            assert_eq!(load.begin().timeout(), Duration::from_secs(129));
        }
        assert_eq!(first.timeout(), Duration::from_secs(240));
    }

    #[test]
    fn resume_paces_backlog_until_driver_ready_but_never_new_arrivals() {
        let now = Instant::now();
        let mut admission = ResumeAdmission::default();
        admission.observe_ready(["one".into(), "two".into()].into_iter());
        assert!(!admission.holds("two", true, now));
        admission.resume(now);
        admission.observe_ready(["one".into(), "two".into()].into_iter());
        assert!(!admission.holds("one", false, now));
        assert!(admission.holds("two", true, now));
        assert!(!admission.holds("two", false, now));
        admission.observe_ready(["two".into(), "new".into()].into_iter());
        assert!(!admission.holds("new", true, now));
        assert!(admission.holds("two", true, now));
        admission.observe_ready(std::iter::empty());
        assert!(!admission.holds("two", true, now));
        admission.resume(now);
        admission.observe_ready(["new".into()].into_iter());
        assert!(admission.holds("new", true, now));
    }

    #[test]
    fn no_signal_fallback_reopens_one_admission_and_restarts_only_on_actual_admission() {
        let now = Instant::now();
        let mut admission = ResumeAdmission::default();
        admission.resume(now);
        admission.observe_ready(["one".into(), "two".into(), "three".into()].into_iter());
        admission.admitted("one", now);
        let before = now + RESUME_STARTUP_INTERVAL - Duration::from_millis(1);
        assert!(admission.holds("two", true, before));
        let due = now + RESUME_STARTUP_INTERVAL;
        assert!(!admission.holds("two", true, due));
        // Merely considering an ineligible row must not consume the allowance.
        assert!(!admission.holds("three", true, due));
        admission.admitted("two", due);
        assert!(admission.holds("three", true, due));
        assert!(!admission.holds("three", false, due));
        assert!(!admission.holds("three", true, due + RESUME_STARTUP_INTERVAL));
    }
}
