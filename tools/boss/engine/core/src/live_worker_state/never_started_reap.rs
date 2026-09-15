//! Atomic eligibility checks and reversible fences for never-started reaps.

use super::{DriverStartExpectation, LiveWorkerStateRegistry, WorkerActivity};

/// Which never-started reap is asking the registry to re-assert eligibility
/// after the liveness probe. Each variant restates the cause-specific
/// predicate [`crate::spawn_ack_sweep`] evaluated before the probe await.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeverStartedReapKind {
    /// Pass 1: still this execution, no driver signal, `shell_pid <= 0`.
    SpawnAckTimeout,
    /// Pass 2: still this execution, no driver signal (a pid may exist).
    DriverStartTimeout,
    /// App NACK / pane-death-before-start: the stale-report guard
    /// ([`crate::spawn_ack_sweep::slot_never_started`]) still holds.
    AppReportedNeverStarted,
}

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
    /// Pass 1: a shell pid was reported while the probe was in flight.
    SpawnAckNowHasPid { shell_pid: i32 },
    /// An app-reported cause is now stale: the slot has shown proof of
    /// life or left `Spawning`.
    NoLongerNeverStarted,
}

impl LiveWorkerStateRegistry {
    /// Re-read `slot_id` under the registry mutex and, if it is still
    /// eligible for a never-started reap of `kind` for `execution_id`,
    /// commit the reap so a concurrent [`Self::record_driver_signal`]
    /// cannot accept evidence the orphan would then destroy.
    ///
    /// Called after the liveness probe returns and immediately before
    /// `mark_execution_orphaned`. The probe is a bounded directory walk on
    /// the blocking pool; a hook or pid report that lands while it is
    /// queued must win.
    pub fn confirm_never_started_reap(
        &self,
        slot_id: u8,
        execution_id: &str,
        kind: NeverStartedReapKind,
    ) -> NeverStartedReapCommit {
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
        match kind {
            NeverStartedReapKind::SpawnAckTimeout => {
                if entry.state.shell_pid > 0 {
                    return NeverStartedReapCommit::SpawnAckNowHasPid {
                        shell_pid: entry.state.shell_pid,
                    };
                }
            }
            NeverStartedReapKind::DriverStartTimeout => {}
            NeverStartedReapKind::AppReportedNeverStarted => {
                let still_never_started = entry.meta.driver_start_expectation != DriverStartExpectation::Readopted
                    && entry.state.shell_pid <= 0
                    && entry.state.last_event_at.is_none()
                    && entry.state.activity == WorkerActivity::Spawning;
                if !still_never_started {
                    return NeverStartedReapCommit::NoLongerNeverStarted;
                }
            }
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
