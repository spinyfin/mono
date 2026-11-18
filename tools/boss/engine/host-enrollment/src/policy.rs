//! The lifetimes the host credential lifecycle runs on.

use std::time::Duration;

/// Lifetimes for host certificates and enrollment tokens.
///
/// The defaults are the design's: 30-day certificates, 15-minute single-use
/// tokens, and a 24-hour renewal overlap (hub-and-spoke design, "Enrollment,
/// service user and credential lifecycle").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How long a host certificate is valid for.
    pub leaf_lifetime: Duration,
    /// How far `notBefore` is moved into the past, so a spoke whose clock runs
    /// slightly behind the hub's can still use the certificate it was just
    /// issued. A larger skew is a preflight error, not something to hide.
    pub clock_skew_allowance: Duration,
    /// How long an unused enrollment token stays valid.
    pub token_lifetime: Duration,
    /// How long after a token is consumed an identical retry still returns the
    /// stored certificate, for a spoke whose reply was lost.
    pub enroll_retry_window: Duration,
    /// How long a superseded certificate keeps authorizing after a renewal,
    /// capped by its own expiry.
    pub renewal_overlap: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        Self {
            leaf_lifetime: Duration::from_secs(30 * DAY),
            clock_skew_allowance: Duration::from_secs(5 * MINUTE),
            token_lifetime: Duration::from_secs(15 * MINUTE),
            enroll_retry_window: Duration::from_secs(HOUR),
            renewal_overlap: Duration::from_secs(24 * HOUR),
        }
    }
}
