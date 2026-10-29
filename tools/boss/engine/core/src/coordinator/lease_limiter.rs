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

#[derive(Default)]
struct State {
    /// In-flight attempts per repo origin.
    repos: HashMap<String, RepoState>,
    /// Execution id -> repo origin for every attempt that still holds a
    /// permit, so a permit can be released by execution id from inside
    /// `schedule_execution` the moment the lease returns.
    held: HashMap<String, Held>,
    next_token: u64,
}

struct Held {
    repo: String,
    /// Distinguishes successive permits for the same execution, so a
    /// stale guard dropping late cannot free a newer attempt's permit.
    token: u64,
}

struct RepoState {
    in_flight: usize,
    /// Set when `try_begin` misses because this repo is at the bound.
    /// Cleared by the release that frees a permit, so the scheduler is
    /// only re-kicked when some row actually waited.
    denied: bool,
}

struct Inner {
    max: usize,
    state: Mutex<State>,
}

impl Inner {
    /// Release the permit held by `execution_id`, if any. The decrement
    /// and the test-and-clear of `denied` happen in one critical section,
    /// so a `try_begin` miss either lands before it (and is reported here)
    /// or after it (and finds the freed permit). Returns whether a waiter
    /// was denied in the meantime.
    fn release(&self, execution_id: &str, token: Option<u64>) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if token.is_some_and(|token| state.held.get(execution_id).is_none_or(|held| held.token != token)) {
            return false;
        }
        let Some(Held { repo, .. }) = state.held.remove(execution_id) else {
            return false;
        };
        let Some(entry) = state.repos.get_mut(&repo) else {
            return false;
        };
        entry.in_flight = entry.in_flight.saturating_sub(1);
        let denied = std::mem::take(&mut entry.denied);
        if entry.in_flight == 0 {
            state.repos.remove(&repo);
        }
        denied
    }
}

/// Counts in-flight lease attempts per repo origin URL.
#[derive(Clone)]
pub(super) struct RepoLeaseLimiter {
    inner: Arc<Inner>,
}

/// Holds one per-repo lease-attempt permit. The permit covers the lease
/// itself: `schedule_execution` releases it as soon as the lease call
/// returns ([`RepoLeaseLimiter::release`]), and dropping the guard frees it
/// on every other path (early return, failure, panic). When the release
/// freed a permit that a row was denied, the scheduler is kicked so that
/// row is drained again.
pub(super) struct LeaseAttemptGuard {
    inner: Arc<Inner>,
    execution_id: String,
    token: u64,
    kick: Option<Arc<ExecutionCoordinator>>,
}

impl RepoLeaseLimiter {
    pub(super) fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                max: max.max(1),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Try to start one more in-flight attempt for `repo` on behalf of
    /// `execution_id`. `None` means the bound is already sat; the caller
    /// must not claim a worker slot.
    pub(super) fn try_begin(&self, repo: &str, execution_id: &str) -> Option<LeaseAttemptGuard> {
        let mut state = self.inner.state.lock().expect("repo lease limiter lock poisoned");
        if state.held.contains_key(execution_id) {
            return None;
        }
        let entry = state.repos.entry(repo.to_owned()).or_insert(RepoState {
            in_flight: 0,
            denied: false,
        });
        if entry.in_flight >= self.inner.max {
            entry.denied = true;
            return None;
        }
        entry.in_flight += 1;
        state.next_token += 1;
        let token = state.next_token;
        state.held.insert(
            execution_id.to_owned(),
            Held {
                repo: repo.to_owned(),
                token,
            },
        );
        Some(LeaseAttemptGuard {
            inner: Arc::clone(&self.inner),
            execution_id: execution_id.to_owned(),
            token,
            kick: None,
        })
    }

    /// Release the permit held for `execution_id` (a no-op when none is
    /// held). Returns `true` when a row was denied while it was held, in
    /// which case the caller must kick the scheduler.
    pub(super) fn release(&self, execution_id: &str) -> bool {
        self.inner.release(execution_id, None)
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self, repo: &str) -> usize {
        self.inner
            .state
            .lock()
            .expect("repo lease limiter lock poisoned")
            .repos
            .get(repo)
            .map(|state| state.in_flight)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn denied(&self, repo: &str) -> bool {
        self.inner
            .state
            .lock()
            .expect("repo lease limiter lock poisoned")
            .repos
            .get(repo)
            .is_some_and(|state| state.denied)
    }

    pub(super) fn max(&self) -> usize {
        self.inner.max
    }
}

impl LeaseAttemptGuard {
    /// Kick the scheduler when this guard frees a permit that a row was
    /// denied, so held rows are drained again without waiting for the
    /// next heartbeat. Attached only once the permit backs a claimed slot:
    /// a permit dropped by the drain before any slot was claimed must not
    /// kick, because that would re-enter the drain for rows that cannot
    /// dispatch.
    pub(super) fn with_kick(mut self, coordinator: Arc<ExecutionCoordinator>) -> Self {
        self.kick = Some(coordinator);
        self
    }
}

impl Drop for LeaseAttemptGuard {
    fn drop(&mut self) {
        let denied = self.inner.release(&self.execution_id, Some(self.token));
        if denied && let Some(coordinator) = self.kick.take() {
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
        let a = limiter.try_begin("origin", "exec-a").expect("first");
        let b = limiter.try_begin("origin", "exec-b").expect("second");
        assert!(limiter.try_begin("origin", "exec-c").is_none());
        assert_eq!(limiter.in_flight("origin"), 2);
        drop(a);
        assert_eq!(limiter.in_flight("origin"), 1);
        let c = limiter.try_begin("origin", "exec-c").expect("after drop");
        assert!(limiter.try_begin("origin", "exec-d").is_none());
        drop(b);
        drop(c);
        assert_eq!(limiter.in_flight("origin"), 0);
    }

    #[test]
    fn bounds_are_per_repo() {
        let limiter = RepoLeaseLimiter::new(1);
        let a = limiter.try_begin("repo-a", "exec-a").expect("a");
        let b = limiter.try_begin("repo-b", "exec-b").expect("b");
        assert!(limiter.try_begin("repo-a", "exec-c").is_none());
        assert!(limiter.try_begin("repo-b", "exec-d").is_none());
        drop(a);
        drop(b);
    }

    #[test]
    fn zero_max_is_clamped_to_one() {
        let limiter = RepoLeaseLimiter::new(0);
        assert_eq!(limiter.max(), 1);
        assert!(limiter.try_begin("origin", "exec-a").is_some());
    }

    #[test]
    fn release_reports_denied_only_when_a_row_waited() {
        let limiter = RepoLeaseLimiter::new(1);
        let a = limiter.try_begin("origin", "exec-a").expect("first");
        assert!(!limiter.denied("origin"));
        assert!(!limiter.release("exec-a"));
        drop(a);

        let b = limiter.try_begin("origin", "exec-b").expect("second");
        assert!(limiter.try_begin("origin", "exec-c").is_none());
        assert!(limiter.denied("origin"));
        assert!(limiter.release("exec-b"), "a denied row must be reported");
        assert!(!limiter.denied("origin"));
        drop(b);
    }

    #[test]
    fn release_by_execution_is_idempotent_and_frees_the_permit_early() {
        let limiter = RepoLeaseLimiter::new(1);
        let guard = limiter.try_begin("origin", "exec-a").expect("first");
        assert!(!limiter.release("exec-a"));
        assert_eq!(limiter.in_flight("origin"), 0);
        let other = limiter.try_begin("origin", "exec-b").expect("freed by early release");
        // The original guard's drop must not release a permit it no longer
        // holds (it would steal `exec-b`'s).
        drop(guard);
        assert_eq!(limiter.in_flight("origin"), 1);
        assert!(!limiter.release("exec-a"));
        drop(other);
        assert_eq!(limiter.in_flight("origin"), 0);
    }
}
