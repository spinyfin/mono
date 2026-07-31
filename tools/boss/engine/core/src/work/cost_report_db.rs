//! Query layer behind `boss cost` — projects `work_runs` rows (joined
//! against their owning execution and task) into [`crate::cost_report::CostRunRecord`].
//! All grouping, ranking, and pricing happens in the pure functions over
//! there; this file's only job is the flat row fetch.

use super::*;

use crate::cost_report::CostRunRecord;

const COST_RECORD_COLUMNS: &str = "
    wr.id,
    wr.execution_id,
    we.work_item_id,
    t.name,
    we.kind,
    we.status,
    wr.status,
    CAST(wr.created_at AS INTEGER),
    t.short_id,
    wr.model,
    t.reasoning,
    t.effort_level,
    wr.input_tokens,
    wr.output_tokens,
    wr.cache_creation_tokens,
    wr.cache_read_tokens,
    wr.agent_active_ms,
    CAST(wr.started_at AS INTEGER)
";

fn map_cost_run_record(row: &Row) -> rusqlite::Result<CostRunRecord> {
    Ok(CostRunRecord {
        run_id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        work_item_name: row.get(3)?,
        execution_kind: row.get(4)?,
        execution_status: row.get(5)?,
        run_status: row.get(6)?,
        created_at_epoch_s: row.get(7)?,
        work_item_short_id: row.get(8)?,
        model: row.get(9)?,
        reasoning: row.get(10)?,
        effort_level: row.get(11)?,
        input_tokens: row.get(12)?,
        output_tokens: row.get(13)?,
        cache_creation_tokens: row.get(14)?,
        cache_read_tokens: row.get(15)?,
        agent_active_ms: row.get(16)?,
        started_at_epoch_s: row.get(17)?,
    })
}

impl WorkDb {
    /// Every `work_runs` row for `work_item_id`'s executions, oldest
    /// first. Backs `boss cost task <id>`.
    pub fn cost_records_for_work_item(&self, work_item_id: &str) -> Result<Vec<CostRunRecord>> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT {COST_RECORD_COLUMNS}
             FROM work_runs wr
             JOIN work_executions we ON we.id = wr.execution_id
             JOIN tasks t ON t.id = we.work_item_id
             WHERE we.work_item_id = ?1
             ORDER BY wr.created_at ASC, wr.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![work_item_id], map_cost_run_record)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every `work_runs` row created in `[since_epoch_s, until_epoch_s)`,
    /// oldest first. Backs both `boss cost window` (aggregated further by
    /// the caller) and `boss cost top` (ranked further by the caller).
    pub fn cost_records_for_window(&self, since_epoch_s: i64, until_epoch_s: i64) -> Result<Vec<CostRunRecord>> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT {COST_RECORD_COLUMNS}
             FROM work_runs wr
             JOIN work_executions we ON we.id = wr.execution_id
             JOIN tasks t ON t.id = we.work_item_id
             WHERE CAST(wr.created_at AS INTEGER) >= ?1 AND CAST(wr.created_at AS INTEGER) < ?2
             ORDER BY wr.created_at ASC, wr.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![since_epoch_s, until_epoch_s], map_cost_run_record)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
    use boss_protocol::CreateRunInput;

    fn create_run(db: &WorkDb, execution_id: &str, status: &str) -> String {
        db.create_run(
            CreateRunInput::builder()
                .agent_id("worker-1")
                .execution_id(execution_id)
                .status(status)
                .build(),
        )
        .unwrap()
        .id
    }

    fn set_tokens(db: &WorkDb, run_id: &str, input_tokens: i64, output_tokens: i64) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs
                 SET input_tokens = ?1, output_tokens = ?2, cache_creation_tokens = 0, cache_read_tokens = 0
                 WHERE id = ?3",
                params![input_tokens, output_tokens, run_id],
            )
            .unwrap();
    }

    #[test]
    fn cost_records_for_work_item_joins_task_and_execution_context() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "Fix cursor flicker");
        let execution = create_ready_chore_execution(&db, &task.id);

        let measured_run = create_run(&db, &execution.id, "completed");
        set_tokens(&db, &measured_run, 100, 50);
        // No tokens set — stays NULL, exercising the unmeasured path.
        create_run(&db, &execution.id, "orphaned");

        let records = db.cost_records_for_work_item(&task.id).expect("query succeeds");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].work_item_name, "Fix cursor flicker");
        assert_eq!(records[0].execution_kind, "chore_implementation");
        assert_eq!(records[0].input_tokens, Some(100));
        assert_eq!(records[1].input_tokens, None);
    }

    #[test]
    fn cost_records_for_window_filters_by_created_at() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "Fix cursor flicker");
        let execution = create_ready_chore_execution(&db, &task.id);
        let run_id = create_run(&db, &execution.id, "completed");
        set_tokens(&db, &run_id, 10, 0);

        let created_at: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT CAST(created_at AS INTEGER) FROM work_runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .unwrap();

        let inside = db
            .cost_records_for_window(created_at, created_at + 1)
            .expect("query succeeds");
        assert_eq!(inside.len(), 1);

        let outside = db
            .cost_records_for_window(created_at + 1, created_at + 100)
            .expect("query succeeds");
        assert!(outside.is_empty());
    }
}
