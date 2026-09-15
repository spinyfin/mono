use super::*;

impl WorkDb {
    /// Recheck successful completion under the same write lock as recovery.
    /// Explicit operator dispatch continues to use request_execution.
    pub fn request_orphan_recovery<F: FnOnce(&str) -> bool>(
        &self,
        input: RequestExecutionInput,
        is_live: F,
    ) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        ensure_dispatch_repo_resolvable(&mut conn, &input.work_item_id)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if query_latest_execution_for_work_item(&tx, &input.work_item_id)?
            .is_some_and(|execution| execution.status == ExecutionStatus::Completed)
        {
            return Ok(None);
        }
        let mut pending = PendingEvents::new();
        let execution = request_execution_in_tx_with_live_check(&mut pending, &tx, input, is_live)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(Some(execution))
    }

    /// Persist the asynchronous completion snapshot without reopening the run.
    pub fn record_completion_head(&self, execution_id: &str, head: Option<&str>) -> Result<()> {
        if head.is_some_and(str::is_empty) {
            bail!("completion head must not be empty");
        }
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE work_executions SET pr_head_after = ?2,
             pr_head_after_capture = CASE WHEN ?2 IS NULL THEN 'unavailable' ELSE 'recorded' END
             WHERE id = ?1",
            params![execution_id, head],
        )?;
        if changed != 1 {
            bail!("completion head: execution {execution_id} is missing");
        }
        Ok(())
    }

    /// Automated review's active-row state, distinct from GitHub's human
    /// review gate on in_review rows. Admission retries consume this field.
    pub fn record_review_admission_wait(&self, task_id: &str, waiting: bool) -> Result<()> {
        let conn = self.connect()?;
        record_review_admission_wait_in(&conn, task_id, waiting)
    }
}

pub(super) fn record_review_admission_wait_in(conn: &Connection, task_id: &str, waiting: bool) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET review_required_state = ?2
         WHERE id = ?1 AND status = 'active' AND deleted_at IS NULL",
        params![
            task_id,
            if waiting {
                "awaiting_admission"
            } else {
                "automated_review"
            }
        ],
    )?;
    Ok(())
}
