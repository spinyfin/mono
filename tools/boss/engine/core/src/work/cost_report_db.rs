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

#[work_db_boundary::instrument]
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

mod sol_report_db {
    //! Fetch complete PR families in a consistent snapshot for the Sol comparison.
    use super::{COST_RECORD_COLUMNS, map_cost_run_record};
    use crate::cost_report::sol::SolPrRecord;
    use crate::work::*;

    #[work_db_boundary::instrument]
    impl WorkDb {
        pub fn sol_comparison_records(&self) -> Result<Vec<SolPrRecord>> {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
            let family = chain_helpers::cycle_root_walk_cte("1=1");
            let sql = format!(
                "{family}
            SELECT roots.cycle_root_id, d.arm, CAST(COALESCE(e.started_at,e.created_at) AS INTEGER)
            FROM execution_sol_decisions d
            JOIN work_executions e ON e.id=d.execution_id
            JOIN roots ON roots.task_id=e.work_item_id
            JOIN tasks root ON root.id=roots.cycle_root_id
            WHERE d.eligible=1 AND e.model IS NOT NULL AND root.pr_url IS NOT NULL
                ORDER BY CAST(COALESCE(e.started_at,e.created_at) AS INTEGER), d.rowid"
            );
            let enrolled = tx
                .prepare(&sql)?
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // Only verdicts the engine acted on count; dropped-duplicate and
            // stale-head passes repeat findings already counted or superseded.
            let accepted = review_verdicts::INFORMATIVE_GATE_OUTCOMES
                .iter()
                .map(|outcome| format!("'{outcome}'"))
                .collect::<Vec<_>>()
                .join(",");
            let mut seen = std::collections::HashSet::new();
            let mut records = Vec::new();
            for (root, arm, enrolled_at) in enrolled {
                if !seen.insert(root.clone()) {
                    continue;
                }
                let family = driver_allocation::sol_selection::family_cte();
                let (ci, merged_at): (Option<String>, Option<String>) = tx
                    .query_row(
                        "SELECT first_ci,merged_at FROM sol_pr_outcomes WHERE work_item_id=?1",
                        [&root],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?
                    .unwrap_or_default();
                let revisions: u64 = tx.query_row(
                    &format!(
                        "{family}
                SELECT COUNT(*) FROM roots JOIN tasks t ON t.id=roots.task_id
                WHERE roots.cycle_root_id=?1 AND t.kind='revision'
                  AND EXISTS (SELECT 1 FROM work_executions e WHERE e.work_item_id=t.id AND e.model IS NOT NULL
                    AND CAST(COALESCE(e.started_at,e.created_at) AS INTEGER)>=?3
                    AND (?2 IS NULL OR CAST(COALESCE(e.started_at,e.created_at) AS INTEGER)<=CAST(?2 AS INTEGER)))"
                    ),
                    params![root, merged_at, enrolled_at],
                    |r| r.get(0),
                )?;
                let findings: u64 = tx.query_row(
                    &format!(
                        "{family}
                SELECT COALESCE(SUM(v.findings_count),0) FROM pr_review_verdicts v
                JOIN roots ON roots.task_id=v.work_item_id WHERE roots.cycle_root_id=?1
                AND v.gate_outcome IN ({accepted})
                AND CAST(v.created_at AS INTEGER)>=?3
                AND (?2 IS NULL OR CAST(v.created_at AS INTEGER)<=CAST(?2 AS INTEGER))"
                    ),
                    params![root, merged_at, enrolled_at],
                    |r| r.get(0),
                )?;
                let runs = tx
                    .prepare(&format!(
                        "{family}
                SELECT {COST_RECORD_COLUMNS} FROM work_runs wr
                JOIN work_executions we ON we.id=wr.execution_id
                JOIN tasks t ON t.id=we.work_item_id
                JOIN roots ON roots.task_id=t.id
                WHERE roots.cycle_root_id=?1
                AND CAST(wr.created_at AS INTEGER)>=?3
                AND (?2 IS NULL OR CAST(wr.created_at AS INTEGER)<=CAST(?2 AS INTEGER))"
                    ))?
                    .query_map(params![root, merged_at, enrolled_at], map_cost_run_record)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                records.push(SolPrRecord {
                    arm,
                    merged: merged_at.is_some(),
                    revisions,
                    findings,
                    runs,
                    first_ci: ci.as_deref().map(|state| state == "success"),
                });
            }
            tx.commit()?;
            Ok(records)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
        use boss_engine_effort::sol::{ASTRA_MODEL, SOL_MODEL};

        fn measured_run(db: &WorkDb, execution: &WorkExecution, model: &str, tokens: i64, time: i64) {
            db.record_execution_launch_config(&execution.id, "codex", model, None)
                .unwrap();
            let run = db
                .create_run(
                    boss_protocol::CreateRunInput::builder()
                        .execution_id(&execution.id)
                        .agent_id("worker")
                        .status("completed")
                        .build(),
                )
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET created_at=?2,started_at=?2,status='completed' WHERE id=?1",
                params![execution.id, time.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE work_runs SET model=?2,input_tokens=?3,output_tokens=0,created_at=?4 WHERE id=?1",
                params![run.id, model, tokens, time.to_string()],
            )
            .unwrap();
        }

        #[test]
        fn report_joins_revisions_retries_and_findings_once_and_observes_only_first_ci() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let task = create_test_chore(&db, &product, "root");
            let first = create_ready_chore_execution(&db, &task.id);
            db.set_codex_sol_percentage(100).unwrap();
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET pr_head_sha='stale' WHERE id=?1", [&task.id])
                .unwrap();
            assert_eq!(
                db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap(),
                SOL_MODEL
            );
            measured_run(&db, &first, SOL_MODEL, 100, 10);
            let historical = create_ready_chore_execution(&db, &task.id);
            db.record_execution_launch_config(&historical.id, "codex", ASTRA_MODEL, None)
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET created_at='5',started_at='5',status='completed' WHERE id=?1",
                [&historical.id],
            )
            .unwrap();
            conn.execute(
                "UPDATE tasks SET pr_url='https://github.com/org/repo/pull/1',status='in_review' WHERE id=?1",
                [&task.id],
            )
            .unwrap();
            // A stale poll from before enrollment must not enter the experiment.
            driver_allocation::sol_selection::observe_ci(&conn, &task.id, Some("stale"), "success").unwrap();
            driver_allocation::sol_selection::observe_ci(&conn, &task.id, Some("first"), "in_progress").unwrap();
            driver_allocation::sol_selection::observe_ci(&conn, &task.id, Some("first"), "fail").unwrap();
            driver_allocation::sol_selection::observe_ci(&conn, &task.id, Some("first"), "success").unwrap();
            conn.execute("UPDATE work_executions SET status='failed' WHERE id=?1", [&first.id])
                .unwrap();
            drop(conn);
            let retry = create_ready_chore_execution(&db, &task.id);
            assert_eq!(
                db.select_codex_sol_model(&retry.id, true, ASTRA_MODEL).unwrap(),
                ASTRA_MODEL
            );
            measured_run(&db, &retry, ASTRA_MODEL, 200, 20);
            // Queue creation order and random execution IDs must not move a
            // PR to the retry's arm when both launches have the same second.
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET created_at='1',started_at='10' WHERE id=?1",
                    [&retry.id],
                )
                .unwrap();
            let revision = create_test_chore(&db, &product, "revision");
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
                    params![revision.id, task.id],
                )
                .unwrap();
            let revision_execution = create_ready_chore_execution(&db, &revision.id);
            db.select_codex_sol_model(&revision_execution.id, true, ASTRA_MODEL)
                .unwrap();
            measured_run(&db, &revision_execution, SOL_MODEL, 300, 30);
            for (id, findings, outcome) in [
                (
                    "verdict",
                    3,
                    review_verdicts::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
                ),
                (
                    "duplicate",
                    5,
                    review_verdicts::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD,
                ),
                ("stale", 4, review_verdicts::REVIEW_GATE_OUTCOME_STALE_HEAD),
            ] {
                db.connect()
                    .unwrap()
                    .execute(
                        "INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,findings_count,gate_outcome,created_at)
                        VALUES (?1,?2,?3,?4,?5,'25')",
                        params![id, retry.id, task.id, findings, outcome],
                    )
                    .unwrap();
            }
            db.mark_chore_pr_merged(&task.id, "https://github.com/org/repo/pull/1")
                .unwrap();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE sol_pr_outcomes SET merged_at='40' WHERE work_item_id=?1",
                    [&task.id],
                )
                .unwrap();
            let records = db.sol_comparison_records().unwrap();
            assert_eq!(records.len(), 1);
            let report = crate::cost_report::sol::build_sol_report(&records);
            let sol = &report.arms[0];
            assert_eq!(sol.prs, 1);
            assert_eq!(sol.first_attempt_ci_passes, 0);
            assert_eq!(sol.ci_observed_prs, 1);
            assert_eq!(sol.revisions_before_merge, 1);
            assert_eq!(sol.review_findings, 3);
            assert_eq!(sol.tokens_per_merged_pr, Some(600.0));
            assert_eq!(report.arms[1].prs, 0);
        }

        #[test]
        fn family_enrolled_through_a_revision_is_not_charged_earlier_ineligible_work() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            db.set_codex_sol_percentage(100).unwrap();
            let root = create_test_chore(&db, &product, "medium root");
            let original = create_ready_chore_execution(&db, &root.id);
            db.select_codex_sol_model(&original.id, false, ASTRA_MODEL).unwrap();
            measured_run(&db, &original, ASTRA_MODEL, 1000, 5);
            let fix = create_test_chore(&db, &product, "ci fix");
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
                params![fix.id, root.id],
            )
            .unwrap();
            conn.execute(
                "UPDATE tasks SET pr_url='https://github.com/org/repo/pull/9' WHERE id=?1",
                [&root.id],
            )
            .unwrap();
            drop(conn);
            let fix_execution = create_ready_chore_execution(&db, &fix.id);
            assert_eq!(
                db.select_codex_sol_model(&fix_execution.id, true, ASTRA_MODEL).unwrap(),
                SOL_MODEL
            );
            measured_run(&db, &fix_execution, SOL_MODEL, 50, 20);
            let records = db.sol_comparison_records().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].arm, "sol");
            assert_eq!(records[0].revisions, 1);
            assert_eq!(records[0].runs.len(), 1);
            assert_eq!(records[0].runs[0].input_tokens, Some(50));
        }

        #[test]
        fn control_cohort_and_historical_or_unlaunched_executions_are_distinct() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            db.set_codex_sol_percentage(0).unwrap();
            for (index, eligible, launched) in [(0, true, true), (1, false, true), (2, true, false)] {
                let task = create_test_chore(&db, &product, format!("item {index}"));
                let execution = create_ready_chore_execution(&db, &task.id);
                db.select_codex_sol_model(&execution.id, eligible, ASTRA_MODEL).unwrap();
                if launched {
                    measured_run(&db, &execution, ASTRA_MODEL, 50, 10);
                }
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET pr_url=?2 WHERE id=?1",
                        params![task.id, format!("https://github.com/org/repo/pull/{index}")],
                    )
                    .unwrap();
            }
            let records = db.sol_comparison_records().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].arm, "astra");
            assert_eq!(records[0].first_ci, None);
            assert!(!records[0].merged);
        }
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
