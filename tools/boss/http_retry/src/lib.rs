//! Shared HTTP transport scaffolding for the engine's outbound REST clients.
//!
//! # Crate boundary
//!
//! This crate owns transport-only concerns shared across every outbound API
//! client the engine builds ([`claude_client`](../boss_claude_client),
//! [`trunk_client`](../boss_trunk_client), and anything added later): the
//! single process-wide `reqwest::Client` (with the `rustls` crypto provider
//! installed in exactly one place — [`http_client`]), the [`RetryPolicy`]
//! shape, and the exponential-backoff / jitter math. It knows nothing about
//! any particular API's endpoints, auth headers, request/response shapes, or
//! error taxonomy — those stay in the caller crate, which is expected to
//! layer its own `CallConfig` (endpoint + this crate's [`RetryPolicy`]) and
//! error classification on top. This crate must never import from the
//! engine — that edge is one-way, engine -> callers -> `boss-http-retry`.

use std::sync::OnceLock;
use std::time::Duration;

/// How many attempts to make and how long to back off between them. Backoff
/// is exponential (`base_backoff * 2^(attempt-1)`), capped at `max_backoff`
/// so a long attempt count can't balloon into an unbounded wait.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, `>= 1`. `1` means no retry.
    pub max_attempts: u32,
    /// Backoff before the first retry; doubles for each subsequent retry.
    pub base_backoff: Duration,
    /// Upper bound on any single backoff.
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// One attempt, no retry.
    pub const NONE: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    };

    pub const fn new(max_attempts: u32, base_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_attempts,
            base_backoff,
            max_backoff,
        }
    }
}

/// Backoff before the `attempt + 1`th try (1-based `attempt`), exponential
/// and capped at `policy.max_backoff`. Not jittered — callers that want
/// jitter (to keep concurrent retriers from lockstepping) apply [`jitter`]
/// on top; callers that don't (a single best-effort retry) use this as-is.
pub fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    // attempt is always >= 1 here; cap the exponent so the shift can't
    // overflow even if a caller sets an unreasonable attempt count.
    let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX).max(1);
    policy.base_backoff.saturating_mul(factor).min(policy.max_backoff)
}

/// Jitter `delay` by +/-25%. A zero delay is never jittered into a nonzero
/// wait, so [`RetryPolicy::NONE`] stays instant.
pub fn jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let factor = 0.75 + fastrand::f64() * 0.5;
    Duration::from_millis((delay.as_millis() as f64 * factor).round() as u64)
}

/// The process-wide reqwest client shared by every outbound HTTP call in the
/// engine. This is the ONLY place in the engine that builds one, and the
/// ONLY place that installs the `rustls` crypto provider.
///
/// The client carries no default timeout — each request applies its own via
/// its caller's `CallConfig::timeout`, so one client serves callers with
/// wildly different budgets (a 5 s live-status one-liner and a 180 s
/// planning call).
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`, so a default
        // crypto provider must be installed before the first TLS handshake or
        // `Client::build` panics. `install_default` errors if one is already
        // set — that's fine, we ignore it.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RetryPolicy::new(6, Duration::from_millis(100), Duration::from_millis(1000));
        assert_eq!(backoff_delay(&policy, 1), Duration::from_millis(100));
        assert_eq!(backoff_delay(&policy, 2), Duration::from_millis(200));
        assert_eq!(backoff_delay(&policy, 3), Duration::from_millis(400));
        // attempt 5 would be 1600ms uncapped; max_backoff caps it at 1000ms.
        assert_eq!(backoff_delay(&policy, 5), Duration::from_millis(1000));
    }

    #[test]
    fn zero_delay_is_never_jittered_into_a_nonzero_wait() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn jitter_stays_within_plus_minus_25_percent() {
        let base = Duration::from_millis(100);
        for _ in 0..100 {
            let jittered = jitter(base).as_millis();
            assert!((75..=125).contains(&jittered), "jittered {jittered}ms out of range");
        }
    }

    #[test]
    fn http_client_returns_the_same_singleton() {
        let a = http_client() as *const reqwest::Client;
        let b = http_client() as *const reqwest::Client;
        assert_eq!(a, b);
    }
}
