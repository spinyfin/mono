//! Dismissal closes a question without granting any of its authorizations.
use super::*;

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    // Dismissal is an attributed withdrawal, kept separately from answer_json.
    // This additive migration preserves the existing status constraint, indexes,
    // views and task-override trigger without rebuilding the question table.
    if !crate::schema_init::table_has_column(conn, "operator_questions", "dismissal_json")? {
        conn.execute_batch("ALTER TABLE operator_questions ADD COLUMN dismissal_json TEXT")?;
    }
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Close the selected question atomically with its human hold. A question id
    /// supplied alongside a task id must belong to that task, including on retry.
    pub fn dismiss_operator_question_from(
        &self,
        id: &str,
        question_id: Option<&str>,
        note: Option<&str>,
        actor: &str,
        provenance: Option<&str>,
    ) -> Result<OperatorAnswerOutcome> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let question = tx
            .query_row(
                &format!(
                    "{OPERATOR_QUESTION_RECORD_SELECT}
                WHERE (id = ?1 OR work_item_id = ?1) AND (?2 IS NULL OR id = ?2)
                ORDER BY (status = 'open') DESC, created_at DESC, id DESC LIMIT 1"
                ),
                params![id, question_id],
                map_question,
            )
            .optional()?
            .ok_or(OperatorQuestionError::NotFound)?;
        let mut task = query_task(&tx, &question.work_item_id)?
            .filter(|task| task.deleted_at.is_none())
            .ok_or(OperatorQuestionError::NotFound)?;
        if question.status == OperatorQuestionStatus::Dismissed {
            project_for_work_item_updated(&tx, &mut task, "dismiss_operator_question");
            return Ok(OperatorAnswerOutcome::unchanged(task_to_item(task)));
        }
        if question.status != OperatorQuestionStatus::Open {
            return Err(OperatorQuestionError::Conflict {
                state: if question.status == OperatorQuestionStatus::Answered {
                    "answered"
                } else {
                    "withdrawn"
                }
                .into(),
                answer: question.answer,
            }
            .into());
        }
        if task.status != TaskStatus::Blocked || task.blocked_reason.as_deref() != Some("awaiting_operator_answer") {
            return Err(OperatorQuestionError::Conflict {
                state: task.status.to_string(),
                answer: None,
            }
            .into());
        }
        let now = now_string();
        let dismissal = boss_protocol::OperatorQuestionDismissal {
            by: actor.into(),
            at: now.clone(),
            note: note.map(str::to_owned),
        };
        let changed = tx.execute(
            "UPDATE operator_questions SET status = 'withdrawn', withdrawn_reason = 'dismissed', dismissal_json = ?2
             WHERE id = ?1 AND status = 'open'",
            params![question.question.id, serde_json::to_string(&dismissal)?],
        )?;
        anyhow::ensure!(changed == 1, "question dismissal lost inside its transaction");
        let basis = crate::task_status_audit::compose_basis("operator_question_dismissed", provenance);
        crate::task_status_audit::execute(
            &tx,
            &basis,
            actor,
            "UPDATE tasks SET blocked_reason = 'question_dismissed', blocked_detail = ?2,
             autostart = 0, last_status_actor = ?3, updated_at = ?4 WHERE id = ?1",
            params![task.id, note, actor, now],
        )?;
        let mut updated = query_task(&tx, &task.id).require("task", &task.id)?;
        project_for_work_item_updated(&tx, &mut updated, "dismiss_operator_question");
        let mut pending = PendingEvents::new();
        stage_task_board_change(&mut pending, &tx, &task.id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(OperatorAnswerOutcome::unchanged(task_to_item(updated)))
    }
}
