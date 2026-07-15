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

/// How long after `started_at` a LOCAL execution may go without its worker
/// pane ever reporting a shell pid before the pane is presumed to have never
/// attached (a wedged spawn that stalled before `pane_spawned`).
///
/// A healthy local worker reports its pid (`UpdateWorkerShellPid`) within
/// seconds of the pane surface attaching — the whole pre-run dispatch
/// (lease/repo-ensure/positioning) plus surface init is comfortably under a
/// minute. 300 s is an order of magnitude beyond that, so a probe on an
/// execution this old with NO recorded pid is positive evidence the pane
/// never came up, not a race against a slow spawn. Deliberately far below the
/// 15-minute automation retry cadence so a wedged spawn self-heals before the
/// next fire re-hits the redundant-spawn guard.
pub const PANE_ATTACH_DEADLINE_SECS: i64 = 300;

/// Restart-robust verdict on whether a LOCAL execution's worker pane has ever
/// attached, derived purely from the durable `(shell_pid, started_at)`
/// signals — no dependency on the in-memory `LiveWorkerStateRegistry` (which
/// is wiped by an engine restart).
///
/// This verdict deliberately says nothing about whether an *already-reported*
/// pid is still alive — that positive-evidence probe (`kill(pid, 0)`) is
/// [`crate::dead_pane_sweep::reconcile_if_pane_dead`]'s exclusive
/// responsibility (it owns the `work_runs.shell_pid` probe), so there is
/// exactly one reaper per death signal.
///
/// Callers MUST have already established the execution is LOCAL (its latest
/// run ran on `host_id == "local"`): a pid recorded on a remote host is
/// meaningless to age-based reasoning on the engine host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLivenessVerdict {
    /// No pane pid was ever recorded and the execution started longer than
    /// [`PANE_ATTACH_DEADLINE_SECS`] ago — the pane never attached. This is
    /// the shape the incident zombies took: a run that reached `run_started`
    /// then stalled before `pane_spawned` ever fired, so no pid was reported,
    /// so the pid-driven reapers ([`crate::dead_pid_sweep`],
    /// [`crate::dead_pane_sweep`]) skip it forever (they only ever act on a
    /// pid that was actually reported).
    NeverAttached,
    /// A pid has already been reported (its liveness is
    /// [`crate::dead_pane_sweep`]'s job, not this classifier's), OR no pid
    /// yet but still within the attach deadline, OR no `started_at` to age
    /// from. Callers MUST treat this as "keep" — we only ever reap on
    /// positive evidence of death.
    LiveOrIndeterminate,
}

impl PaneLivenessVerdict {
    /// `true` for the one verdict that is positive evidence of death.
    pub fn is_dead(self) -> bool {
        matches!(self, PaneLivenessVerdict::NeverAttached)
    }

    /// Stable identifier folded into the reconcile trace event's `reason`
    /// field so a recurrence is attributable in one read.
    pub fn reason(self) -> &'static str {
        match self {
            PaneLivenessVerdict::NeverAttached => "pane_never_attached",
            PaneLivenessVerdict::LiveOrIndeterminate => "pane_live_or_indeterminate",
        }
    }
}

/// Classify whether a LOCAL execution's pane has ever attached, from its
/// durable signals.
///
/// `shell_pid` is the latest run's `work_runs.shell_pid` (the pane pid the
/// app reported, or `None` if it never did). `started_at_epoch` is the
/// execution's `started_at` parsed as epoch-seconds (`None` if it never
/// started a run). `now_epoch` is the current wall clock (injected for
/// deterministic tests).
///
/// Pure: it reads only these three values, so it returns the same verdict
/// whether called at engine startup, on a periodic sweep, or inline from the
/// redundant-spawn guard. It errs exclusively toward
/// [`PaneLivenessVerdict::LiveOrIndeterminate`] on any ambiguity — a reported
/// pid, live or not, is never this classifier's business to reap.
pub fn classify_pane_liveness(
    shell_pid: Option<i64>,
    started_at_epoch: Option<i64>,
    now_epoch: i64,
) -> PaneLivenessVerdict {
    match shell_pid {
        // A pid has already been reported — whether it is still alive is
        // `dead_pane_sweep`'s job, not this classifier's.
        Some(pid) if pid > 0 => PaneLivenessVerdict::LiveOrIndeterminate,
        // No pid ever recorded. Only positive evidence of death is age: a
        // healthy local pane reports its pid within seconds, so no pid past
        // the attach deadline means it never came up.
        _ => match started_at_epoch {
            Some(started) if now_epoch.saturating_sub(started) >= PANE_ATTACH_DEADLINE_SECS => {
                PaneLivenessVerdict::NeverAttached
            }
            _ => PaneLivenessVerdict::LiveOrIndeterminate,
        },
    }
}

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
/// failures") — it is folded into the recorded detail text, but only ever as
/// a demoted diagnostic suffix: the operator-visible detail must lead with
/// the real outcome (produced a task, or didn't) exactly like the normal
/// Stop-driven finalize does, not with pane-death forensics — an operator
/// reading a green "produced task" run should not have to parse "died" to
/// tell whether it succeeded. Best-effort: failures are logged, never
/// propagated, since the execution itself is already finalized by the time
/// this is called.
pub fn finalize_dead_automation_triage_run(work_db: &WorkDb, execution: &WorkExecution, death_reason: &str) {
    let automation_id = &execution.work_item_id;
    let (outcome, produced_task_id, detail) = match work_db.find_most_recent_open_task_for_automation(automation_id) {
        Ok(Some(task)) => {
            let detail = format!(
                "produced task {} (recovered after pane death before Stop: {death_reason})",
                task.short_label()
            );
            (AUTOMATION_OUTCOME_PRODUCED_TASK, Some(task.id), detail)
        }
        Ok(None) => (
            AUTOMATION_OUTCOME_FAILED_GAVE_UP,
            None,
            format!("no task was produced (pane died before Stop: {death_reason})"),
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
                format!("no task was produced (pane died before Stop: {death_reason})"),
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

/// Shared terminal-finalize for a non-terminal execution a reconciler has
/// proven dead. The two DB-driven reconcilers — [`crate::lost_workspace_sweep`]
/// (workspace directory gone) and [`crate::dead_pane_sweep`] (worker pane pid
/// dead) — both funnel their reap through here so the orphan → triage
/// bookkeeping → dispatch-event flow lives in exactly one place; each caller
/// supplies only what distinguishes its signal.
///
/// - `reason` is recorded on the orphan (`mark_execution_orphaned`, which
///   deliberately preserves the cube lease + workspace so a resume redispatch
///   can reclaim the work in place).
/// - `triage_death_clause` is folded into the automation-run bookkeeping for
///   `automation_triage` executions (produced_task if a task was created before
///   the worker died, else failed_gave_up).
/// - `stage` + `details` identify which signal fired on the dispatch event.
///
/// Returns `true` when the row was (or already had been) reconciled to a
/// terminal status; `false` when the orphan failed and the row is still live
/// (a later pass retries). Idempotent against a concurrent reconciler: if
/// another path finalized the row first, that still counts as reconciled.
pub async fn finalize_gone_execution(
    work_db: &WorkDb,
    dispatch_events: &dyn crate::dispatch_events::DispatchEventSink,
    execution: &WorkExecution,
    reason: &str,
    triage_death_clause: &str,
    stage: crate::dispatch_events::Stage,
    details: serde_json::Value,
) -> bool {
    match work_db.mark_execution_orphaned(&execution.id, reason) {
        Ok(_) => {}
        Err(err) => {
            let already_terminal = work_db
                .get_execution(&execution.id)
                .map(|cur| cur.status.is_terminal())
                .unwrap_or(false);
            if already_terminal {
                return true;
            }
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "reconcile: failed to orphan gone execution; leaving row as-is",
            );
            return false;
        }
    }

    if execution.kind == boss_protocol::ExecutionKind::AutomationTriage {
        finalize_dead_automation_triage_run(work_db, execution, triage_death_clause);
    }

    dispatch_events
        .emit(
            crate::dispatch_events::DispatchEvent::new(stage, crate::dispatch_events::Outcome::Ok, &execution.id)
                .with_work_item(&execution.work_item_id)
                .with_details(details),
        )
        .await;

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{ExecutionKind, ExecutionStatus};

    use crate::dispatch_events::{RecordingDispatchEventSink, Stage};
    use crate::test_support::{
        create_test_chore_manual, create_test_product_with_repo, open_db, seed_daily_automation,
    };
    use crate::work::AutomationFireRecord;
    use boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY;

    const TEST_REPO: &str = "https://github.com/test/repo";

    fn create_product(db: &WorkDb) -> String {
        create_test_product_with_repo(db, "test-product", Some(TEST_REPO)).id
    }

    fn create_automation(db: &WorkDb, product_id: &str) -> String {
        seed_daily_automation(db, product_id).id
    }

    /// Seed the pessimistic dispatch-time run row the scheduler writes at
    /// fire time (`failed_will_retry` + the "awaiting triage worker decision"
    /// placeholder) so a `finalize_gone_execution` on the triage kind has an
    /// `automation_runs` row to finalize — matching production shape.
    fn seed_dispatch_run(db: &WorkDb, automation_id: &str, triage_execution_id: &str) {
        db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation_id.to_owned())
                .scheduled_for(1_700_000_000)
                .started_at(1_700_000_000)
                .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                .detail("dispatched; awaiting triage worker decision (Stop not yet received)")
                .triage_execution_id(triage_execution_id.to_owned())
                .build(),
        )
        .unwrap();
    }

    /// The operator-visible detail for a `produced_task` dead-pane recovery
    /// must lead with the success ("produced task {id}"), exactly like the
    /// normal Stop-driven finalize does, with the pane-death forensics
    /// demoted to a parenthetical diagnostic suffix — not the other way
    /// around. An operator staring at a green "produced task" run history
    /// entry that opens with "... pane died ..." can't tell at a glance
    /// whether the run failed.
    #[test]
    fn dead_pane_recovery_detail_leads_with_the_produced_task_not_the_death() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = db.create_automation_triage_execution(&automation, TEST_REPO).unwrap();
        seed_dispatch_run(&db, &automation, &exec.id);

        let task = create_test_chore_manual(&db, product.as_str(), "produced by triage");
        db.stamp_task_source_automation_for_test(&task.id, &automation, "todo")
            .unwrap();

        finalize_dead_automation_triage_run(
            &db,
            &exec,
            "its worker shell pid 68417 was gone (pane died with the host app)",
        );

        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_PRODUCED_TASK);
        let detail = runs[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.starts_with(&format!("produced task {}", task.short_label())),
            "detail must lead with the success, got {detail:?}"
        );
        assert!(
            detail.contains("pane died"),
            "the pane-death forensics must still be present, just demoted, got {detail:?}"
        );
    }

    /// The failure-path detail (no task produced) must plainly state that
    /// first, with the pane-death clause folded in as an explanation rather
    /// than the leading claim — consistent phrasing with the success path
    /// above regardless of which outcome fired.
    #[test]
    fn dead_pane_recovery_detail_leads_with_no_task_produced_on_failure() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = db.create_automation_triage_execution(&automation, TEST_REPO).unwrap();
        seed_dispatch_run(&db, &automation, &exec.id);

        finalize_dead_automation_triage_run(&db, &exec, "its cube workspace `/tmp/ws-x` is gone");

        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
        let detail = runs[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.starts_with("no task was produced"),
            "detail must lead with the outcome, got {detail:?}"
        );
    }

    /// The `details` blob a lost-workspace reconcile would attach — the exact
    /// value is irrelevant to these tests, which assert on whether an event
    /// was emitted at all, not on its payload.
    fn reap_details() -> serde_json::Value {
        serde_json::json!({ "reason": "workspace_dir_missing" })
    }

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

    /// Concurrent-reconciler idempotency (the `Err`-arm `already_terminal`
    /// branch of [`finalize_gone_execution`]). Two reconcilers snapshot the
    /// same live triage row; the first to act ("winner") finalizes it and
    /// emits exactly one dispatch event. The second ("loser") calls
    /// `finalize_gone_execution` with its now-stale non-terminal snapshot:
    /// `mark_execution_orphaned` bails because the DB row is already terminal,
    /// so the loser must still report `true` (the contract's "that still
    /// counts as reconciled"), must NOT re-finalize the row, and must NOT emit
    /// a second, spurious event.
    #[tokio::test]
    async fn concurrent_loser_reports_reconciled_without_double_finalizing() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let automation = create_automation(&db, &product);
        let exec = db.create_automation_triage_execution(&automation, TEST_REPO).unwrap();
        seed_dispatch_run(&db, &automation, &exec.id);
        assert!(!exec.status.is_terminal(), "precondition: snapshot must be live");

        let sink = RecordingDispatchEventSink::new();

        // Winner: drives the row terminal, finalizes the automation run, and
        // emits one `lost_workspace_reconcile` event.
        let winner = finalize_gone_execution(
            &db,
            &sink,
            &exec,
            "winner reconcile",
            "its cube workspace is gone",
            Stage::LostWorkspaceReconcile,
            reap_details(),
        )
        .await;
        assert!(winner, "the winning reconciler finalizes and returns reconciled");

        let after_win = db.get_execution(&exec.id).unwrap();
        assert_eq!(after_win.status, ExecutionStatus::Orphaned);
        assert_eq!(
            sink.events_for(&exec.id).await.len(),
            1,
            "the winner emits exactly one dispatch event"
        );

        // Loser: same stale (still non-terminal) snapshot. The DB row is now
        // terminal, so `mark_execution_orphaned` bails and we take the
        // idempotent `already_terminal` branch.
        let loser = finalize_gone_execution(
            &db,
            &sink,
            &exec,
            "loser reconcile",
            "its cube workspace is gone",
            Stage::LostWorkspaceReconcile,
            reap_details(),
        )
        .await;
        assert!(
            loser,
            "a row a concurrent reconciler already finalized still counts as reconciled"
        );

        let after_loss = db.get_execution(&exec.id).unwrap();
        assert_eq!(
            after_loss.status,
            ExecutionStatus::Orphaned,
            "the loser must not change the already-terminal status"
        );
        assert_eq!(
            after_loss.finished_at, after_win.finished_at,
            "the loser must not re-stamp finished_at (no double-finalize)"
        );
        assert_eq!(
            sink.events_for(&exec.id).await.len(),
            1,
            "the loser must NOT emit a second, spurious dispatch event"
        );

        // The automation run was finalized once by the winner and left
        // untouched by the loser — the placeholder is gone, replaced by the
        // winner's terminal outcome, and not re-processed.
        let runs = db.list_automation_runs(&automation).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
    }

    /// Still-live retry path (the `Err`-arm `!already_terminal` branch). When
    /// `mark_execution_orphaned` fails and the row is NOT observably terminal,
    /// [`finalize_gone_execution`] must return `false` — so a later sweep pass
    /// retries — and must NOT emit a dispatch event (nothing was finalized).
    ///
    /// We drive this branch with a non-terminal snapshot whose id is absent
    /// from the DB: `mark_execution_orphaned` fails on the missing row, and
    /// the terminal-status re-probe (`get_execution`) cannot confirm
    /// terminality, so `already_terminal` resolves to `false`. That is the
    /// identical branch a transient orphan failure on a genuinely-live row
    /// would take (`get_execution` → non-terminal → `false`); a missing row is
    /// the only orphan failure a unit test can induce without a
    /// fault-injecting DB or production changes.
    #[tokio::test]
    async fn orphan_failure_on_non_terminal_row_returns_false_and_emits_nothing() {
        let (_dir, db) = open_db();
        // `execution_with_workspace(None)` builds a live (`waiting_human`)
        // triage snapshot with id `exec_test`, which does not exist in this
        // fresh DB.
        let exec = execution_with_workspace(None);
        assert!(!exec.status.is_terminal(), "precondition: snapshot must be live");
        assert!(
            db.get_execution(&exec.id).is_err(),
            "precondition: the row must be absent so the orphan fails"
        );

        let sink = RecordingDispatchEventSink::new();
        let reconciled = finalize_gone_execution(
            &db,
            &sink,
            &exec,
            "reconcile a row that cannot be orphaned",
            "its cube workspace is gone",
            Stage::LostWorkspaceReconcile,
            reap_details(),
        )
        .await;

        assert!(
            !reconciled,
            "an orphan failure on a non-terminal row must report NOT reconciled so a later pass retries"
        );
        assert!(
            sink.events_for(&exec.id).await.is_empty(),
            "no dispatch event may be emitted when the row was not finalized"
        );
    }

    // ── classify_pane_liveness ────────────────────────────────────────────────

    #[test]
    fn pane_indeterminate_once_any_pid_has_been_reported() {
        // Once a pid has been reported, its liveness is `dead_pane_sweep`'s
        // job — this classifier never claims death for a reported pid, dead
        // or alive.
        let alive = std::process::id() as i64;
        let verdict = classify_pane_liveness(Some(alive), Some(1_700_000_000), 1_700_009_999);
        assert_eq!(verdict, PaneLivenessVerdict::LiveOrIndeterminate);
        assert!(!verdict.is_dead());
    }

    #[test]
    fn pane_never_attached_when_no_pid_and_past_attach_deadline() {
        // No pid ever reported and started well over the deadline ago — the
        // pane never came up (the incident zombies' stalled-at-run_started
        // shape). This is the case every pid-driven reaper skips.
        let started = 1_700_000_000;
        let now = started + PANE_ATTACH_DEADLINE_SECS + 1;
        let verdict = classify_pane_liveness(None, Some(started), now);
        assert_eq!(verdict, PaneLivenessVerdict::NeverAttached);
        assert!(verdict.is_dead());
        assert_eq!(verdict.reason(), "pane_never_attached");
    }

    #[test]
    fn pane_indeterminate_when_no_pid_but_within_attach_deadline() {
        // A just-started worker whose pid RPC is still in flight must NOT be
        // reaped — give it the full attach deadline to report.
        let started = 1_700_000_000;
        let now = started + PANE_ATTACH_DEADLINE_SECS - 1;
        let verdict = classify_pane_liveness(None, Some(started), now);
        assert_eq!(verdict, PaneLivenessVerdict::LiveOrIndeterminate);
        assert!(!verdict.is_dead());
    }

    #[test]
    fn pane_indeterminate_when_no_pid_and_no_started_at() {
        // No pid and no started_at (never ran) → we have no age to judge
        // from, so we hold rather than reap.
        assert_eq!(
            classify_pane_liveness(None, None, 1_700_009_999),
            PaneLivenessVerdict::LiveOrIndeterminate,
        );
    }

    #[test]
    fn pane_indeterminate_when_pid_zero_within_deadline() {
        // A zero/negative pid is treated as "not reported", same as None.
        let started = 1_700_000_000;
        assert_eq!(
            classify_pane_liveness(Some(0), Some(started), started + 10),
            PaneLivenessVerdict::LiveOrIndeterminate,
        );
    }
}
