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
/// Completed, cancelled and failed discoveries release their budget on drop;
/// established sessions never count as startup contention.
#[derive(Default)]
pub struct DiscoveryLoad {
    pending: Mutex<Vec<Weak<AtomicUsize>>>,
}

impl DiscoveryLoad {
    pub fn begin(&self) -> DiscoveryBudget {
        let mut pending = self.pending.lock().expect("discovery load mutex poisoned");
        pending.retain(|entry| entry.strong_count() > 0);
        let count = pending.len() + 1;
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
        // solo allowance: six peers get 165s, retaining >=36s of headroom.
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
/// this policy never sleeps or requires an extra operator action.
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
mod tests;
