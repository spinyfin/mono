//! Per-run semantic-progress checkpoint accessors.
//!
//! Columns live on `work_runs` for the same scoping reason as
//! `progress_ingress_checkpoint`: the stamp describes one spawned process,
//! so a later run must never inherit an earlier one's clock.

use super::run_rows::resolve_run_id_for_execution_hooks;
use super::*;

use boss_engine_storage_values::{SemanticProgressCheckpoint, SemanticToolCondition, SemanticTurnCondition};
use boss_engine_storage_values::{tool_condition_signal, turn_condition_signal};
use boss_protocol::WorkerEvent;

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record a driver-originated progress event against the run's
    /// agent-session row, deriving the conditions from the event alone.
    ///
    /// Use this only when the live-state reducer has no slot to settle the
    /// event (see [`Self::record_semantic_checkpoint`] otherwise): it applies
    /// events in arrival order and knows nothing of ingress ordering or
    /// subagent origin.
    ///
    /// Notifications leave the checkpoint unchanged; other calls update its timestamp. The tool
    /// condition advances through [`next_tool_condition`], but never by
    /// reading `previous` back from the row first: `next_tool_condition`
    /// only consults `previous` to carry it through unchanged (session
    /// start/end and `Notification` are not tool-state signals), and a
    /// carry is exactly "don't touch the column" in SQL — so a `PreToolUse`
    /// or tool-boundary event writes a single keyed UPDATE, and a carry
    /// event writes the timestamp alone, skipping both the read and the
    /// tool-condition write this per-event hot path would otherwise repeat
    /// on every dispatched event.
    ///
    /// Returns `true` when a row was updated, `false` when no run row exists
    /// yet for this execution — benign and routine (this fan-out reaches
    /// every dispatched event from every source, including ones that beat
    /// run-row creation), matching
    /// [`Self::record_run_turn_boundary_for_execution`]'s convention rather
    /// than erroring: the caller decides how loudly to log the no-row case,
    /// and a genuine DB failure still surfaces as `Err`.
    pub fn record_semantic_progress(&self, execution_id: &str, event: &WorkerEvent) -> Result<bool> {
        self.record_semantic_progress_observed(execution_id, event, None)
    }

    /// [`Self::record_semantic_progress`] stamped with the time the event was
    /// observed rather than now. A replayed event from a spoke carries its
    /// original time; the checkpoint never moves backwards.
    pub fn record_semantic_progress_observed(
        &self,
        execution_id: &str,
        event: &WorkerEvent,
        observed_at_ms: Option<i64>,
    ) -> Result<bool> {
        self.enqueue_record_semantic_progress_observed(execution_id, event, observed_at_ms)?
            .wait()
    }

    pub fn enqueue_record_semantic_progress_observed(
        &self,
        execution_id: &str,
        event: &WorkerEvent,
        observed_at_ms: Option<i64>,
    ) -> Result<IngressReceipt<bool>> {
        let now = progress_time(observed_at_ms);
        // Bound independently: each column is carried through unchanged
        // (`COALESCE`) when its own signal is absent, so a divergence between
        // the two signal functions can never drop a transition.
        let tool = tool_condition_signal(event).map(|c| c.as_str());
        let turn = turn_condition_signal(event).map(|c| c.as_str());
        let execution_id = execution_id.to_owned();
        let notification = matches!(event, WorkerEvent::Notification { .. });
        let urgent = matches!(event, WorkerEvent::Stop { .. } | WorkerEvent::UserPromptSubmit { .. });
        self.ingress_writer()?
            .submit("local".into(), execution_id.len() + 128, urgent, move |conn| {
                if notification {
                    return Ok(false);
                }

                let Some(run_id) = resolve_run_id_for_execution_hooks(conn, &execution_id)? else {
                    return Ok(false);
                };
                conn.execute(
                    "UPDATE work_runs
             SET semantic_progress_at = CASE WHEN semantic_progress_at IS NULL OR semantic_progress_at < ?2
                                             THEN ?2 ELSE semantic_progress_at END,
                 semantic_tool_condition = COALESCE(?3, semantic_tool_condition),
                 semantic_turn_condition = COALESCE(?4, semantic_turn_condition)
             WHERE id = ?1
               AND (semantic_progress_at IS NOT ?2
                    OR (?3 IS NOT NULL AND semantic_tool_condition IS NOT ?3)
                    OR (?4 IS NOT NULL AND semantic_turn_condition IS NOT ?4))",
                    params![run_id, now, tool, turn],
                )?;
                Ok(true)
            })
    }

    /// Persist the registry's post-apply checkpoint for a run.
    ///
    /// The live-state reducer is the authority on ingress ordering and on
    /// which events count as main-session activity, so the durable column is
    /// written from what it settled on rather than from the event as it
    /// arrived: a late `Stop` the reducer refused, or a subagent tool hook it
    /// does not count, therefore cannot overwrite the turn state. An
    /// `Unknown` condition leaves the stored column untouched.
    ///
    /// Returns `false` when no run row exists yet, like
    /// [`Self::record_semantic_progress`].
    pub fn record_semantic_checkpoint(
        &self,
        execution_id: &str,
        checkpoint: &SemanticProgressCheckpoint,
    ) -> Result<bool> {
        self.enqueue_record_semantic_checkpoint(execution_id, checkpoint)?
            .wait()
    }

    pub fn enqueue_record_semantic_checkpoint(
        &self,
        execution_id: &str,
        checkpoint: &SemanticProgressCheckpoint,
    ) -> Result<IngressReceipt<bool>> {
        let tool =
            (checkpoint.tool_condition != SemanticToolCondition::Unknown).then(|| checkpoint.tool_condition.as_str());
        let turn =
            (checkpoint.turn_condition != SemanticTurnCondition::Unknown).then(|| checkpoint.turn_condition.as_str());
        let urgent = checkpoint.turn_condition == SemanticTurnCondition::Ended;
        let progress_at = checkpoint.progress_at.clone();
        let execution_id = execution_id.to_owned();
        self.ingress_writer()?
            .submit("local".into(), execution_id.len() + 128, urgent, move |conn| {
                let Some(run_id) = resolve_run_id_for_execution_hooks(conn, &execution_id)? else {
                    return Ok(false);
                };
                conn.execute(
                    "UPDATE work_runs
             SET semantic_progress_at = CASE WHEN semantic_progress_at IS NULL OR semantic_progress_at < ?2
                                             THEN ?2 ELSE semantic_progress_at END,
                 semantic_tool_condition = COALESCE(?3, semantic_tool_condition),
                 semantic_turn_condition = COALESCE(?4, semantic_turn_condition)
             WHERE id = ?1",
                    params![run_id, progress_at, tool, turn],
                )?;
                Ok(true)
            })
    }

    /// Stamp driver progress time alone, leaving both conditions as stored.
    /// For an event the reducer refused as stale: it still proves the worker
    /// is alive, but describes a state the worker has already left.
    pub fn record_semantic_progress_stamp(&self, execution_id: &str) -> Result<bool> {
        self.record_semantic_progress_stamp_observed(execution_id, None)
    }

    /// [`Self::record_semantic_progress_stamp`] at the observed time of a
    /// replayed event; the stamp never moves backwards.
    pub fn record_semantic_progress_stamp_observed(
        &self,
        execution_id: &str,
        observed_at_ms: Option<i64>,
    ) -> Result<bool> {
        self.enqueue_record_semantic_progress_stamp_observed(execution_id, observed_at_ms)?
            .wait()
    }

    pub fn enqueue_record_semantic_progress_stamp_observed(
        &self,
        execution_id: &str,
        observed_at_ms: Option<i64>,
    ) -> Result<IngressReceipt<bool>> {
        let now = progress_time(observed_at_ms);
        let execution_id = execution_id.to_owned();
        self.ingress_writer()?
            .submit("local".into(), execution_id.len() + 128, false, move |conn| {
                let Some(run_id) = resolve_run_id_for_execution_hooks(conn, &execution_id)? else {
                    return Ok(false);
                };
                conn.execute(
                    "UPDATE work_runs SET semantic_progress_at = ?2
             WHERE id = ?1 AND (semantic_progress_at IS NULL OR semantic_progress_at < ?2)",
                    params![run_id, now],
                )?;
                Ok(true)
            })
    }

    /// Read back the run's semantic-progress checkpoint.
    ///
    /// `Ok(None)` means the run never recorded one — a legitimate answer
    /// for a run dispatched by an engine that predates the columns, and a
    /// distinct one from a read failure, which surfaces as `Err`. A row
    /// whose timestamp is set but whose condition is NULL is returned as
    /// [`SemanticToolCondition::Unknown`]; likewise a NULL turn condition
    /// (every row written before the column existed) is
    /// [`SemanticTurnCondition::Unknown`].
    pub fn get_run_semantic_progress_checkpoint(
        &self,
        execution_id: &str,
    ) -> Result<Option<SemanticProgressCheckpoint>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT semantic_progress_at, semantic_tool_condition, semantic_turn_condition
                 FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(progress_at), tool_condition, turn_condition)) => Some(SemanticProgressCheckpoint {
                progress_at,
                tool_condition: SemanticToolCondition::parse(tool_condition.as_deref()),
                turn_condition: SemanticTurnCondition::parse(turn_condition.as_deref()),
            }),
            Some((None, _, _)) | None => None,
        })
    }
}

/// The checkpoint time for an event: when it was observed, else now.
/// A spoke clock that runs ahead of the hub must not pin the monotonic
/// checkpoint in the future, or hung-worker and stale-worker sweeps would
/// treat a dead worker as fresh for the skew.
fn progress_time(observed_at_ms: Option<i64>) -> String {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let secs = observed_at_ms
        .filter(|ms| *ms > 0)
        .map(|ms| ms.div_euclid(1000).min(now))
        .unwrap_or(now);
    boss_engine_utils::iso8601::format_epoch_iso8601(secs)
}
