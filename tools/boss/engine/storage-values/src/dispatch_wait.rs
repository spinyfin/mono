//! Persisted dispatch hold vocabulary shared by scheduling and reporting.

/// The per-repository workspace lease limiter is at capacity.
pub const WAITING_FOR_WORKSPACE_REASON: &str = "Waiting for a workspace";

/// Worker pool has no free slot this drain pass.
pub const DISPATCH_WAIT_REASON_POOL_EXHAUSTED: &str = "pool_exhausted";

/// Global dispatch pause is holding this ready row.
pub const DISPATCH_WAIT_REASON_DISPATCH_PAUSED: &str = "dispatch_paused";

/// Automation pause is holding this automation-pool row.
pub const DISPATCH_WAIT_REASON_AUTOMATION_PAUSED: &str = "automation_paused";

/// A `progression` row is held `ready` because the engine cannot yet spawn a
/// progression agent (no worker kind of its own). Not a capacity wait: nothing
/// is short of slots, so it is deliberately absent from
/// [`is_capacity_wait_reason`].
pub const DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE: &str = "progression_unavailable";

/// Dispatch focus is holding an out-of-scope ready row.
pub use boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE;

/// User-visible interactive-cap hold text. Lease-pending slots are
/// never called "working".
pub fn interactive_cap_wait_reason(working: usize, lease_pending: usize) -> String {
    if lease_pending == 0 {
        format!("Waiting for an available worker ({working} working)")
    } else {
        format!("Waiting for an available worker ({working} working, {lease_pending} waiting for a workspace)")
    }
}

/// Whether a persisted hold is due to capacity or an operator scheduling pause.
pub fn is_capacity_wait_reason(reason: &str) -> bool {
    matches!(
        reason,
        WAITING_FOR_WORKSPACE_REASON
            | DISPATCH_WAIT_REASON_POOL_EXHAUSTED
            | DISPATCH_WAIT_REASON_DISPATCH_PAUSED
            | DISPATCH_WAIT_REASON_AUTOMATION_PAUSED
            | DISPATCH_WAIT_REASON_FOCUS_MODE
    ) || reason.starts_with("Waiting for an available worker (")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_recognizes_persisted_text_and_not_event_codes() {
        for reason in [
            WAITING_FOR_WORKSPACE_REASON.to_owned(),
            interactive_cap_wait_reason(8, 0),
            interactive_cap_wait_reason(6, 2),
            DISPATCH_WAIT_REASON_POOL_EXHAUSTED.to_owned(),
            DISPATCH_WAIT_REASON_DISPATCH_PAUSED.to_owned(),
            DISPATCH_WAIT_REASON_AUTOMATION_PAUSED.to_owned(),
            DISPATCH_WAIT_REASON_FOCUS_MODE.to_owned(),
        ] {
            assert!(is_capacity_wait_reason(&reason), "{reason}");
        }
        for reason in [
            "interactive_concurrency_cap",
            "slot_busy",
            "waiting_dependency",
            DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE,
        ] {
            assert!(!is_capacity_wait_reason(reason), "{reason}");
        }
    }
}
