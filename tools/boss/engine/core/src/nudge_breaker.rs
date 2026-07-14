//! Circuit breaker for the auto-nudge ("produce a PR") loop.
//!
//! Background: when a worker stops without the engine being able to
//! finalize a PR, the on-Stop handler queues a probe ("produce a PR" /
//! "push to the existing PR" / etc.) and waits for the next Stop. If the
//! worker keeps stopping without changing state — e.g. a
//! `ci_remediation` worker whose chore already has a merged PR, so there
//! is genuinely nothing for it to do — the handler would re-queue the
//! same nudge forever. That is exactly the Worf incident
//! (`exec_18b3945c5b7d7e78_1b`): the worker was nudged 20 times and
//! replied with the same already-merged PR URL 19 times before going
//! idle, never accepted, never parked.
//!
//! This breaker caps *consecutive unproductive* nudges per execution.
//! "Unproductive" is defined deterministically by the caller via a
//! `fingerprint` string that encodes the work state at nudge time (the
//! bound PR's head SHA, "no PR", etc.). When two consecutive nudges
//! carry the same fingerprint the worker made no progress, so the count
//! advances; a different fingerprint means progress (new commit, PR
//! opened, transition) and resets the count. Once the cap is exhausted
//! the caller parks the execution instead of nudging again.
//!
//! It also debounces *identical* nudges in time (`MIN_RENUDGE_INTERVAL`):
//! a Stop hook fires after every worker turn, and a worker that's told
//! "say so if there's nothing left to do" and complies produces a new
//! Stop within seconds of the previous one. Re-sending the same probe
//! text on that immediate next Stop can't possibly have new information —
//! the only way the fingerprint could legitimately have changed is
//! external state (CI, a merge queue) moving, which takes longer than a
//! worker's reply turn. Without this, three unproductive-but-legitimate
//! nudges (e.g. "your PR is queued for merge, wait") burn the whole
//! circuit-breaker budget in under a minute and park/abandon an execution
//! that was actually finished (2026-07-14 incident,
//! exec_18c21b03972f3920_49: three identical "push to the existing PR"
//! probes fired at 08:59:24Z/:33Z/:41Z — 8-9s apart — against a revision
//! that had already pushed and re-enqueued its PR for auto-merge).
//!
//! State is in-memory only, mirroring [`crate::pr_url_capture`]: the
//! probe FIFO it guards also lives in memory, so an engine restart
//! resets the whole nudge loop anyway. Durability across restarts is
//! intentionally not a goal here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default cap on consecutive unproductive auto-nudges before the
/// breaker trips. After this many nudges that produce no state change,
/// the engine parks the execution rather than nudging again.
pub const DEFAULT_MAX_UNPRODUCTIVE_NUDGES: u32 = 3;

/// Absolute, cross-fingerprint cap on nudges sent for a single execution.
/// Unlike `max` in [`NudgeBreaker::record`] (which only bounds a run of
/// *identical* fingerprints), this counts every nudge ever sent for the
/// execution id and never resets on a fingerprint change. It exists as a
/// backstop of last resort: whatever produces a fingerprint that keeps
/// changing between nudges — a flapping state detector, a probe reading
/// drifting values, a caller composing the fingerprint from something that
/// varies without real progress — must still terminate. An unbounded nudge
/// loop is never correct, regardless of what the per-fingerprint state
/// detector concludes.
pub const ABSOLUTE_MAX_NUDGES: u32 = 12;

/// Minimum time that must elapse between two nudges sent at the same
/// fingerprint. A Stop that would otherwise re-send an identical probe
/// before this interval has passed is told to wait quietly instead —
/// see the module docs for why. This does not consume any part of the
/// `max`/`ABSOLUTE_MAX_NUDGES` budget: it only paces how fast that
/// budget can be spent, so a worker that is genuinely stuck is still
/// nudged (and eventually parked) at the same total count, just not
/// faster than external state could plausibly have changed.
pub const MIN_RENUDGE_INTERVAL: Duration = Duration::from_secs(60);

/// Decision returned by [`NudgeBreaker::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeDecision {
    /// The nudge is allowed to fire. `count` is the number of
    /// consecutive unproductive nudges including this one (1-based).
    Proceed { count: u32 },
    /// The breaker has tripped: the cap of consecutive unproductive
    /// nudges has already been sent. The caller must park the execution
    /// and must NOT nudge again. `count` is the number of nudges
    /// already sent at this fingerprint (== the configured cap).
    Trip { count: u32 },
    /// The same fingerprint was nudged less than `MIN_RENUDGE_INTERVAL`
    /// ago. The caller must NOT send a probe and must NOT count this as
    /// a nudge (neither `count` nor `total_count` advance) — it should
    /// simply let the execution sit quietly until either state changes
    /// (a fresh fingerprint) or the interval elapses and the same
    /// fingerprint is recorded again.
    TooSoon { since_last: Duration },
}

#[derive(Debug, Clone)]
struct NudgeRecord {
    /// Fingerprint of the work state at the most recent nudge. A nudge
    /// whose fingerprint differs from this means the worker made
    /// progress, so the counter resets.
    fingerprint: String,
    /// Number of consecutive nudges already sent at `fingerprint`.
    count: u32,
    /// Total nudges ever sent for this execution id, across every
    /// fingerprint seen. Never reset by a fingerprint change — only
    /// `forget` clears it. Compared against [`ABSOLUTE_MAX_NUDGES`].
    total_count: u32,
    /// When the most recent nudge at `fingerprint` was actually sent
    /// (i.e. the last time `record` returned `Proceed`). Compared
    /// against [`MIN_RENUDGE_INTERVAL`] to debounce rapid identical
    /// Stop→probe→reply→Stop cycles.
    last_nudge_at: Instant,
}

/// In-memory `execution_id -> (fingerprint, count)` tracker for the
/// auto-nudge circuit breaker. Thread-safe; cheap to clone-share behind
/// an `Arc`.
#[derive(Debug, Default)]
pub struct NudgeBreaker {
    inner: Mutex<HashMap<String, NudgeRecord>>,
}

impl NudgeBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an intent to nudge `execution_id` whose current work state
    /// is captured by `fingerprint`, capping at `max` consecutive
    /// unproductive nudges. `now` is the caller's current time — callers
    /// pass `Instant::now()` in production; tests pass deterministic,
    /// pre-computed `Instant` values so debounce behaviour is testable
    /// without real sleeps.
    ///
    /// - If the fingerprint differs from the last recorded nudge (the
    ///   worker made progress, or this is the first nudge), reset to a
    ///   single fresh nudge and return `Proceed { count: 1 }` immediately
    ///   — a fingerprint change is real news and is never debounced.
    /// - If the fingerprint matches, fewer than `max` nudges have been
    ///   sent at it, and at least [`MIN_RENUDGE_INTERVAL`] has elapsed
    ///   since the last nudge at this fingerprint, increment and return
    ///   `Proceed { count }`.
    /// - If the fingerprint matches but less than `MIN_RENUDGE_INTERVAL`
    ///   has elapsed since the last nudge at it, return `TooSoon` without
    ///   advancing `count` or `total_count` — this nudge doesn't count
    ///   against either budget, it's simply not sent yet.
    /// - If `max` unproductive nudges have already been sent at this
    ///   fingerprint, return `Trip { count: max }` and do not advance
    ///   the count further.
    ///
    /// With `max = 3` and calls spaced past `MIN_RENUDGE_INTERVAL`, the
    /// sequence of identical-fingerprint calls is `Proceed{1}`,
    /// `Proceed{2}`, `Proceed{3}`, `Trip{3}`, `Trip{3}`, … — three nudges
    /// fire, then the breaker trips and stays tripped. Calls spaced
    /// *closer* than `MIN_RENUDGE_INTERVAL` return `TooSoon` instead of
    /// advancing the sequence at all.
    pub fn record(&self, execution_id: &str, fingerprint: &str, max: u32, now: Instant) -> NudgeDecision {
        let mut guard = self.inner.lock().expect("NudgeBreaker mutex poisoned");
        let entry = guard.entry(execution_id.to_owned()).or_insert_with(|| NudgeRecord {
            fingerprint: fingerprint.to_owned(),
            count: 0,
            total_count: 0,
            last_nudge_at: now,
        });
        if entry.fingerprint != fingerprint {
            // Progress since the last nudge — reset the consecutive-run
            // counter to a fresh cycle. `total_count` is deliberately left
            // untouched: it is the absolute, cross-fingerprint backstop
            // below and must not be resettable by a changing fingerprint.
            // The debounce clock also resets: a genuinely new fingerprint
            // is real news and must never wait out the old fingerprint's
            // interval.
            entry.fingerprint = fingerprint.to_owned();
            entry.count = 0;
        } else if entry.count > 0 {
            let since_last = now.saturating_duration_since(entry.last_nudge_at);
            if since_last < MIN_RENUDGE_INTERVAL {
                return NudgeDecision::TooSoon { since_last };
            }
        }
        if entry.total_count >= ABSOLUTE_MAX_NUDGES || entry.count >= max {
            NudgeDecision::Trip { count: entry.count }
        } else {
            entry.count += 1;
            entry.total_count += 1;
            entry.last_nudge_at = now;
            NudgeDecision::Proceed { count: entry.count }
        }
    }

    /// Drop any tracked state for `execution_id`. Called when the worker
    /// makes real progress (a PR is finalized) so a later, unrelated
    /// nudge cycle starts clean. Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("NudgeBreaker mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n`-th tick of a synthetic clock, spaced comfortably past
    /// `MIN_RENUDGE_INTERVAL` apart. `Instant` arithmetic doesn't require
    /// real time to pass, so tests that aren't specifically about the
    /// debounce window use this to get deterministic, always-safely-spaced
    /// timestamps without sleeping.
    fn tick(n: u32) -> Instant {
        Instant::now() + MIN_RENUDGE_INTERVAL * (n + 1)
    }

    #[test]
    fn first_nudge_proceeds_with_count_one() {
        let breaker = NudgeBreaker::new();
        assert_eq!(
            breaker.record("exec_a", "no_pr", 3, tick(0)),
            NudgeDecision::Proceed { count: 1 },
        );
    }

    #[test]
    fn identical_fingerprint_advances_then_trips_at_cap() {
        // The Worf loop: the same unproductive nudge repeats. Three fire,
        // the fourth and all later attempts trip. Calls are spaced past
        // MIN_RENUDGE_INTERVAL so the debounce guard doesn't mask the
        // count/trip behaviour under test — see `too_soon_*` tests below
        // for the debounce itself.
        let breaker = NudgeBreaker::new();
        let fp = "push_existing:https://github.com/spinyfin/mono/pull/869";
        assert_eq!(
            breaker.record("exec_w", fp, 3, tick(0)),
            NudgeDecision::Proceed { count: 1 }
        );
        assert_eq!(
            breaker.record("exec_w", fp, 3, tick(1)),
            NudgeDecision::Proceed { count: 2 }
        );
        assert_eq!(
            breaker.record("exec_w", fp, 3, tick(2)),
            NudgeDecision::Proceed { count: 3 }
        );
        assert_eq!(
            breaker.record("exec_w", fp, 3, tick(3)),
            NudgeDecision::Trip { count: 3 }
        );
        assert_eq!(
            breaker.record("exec_w", fp, 3, tick(4)),
            NudgeDecision::Trip { count: 3 }
        );
    }

    #[test]
    fn changed_fingerprint_resets_the_count() {
        // Progress (a new commit moves the bound PR head, the PR opens,
        // etc.) changes the fingerprint and gives the worker a fresh
        // budget instead of tripping on stale history.
        let breaker = NudgeBreaker::new();
        breaker.record("exec_b", "no_pr", 3, tick(0));
        breaker.record("exec_b", "no_pr", 3, tick(1));
        assert_eq!(
            breaker.record("exec_b", "stale:https://github.com/x/y/pull/1", 3, tick(2)),
            NudgeDecision::Proceed { count: 1 },
            "a different fingerprint must reset the counter",
        );
    }

    #[test]
    fn absolute_cap_trips_even_when_fingerprint_keeps_changing() {
        // A per-fingerprint cap alone is not a real bound if whatever
        // composes the fingerprint keeps drifting (flapping CI status, a
        // probe that reads a slightly different value each time, etc.) —
        // every call would look like "progress" and the consecutive-run
        // counter would never reach `max`. The absolute, cross-fingerprint
        // ceiling must still trip the breaker after ABSOLUTE_MAX_NUDGES
        // nudges, whatever the fingerprints were.
        let breaker = NudgeBreaker::new();
        // A generous per-fingerprint cap that would never trip on its own
        // for a sequence of all-distinct fingerprints.
        let max = 1000;
        for i in 0..ABSOLUTE_MAX_NUDGES {
            let fp = format!("state:{i}");
            assert_eq!(
                breaker.record("exec_flapping", &fp, max, tick(i)),
                NudgeDecision::Proceed { count: 1 },
                "each distinct fingerprint proceeds as a fresh per-fingerprint cycle",
            );
        }
        let fp = format!("state:{ABSOLUTE_MAX_NUDGES}");
        assert_eq!(
            breaker.record("exec_flapping", &fp, max, tick(ABSOLUTE_MAX_NUDGES)),
            // The fresh fingerprint resets the per-fingerprint `count` to 0
            // before the absolute check runs, so the trip reports count: 0
            // — the absolute ceiling fired independently of it.
            NudgeDecision::Trip { count: 0 },
            "absolute cap must trip the breaker even though the fingerprint never repeated",
        );
    }

    #[test]
    fn cap_of_one_trips_after_a_single_nudge() {
        let breaker = NudgeBreaker::new();
        assert_eq!(
            breaker.record("exec_c", "no_pr", 1, tick(0)),
            NudgeDecision::Proceed { count: 1 }
        );
        assert_eq!(
            breaker.record("exec_c", "no_pr", 1, tick(1)),
            NudgeDecision::Trip { count: 1 }
        );
    }

    #[test]
    fn executions_are_tracked_independently() {
        let breaker = NudgeBreaker::new();
        breaker.record("exec_a", "no_pr", 3, tick(0));
        breaker.record("exec_a", "no_pr", 3, tick(1));
        // A different execution starts its own cycle.
        assert_eq!(
            breaker.record("exec_b", "no_pr", 3, tick(0)),
            NudgeDecision::Proceed { count: 1 },
        );
    }

    #[test]
    fn forget_clears_state_and_allows_a_fresh_cycle() {
        let breaker = NudgeBreaker::new();
        breaker.record("exec_a", "no_pr", 3, tick(0));
        breaker.record("exec_a", "no_pr", 3, tick(1));
        breaker.record("exec_a", "no_pr", 3, tick(2));
        assert_eq!(
            breaker.record("exec_a", "no_pr", 3, tick(3)),
            NudgeDecision::Trip { count: 3 }
        );
        breaker.forget("exec_a");
        assert_eq!(
            breaker.record("exec_a", "no_pr", 3, tick(4)),
            NudgeDecision::Proceed { count: 1 },
            "forget must reset the cycle",
        );
    }

    #[test]
    fn forget_is_idempotent() {
        let breaker = NudgeBreaker::new();
        breaker.forget("never-tracked");
        breaker.forget("never-tracked");
        assert_eq!(
            breaker.record("never-tracked", "no_pr", 3, tick(0)),
            NudgeDecision::Proceed { count: 1 },
        );
    }

    // ── Debounce (MIN_RENUDGE_INTERVAL) ──────────────────────────────

    #[test]
    fn identical_fingerprint_within_interval_returns_too_soon() {
        // The 2026-07-14 incident this closes: a worker complies with a
        // probe and stops again within seconds, well inside a single CI /
        // merge-queue polling interval. The breaker must not treat that as
        // a fresh unproductive nudge — there's been no time for any
        // external state to actually change.
        let breaker = NudgeBreaker::new();
        let fp = "nocontribution:https://github.com/spinyfin/mono/pull/1980";
        let t0 = Instant::now();
        assert_eq!(breaker.record("exec_r", fp, 3, t0), NudgeDecision::Proceed { count: 1 },);
        let t1 = t0 + Duration::from_secs(8);
        assert_eq!(
            breaker.record("exec_r", fp, 3, t1),
            NudgeDecision::TooSoon {
                since_last: Duration::from_secs(8)
            },
        );
        let t2 = t1 + Duration::from_secs(9);
        assert_eq!(
            breaker.record("exec_r", fp, 3, t2),
            NudgeDecision::TooSoon {
                since_last: Duration::from_secs(17)
            },
            "still inside the interval measured from the last actual nudge, not the last call",
        );
    }

    #[test]
    fn too_soon_does_not_advance_count_or_total_count() {
        // A rejected (too-soon) call must be free: it cannot consume any
        // of the per-fingerprint or absolute nudge budget, or a burst of
        // rapid Stops would silently exhaust the breaker without a single
        // probe having actually been sent.
        let breaker = NudgeBreaker::new();
        let fp = "no_pr";
        let t0 = Instant::now();
        assert_eq!(breaker.record("exec_r", fp, 3, t0), NudgeDecision::Proceed { count: 1 });
        for i in 1..20 {
            let t = t0 + Duration::from_millis(i);
            assert!(
                matches!(breaker.record("exec_r", fp, 3, t), NudgeDecision::TooSoon { .. }),
                "call {i} inside the debounce window must be suppressed",
            );
        }
        // The budget must be exactly as if only the first call happened.
        let t1 = t0 + MIN_RENUDGE_INTERVAL;
        assert_eq!(
            breaker.record("exec_r", fp, 3, t1),
            NudgeDecision::Proceed { count: 2 },
            "once past the interval, the second real nudge proceeds — the flood above must not \
             have been counted",
        );
    }

    #[test]
    fn nudge_past_the_interval_proceeds_normally() {
        let breaker = NudgeBreaker::new();
        let fp = "no_pr";
        let t0 = Instant::now();
        assert_eq!(breaker.record("exec_r", fp, 3, t0), NudgeDecision::Proceed { count: 1 });
        let t1 = t0 + MIN_RENUDGE_INTERVAL + Duration::from_secs(1);
        assert_eq!(
            breaker.record("exec_r", fp, 3, t1),
            NudgeDecision::Proceed { count: 2 },
            "a call at/after exactly MIN_RENUDGE_INTERVAL must proceed, not debounce",
        );
    }

    #[test]
    fn changed_fingerprint_bypasses_debounce_even_immediately_after() {
        // Real progress must never wait out the old fingerprint's debounce
        // clock — a worker that pushes a new commit a second after being
        // nudged must be recognised as making progress right away.
        let breaker = NudgeBreaker::new();
        let t0 = Instant::now();
        breaker.record("exec_r", "no_pr", 3, t0);
        let t1 = t0 + Duration::from_millis(1);
        assert_eq!(
            breaker.record("exec_r", "stale:https://github.com/x/y/pull/1", 3, t1),
            NudgeDecision::Proceed { count: 1 },
            "a fingerprint change must proceed immediately, not debounce",
        );
    }

    #[test]
    fn too_soon_does_not_reset_or_extend_the_debounce_clock() {
        // A TooSoon call must be a pure no-op on state: it must not shift
        // `last_nudge_at` forward, or a steady drizzle of Stops closer
        // together than the interval could push the next legitimate nudge
        // out indefinitely.
        let breaker = NudgeBreaker::new();
        let fp = "no_pr";
        let t0 = Instant::now();
        breaker.record("exec_r", fp, 3, t0);
        breaker.record("exec_r", fp, 3, t0 + Duration::from_secs(1)); // TooSoon, ignored
        breaker.record("exec_r", fp, 3, t0 + Duration::from_secs(2)); // TooSoon, ignored
        let t_ready = t0 + MIN_RENUDGE_INTERVAL;
        assert_eq!(
            breaker.record("exec_r", fp, 3, t_ready),
            NudgeDecision::Proceed { count: 2 },
            "the debounce clock must still be measured from the original nudge at t0, not from \
             the intervening TooSoon calls",
        );
    }
}
