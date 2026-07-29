//! Restart-robust "is this worker's process still running?" probe, keyed by
//! execution id and sourced entirely from durable state.
//!
//! ## Why this exists
//!
//! Every liveness decision in this crate used to read one of two in-memory
//! structures: [`crate::live_worker_state::LiveWorkerStateRegistry`] (the
//! per-slot live state) or the [`crate::coordinator::WorkerPool`]'s claim
//! table. Both are *derived* bookkeeping — the engine's belief about what is
//! running — and both are cleared unconditionally at the end of
//! [`crate::app::ServerState::release_worker_pane`], which runs on every
//! terminal path "successfully or not".
//!
//! That is fine while the belief is right. It is catastrophic when the belief
//! is wrong, because every consumer of the belief then agrees with it. The
//! 2026-07-28 incident is the shape: eight `claude` workers alive, two visible
//! to the engine, six executions terminalized seconds after start while their
//! processes kept working for another nine minutes. The re-dispatchers asked
//! the pool "is this execution claimed?", got `false` (correct — the claim had
//! been released), and dispatched a second worker onto a row whose first worker
//! was still running. Three workers ended up on one chore, two on another.
//!
//! The engine already persists the one fact that would have settled it:
//! `work_runs.shell_pid`, written by
//! [`crate::work::WorkDb::set_run_shell_pid_for_execution`] the moment the
//! app reports the pane's shell pid. It survives an engine restart, it
//! survives `release_worker_pane` clearing the registry, and it survives the
//! execution row going terminal. [`crate::dead_pane_sweep`] probes it; nothing
//! on the re-dispatch or re-adoption paths did.
//!
//! This module makes that probe a first-class primitive so those paths can
//! consult reality instead of the engine's own opinion.
//!
//! ## What it is not
//!
//! This answers "does a process with the recorded pid exist", NOT "is the
//! worker healthy" or "is the worker still doing useful work". A pane whose
//! `claude` exited but whose host shell lingers reports [`WorkerProcess::Alive`]
//! here, exactly as it does for [`crate::husk_pane_sweep::live_process_evidence`].
//! That is the correct bias for *this* module's callers, which are all deciding
//! whether it is safe to start a SECOND worker or to tear a pane down — both
//! irreversible in the direction that matters. A caller that needs "alive AND
//! working" must corroborate with hook recency on top, the way
//! `live_process_evidence` does.

use crate::dead_pid_sweep::PidStatus;
use crate::work::WorkDb;

/// How far back a recorded `work_runs.shell_pid` is trusted by the
/// re-dispatch guard ([`crate::orphan_sweep`]).
///
/// A pid is only meaningful until the OS recycles it, so a probe against an
/// arbitrarily old row is not evidence — it is a coin flip that gets more
/// biased the longer you wait. One hour is chosen against the failure it
/// guards: the re-dispatch storm fires ~60–90 s after a run is terminalized
/// (`ORPHAN_MIN_AGE_SECS` plus one sweep interval), so an hour is two orders
/// of magnitude of headroom for the case that matters while keeping the
/// recycling window small.
///
/// A worker still running an hour after its execution went terminal is a
/// different (and much louder) problem that belongs to
/// [`crate::stale_worker_sweep`] and [`crate::husk_pane_sweep`], not to a
/// guard whose only job is "don't double-dispatch onto a live process".
pub const REDISPATCH_PID_TRUST_SECS: i64 = 3600;

/// Verdict on the worker process behind an execution, derived from durable
/// state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerProcess {
    /// A local pid was recorded and `kill(pid, 0)` says a process with that
    /// id exists (either signalable by us, or `EPERM` — present but owned by
    /// someone else).
    Alive { shell_pid: i32 },
    /// A local pid was recorded and `kill(pid, 0)` returned `ESRCH`: the
    /// process is definitively gone.
    Gone,
    /// No usable evidence: no pid was ever recorded, the run was remote, the
    /// row aged out of the trust window, or the probe itself failed with an
    /// unexpected errno. Callers MUST treat this as "I don't know", never as
    /// "gone" — it is the state a mid-spawn worker legitimately occupies.
    Unknown,
}

impl WorkerProcess {
    /// `true` only for positive evidence that a process exists.
    pub fn is_alive(self) -> bool {
        matches!(self, WorkerProcess::Alive { .. })
    }

    /// The recorded pid when one was probed, else `None`.
    pub fn shell_pid(self) -> Option<i32> {
        match self {
            WorkerProcess::Alive { shell_pid } => Some(shell_pid),
            _ => None,
        }
    }

    /// Stable identifier folded into dispatch-event details and trace fields
    /// so a recurrence is attributable in one read.
    pub fn reason(self) -> &'static str {
        match self {
            WorkerProcess::Alive { .. } => "process_alive",
            WorkerProcess::Gone => "process_gone",
            WorkerProcess::Unknown => "process_unknown",
        }
    }
}

/// Pure classifier: turn a recorded pid plus a probe result into a verdict.
///
/// Split from [`probe_execution_worker`] so the decision is unit-testable
/// without spawning real processes, mirroring
/// [`crate::husk_pane_sweep::live_process_evidence_with`].
///
/// `PermissionDenied` (`EPERM`) counts as [`WorkerProcess::Alive`]: the
/// process demonstrably exists, we simply may not signal it. Reading it as
/// "gone" is the failure direction that double-dispatches onto a live worker.
/// Crate-visible because [`PidStatus`] is: the injected-probe form is an
/// internal testing seam, while the DB-backed [`probe_execution_worker`] /
/// [`probe_work_item_worker`] entry points stay `pub`.
pub(crate) fn classify_worker_process(shell_pid: Option<i64>, status: &PidStatus) -> WorkerProcess {
    // Guard before trusting the value: `kill(0, 0)` signals the caller's own
    // process group and a negative pid signals an arbitrary one, so neither is
    // a probe we would ever want to have run.
    let Some(pid) = shell_pid.filter(|pid| *pid > 0) else {
        return WorkerProcess::Unknown;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return WorkerProcess::Unknown;
    };
    match status {
        PidStatus::Alive | PidStatus::PermissionDenied => WorkerProcess::Alive { shell_pid: pid },
        PidStatus::Dead => WorkerProcess::Gone,
        PidStatus::Unknown(_) => WorkerProcess::Unknown,
    }
}

/// Probe the worker process behind `execution_id` using the pid persisted on
/// its latest LOCAL run.
///
/// Restart-robust and terminal-safe: it reads `work_runs`, so it works after
/// an engine restart wiped the in-memory registry AND after the execution row
/// went terminal — the two cases where every registry-driven check returns
/// "nothing here".
pub fn probe_execution_worker(work_db: &WorkDb, execution_id: &str) -> WorkerProcess {
    let shell_pid = match work_db.latest_local_shell_pid_for_execution(execution_id) {
        Ok(pid) => pid,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "durable liveness: failed to read shell pid; treating as unknown",
            );
            return WorkerProcess::Unknown;
        }
    };
    probe_recorded_pid(shell_pid)
}

/// The newest local worker process recorded against `work_item_id` within
/// [`REDISPATCH_PID_TRUST_SECS`], and whether it is still running.
///
/// Returns `None` when the item has no trustworthy recorded pid at all. When
/// it does, the returned execution id names the run that owns the process —
/// which for the failure this guards is a run the engine has ALREADY
/// terminalized, so callers must not assume it is non-terminal.
pub fn probe_work_item_worker(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
) -> Option<(String, WorkerProcess)> {
    let recorded = match work_db.latest_local_worker_process_for_work_item(
        work_item_id,
        REDISPATCH_PID_TRUST_SECS,
        now_epoch_secs,
    ) {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                error = %format!("{err:#}"),
                "durable liveness: failed to read work item worker process; treating as absent",
            );
            None
        }
    };
    let (execution_id, shell_pid) = recorded?;
    Some((execution_id, probe_recorded_pid(Some(shell_pid))))
}

fn probe_recorded_pid(shell_pid: Option<i64>) -> WorkerProcess {
    let Some(pid) = shell_pid.filter(|pid| *pid > 0) else {
        return WorkerProcess::Unknown;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return WorkerProcess::Unknown;
    };
    classify_worker_process(Some(i64::from(pid)), &crate::dead_pid_sweep::probe_pid(pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// A pid that is guaranteed not to exist: `kill(pid, 0)` returns `ESRCH`.
    /// Mirrors the same helper in `dead_pid_sweep`'s tests.
    fn dead_pid() -> i64 {
        // Well above any plausible live pid on macOS/Linux test hosts.
        4_194_303
    }

    #[test]
    fn zero_and_negative_pids_are_never_probed() {
        assert_eq!(
            classify_worker_process(Some(0), &PidStatus::Alive),
            WorkerProcess::Unknown,
        );
        assert_eq!(
            classify_worker_process(Some(-1), &PidStatus::Alive),
            WorkerProcess::Unknown,
        );
        assert_eq!(classify_worker_process(None, &PidStatus::Alive), WorkerProcess::Unknown);
    }

    /// `EPERM` means the process EXISTS but is not ours to signal. Reading it
    /// as gone is the direction that double-dispatches onto a live worker, so
    /// it must classify as alive.
    #[test]
    fn permission_denied_counts_as_alive() {
        assert_eq!(
            classify_worker_process(Some(42), &PidStatus::PermissionDenied),
            WorkerProcess::Alive { shell_pid: 42 },
        );
    }

    #[test]
    fn esrch_is_gone_and_other_errno_is_unknown() {
        assert_eq!(classify_worker_process(Some(42), &PidStatus::Dead), WorkerProcess::Gone);
        assert_eq!(
            classify_worker_process(
                Some(42),
                &PidStatus::Unknown(std::io::Error::from(std::io::ErrorKind::Other)),
            ),
            WorkerProcess::Unknown,
        );
    }

    /// The whole point of the module: the probe still answers after the
    /// execution row has gone TERMINAL. Every registry-driven check returns
    /// "nothing here" in this state; the durable row does not.
    #[test]
    fn probe_answers_for_a_terminal_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Our own pid is by definition alive.
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "engine believed this run was dead")
            .unwrap();

        assert!(
            probe_execution_worker(&db, &execution_id).is_alive(),
            "a terminal execution whose process is alive must still probe as alive",
        );
    }

    #[test]
    fn probe_reports_gone_for_a_dead_recorded_pid() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());

        assert_eq!(probe_execution_worker(&db, &execution_id), WorkerProcess::Gone);
    }

    /// No pid recorded is `Unknown`, never `Gone` — that is the state a
    /// mid-spawn worker legitimately occupies, and treating it as death is
    /// how a live-but-slow spawn gets reaped.
    #[test]
    fn probe_reports_unknown_when_no_pid_was_ever_recorded() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);

        assert_eq!(probe_execution_worker(&db, &execution_id), WorkerProcess::Unknown);
    }

    /// The work-item-scoped lookup finds the pid of a TERMINAL execution —
    /// the exact row a re-dispatcher is about to duplicate.
    #[test]
    fn work_item_probe_finds_the_terminal_executions_live_process() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "wrongly terminalized")
            .unwrap();

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let (found_exec, process) =
            probe_work_item_worker(&db, &work_item_id, now).expect("recorded pid must be found");
        assert_eq!(found_exec, execution_id);
        assert!(process.is_alive());
    }

    /// Pid-reuse guard: a row older than the trust window is not returned at
    /// all, so no caller can act on a pid the table can no longer vouch for.
    #[test]
    fn work_item_probe_ignores_rows_outside_the_trust_window() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));

        // Ask as if "now" were far in the future: the run's created_at falls
        // outside REDISPATCH_PID_TRUST_SECS.
        let far_future = boss_engine_utils::epoch_time::now_epoch_secs() + REDISPATCH_PID_TRUST_SECS + 60;
        assert!(
            probe_work_item_worker(&db, &work_item_id, far_future).is_none(),
            "a pid outside the trust window must not be offered to callers",
        );
    }
}
