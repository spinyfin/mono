//! Transport-independent exponential-backoff and jitter primitives.
//!
//! Shared by the HTTP retry layer (`boss-http-retry`) and the engine-IPC
//! client (`boss-client`) so the capped-doubling and jitter math lives in one
//! place. This crate knows nothing about any transport, deadline, or notice
//! policy; callers layer those on top.

use std::time::Duration;

/// `initial * 2^doublings`, capped at `max`. The exponent is bounded so an
/// unreasonable attempt count cannot overflow.
pub fn exponential_delay(initial: Duration, max: Duration, doublings: u32) -> Duration {
    let factor = 1u32.checked_shl(doublings).unwrap_or(u32::MAX).max(1);
    initial.saturating_mul(factor).min(max)
}

/// Scale `delay` by a factor between `low` and `high` selected by `unit`
/// (`0.0..=1.0`, clamped). A zero delay stays zero.
pub fn jitter_at(delay: Duration, low: f64, high: f64, unit: f64) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    delay.mul_f64(low + (high - low) * unit.clamp(0.0, 1.0))
}

/// [`jitter_at`] with a random `unit`.
pub fn jitter(delay: Duration, low: f64, high: f64) -> Duration {
    jitter_at(delay, low, high, fastrand::f64())
}

/// "Equal jitter": a point in `[delay/2, delay]` chosen by `unit`.
pub fn equal_jitter_at(delay: Duration, unit: f64) -> Duration {
    jitter_at(delay, 0.5, 1.0, unit)
}

/// [`equal_jitter_at`] with a random `unit`.
pub fn equal_jitter(delay: Duration) -> Duration {
    equal_jitter_at(delay, fastrand::f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_doubles_then_caps() {
        let (initial, max) = (Duration::from_millis(100), Duration::from_millis(1000));
        assert_eq!(exponential_delay(initial, max, 0), Duration::from_millis(100));
        assert_eq!(exponential_delay(initial, max, 2), Duration::from_millis(400));
        assert_eq!(exponential_delay(initial, max, 5), max);
        assert_eq!(exponential_delay(initial, max, 500), max, "no overflow far out");
    }

    #[test]
    fn jitter_at_clamps_and_spans_the_range() {
        let d = Duration::from_millis(100);
        assert_eq!(jitter_at(d, 0.75, 1.25, 0.0), Duration::from_millis(75));
        assert_eq!(jitter_at(d, 0.75, 1.25, 1.0), Duration::from_millis(125));
        assert_eq!(jitter_at(d, 0.75, 1.25, 9.0), Duration::from_millis(125));
        assert_eq!(jitter_at(d, 0.75, 1.25, -9.0), Duration::from_millis(75));
    }

    #[test]
    fn equal_jitter_is_half_to_full() {
        let d = Duration::from_millis(200);
        assert_eq!(equal_jitter_at(d, 0.0), Duration::from_millis(100));
        assert_eq!(equal_jitter_at(d, 1.0), d);
        for _ in 0..200 {
            let j = equal_jitter(d);
            assert!(j >= Duration::from_millis(100) && j <= d, "{j:?}");
        }
    }

    #[test]
    fn zero_delay_is_never_jittered_up() {
        assert_eq!(jitter(Duration::ZERO, 0.5, 2.0), Duration::ZERO);
    }
}
