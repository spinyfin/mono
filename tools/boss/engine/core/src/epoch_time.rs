//! Single source of truth for reading the current wall-clock time as a
//! Unix epoch offset.
//!
//! The engine reads "now as epoch seconds/millis" in dozens of places
//! (sweeps, dispatch, audit, metrics, …). Historically each site
//! reimplemented the same `SystemTime::now().duration_since(UNIX_EPOCH)`
//! incantation, which drifted in return type and error handling. These
//! helpers consolidate that computation with a consistent fallback: on a
//! pre-epoch clock (`duration_since` error) they return `0` rather than
//! panicking, matching what almost every call site already did.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as whole seconds since the Unix epoch.
///
/// Falls back to `0` if the system clock is set before the epoch.
pub fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current wall-clock time as whole milliseconds since the Unix epoch.
///
/// Falls back to `0` if the system clock is set before the epoch.
pub fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
