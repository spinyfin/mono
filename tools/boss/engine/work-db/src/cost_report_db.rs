//! Query layer behind `boss cost` — projects `work_runs` rows (joined
//! against their owning execution and task) into [`boss_engine_storage_values::CostRunRecord`].
//! All grouping, ranking, and pricing stays in `boss_engine::cost_report`;
//! this file's only job is the flat row fetch.

use super::*;

use boss_engine_storage_values::CostRunRecord;

const COST_RECORD_COLUMNS: &str = "
    wr.id,
    wr.execution_id,
    we.work_item_id,
    COALESCE(t.name, we.work_item_id),
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
    CAST(wr.started_at AS INTEGER),
    we.driver,
    wr.usage_unavailable,
    CAST(wr.finished_at AS INTEGER)
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
        driver: row.get(18)?,
        usage_unavailable: row.get(19)?,
        finished_at_epoch_s: row.get(20)?,
    })
}

#[cfg(test)]
#[test]
fn cost_row_mapping_preserves_every_owned_field_and_null() {
    let conn = Connection::open_in_memory().unwrap();
    let record: boss_engine_storage_values::CostRunRecord = conn
        .query_row(
            "SELECT 'run-a', 'exec-a', 'task-a', 'Work name', 'task_implementation',
                    'completed', 'orphaned', 100, 42, 'model-a', 'high', 'medium',
                    0, 11, 12, 13, 14, 99, 'grok', NULL, 120",
            [],
            map_cost_run_record,
        )
        .unwrap();
    let builder: boss_engine_storage_values::CostRunRecordBuilder =
        boss_engine_storage_values::CostRunRecord::builder();
    let expected = builder
        .run_id("run-a")
        .execution_id("exec-a")
        .work_item_id("task-a")
        .work_item_name("Work name")
        .execution_kind("task_implementation")
        .execution_status("completed")
        .run_status("orphaned")
        .created_at_epoch_s(100)
        .work_item_short_id(42)
        .model("model-a")
        .reasoning("high")
        .effort_level("medium")
        .input_tokens(0)
        .output_tokens(11)
        .cache_creation_tokens(12)
        .cache_read_tokens(13)
        .agent_active_ms(14)
        .started_at_epoch_s(99)
        .driver("grok")
        .finished_at_epoch_s(120)
        .build();
    assert_eq!(record, expected);

    let absent = conn
        .query_row(
            "SELECT 'run-a', 'exec-a', 'task-a', 'Work name', 'task_implementation',
                    'completed', 'orphaned', 100, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL",
            [],
            map_cost_run_record,
        )
        .unwrap();
    assert_eq!(
        absent,
        CostRunRecord::builder()
            .run_id("run-a")
            .execution_id("exec-a")
            .work_item_id("task-a")
            .work_item_name("Work name")
            .execution_kind("task_implementation")
            .execution_status("completed")
            .run_status("orphaned")
            .created_at_epoch_s(100)
            .build()
    );
}

/// Filter and ordering options for [`WorkDb::run_records`].
#[derive(Debug, Clone, Default, bon::Builder)]
pub struct RunQueryFilter<'a> {
    pub since_epoch_s: Option<i64>,
    pub until_epoch_s: Option<i64>,
    pub driver: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub status: Option<&'a str>,
    pub work_item_id: Option<&'a str>,
    pub limit: Option<u32>,
    #[builder(default)]
    pub newest_first: bool,
    /// When true, drop runs whose execution is not bound to a `tasks` row
    /// (`INNER JOIN`). `boss cost` sets this so window/top stay task-only;
    /// `boss runs list` leaves it off so answer-agent, automation-triage,
    /// and review-guide runs appear.
    #[builder(default)]
    pub require_task: bool,
}

fn compose_run_records_sql(filter: &RunQueryFilter<'_>) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(since) = filter.since_epoch_s {
        values.push(since.into());
        clauses.push(format!("CAST(wr.created_at AS INTEGER) >= ?{}", values.len()));
    }
    if let Some(until) = filter.until_epoch_s {
        values.push(until.into());
        clauses.push(format!("CAST(wr.created_at AS INTEGER) < ?{}", values.len()));
    }
    if let Some(driver) = filter.driver {
        values.push(driver.to_owned().into());
        clauses.push(format!("we.driver = ?{}", values.len()));
    }
    if let Some(kind) = filter.kind {
        values.push(kind.to_owned().into());
        clauses.push(format!("we.kind = ?{}", values.len()));
    }
    if let Some(status) = filter.status {
        values.push(status.to_owned().into());
        clauses.push(format!("wr.status = ?{}", values.len()));
    }
    if let Some(work_item_id) = filter.work_item_id {
        values.push(work_item_id.to_owned().into());
        clauses.push(format!("we.work_item_id = ?{}", values.len()));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let order_sql = if filter.newest_first {
        "ORDER BY CAST(wr.created_at AS INTEGER) DESC, wr.id DESC"
    } else {
        "ORDER BY CAST(wr.created_at AS INTEGER) ASC, wr.id ASC"
    };
    let limit_sql = if let Some(limit) = filter.limit {
        values.push(i64::from(limit).into());
        format!("LIMIT ?{}", values.len())
    } else {
        String::new()
    };
    let task_join = if filter.require_task {
        "JOIN tasks t ON t.id = we.work_item_id"
    } else {
        "LEFT JOIN tasks t ON t.id = we.work_item_id"
    };

    let sql = format!(
        "SELECT {COST_RECORD_COLUMNS}
         FROM work_runs wr
         JOIN work_executions we ON we.id = wr.execution_id
         {task_join}
         {where_sql}
         {order_sql}
         {limit_sql}"
    );
    (sql, values)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Shared query over `work_runs` joined with `work_executions` and
    /// optionally `tasks`. Backs `boss cost task`, `boss cost window`,
    /// `boss cost top`, and `boss runs list`.
    pub fn run_records(&self, filter: &RunQueryFilter<'_>) -> Result<Vec<CostRunRecord>> {
        let conn = self.connect()?;
        let (sql, values) = compose_run_records_sql(filter);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), map_cost_run_record)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every `work_runs` row for `work_item_id`'s executions, oldest
    /// first. Backs `boss cost task <id>`.
    pub fn cost_records_for_work_item(&self, work_item_id: &str) -> Result<Vec<CostRunRecord>> {
        self.run_records(&RunQueryFilter {
            work_item_id: Some(work_item_id),
            require_task: true,
            ..Default::default()
        })
    }

    /// Every `work_runs` row created in `[since_epoch_s, until_epoch_s)`,
    /// oldest first. Backs both `boss cost window` (aggregated further by
    /// the caller) and `boss cost top` (ranked further by the caller).
    /// Restricted to executions bound to a `tasks` row.
    pub fn cost_records_for_window(&self, since_epoch_s: i64, until_epoch_s: i64) -> Result<Vec<CostRunRecord>> {
        self.run_records(&RunQueryFilter {
            since_epoch_s: Some(since_epoch_s),
            until_epoch_s: Some(until_epoch_s),
            require_task: true,
            ..Default::default()
        })
    }
}

mod sol_report_db {
    //! Fetch complete PR families in a consistent snapshot for the Sol comparison.
    use super::{COST_RECORD_COLUMNS, map_cost_run_record};
    use crate::*;
    use boss_engine_storage_values::SolPrRecord;

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

    #[test]
    fn run_records_filters_by_driver_kind_status_and_orders_newest_first() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "Investigate driver failures");
        let exec_claude = create_ready_chore_execution(&db, &task.id);
        let exec_agy = create_ready_chore_execution(&db, &task.id);
        db.record_execution_launch_config(&exec_claude.id, "claude", "claude-sonnet-5", None)
            .unwrap();
        db.record_execution_launch_config(&exec_agy.id, "agy", "gemini-4-hydrogen-medium", None)
            .unwrap();

        let run_1 = create_run(&db, &exec_claude.id, "failed");
        let run_2 = create_run(&db, &exec_agy.id, "failed");
        let run_3 = create_run(&db, &exec_agy.id, "completed");
        set_tokens(&db, &run_3, 42, 8);
        db.connect()
            .unwrap()
            .execute_batch(&format!(
                "UPDATE work_runs SET created_at = '100', started_at = '101', finished_at = '105' WHERE id = '{run_1}';
                 UPDATE work_runs SET created_at = '200', started_at = '201', finished_at = '202' WHERE id = '{run_2}';
                 UPDATE work_runs SET created_at = '300', started_at = '301', finished_at = '310' WHERE id = '{run_3}';"
            ))
            .unwrap();

        let agy_failed = db
            .run_records(&RunQueryFilter {
                since_epoch_s: Some(100),
                until_epoch_s: Some(400),
                driver: Some("agy"),
                kind: Some("chore_implementation"),
                status: Some("failed"),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(agy_failed.len(), 1);
        assert_eq!(agy_failed[0].run_id, run_2);
        assert_eq!(agy_failed[0].driver.as_deref(), Some("agy"));
        assert_eq!(agy_failed[0].started_at_epoch_s, Some(201));
        assert_eq!(agy_failed[0].finished_at_epoch_s, Some(202));
        assert_eq!(agy_failed[0].input_tokens, None);

        let newest = db
            .run_records(&RunQueryFilter {
                since_epoch_s: Some(100),
                until_epoch_s: Some(400),
                limit: Some(2),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            newest.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            vec![run_3.as_str(), run_2.as_str()]
        );
    }

    #[test]
    fn window_query_uses_work_runs_created_at_index_and_stays_fast_for_6000_runs() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "Synthetic load test");
        let exec = create_ready_chore_execution(&db, &task.id);
        db.record_execution_launch_config(&exec.id, "codex", "gpt-5-codex", None)
            .unwrap();

        let conn = db.connect().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, finished_at)
                     VALUES (?1, ?2, 'worker-1', 'completed', ?3, ?3, ?4)",
                )
                .unwrap();
            let base_ts: i64 = 1_790_000_000;
            // Seed 12,000 runs across 28 days (~6,000 in the recent 14-day window).
            for i in 0..12_000i64 {
                let ts = base_ts + i * 200;
                let id = format!("run_perf_{i:05}");
                stmt.execute(params![id, exec.id, ts.to_string(), (ts + 15).to_string()])
                    .unwrap();
            }
        }
        conn.execute_batch("COMMIT; ANALYZE;").unwrap();

        let window_end = 1_790_000_000 + 12_000 * 200;
        let window_start = window_end - 14 * 86_400;

        let (sql, values) = compose_run_records_sql(&RunQueryFilter {
            since_epoch_s: Some(window_start),
            until_epoch_s: Some(window_end),
            newest_first: true,
            ..Default::default()
        });
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(rusqlite::params_from_iter(values.iter()), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let plan_text = plan.join(" | ");
        assert!(
            plan_text.contains("work_runs_created_at_idx"),
            "expected window query to use work_runs_created_at_idx, got: {plan_text}"
        );
        assert!(
            !plan_text.contains("USE TEMP B-TREE FOR ORDER BY"),
            "expected index order to avoid temp B-tree sort, got: {plan_text}"
        );
        drop(conn);

        let t0 = std::time::Instant::now();
        let rows = db
            .run_records(&RunQueryFilter {
                since_epoch_s: Some(window_start),
                until_epoch_s: Some(window_end),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        let elapsed = t0.elapsed();
        eprintln!(
            "work_runs 14-day window query: {} rows in {:.2}ms (plan: {plan_text})",
            rows.len(),
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(rows.len() >= 6_000);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "14-day window query took too long: {elapsed:?}"
        );
    }

    #[test]
    fn run_records_includes_unmeasured_runs_without_a_tasks_row() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "Ordinary task run");
        let task_exec = create_ready_chore_execution(&db, &task.id);
        let task_run = create_run(&db, &task_exec.id, "completed");
        set_tokens(&db, &task_run, 10, 2);

        let comment_id = "comment_owner_1";
        let auto_id = "auto_owner_1";
        let comparison_id = "cmp_owner_1";
        let answer_exec = db
            .create_answer_agent_execution(comment_id, "https://example.test/repo.git")
            .unwrap();
        let triage_exec = db
            .create_automation_triage_execution(auto_id, "https://example.test/repo.git")
            .unwrap();
        let guide_exec = db
            .create_pr_review_guide_execution(comparison_id, "https://example.test/repo.git")
            .unwrap();
        let answer_run = create_run(&db, &answer_exec.id, "failed");
        let triage_run = create_run(&db, &triage_exec.id, "failed");
        let guide_run = create_run(&db, &guide_exec.id, "orphaned");
        db.connect()
            .unwrap()
            .execute_batch(&format!(
                "UPDATE work_runs SET created_at = '100' WHERE id = '{task_run}';
                 UPDATE work_runs SET created_at = '200' WHERE id = '{answer_run}';
                 UPDATE work_runs SET created_at = '300' WHERE id = '{triage_run}';
                 UPDATE work_runs SET created_at = '400' WHERE id = '{guide_run}';"
            ))
            .unwrap();

        let listed = db
            .run_records(&RunQueryFilter {
                since_epoch_s: Some(50),
                until_epoch_s: Some(500),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            listed.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            vec![
                guide_run.as_str(),
                triage_run.as_str(),
                answer_run.as_str(),
                task_run.as_str()
            ]
        );

        let answer = listed.iter().find(|r| r.run_id == answer_run).unwrap();
        assert_eq!(answer.execution_kind, "answer_agent");
        assert_eq!(answer.work_item_id, comment_id);
        assert_eq!(answer.work_item_name, comment_id);
        assert_eq!(answer.work_item_short_id, None);
        assert_eq!(answer.input_tokens, None);
        assert_eq!(answer.run_status, "failed");

        let triage = listed.iter().find(|r| r.run_id == triage_run).unwrap();
        assert_eq!(triage.execution_kind, "automation_triage");
        assert_eq!(triage.work_item_id, auto_id);
        assert_eq!(triage.work_item_name, auto_id);
        assert_eq!(triage.work_item_short_id, None);

        let guide = listed.iter().find(|r| r.run_id == guide_run).unwrap();
        assert_eq!(guide.execution_kind, "pr_review_guide");
        assert_eq!(guide.work_item_id, comparison_id);
        assert_eq!(guide.work_item_name, comparison_id);
        assert_eq!(guide.work_item_short_id, None);

        let task_row = listed.iter().find(|r| r.run_id == task_run).unwrap();
        assert_eq!(task_row.work_item_name, "Ordinary task run");
        assert_eq!(task_row.work_item_short_id, task.short_id);

        let by_comment = db
            .run_records(&RunQueryFilter {
                work_item_id: Some(comment_id),
                newest_first: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_comment.len(), 1);
        assert_eq!(by_comment[0].run_id, answer_run);

        let cost_window = db.cost_records_for_window(50, 500).unwrap();
        assert_eq!(
            cost_window.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            vec![task_run.as_str()]
        );
    }
}
