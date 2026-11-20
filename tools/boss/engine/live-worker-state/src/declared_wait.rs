//! Folding a worker's own declared wait into its parked activity.
//!
//! Claude Code emits an idle-prompt Notification after a Stop, including
//! when the worker is parked on work it declared. This module consults that
//! declaration so the idle notification does not imply a human must answer.
//!
//! The authority for "this worker is waiting on its own work" is the wait it
//! declared through `boss propose wait` (the completion wait registry) — a typed
//! signal, never pane text. This module consults it at the one seam that
//! decides a parked worker's activity, so the pill, the header subtitle, the
//! top-bar count, the durable `waiting_human` mirror and `bossctl` all read one
//! value and cannot disagree:
//!
//! | parked worker                                             | activity         |
//! | --------------------------------------------------------- | ---------------- |
//! | blocking prompt (permission, elicitation, …)              | `WaitingForInput` |
//! | idle prompt / `Stop`, declared wait still live            | `WaitingOnWork`  |
//! | idle prompt / `Stop`, wait lapsed with no re-wake         | `WaitOverdue`    |
//! | idle prompt / `Stop` after a re-wake; earlier wait lapsed | `WaitingForInput` |
//! | idle prompt, no wait                                      | `WaitingForInput` |
//! | `Stop`, no wait                                           | `WaitingForInput`           |
//!
//! A blocking prompt always wins: a worker with a live wait that hits a
//! permission prompt really does need a person.

use std::sync::Arc;

use boss_protocol::WorkerActivity;

/// What the wait registry says about one run at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeclaredWait {
    /// No wait was ever declared, or it was cleared.
    #[default]
    None,
    /// A declared wait whose `wait_expires_at` is still in the future.
    Live,
    /// A declared wait whose deadline has passed.
    Lapsed,
}

/// Identity of one wait declaration, so park/resume bookkeeping can follow
/// that record rather than the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredWaitIdentity {
    /// Monotonic per-run generation; a later `declare` is a different wait.
    pub generation: u64,
    pub declared_at_epoch: i64,
    pub expires_at_epoch: i64,
    /// Newest accepted hook sequence when this wait was declared.
    pub declared_after_seq: Option<u64>,
}

/// Declared wait lifecycle, keyed by run id (the execution id).
///
/// A trait rather than a direct wait-registry dependency so this crate stays
/// below completion: the wait registry implements the trait there.
pub trait DeclaredWaitSource: Send + Sync {
    fn declared_wait(&self, run_id: &str, now_epoch_secs: i64) -> DeclaredWait;

    /// The current record's identity, including a lapsed one still held so
    /// overdue UI can name it. `None` when this run has no record.
    fn wait_identity(&self, _run_id: &str) -> Option<DeclaredWaitIdentity> {
        None
    }
}

pub(super) type SharedWaitSource = Arc<dyn DeclaredWaitSource>;

/// Whether a `Notification` message is the driver's *idle* prompt ("Claude is
/// waiting for your input") rather than something that blocks on a person
/// (a permission request, an elicitation). An empty or unrecognised message is
/// treated as blocking: the honest default is to ask a human.
pub(super) fn is_idle_prompt_message(message: &str) -> bool {
    message.to_ascii_lowercase().contains("waiting for your input")
}

/// A lapsed wait is overdue only for the park it applied to. Once the
/// main session has resumed after that park, the expired record classifies
/// as [`DeclaredWait::None`]. Tool hooks from the declaring turn do not
/// count as that resume. A declaration that never parked still classifies
/// as none when the main session kept working past it (mid-turn expiry).
pub(super) fn effective_wait(wait: DeclaredWait, lapsed_consumed: bool) -> DeclaredWait {
    if wait == DeclaredWait::Lapsed && lapsed_consumed {
        DeclaredWait::None
    } else {
        wait
    }
}

/// Settle the activity of a worker that the raw reducer parked as `raw`
/// (`Idle` or `WaitingForInput`).
///
/// A pending `blocking_prompt` needs a person, whatever the wait says.
///
/// Live declarations survive resumed turns until their bounded deadline.
/// Callers fold [`effective_wait`] in first so a lapsed wait from an earlier
/// park cannot poison a later Stop.
pub(super) fn settle_parked(raw: WorkerActivity, blocking_prompt: bool, wait: DeclaredWait) -> WorkerActivity {
    if raw == WorkerActivity::WaitingForInput && blocking_prompt {
        return raw;
    }
    match wait {
        DeclaredWait::Live => WorkerActivity::WaitingOnWork,
        DeclaredWait::Lapsed => WorkerActivity::WaitOverdue,
        DeclaredWait::None => WorkerActivity::WaitingForInput,
    }
}

/// The periodic reconcile for one already-parked slot: a declared wait that
/// was declared (or lapsed) after the worker parked still moves it.
///
/// * `Idle` / non-blocking `WaitingForInput` + live wait → `WaitingOnWork`.
/// * `WaitingOnWork` + wait no longer live → `WaitOverdue` (expired without a
///   re-wake). If the wait was cleared (`None`), the worker is waiting on
///   nothing, so it falls back to `Idle` rather than being flagged overdue.
/// * `WaitOverdue` + a fresh live wait (renewal) → `WaitingOnWork`.
///
/// `None` means "leave the activity alone".
pub(super) fn reconcile_parked(
    current: WorkerActivity,
    blocking_prompt: bool,
    wait: DeclaredWait,
) -> Option<WorkerActivity> {
    let next = match (current, wait) {
        (WorkerActivity::WaitingForInput, _) if blocking_prompt => return None,
        (WorkerActivity::Idle | WorkerActivity::WaitingForInput | WorkerActivity::WaitOverdue, DeclaredWait::Live) => {
            WorkerActivity::WaitingOnWork
        }
        (
            WorkerActivity::Idle | WorkerActivity::WaitingForInput | WorkerActivity::WaitingOnWork,
            DeclaredWait::Lapsed,
        ) => WorkerActivity::WaitOverdue,
        (WorkerActivity::WaitingOnWork, DeclaredWait::None) => WorkerActivity::Idle,
        _ => return None,
    };
    (next != current).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorkerActivity::*;

    #[test]
    fn stop_with_live_wait_is_waiting_on_work() {
        assert_eq!(settle_parked(Idle, false, DeclaredWait::Live), WaitingOnWork);
    }

    #[test]
    fn stop_without_wait_needs_input() {
        assert_eq!(settle_parked(Idle, false, DeclaredWait::None), WaitingForInput);
    }

    #[test]
    fn idle_prompt_without_wait_still_needs_input() {
        assert_eq!(
            settle_parked(WaitingForInput, false, DeclaredWait::None),
            WaitingForInput
        );
    }

    #[test]
    fn idle_prompt_with_live_wait_is_waiting_on_work() {
        assert_eq!(settle_parked(WaitingForInput, false, DeclaredWait::Live), WaitingOnWork);
    }

    #[test]
    fn blocking_prompt_beats_a_live_wait() {
        assert_eq!(
            settle_parked(WaitingForInput, true, DeclaredWait::Live),
            WaitingForInput
        );
    }

    #[test]
    fn idle_prompt_after_lapse_stays_overdue_not_needs_input() {
        assert_eq!(settle_parked(WaitingForInput, false, DeclaredWait::Lapsed), WaitOverdue);
    }

    #[test]
    fn a_stop_after_an_unconsumed_lapsed_wait_is_overdue() {
        assert_eq!(settle_parked(Idle, false, DeclaredWait::Lapsed), WaitOverdue);
    }

    #[test]
    fn expiry_moves_waiting_on_work_to_overdue() {
        assert_eq!(
            reconcile_parked(WaitingOnWork, false, DeclaredWait::Lapsed),
            Some(WaitOverdue)
        );
        assert_eq!(reconcile_parked(WaitOverdue, false, DeclaredWait::Lapsed), None);
    }

    #[test]
    fn a_cleared_wait_falls_back_to_idle_not_overdue() {
        assert_eq!(reconcile_parked(WaitingOnWork, false, DeclaredWait::None), Some(Idle));
    }

    #[test]
    fn a_late_declaration_or_renewal_reclassifies_a_parked_worker() {
        assert_eq!(reconcile_parked(Idle, false, DeclaredWait::Live), Some(WaitingOnWork));
        assert_eq!(
            reconcile_parked(WaitingForInput, false, DeclaredWait::Live),
            Some(WaitingOnWork)
        );
        assert_eq!(
            reconcile_parked(WaitOverdue, false, DeclaredWait::Live),
            Some(WaitingOnWork)
        );
        assert_eq!(reconcile_parked(WaitingForInput, true, DeclaredWait::Live), None);
        assert_eq!(reconcile_parked(Working, false, DeclaredWait::Live), None);
    }

    #[test]
    fn idle_prompt_message_is_recognised() {
        assert!(is_idle_prompt_message("Claude is waiting for your input"));
        assert!(!is_idle_prompt_message("Claude needs your permission to use Bash"));
        assert!(!is_idle_prompt_message(""));
    }

    #[test]
    fn a_lapsed_wait_after_resume_reads_as_none() {
        assert_eq!(effective_wait(DeclaredWait::Lapsed, true), DeclaredWait::None);
        assert_eq!(effective_wait(DeclaredWait::Lapsed, false), DeclaredWait::Lapsed);
        assert_eq!(effective_wait(DeclaredWait::Live, true), DeclaredWait::Live);
        assert_eq!(effective_wait(DeclaredWait::None, true), DeclaredWait::None);
    }
}
