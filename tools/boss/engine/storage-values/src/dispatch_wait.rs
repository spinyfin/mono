//! Persisted dispatch hold vocabulary shared by scheduling and reporting.

/// The per-repository workspace lease limiter is at capacity.
pub const WAITING_FOR_WORKSPACE_REASON: &str = "Waiting for a workspace";

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
        WAITING_FOR_WORKSPACE_REASON | "pool_exhausted" | "dispatch_paused" | "automation_paused" | "focus_mode"
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
            "pool_exhausted".into(),
            "dispatch_paused".into(),
            "automation_paused".into(),
            "focus_mode".into(),
        ] {
            assert!(is_capacity_wait_reason(&reason), "{reason}");
        }
        for reason in ["interactive_concurrency_cap", "slot_busy", "waiting_dependency"] {
            assert!(!is_capacity_wait_reason(reason), "{reason}");
        }
    }
}
