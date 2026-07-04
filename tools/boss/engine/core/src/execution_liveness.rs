//! Restart-robust liveness primitives for `work_executions` rows.
//!
//! The engine's `ExecutionStatus::is_live()` (`running` / `waiting_human`)
//! is a *paper* liveness signal: it says the engine last recorded the row
//! as non-terminal, not that a worker is actually running. The gap between
//! the two is what produced the 2026-06-14 automation wedge — three triage
//! panes died without emitting a `Stop` hook (the cube workspace-root
//! migration relocated the pool out from under them), so their rows stayed
//! `waiting_human` forever and the redundant-spawn guard treated them as
//! live, blocking every subsequent fire.
//!
//! This module provides the *positive* liveness evidence the guard and the
//! reconciler need: a worker's cube workspace directory is its cwd for the
//! whole lifetime of its pane, so if that directory has vanished from disk
//! the worker cannot still be running. The check is:
//!
//! - **Restart-robust** — it reads the DB row + the filesystem, not the
//!   in-memory `LiveWorkerStateRegistry` (which is empty after any engine
//!   restart, so registry-driven reapers like `dead_pid_sweep` never see a
//!   pre-restart zombie).
//! - **Conservative** — it returns `false` (NOT provably gone) whenever
//!   `workspace_path` is absent/empty, so callers only ever act on
//!   *positive* evidence of death.
//!
//! Callers MUST additionally gate on host locality (the execution's run
//! ran on `host_id == "local"`): a `.exists()` probe on the engine host is
//! meaningless for a remote worker whose `workspace_path` lives on another
//! machine. See [`crate::lost_workspace_sweep`].

use std::path::Path;

use boss_protocol::{AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_PRODUCED_TASK, WorkExecution};

use crate::work::WorkDb;

/// `true` when `execution` records a non-empty `workspace_path` that no
/// longer exists on the local filesystem — positive evidence that the
/// worker's checkout (and therefore its pane) is gone.
///
/// Returns `false` when `workspace_path` is `None`/empty: absence of a
/// recorded workspace is not evidence of death, and we never want to
/// finalize a row on a mere absence of information.
///
/// This does **not** check the execution's status or host — that is the
/// caller's responsibility (only non-terminal, local executions are
/// eligible). Keeping this a pure `WorkExecution → bool` function makes it
/// trivial to unit-test and reuse from both the periodic sweep and the
/// coordinator's redundant-spawn guard.
pub fn execution_workspace_dir_missing(execution: &WorkExecution) -> bool {
    match execution.workspace_path.as_deref() {
        Some(path) if !path.is_empty() => !Path::new(path).exists(),
        _ => false,
    }
}

/// Record the terminal `automation_runs` outcome for an `automation_triage`
/// execution that died before ever reaching a Stop-driven finalize — shared
/// by every reconciler that discovers positive evidence a triage pane is
/// gone ([`crate::lost_workspace_sweep`]'s missing-workspace-directory check
/// and [`crate::cube_lease_heartbeat`]'s repeated-heartbeat-failure
/// auto-reap). Both need the same open-task-recovery bookkeeping: a triage
/// that created a task before its pane died is recorded as `produced_task`
/// (fixing the historical bug where a crash-before-`Stop` silently dropped
/// the produced task), otherwise the occurrence is `failed_gave_up`. Either
/// way this overwrites the pessimistic dispatch-time placeholder
/// ("dispatched; awaiting triage worker decision …") with the truth so the
/// automation's run history is honest.
///
/// `death_reason` is a human-readable clause describing *why* the caller
/// believes the pane is gone (e.g. "its cube workspace `{path}` is gone" or
/// "its cube lease `{id}` was no longer tracked after N heartbeat
/// failures") — it is folded into the recorded detail text. Best-effort:
/// failures are logged, never propagated, since the execution itself is
/// already finalized by the time this is called.
pub fn finalize_dead_automation_triage_run(work_db: &WorkDb, execution: &WorkExecution, death_reason: &str) {
    let automation_id = &execution.work_item_id;
    let (outcome, produced_task_id, detail) = match work_db.find_most_recent_open_task_for_automation(automation_id) {
        Ok(Some(task)) => {
            let detail = format!(
                "produced_task (dead-pane recovery): task {} was created before the triage pane died; {death_reason}",
                task.id
            );
            (AUTOMATION_OUTCOME_PRODUCED_TASK, Some(task.id), detail)
        }
        Ok(None) => (
            AUTOMATION_OUTCOME_FAILED_GAVE_UP,
            None,
            format!("triage pane died before Stop and {death_reason}; no task was produced"),
        ),
        Err(err) => {
            tracing::warn!(
                execution_id = %execution.id,
                automation_id = %automation_id,
                error = %format!("{err:#}"),
                "dead-triage reconcile: open-task lookup failed; recording failed_gave_up",
            );
            (
                AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                None,
                format!("triage pane died before Stop and {death_reason}"),
            )
        }
    };

    match work_db.finalize_automation_triage_run(&execution.id, outcome, produced_task_id.as_deref(), Some(&detail)) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            execution_id = %execution.id,
            automation_id = %automation_id,
            "dead-triage reconcile: no automation_runs row matched this triage execution; outcome not recorded",
        ),
        Err(err) => tracing::warn!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "dead-triage reconcile: failed to finalize automation_runs row",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{ExecutionKind, ExecutionStatus};

    fn execution_with_workspace(path: Option<&str>) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_test")
            .work_item_id("auto_test")
            .kind(ExecutionKind::AutomationTriage)
            .status(ExecutionStatus::WaitingHuman)
            .repo_remote_url("git@example.com:foo.git")
            .created_at("2026-06-14T22:00:00Z")
            .maybe_workspace_path(path.map(str::to_owned))
            .build()
    }

    #[test]
    fn missing_when_recorded_path_absent_from_disk() {
        // A path that cannot exist (old cube root that was migrated away).
        let exec = execution_with_workspace(Some("/nonexistent/Documents/dev/workspaces/mono-agent-028"));
        assert!(
            execution_workspace_dir_missing(&exec),
            "a recorded workspace_path that is absent on disk is a lost workspace"
        );
    }

    #[test]
    fn not_missing_when_recorded_path_exists() {
        // The engine host itself always exists — use a directory that is
        // guaranteed present so the check is deterministic in the sandbox.
        let dir = std::env::temp_dir();
        let exec = execution_with_workspace(Some(dir.to_str().unwrap()));
        assert!(
            !execution_workspace_dir_missing(&exec),
            "a recorded workspace_path that exists on disk is NOT a lost workspace"
        );
    }

    #[test]
    fn conservative_when_no_workspace_recorded() {
        // No workspace_path → we have no evidence either way → false.
        let exec = execution_with_workspace(None);
        assert!(
            !execution_workspace_dir_missing(&exec),
            "absence of a recorded workspace_path is not evidence of death"
        );
    }

    #[test]
    fn conservative_when_workspace_path_empty() {
        let exec = execution_with_workspace(Some(""));
        assert!(
            !execution_workspace_dir_missing(&exec),
            "an empty workspace_path is not evidence of death"
        );
    }
}
