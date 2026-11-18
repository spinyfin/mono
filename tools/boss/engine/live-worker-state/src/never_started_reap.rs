//! Atomic eligibility checks and reversible fences for never-started reaps.

use super::LiveWorkerStateRegistry;

/// Outcome of [`LiveWorkerStateRegistry::confirm_never_started_reap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeverStartedReapCommit {
    /// Eligibility still holds; the slot is now fenced against accepting
    /// a later driver signal for this registration.
    Committed { shell_pid: i32 },
    /// A driver-originated signal landed while the probe was in flight.
    DriverSignalled,
    /// No live entry, or the slot belongs to a different execution.
    SlotGone,
    /// A never-started reap already committed for this registration.
    /// Distinct from [`Self::SlotGone`]: the slot still belongs to this
    /// execution, but a later driver signal must not be accepted until
    /// the fence is released or the slot is re-registered.
    AlreadyCommitted,
}

impl LiveWorkerStateRegistry {
    /// Re-read `slot_id` under the registry mutex and, if it is still
    /// eligible for a never-started reap for `execution_id` (same run,
    /// no driver signal, not already committed), commit the reap so a
    /// concurrent [`Self::record_driver_signal`] cannot accept evidence
    /// the orphan would then destroy.
    ///
    /// Called after the liveness probe returns and immediately before
    /// `mark_execution_orphaned`. The probe is a bounded directory walk on
    /// the blocking pool; a hook that lands while it is queued must win.
    pub fn confirm_never_started_reap(&self, slot_id: u8, execution_id: &str) -> NeverStartedReapCommit {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(entry) = guard.get_mut(&slot_id) else {
            return NeverStartedReapCommit::SlotGone;
        };
        if entry.state.run_id != execution_id {
            return NeverStartedReapCommit::SlotGone;
        }
        if entry.meta.reap_committed {
            return NeverStartedReapCommit::AlreadyCommitted;
        }
        if entry.meta.driver_signal_at.is_some() {
            return NeverStartedReapCommit::DriverSignalled;
        }
        entry.meta.reap_committed = true;
        NeverStartedReapCommit::Committed {
            shell_pid: entry.state.shell_pid,
        }
    }

    /// Clear [`super::SlotMeta::reap_committed`] for `slot_id` when it still
    /// belongs to `execution_id`. Used when a never-started reap committed
    /// the fence and then a later fallible step (the orphan write) failed,
    /// so a subsequent pass can still reap and a recovering hook can still
    /// prove the driver alive.
    ///
    /// No-op when the slot is gone or now belongs to a different run —
    /// that registration's fence is not this execution's to release.
    pub fn release_never_started_reap(&self, slot_id: u8, execution_id: &str) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(entry) = guard.get_mut(&slot_id) else {
            return;
        };
        if entry.state.run_id != execution_id {
            return;
        }
        entry.meta.reap_committed = false;
    }
}
