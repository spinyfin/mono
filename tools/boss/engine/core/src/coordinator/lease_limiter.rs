//! Per-repo bound on concurrent cube workspace-lease attempts.
//!
//! Cube serializes the registry-mutation portion of `workspace lease` on a
//! per-repo `RepoLock` and then drops that lock before the local jj probe
//! and setup so one other lease can overlap. A pile of concurrent
//! `cube workspace lease` processes on the same repo does not make that
//! faster: they queue on the lock, and a `cube repo gc` holding it makes
//! every waiter hit the engine's 90s lease timeout. This limiter keeps
//! in-flight attempts at cube's designed overlap (see
//! [`super::MAX_CONCURRENT_LEASE_ATTEMPTS_PER_REPO`]).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::ExecutionCoordinator;

struct RepoState {
    in_flight: usize,
    /// Set when `try_begin` misses because this repo is at the bound.
    /// Cleared when a finishing attempt observes it so a successful
    /// lease only re-kicks the scheduler if someone actually waited.
    denied: bool,
}

struct Inner {
    max: usize,
    counts: Mutex<HashMap<String, RepoState>>,
}

/// Counts in-flight lease attempts per repo origin URL.
#[derive(Clone)]
pub(super) struct RepoLeaseLimiter {
    inner: Arc<Inner>,
}

/// Decrements the per-repo in-flight count when dropped, and kicks the
/// scheduler so rows that waited on the bound get another drain pass.
pub(super) struct LeaseAttemptGuard {
    inner: Arc<Inner>,
    repo: String,
    kick: Option<Arc<ExecutionCoordinator>>,
}

impl RepoLeaseLimiter {
    pub(super) fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                max: max.max(1),
                counts: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Try to start one more in-flight attempt for `repo`. `None` means
    /// the bound is already sat; the caller must not claim a worker slot.
    pub(super) fn try_begin(&self, repo: &str) -> Option<LeaseAttemptGuard> {
        let mut counts = self.inner.counts.lock().expect("repo lease limiter lock poisoned");
        let entry = counts.entry(repo.to_owned()).or_insert(RepoState {
            in_flight: 0,
            denied: false,
        });
        if entry.in_flight >= self.inner.max {
            entry.denied = true;
            return None;
        }
        entry.in_flight += 1;
        Some(LeaseAttemptGuard {
            inner: Arc::clone(&self.inner),
            repo: repo.to_owned(),
            kick: None,
        })
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self, repo: &str) -> usize {
        self.inner
            .counts
            .lock()
            .expect("repo lease limiter lock poisoned")
            .get(repo)
            .map(|state| state.in_flight)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn denied(&self, repo: &str) -> bool {
        self.inner
            .counts
            .lock()
            .expect("repo lease limiter lock poisoned")
            .get(repo)
            .is_some_and(|state| state.denied)
    }

    pub(super) fn max(&self) -> usize {
        self.inner.max
    }
}

impl LeaseAttemptGuard {
    /// Kick the scheduler when this attempt ends so rows held on the
    /// per-repo bound are drained again as soon as a permit frees.
    pub(super) fn with_kick(mut self, coordinator: Arc<ExecutionCoordinator>) -> Self {
        self.kick = Some(coordinator);
        self
    }

    /// On a successful spawn, only re-enter the scheduler if this repo
    /// actually had a bound miss. Unconditional kick after spawn re-drains
    /// duplicate `ready` rows for the same work item while the winner is
    /// still attaching, and the lost-workspace reconciler then treats a
    /// test (or otherwise empty) workspace dir as a dead winner.
    pub(super) fn take_kick_if_denied(&mut self) -> Option<Arc<ExecutionCoordinator>> {
        let kick = self.kick.take();
        if self.take_denied() { kick } else { None }
    }

    fn take_denied(&self) -> bool {
        let Ok(mut counts) = self.inner.counts.lock() else {
            return false;
        };
        let Some(entry) = counts.get_mut(&self.repo) else {
            return false;
        };
        let denied = entry.denied;
        entry.denied = false;
        denied
    }
}

impl Drop for LeaseAttemptGuard {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.inner.counts.lock()
            && let Some(entry) = counts.get_mut(&self.repo)
        {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            if entry.in_flight == 0 {
                counts.remove(&self.repo);
            }
        }
        if let Some(coordinator) = self.kick.take() {
            coordinator.kick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RepoLeaseLimiter;

    #[test]
    fn try_begin_respects_the_bound_and_drop_frees_a_slot() {
        let limiter = RepoLeaseLimiter::new(2);
        let a = limiter.try_begin("origin").expect("first");
        let b = limiter.try_begin("origin").expect("second");
        assert!(limiter.try_begin("origin").is_none());
        assert_eq!(limiter.in_flight("origin"), 2);
        drop(a);
        assert_eq!(limiter.in_flight("origin"), 1);
        let c = limiter.try_begin("origin").expect("after drop");
        assert!(limiter.try_begin("origin").is_none());
        drop(b);
        drop(c);
        assert_eq!(limiter.in_flight("origin"), 0);
    }

    #[test]
    fn bounds_are_per_repo() {
        let limiter = RepoLeaseLimiter::new(1);
        let a = limiter.try_begin("repo-a").expect("a");
        let b = limiter.try_begin("repo-b").expect("b");
        assert!(limiter.try_begin("repo-a").is_none());
        assert!(limiter.try_begin("repo-b").is_none());
        drop(a);
        drop(b);
    }

    #[test]
    fn zero_max_is_clamped_to_one() {
        let limiter = RepoLeaseLimiter::new(0);
        assert_eq!(limiter.max(), 1);
        assert!(limiter.try_begin("origin").is_some());
    }

    #[test]
    fn denied_stays_false_when_nobody_waits() {
        let limiter = RepoLeaseLimiter::new(2);
        let a = limiter.try_begin("origin").expect("first");
        assert!(!limiter.denied("origin"));
        assert!(!a.take_denied());
        drop(a);
    }

    #[test]
    fn denied_is_one_shot_after_a_bound_miss() {
        let limiter = RepoLeaseLimiter::new(1);
        let a = limiter.try_begin("origin").expect("first");
        assert!(limiter.try_begin("origin").is_none());
        assert!(limiter.denied("origin"));
        assert!(a.take_denied());
        assert!(!limiter.denied("origin"));
        drop(a);
    }
}
