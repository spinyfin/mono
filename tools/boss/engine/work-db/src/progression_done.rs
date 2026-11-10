//! The progression branch of `boss propose done` (design 2.4 and 2.5).
//!
//! A progression run ends by declaring how it ended, and the declaration is the
//! run's only disposition:
//!
//! | `done` | disposition | meaning |
//! | --- | --- | --- |
//! | `--outcome delivered` | `progressed` | the run acted, or diagnosed and left the item to the work it created |
//! | `--outcome no-changes-needed` | `no_action` | the run judged the item not stuck in a way any verb addresses; files nothing |
//! | `--outcome blocked --question … --criterion …` | `asked` | the owner was asked one specific question |
//!
//! There is no PR to resolve (the run holds no checkout), so `delivered` and
//! `no-changes-needed` finish the run without looking for one. A bare `blocked`
//! is refused: a human-needed outcome has to be a specific question. A run that
//! ends without any declaration is `failed` with `error_text = 'no disposition'`.
//!
//! Three refusals are worth naming, because each closes a way for an agent to
//! wave an item through:
//!
//! - **a bare `blocked`**, since an attention nobody reads is the failure this
//!   project exists to end;
//! - **a question with no `--criterion`**, since the report audits which of the
//!   four numbered reasons each progression question cited;
//! - **`no-changes-needed` on a declared-blocker item** (`deliberate_park`, or a
//!   `worker_failed` produced by `declared_blocked` / `nudge_breaker`): the worker
//!   said it needed something, so the run must act on that (a `redispatch`
//!   carrying the evidence it is stale, a mechanical fix) or ask.

use super::*;
use boss_protocol::{ProgressionDisposition, RunDoneOutcome, RunDoneProposalPayload, run_done_question_criterion};

/// Why a progression `done` declaration is refused, in words for the agent; `None`
/// when it may be recorded. The caller (`apply_run_done`) stores the refusal as a
/// rejected proposal, which `boss propose done` renders as the command's error.
pub(crate) fn refuse_progression_done(
    conn: &Connection,
    execution: &WorkExecution,
    payload: &RunDoneProposalPayload,
    payload_json: &str,
) -> Result<Option<String>> {
    match payload.outcome {
        RunDoneOutcome::Delivered => Ok(None),
        RunDoneOutcome::NoChangesNeeded => {
            let Some(task) = query_task(conn, &execution.work_item_id)? else {
                return Ok(None);
            };
            if declared_blocker(conn, &task)? {
                return Ok(Some(
                    "no-changes-needed is refused for an item whose last worker declared a blocker: the worker \
                     said it needed something, so either `redispatch` with a brief addendum quoting the blocker \
                     and the evidence it is stale, fix the mechanical gap with `precursor`, `archive-revision` or \
                     `undepend`, or ask the owner one specific question"
                        .to_owned(),
                ));
            }
            Ok(None)
        }
        RunDoneOutcome::Blocked => {
            if let Some((id, status)) =
                crate::progression_verbs::live_or_pending_execution(conn, &execution.work_item_id)?
            {
                return Ok(Some(format!(
                    "work has resumed ({id}, {status}); a stale progression question cannot park it"
                )));
            }
            if payload.question.is_none() {
                return Ok(Some(
                    "a progression run that needs the owner must ask one specific question; a bare `blocked` is \
                     refused. Re-run `done --outcome blocked` with `--question`, `--answer-type`, `--explanation` \
                     and `--criterion`, or end with `delivered` or `no-changes-needed` if no human is needed"
                        .to_owned(),
                ));
            }
            if run_done_question_criterion(payload_json).is_none() {
                return Ok(Some(
                    "a progression question must cite why it may be asked: add `--criterion 1|2|3|4` (1 the action \
                     is human-only, 2 every direct verb is spent or refused, 3 a decision only the owner can make, \
                     4 the evidence contradicts itself)"
                        .to_owned(),
                ));
            }
            // One open question per item, and never a second on top of someone else's.
            let other: Option<String> = conn
                .query_row(
                    "SELECT execution_id FROM operator_questions
                     WHERE work_item_id = ?1 AND status = 'open' AND execution_id <> ?2 LIMIT 1",
                    params![execution.work_item_id, execution.id],
                    |row| row.get(0),
                )
                .optional()?;
            if other.is_some() {
                return Ok(Some(
                    "this item already has an open question to the owner; progression never asks a second one on \
                     top of it"
                        .to_owned(),
                ));
            }
            Ok(None)
        }
    }
}

/// A declared-blocker item (design 2.9): `deliberate_park`, or a `worker_failed`
/// whose producer is `declared_blocked` or `nudge_breaker`. The same definition
/// the brief uses when it renders the worker's own words.
fn declared_blocker(conn: &Connection, task: &Task) -> Result<bool> {
    if task.dispatch_failed_reason.as_deref() == Some("deliberate_park") {
        return Ok(true);
    }
    if task.blocked_reason.as_deref() != Some("worker_failed") {
        return Ok(false);
    }
    let basis = crate::stuck_report::latest_basis(conn, &task.id, "blocked_reason")?;
    Ok(matches!(basis.as_deref(), Some("declared_blocked" | "nudge_breaker")))
}

/// Record the disposition a `done` outcome earns, in the transaction that stamps
/// the declaration, so the two can never disagree. A re-declaration overwrites.
pub(crate) fn record_disposition(conn: &Connection, execution_id: &str, outcome: RunDoneOutcome) -> Result<()> {
    conn.execute(
        "UPDATE work_executions SET progression_disposition = ?2 WHERE id = ?1",
        params![execution_id, ProgressionDisposition::for_outcome(outcome).as_str()],
    )?;
    Ok(())
}

/// Park the item on the question the run asked: `blocked: awaiting_operator_answer`
/// with the question text as its detail. The question supersedes the halt the
/// item was stuck in, so a `deliberate_park` / `nudge_breaker` attention is
/// resolved and any dispatch-failure stamp is cleared; the question is the one
/// hold, and the owner's answer restarts the item.
fn park_item_on_question(conn: &Connection, execution: &WorkExecution) -> Result<()> {
    if crate::progression_verbs::live_or_pending_execution(conn, &execution.work_item_id)?.is_some() {
        conn.execute(
            "UPDATE operator_questions SET status = 'withdrawn', withdrawn_reason = 'work_resumed'
             WHERE execution_id = ?1 AND status = 'open'",
            [&execution.id],
        )?;
        return Ok(());
    }
    let question: Option<String> = conn
        .query_row(
            "SELECT json_extract(question_json, '$.text') FROM operator_questions
             WHERE execution_id = ?1 AND status = 'open' ORDER BY created_at DESC, id DESC LIMIT 1",
            [&execution.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(question) = question else {
        // Superseded or withdrawn: there is nothing for the item to wait on.
        return Ok(());
    };
    crate::task_status_audit::execute(
        conn,
        "progression_question",
        boss_protocol::LAST_STATUS_ACTOR_PROGRESSION,
        "UPDATE tasks
         SET status = 'blocked', blocked_reason = 'awaiting_operator_answer', blocked_detail = ?2,
             blocked_attempt_id = NULL, autostart = 0,
             dispatch_failed_reason = NULL, dispatch_failed_error = NULL, dispatch_failed_at = NULL,
             last_status_actor = ?4, updated_at = ?3
         WHERE id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'archived')",
        params![
            execution.work_item_id,
            question,
            now_string(),
            boss_protocol::LAST_STATUS_ACTOR_PROGRESSION
        ],
    )?;
    crate::dispatch_admission::resolve_deliberate_park_attention(conn, &execution.work_item_id)?;
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The disposition a progression execution recorded when it declared `done`,
    /// or `None` when it has not (or is not a progression run).
    pub fn progression_disposition(&self, execution_id: &str) -> Result<Option<ProgressionDisposition>> {
        let conn = self.connect()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT progression_disposition FROM work_executions WHERE id = ?1",
                [execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        raw.map(|value| value.parse::<ProgressionDisposition>().map_err(anyhow::Error::msg))
            .transpose()
    }

    /// Finish a progression run that declared `done`: when it asked, park the item
    /// on the question, then complete the execution and its run. One transaction,
    /// so an `asked` run is never completed with the item left unparked. Never
    /// touches a PR, a lease or a checkout (the run holds none). Idempotent:
    /// `Ok(None)` when the execution is already terminal. An execution with no
    /// recorded disposition is an error; the caller fails it as `no disposition`
    /// instead.
    pub fn finish_progression_declared(&self, execution_id: &str) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status.is_terminal() {
            return Ok(None);
        }
        anyhow::ensure!(
            execution.kind == ExecutionKind::Progression,
            "execution {execution_id} is a `{}` run, not a progression run",
            execution.kind
        );
        let disposition: Option<String> = tx.query_row(
            "SELECT progression_disposition FROM work_executions WHERE id = ?1",
            [execution_id],
            |row| row.get(0),
        )?;
        let disposition = disposition
            .context("progression run has no recorded disposition")?
            .parse::<ProgressionDisposition>()
            .map_err(anyhow::Error::msg)?;
        let mut pending = PendingEvents::new();
        if disposition == ProgressionDisposition::Asked {
            park_item_on_question(&tx, &execution)?;
            stage_task_board_change(&mut pending, &tx, &execution.work_item_id)?;
        }
        let summary: Option<String> = tx
            .query_row(
                "SELECT json_extract(payload_json, '$.summary') FROM worker_proposals
                 WHERE execution_id = ?1 AND kind = 'run_done' AND state = 'applied'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [execution_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let now = now_string();
        tx.execute(
            "UPDATE work_runs
             SET status = 'completed',
                 result_summary = COALESCE(?2, result_summary),
                 finished_at = COALESCE(finished_at, ?3)
             WHERE execution_id = ?1 AND finished_at IS NULL",
            params![execution_id, summary, now],
        )?;
        tx.execute(
            "UPDATE work_executions SET status = 'completed', finished_at = ?2 WHERE id = ?1",
            params![execution_id, now],
        )?;
        let updated = query_execution(&tx, execution_id).require("execution", execution_id)?;
        stage_execution_terminal(&mut pending, &tx, execution_id, &updated.work_item_id)?;
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(Some(updated))
    }
}
