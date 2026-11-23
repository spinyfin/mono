//! Query layer behind `boss cost` — projects `work_runs` rows (joined
//! against their owning execution and task) into [`boss_engine_storage_values::CostRunRecord`].
//! All grouping, ranking, and pricing stays in `boss_engine::cost_report`;
//! this file's only job is the flat row fetch.
//!
//! [`WorkDb::usage_records_for_window`] is the same fetch widened for the
//! usage rows request: every execution is kept (a left join to `tasks`, so the
//! kinds that have no task row are not silently dropped), plus the launch
//! tuple, timestamps and chain roots that request groups by. NULL launch
//! tuples are then filled once per distinct execution from the same chain
//! `driver_transcript::resolve_execution_driver_slug` uses. The builders
//! live in `boss_usage_report`; this file still only fetches.

use super::*;

use boss_engine_storage_values::CostRunRecord;
use boss_protocol::DriverSource;
use boss_usage_report::{
    RecordedDriver, UsageExecutionRecord, UsageRunRecord, UsageSeriesRecords, UsageTaskRecord, UsageWindowRecords,
};

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
    CAST(wr.started_at AS INTEGER),
    we.driver,
    wr.usage_unavailable
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
    })
}

/// Migration id for [`USAGE_WINDOW_INDEX_SQL`].
pub(super) const USAGE_WINDOW_INDEX_MIGRATION_ID: &str = "20261123_1830_work_runs_created_at_epoch_idx";

/// Lets the window predicate below seek `work_runs` by creation time. Nothing
/// else indexes `work_runs.created_at`, so a window of a week would otherwise
/// read the whole table. The index is on the expression the predicate uses
/// (`created_at` is stored as epoch-second text) so SQLite can match them;
/// `IF NOT EXISTS` makes the migration a no-op when another change has
/// already added an equivalent index under this name.
pub(super) const USAGE_WINDOW_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS work_runs_created_at_epoch_idx ON work_runs(CAST(created_at AS INTEGER))";

pub(super) fn migrate_usage_window_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(USAGE_WINDOW_INDEX_SQL)?;
    Ok(())
}

/// Run, execution and launch-tuple columns of [`UsageRunRecord`], in field
/// order of [`map_usage_run_record`]. Timestamps are cast in the projection
/// only, so the `WHERE` below matches the index expression.
const USAGE_RUN_COLUMNS: &str = "
    wr.id,
    wr.status,
    CAST(wr.created_at AS INTEGER),
    wr.host_id,
    we.id,
    we.work_item_id,
    we.kind,
    we.status,
    CAST(we.created_at AS INTEGER),
    CAST(wr.started_at AS INTEGER),
    CAST(wr.finished_at AS INTEGER),
    wr.rounds,
    wr.agent_active_ms,
    wr.model,
    wr.input_tokens,
    wr.output_tokens,
    wr.cache_creation_tokens,
    wr.cache_creation_5m_tokens,
    wr.cache_creation_1h_tokens,
    wr.cache_read_tokens,
    wr.usage_unavailable,
    CAST(we.started_at AS INTEGER),
    CAST(we.finished_at AS INTEGER),
    we.pr_url,
    we.host_id,
    we.driver,
    we.model,
    we.effort_level
";

fn map_usage_run_record(row: &Row) -> rusqlite::Result<UsageRunRecord> {
    let driver = row
        .get::<_, Option<String>>(25)?
        .filter(|slug| !slug.is_empty())
        .map_or(RecordedDriver::Unknown, RecordedDriver::LaunchTuple);
    Ok(UsageRunRecord {
        run_id: row.get(0)?,
        run_status: row.get(1)?,
        run_created_at_epoch_s: row.get(2)?,
        run_host_id: row.get(3)?,
        execution_id: row.get(4)?,
        work_item_id: row.get(5)?,
        execution_kind: row.get(6)?,
        execution_status: row.get(7)?,
        execution_created_at_epoch_s: row.get(8)?,
        run_started_at_epoch_s: row.get(9)?,
        run_finished_at_epoch_s: row.get(10)?,
        rounds: row.get(11)?,
        agent_active_ms: row.get(12)?,
        run_model: row.get(13)?,
        input_tokens: row.get(14)?,
        output_tokens: row.get(15)?,
        cache_creation_tokens: row.get(16)?,
        cache_creation_5m_tokens: row.get(17)?,
        cache_creation_1h_tokens: row.get(18)?,
        cache_read_tokens: row.get(19)?,
        usage_unavailable: row.get(20)?,
        execution_started_at_epoch_s: row.get(21)?,
        execution_finished_at_epoch_s: row.get(22)?,
        execution_pr_url: row.get(23)?,
        execution_host_id: row.get(24)?,
        driver,
        execution_model: row.get(26)?,
        execution_effort: row.get(27)?,
    })
}

/// Fill a NULL launch tuple from [`WorkDb::get_execution_driver_slug`], once
/// per distinct execution. That is the same chain
/// `driver_transcript::resolve_execution_driver_slug` uses; calling it here
/// (after the snapshot is released) avoids a lookup per run and avoids
/// holding the pooled connection while that chain re-enters `WorkDb`.
/// An error on one execution (for example an unrecognised `kind` written by
/// another engine build) is logged and leaves that run's driver unknown so
/// one bad row never makes the whole window unreadable.
fn fill_resolved_drivers(db: &WorkDb, runs: &mut [UsageRunRecord]) {
    let mut resolved: HashMap<String, Option<String>> = HashMap::new();
    fill_resolved_drivers_with_cache(db, runs, &mut resolved);
}

fn fill_resolved_drivers_with_cache(
    db: &WorkDb,
    runs: &mut [UsageRunRecord],
    resolved: &mut HashMap<String, Option<String>>,
) {
    for run in runs {
        if run.driver.slug().is_some() {
            continue;
        }
        let slug = resolved.entry(run.execution_id.clone()).or_insert_with(|| {
            match db.get_execution_driver_slug(&run.execution_id) {
                Ok(slug) => slug.filter(|s| !s.is_empty()),
                Err(err) => {
                    tracing::warn!(
                        execution_id = %run.execution_id,
                        error = %format!("{err:#}"),
                        "failed to resolve driver for usage report execution; leaving unknown"
                    );
                    None
                }
            }
        });
        run.driver = match slug.as_deref() {
            Some(slug) => RecordedDriver::Resolved(slug.to_owned()),
            None => RecordedDriver::Unknown,
        };
    }
}

fn fill_resolved_execution_drivers_with_cache(
    db: &WorkDb,
    executions: &mut [UsageExecutionRecord],
    resolved: &mut HashMap<String, Option<String>>,
) {
    for exec in executions {
        if exec.driver.as_deref().is_some_and(|slug| !slug.is_empty()) {
            continue;
        }
        if !resolved.contains_key(&exec.execution_id) {
            let slug = match db.get_execution_driver_slug(&exec.execution_id) {
                Ok(slug) => slug.filter(|s| !s.is_empty()),
                Err(err) => {
                    tracing::warn!(
                        execution_id = %exec.execution_id,
                        error = %format!("{err:#}"),
                        "failed to resolve driver for usage report execution; leaving unknown"
                    );
                    None
                }
            };
            resolved.insert(exec.execution_id.clone(), slug);
        }
        match resolved.get(&exec.execution_id).and_then(|slug| slug.as_deref()) {
            Some(slug) => {
                exec.driver = Some(slug.to_owned());
                exec.driver_source = DriverSource::Resolved;
            }
            None => {
                exec.driver = None;
                exec.driver_source = DriverSource::Unknown;
            }
        }
    }
}

/// Execution columns of [`UsageExecutionRecord`] in field order of
/// [`map_usage_execution_record`].
const USAGE_EXECUTION_COLUMNS: &str = "
    we.id,
    we.work_item_id,
    we.kind,
    we.status,
    CAST(we.created_at AS INTEGER),
    CAST(we.finished_at AS INTEGER),
    CAST(we.started_at AS INTEGER),
    we.pr_url,
    we.host_id,
    we.driver,
    we.model,
    we.effort_level
";

fn map_usage_execution_record(row: &Row) -> rusqlite::Result<UsageExecutionRecord> {
    let driver: Option<String> = row.get(9)?;
    let driver = driver.filter(|slug| !slug.is_empty());
    let driver_source = if driver.is_some() {
        DriverSource::LaunchTuple
    } else {
        DriverSource::Unknown
    };
    Ok(UsageExecutionRecord {
        execution_id: row.get(0)?,
        work_item_id: row.get(1)?,
        execution_kind: row.get(2)?,
        execution_status: row.get(3)?,
        execution_created_at_epoch_s: row.get(4)?,
        execution_finished_at_epoch_s: row.get(5)?,
        execution_started_at_epoch_s: row.get(6)?,
        execution_pr_url: row.get(7)?,
        execution_host_id: row.get(8)?,
        driver,
        driver_source,
        execution_model: row.get(10)?,
        execution_effort: row.get(11)?,
    })
}

/// Lean task projection for the series builder: omits the per-task all-time
/// run-count subquery that only `boss cost rows` needs (`runs_outside_window`).
const USAGE_SERIES_TASK_COLUMNS: &str = "
    t.id,
    t.name,
    t.kind,
    t.status,
    t.product_id,
    t.short_id,
    p.name,
    t.project_id,
    pr.name,
    t.pr_url,
    CAST(t.completed_at AS INTEGER),
    t.reasoning,
    t.effort_level,
    0
";

/// Task columns of [`UsageTaskRecord`] in field order of [`map_usage_task_record`],
/// ending with the task's all-time run count.
const USAGE_TASK_COLUMNS: &str = "
    t.id,
    t.name,
    t.kind,
    t.status,
    t.product_id,
    t.short_id,
    p.name,
    t.project_id,
    pr.name,
    t.pr_url,
    CAST(t.completed_at AS INTEGER),
    t.reasoning,
    t.effort_level,
    (SELECT COUNT(*)
       FROM work_executions we
       JOIN work_runs wr ON wr.execution_id = we.id
      WHERE we.work_item_id = t.id)
";

fn map_usage_task_record(row: &Row, roots: &HashMap<String, String>) -> rusqlite::Result<UsageTaskRecord> {
    let task_id: String = row.get(0)?;
    let chain_root_id = roots.get(&task_id).cloned().unwrap_or_else(|| task_id.clone());
    Ok(UsageTaskRecord {
        name: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        product_id: row.get(4)?,
        chain_root_id,
        run_count_all_time: row.get(13)?,
        short_id: row.get(5)?,
        product_name: row.get(6)?,
        project_id: row.get(7)?,
        project_name: row.get(8)?,
        pr_url: row.get(9)?,
        completed_at_epoch_s: row.get(10)?,
        reasoning: row.get(11)?,
        effort_level: row.get(12)?,
        task_id,
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
                    0, 11, 12, 13, 14, 99, 'grok', NULL",
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
        .build();
    assert_eq!(record, expected);

    let absent = conn
        .query_row(
            "SELECT 'run-a', 'exec-a', 'task-a', 'Work name', 'task_implementation',
                    'completed', 'orphaned', 100, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL",
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

    /// Every `work_runs` row created in `[since_epoch_s, until_epoch_s)`
    /// with its execution, launch tuple and task context, for the usage rows
    /// request. Executions whose work item has no `tasks` row (answer agent,
    /// review guide, automation triage) are kept; the task list simply has no
    /// entry for them. The task list carries each referenced task plus its
    /// chain root, so a root with no run of its own in the window is available
    /// to nest its revisions under. Chain roots come from the same walk the
    /// rest of the engine uses. All of it is read in one snapshot. After the
    /// snapshot is released, executions whose launch tuple is NULL are
    /// resolved once each via [`Self::get_execution_driver_slug`].
    pub fn usage_records_for_window(&self, since_epoch_s: i64, until_epoch_s: i64) -> Result<UsageWindowRecords> {
        let mut runs;
        let tasks;
        {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
            let sql = format!(
                "SELECT {USAGE_RUN_COLUMNS}
                 FROM work_runs wr
                 JOIN work_executions we ON we.id = wr.execution_id
                 WHERE CAST(wr.created_at AS INTEGER) >= ?1 AND CAST(wr.created_at AS INTEGER) < ?2
                 ORDER BY CAST(wr.created_at AS INTEGER) ASC, wr.id ASC"
            );
            runs = tx
                .prepare(&sql)?
                .query_map(params![since_epoch_s, until_epoch_s], map_usage_run_record)?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut work_item_ids: Vec<&str> = runs.iter().map(|run| run.work_item_id.as_str()).collect();
            work_item_ids.sort_unstable();
            work_item_ids.dedup();
            let roots = chain_helpers::chain_roots(&tx, &work_item_ids)?;
            let mut task_ids: Vec<&str> = work_item_ids.clone();
            task_ids.extend(roots.values().map(String::as_str));
            task_ids.sort_unstable();
            task_ids.dedup();

            let sql = format!(
                "SELECT {USAGE_TASK_COLUMNS}
                 FROM tasks t
                 LEFT JOIN products p ON p.id = t.product_id
                 LEFT JOIN projects pr ON pr.id = t.project_id
                 WHERE t.id IN (SELECT value FROM json_each(?1))
                 ORDER BY t.id"
            );
            tasks = tx
                .prepare(&sql)?
                .query_map([query_ensure::id_array(&task_ids)], |row| {
                    map_usage_task_record(row, &roots)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            tx.commit()?;
        }
        fill_resolved_drivers(self, &mut runs);
        Ok(UsageWindowRecords { runs, tasks })
    }

    /// Everything the series builder needs for `[since_epoch_s, until_epoch_s)`,
    /// read in one snapshot:
    ///
    /// * every `work_runs` row created in `[since, until)` (seeking
    ///   `work_runs_created_at_epoch_idx`),
    /// * every `work_executions` row with `finished_at` in `[since, until)`
    ///   (for `executions_completed` and `distinct_prs`),
    /// * out-of-window runs of those finished executions (attribution only:
    ///   model, host, coverage for grouping/filters — not bucket measurement),
    /// * every chain-root task with `completed_at` in `[since, until)`, all of
    ///   its descendant revisions (including legacy nested chains), and any
    ///   out-of-window runs belonging to those chains so `tasks_completed_cost`
    ///   carries the full inclusive cost whenever spent, and
    /// * the task context (with resolved `chain_root_id`) for every referenced
    ///   task.
    ///
    /// After the snapshot is released, NULL launch-tuple drivers are resolved
    /// once per distinct execution across both runs and finished executions.
    pub fn usage_series_records_for_window(
        &self,
        since_epoch_s: i64,
        until_epoch_s: i64,
    ) -> Result<UsageSeriesRecords> {
        let mut runs;
        let mut finished_executions;
        let tasks;
        {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;

            let run_sql = format!(
                "SELECT {USAGE_RUN_COLUMNS}
                 FROM work_runs wr
                 JOIN work_executions we ON we.id = wr.execution_id
                 WHERE CAST(wr.created_at AS INTEGER) >= ?1 AND CAST(wr.created_at AS INTEGER) < ?2
                 ORDER BY CAST(wr.created_at AS INTEGER) ASC, wr.id ASC"
            );
            runs = tx
                .prepare(&run_sql)?
                .query_map(params![since_epoch_s, until_epoch_s], map_usage_run_record)?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let exec_sql = format!(
                "SELECT {USAGE_EXECUTION_COLUMNS}
                 FROM work_executions we
                 WHERE we.finished_at IS NOT NULL
                   AND CAST(we.finished_at AS INTEGER) >= ?1
                   AND CAST(we.finished_at AS INTEGER) < ?2
                 ORDER BY CAST(we.finished_at AS INTEGER) ASC, we.id ASC"
            );
            finished_executions = tx
                .prepare(&exec_sql)?
                .query_map(params![since_epoch_s, until_epoch_s], map_usage_execution_record)?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            // Runs of in-window finished executions that started before `since`
            // (or after `until`) so grouping by model/coverage/host uses the
            // observed latest run, not the launch alias. These are not added
            // to bucket measurement; the builder keeps measurement on
            // `wr.created_at` in `[since, until)`.
            if !finished_executions.is_empty() {
                let mut exec_ids: Vec<&str> = finished_executions
                    .iter()
                    .map(|exec| exec.execution_id.as_str())
                    .collect();
                exec_ids.sort_unstable();
                exec_ids.dedup();
                let extra_exec_run_sql = format!(
                    "SELECT {USAGE_RUN_COLUMNS}
                     FROM work_runs wr
                     JOIN work_executions we ON we.id = wr.execution_id
                     WHERE we.id IN (SELECT value FROM json_each(?1))
                       AND (CAST(wr.created_at AS INTEGER) < ?2 OR CAST(wr.created_at AS INTEGER) >= ?3)
                     ORDER BY CAST(wr.created_at AS INTEGER) ASC, wr.id ASC"
                );
                let extra_exec_runs = tx
                    .prepare(&extra_exec_run_sql)?
                    .query_map(
                        params![query_ensure::id_array(&exec_ids), since_epoch_s, until_epoch_s],
                        map_usage_run_record,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                runs.extend(extra_exec_runs);
            }

            // All chain-root tasks completed in [since, until) plus every
            // descendant revision in their chains. `UNION` deduplicates and
            // guards against cycles.
            let chain_members: Vec<(String, String)> = tx
                .prepare(
                    "WITH RECURSIVE chain(task_id, root_id) AS (
                         SELECT t.id, t.id
                         FROM tasks t
                         WHERE t.completed_at IS NOT NULL
                           AND CAST(t.completed_at AS INTEGER) >= ?1
                           AND CAST(t.completed_at AS INTEGER) < ?2
                           AND t.kind != 'revision'
                         UNION
                         SELECT r.id, c.root_id
                         FROM tasks r
                         JOIN chain c ON r.parent_task_id = c.task_id
                         WHERE r.kind = 'revision'
                     )
                     SELECT task_id, root_id FROM chain",
                )?
                .query_map(params![since_epoch_s, until_epoch_s], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            if !chain_members.is_empty() {
                let mut chain_task_ids: Vec<&str> = chain_members.iter().map(|(id, _)| id.as_str()).collect();
                chain_task_ids.sort_unstable();
                chain_task_ids.dedup();
                let extra_run_sql = format!(
                    "SELECT {USAGE_RUN_COLUMNS}
                     FROM work_runs wr
                     JOIN work_executions we ON we.id = wr.execution_id
                     WHERE we.work_item_id IN (SELECT value FROM json_each(?1))
                       AND (CAST(wr.created_at AS INTEGER) < ?2 OR CAST(wr.created_at AS INTEGER) >= ?3)
                     ORDER BY CAST(wr.created_at AS INTEGER) ASC, wr.id ASC"
                );
                let extra_runs = tx
                    .prepare(&extra_run_sql)?
                    .query_map(
                        params![query_ensure::id_array(&chain_task_ids), since_epoch_s, until_epoch_s],
                        map_usage_run_record,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                runs.extend(extra_runs);
            }

            let mut work_item_ids: Vec<&str> = runs
                .iter()
                .map(|run| run.work_item_id.as_str())
                .chain(finished_executions.iter().map(|exec| exec.work_item_id.as_str()))
                .chain(chain_members.iter().map(|(id, _)| id.as_str()))
                .chain(chain_members.iter().map(|(_, root)| root.as_str()))
                .collect();
            work_item_ids.sort_unstable();
            work_item_ids.dedup();

            let mut roots = chain_helpers::chain_roots(&tx, &work_item_ids)?;
            for (task_id, root_id) in &chain_members {
                roots.entry(task_id.clone()).or_insert_with(|| root_id.clone());
            }
            let mut task_ids: Vec<&str> = work_item_ids;
            task_ids.extend(roots.values().map(String::as_str));
            task_ids.sort_unstable();
            task_ids.dedup();

            let task_sql = format!(
                "SELECT {USAGE_SERIES_TASK_COLUMNS}
                 FROM tasks t
                 LEFT JOIN products p ON p.id = t.product_id
                 LEFT JOIN projects pr ON pr.id = t.project_id
                 WHERE t.id IN (SELECT value FROM json_each(?1))
                 ORDER BY t.id"
            );
            tasks = tx
                .prepare(&task_sql)?
                .query_map([query_ensure::id_array(&task_ids)], |row| {
                    map_usage_task_record(row, &roots)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            tx.commit()?;
        }
        let mut resolved: HashMap<String, Option<String>> = HashMap::new();
        fill_resolved_drivers_with_cache(self, &mut runs, &mut resolved);
        fill_resolved_execution_drivers_with_cache(self, &mut finished_executions, &mut resolved);
        Ok(UsageSeriesRecords {
            runs,
            finished_executions,
            tasks,
        })
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
    // ── usage rows projection ─────────────────────────────────────────────

    fn insert_task(conn: &Connection, id: &str, product_id: &str, kind: &str, parent: Option<&str>) {
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, ?3, ?1, 'done', '1', '1', ?4)",
            params![id, product_id, kind, parent],
        )
        .unwrap();
    }

    fn insert_run(conn: &Connection, run_id: &str, execution_id: &str, work_item_id: &str, kind: &str, created: i64) {
        insert_run_with_driver(conn, run_id, execution_id, work_item_id, kind, created, Some("claude"));
    }

    fn insert_run_with_driver(
        conn: &Connection,
        run_id: &str,
        execution_id: &str,
        work_item_id: &str,
        kind: &str,
        created: i64,
        driver: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, driver)
             VALUES (?1, ?2, ?3, 'completed', 'https://example.test/repo', ?4, ?5)",
            params![execution_id, work_item_id, kind, created.to_string(), driver],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens)
             VALUES (?1, ?2, 'worker', 'completed', ?3, 5, 7)",
            params![run_id, execution_id, created.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn usage_records_keep_taskless_executions_and_carry_every_chain_root() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "root", &product_id, "chore", None);
        insert_task(&conn, "rev_a", &product_id, "revision", Some("root"));
        // Legacy nesting: a revision parented to a revision still resolves to the root.
        insert_task(&conn, "rev_b", &product_id, "revision", Some("rev_a"));
        insert_run(
            &conn,
            "run_root_old",
            "exec_root_old",
            "root",
            "chore_implementation",
            100,
        );
        insert_run(
            &conn,
            "run_rev_b",
            "exec_rev_b",
            "rev_b",
            "revision_implementation",
            2_000,
        );
        insert_run(
            &conn,
            "run_guide",
            "exec_guide",
            "comparison_1",
            "pr_review_guide",
            2_001,
        );
        drop(conn);

        let records = db.usage_records_for_window(1_000, 3_000).unwrap();
        let run_ids: Vec<&str> = records.runs.iter().map(|run| run.run_id.as_str()).collect();
        assert_eq!(run_ids, ["run_rev_b", "run_guide"], "the old run is outside the window");

        let guide = records.runs.iter().find(|run| run.run_id == "run_guide").unwrap();
        assert_eq!(guide.work_item_id, "comparison_1");
        assert_eq!(guide.driver, RecordedDriver::LaunchTuple("claude".into()));
        assert_eq!(guide.input_tokens, Some(5));
        assert_eq!(guide.cache_creation_tokens, None, "an unset class stays NULL");

        let task_ids: Vec<&str> = records.tasks.iter().map(|task| task.task_id.as_str()).collect();
        assert_eq!(
            task_ids,
            ["rev_b", "root"],
            "the revision's task and its chain root, and nothing for the taskless execution"
        );
        let rev = records.tasks.iter().find(|task| task.task_id == "rev_b").unwrap();
        assert_eq!(rev.chain_root_id, "root");
        let root = records.tasks.iter().find(|task| task.task_id == "root").unwrap();
        assert_eq!(root.chain_root_id, "root");
        assert_eq!(
            root.run_count_all_time, 1,
            "the root's run is outside the window but counted"
        );
        assert_eq!(rev.run_count_all_time, 1);
    }

    #[test]
    fn a_launch_tuple_driver_wins_over_the_current_task_pin() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "t", &product_id, "chore", None);
        conn.execute("UPDATE tasks SET driver = 'codex' WHERE id = 't'", [])
            .unwrap();
        insert_run_with_driver(
            &conn,
            "run_launch",
            "exec_launch",
            "t",
            "chore_implementation",
            1_500,
            Some("claude"),
        );
        drop(conn);

        let records = db.usage_records_for_window(1_000, 2_000).unwrap();
        assert_eq!(records.runs.len(), 1);
        assert_eq!(records.runs[0].driver, RecordedDriver::LaunchTuple("claude".into()));
    }

    #[test]
    fn a_null_launch_tuple_resolves_from_the_task_pin() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "t", &product_id, "chore", None);
        conn.execute("UPDATE tasks SET driver = 'codex' WHERE id = 't'", [])
            .unwrap();
        insert_run_with_driver(
            &conn,
            "run_resolved",
            "exec_resolved",
            "t",
            "chore_implementation",
            1_500,
            None,
        );
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens)
             VALUES ('run_resolved_2', 'exec_resolved', 'worker', 'completed', '1600', 5, 7)",
            [],
        )
        .unwrap();
        drop(conn);

        let records = db.usage_records_for_window(1_000, 2_000).unwrap();
        assert_eq!(records.runs.len(), 2);
        for run in &records.runs {
            assert_eq!(run.driver, RecordedDriver::Resolved("codex".into()));
        }
    }

    #[test]
    fn a_null_launch_tuple_that_cannot_resolve_stays_unknown() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        insert_run_with_driver(
            &conn,
            "run_unknown",
            "exec_unknown",
            "not_a_task",
            "progression",
            1_500,
            None,
        );
        drop(conn);

        let records = db.usage_records_for_window(1_000, 2_000).unwrap();
        assert_eq!(records.runs.len(), 1);
        assert_eq!(records.runs[0].driver, RecordedDriver::Unknown);
    }

    #[test]
    fn an_unrecognised_execution_kind_with_null_driver_does_not_fail_the_window() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "t", &product_id, "chore", None);
        insert_run_with_driver(
            &conn,
            "run_task_unknown_kind",
            "exec_task_unknown_kind",
            "t",
            "kind_from_another_build",
            1_500,
            None,
        );
        insert_run_with_driver(
            &conn,
            "run_taskless_unknown_kind",
            "exec_taskless_unknown_kind",
            "not_a_task",
            "kind_from_another_build",
            1_501,
            None,
        );
        drop(conn);

        let records = db.usage_records_for_window(1_000, 2_000).unwrap();
        assert_eq!(records.runs.len(), 2);
        for run in &records.runs {
            assert_eq!(run.driver, RecordedDriver::Unknown);
        }
    }

    #[test]
    fn usage_window_is_half_open_on_run_creation() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "t", &product_id, "chore", None);
        insert_run(&conn, "at_since", "e1", "t", "chore_implementation", 1_000);
        insert_run(&conn, "at_until", "e2", "t", "chore_implementation", 2_000);
        drop(conn);
        let records = db.usage_records_for_window(1_000, 2_000).unwrap();
        assert_eq!(records.runs.len(), 1);
        assert_eq!(records.runs[0].run_id, "at_since");
    }

    #[test]
    fn the_window_predicate_seeks_the_created_at_index() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        let plan: String = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT {USAGE_RUN_COLUMNS}
                 FROM work_runs wr JOIN work_executions we ON we.id = wr.execution_id
                 WHERE CAST(wr.created_at AS INTEGER) >= 1 AND CAST(wr.created_at AS INTEGER) < 2"
            ))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("work_runs_created_at_epoch_idx"),
            "the window read must use the expression index, plan was:\n{plan}"
        );
    }

    #[test]
    fn a_database_without_the_created_at_index_gains_it_on_open() {
        let (dir, db) = open_db();
        let path = db.path().to_path_buf();
        {
            let conn = db.connect().unwrap();
            conn.execute_batch(&format!(
                "DROP INDEX work_runs_created_at_epoch_idx;
                 DELETE FROM schema_migrations WHERE id = '{USAGE_WINDOW_INDEX_MIGRATION_ID}';"
            ))
            .unwrap();
        }
        drop(db);
        let upgraded = crate::WorkDb::open(path).unwrap();
        let conn = upgraded.connect().unwrap();
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'work_runs_created_at_epoch_idx')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "opening an older database must add the index");
        drop(dir);
    }

    /// The seed fixture is the only way an isolated engine gets usage to show,
    /// so it must keep applying to the current schema and keep covering every
    /// case the usage rows request has to render.
    #[test]
    fn the_seed_fixture_applies_and_covers_every_driver_and_coverage_state() {
        use boss_protocol::{RunCoverage, UsageDimension};
        use boss_usage_report::{UsageRowsQuery, build_usage_rows_report, cause, caveat};

        let (_dir, db) = open_db();
        db.connect()
            .unwrap()
            .execute_batch(include_str!(env!("BOSS_USAGE_SEED_SQL")))
            .expect("the seed fixture applies to a freshly migrated database");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let records = db.usage_records_for_window(0, now + 86_400).unwrap();
        assert_eq!(records.runs.len(), 20);

        let query = |since: i64| {
            UsageRowsQuery::builder()
                .since_epoch_s(since)
                .until_epoch_s(now + 86_400)
                .generated_at_epoch_s(now)
                .capture_start_epoch_s(1_785_144_180)
                .retention_max_age_secs(14 * 86_400)
                .build()
        };
        let priced = |model: &str, _tokens: boss_usage_report::PricedTokens| model.contains("claude").then_some(1.0);
        let report = build_usage_rows_report(&query(0), &records, Some(&priced));

        let coverage = &report.coverage;
        assert_eq!(
            (
                coverage.runs_total,
                coverage.runs_complete,
                coverage.runs_partial,
                coverage.runs_zero,
                coverage.runs_unmeasured
            ),
            (20, 14, 2, 1, 3)
        );
        assert_eq!(
            report.taskless.len(),
            4,
            "answer agent, review guide, automation triage and an unresolvable progression"
        );
        let drivers = report
            .dimension_values
            .iter()
            .find(|d| d.dimension == UsageDimension::Driver)
            .unwrap();
        let driver_values: Vec<&str> = drivers.values.iter().map(|v| v.value.as_str()).collect();
        for expected in ["claude", "codex", "grok", "agy", "(none)"] {
            assert!(
                driver_values.contains(&expected),
                "{expected} missing from {driver_values:?}"
            );
        }
        let count = |code: &str| coverage.by_caveat.iter().find(|c| c.code == code).map_or(0, |c| c.runs);
        for code in [
            caveat::PRE_CAPTURE,
            caveat::DRIVER_NOT_RECORDED,
            caveat::GROSS_INPUT,
            caveat::SUBAGENTS_EXCLUDED,
            caveat::SYNTHETIC_MODEL,
            caveat::MODEL_FROM_LAUNCH_TUPLE,
            caveat::TOKENS_PARTIAL,
            caveat::USAGE_UNAVAILABLE,
        ] {
            assert!(count(code) > 0, "the seed has no run with caveat {code}");
        }
        assert!(report.spans_capture_boundary);
        assert!(report.overall.runs_unmeasured == 3 && report.overall.runs_zero == 1);

        // Grok and Antigravity: measured but partial, and never priced even
        // though the Antigravity model name is one an API table could match.
        let all_runs: Vec<_> = report
            .tasks
            .iter()
            .flat_map(|t| std::iter::once(t).chain(t.revisions.iter()))
            .flat_map(|t| t.executions.iter())
            .flat_map(|e| e.runs.iter().map(move |r| (e.driver.as_deref(), r)))
            .collect();
        for driver in ["grok", "agy"] {
            let (_, run) = all_runs
                .iter()
                .find(|(d, r)| *d == Some(driver) && r.coverage == RunCoverage::Partial)
                .unwrap_or_else(|| panic!("a partial {driver} run"));
            assert_eq!(run.tokens.as_ref().unwrap().cache_creation, None);
            assert_eq!(run.agent_active_ms, None);
            assert_eq!(run.unpriceable_cause.as_deref(), Some(cause::SUBSCRIPTION_UNPRICED));
        }

        // A recent window: the chain root whose own run is 40 days old is
        // still emitted to hold its recent revision.
        let recent = build_usage_rows_report(&query(now - 7 * 86_400), &records, None);
        let ghost = recent
            .tasks
            .iter()
            .find(|t| t.task_id == "task_seed_c_ghost_root")
            .unwrap();
        assert_eq!(ghost.direct.runs_total, 0);
        assert_eq!(ghost.runs_outside_window, 1);
        assert_eq!(ghost.revisions.len(), 1);
        assert!(ghost.inclusive.as_ref().unwrap().total_tokens > 0);
    }

    #[test]
    fn usage_series_records_pull_out_of_window_chain_runs_for_tasks_completed_in_window() {
        use boss_protocol::UsageBucketWidth;
        use boss_usage_report::{UsageSeriesQuery, build_usage_series_report, note};

        let (_dir, db) = open_db();
        db.connect()
            .unwrap()
            .execute_batch(include_str!(env!("BOSS_USAGE_SEED_SQL")))
            .expect("the seed fixture applies to a freshly migrated database");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();

        // 7-day window: task_seed_c_ghost_root completed 10h ago, while its own
        // run (`run_seed_c0_1`) was 40 days ago (outside the 7d window!).
        let since = now - 7 * 86_400;
        let until = now + 86_400;
        let records = db.usage_series_records_for_window(since, until).unwrap();
        assert!(
            records.runs.iter().any(|r| r.run_id == "run_seed_c0_1"),
            "out-of-window root run must be pulled for attributed task cost"
        );
        let q = UsageSeriesQuery::builder()
            .since_epoch_s(since)
            .until_epoch_s(until)
            .generated_at_epoch_s(now)
            .capture_start_epoch_s(1_785_144_180)
            .completed_at_start_epoch_s(1_782_805_232)
            .retention_max_age_secs(14 * 86_400)
            .bucket(UsageBucketWidth::Week)
            .build();
        let report = build_usage_series_report(&q, &records, None);
        assert!(!report.buckets.is_empty());
        // Grok + Antigravity both lack agent_active_ms in the seed fixture -> 2 excluded.
        assert_eq!(report.coverage.runs_excluded_from_active_hour, 2);
        assert!(
            report
                .coverage
                .notes
                .iter()
                .any(|n| n.code == note::ACTIVE_TIME_MISSING)
        );
        let total_completed_task_runs: u32 = report
            .buckets
            .iter()
            .flat_map(|b| b.cells.iter())
            .map(|c| c.tasks_completed_cost.runs_total)
            .sum();
        let total_window_runs: u32 = report
            .buckets
            .iter()
            .flat_map(|b| b.cells.iter())
            .map(|c| c.measurement.runs_total)
            .sum();
        // Window has 18 runs (all 20 minus pre-capture h1 and 40d-old c0).
        // Completed tasks in 7d include ghost_root (which pulls in c0!).
        assert_eq!(total_window_runs, 18);
        assert!(
            total_completed_task_runs > 0,
            "completed tasks in 7d must carry attributed chain runs"
        );
        // Specifically verify ghost_root's out-of-window run (750,800 tokens) is
        // included in tasks_completed_cost across the 7d window.
        let completed_tasks_total: u32 = report
            .buckets
            .iter()
            .flat_map(|b| b.cells.iter())
            .map(|c| c.tasks_completed)
            .sum();
        // 13 seed tasks total: 3 are revisions (b_rev1, b_rev2, c_ghost_rev),
        // 1 completed pre-capture (h_precapture) => 9 completed chain-root tasks in 7d.
        assert_eq!(completed_tasks_total, 9);
    }

    #[test]
    fn usage_series_loads_pre_window_runs_of_in_window_finished_executions() {
        use boss_protocol::{UsageBucketWidth, UsageDimension, UsageFilter};
        use boss_usage_report::{UsageSeriesQuery, build_usage_series_report};

        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        insert_task(&conn, "t_open", &product_id, "chore", None);
        conn.execute(
            "UPDATE tasks SET status = 'active', completed_at = NULL WHERE id = 't_open'",
            [],
        )
        .unwrap();
        insert_run_with_driver(
            &conn,
            "r_early",
            "e_late",
            "t_open",
            "chore_implementation",
            1_000,
            Some("claude"),
        );
        conn.execute(
            "UPDATE work_executions SET finished_at = '5000', model = 'opus' WHERE id = 'e_late'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_runs SET model = 'claude-sonnet-4', input_tokens = 40, output_tokens = 10,
                 cache_creation_tokens = 0, cache_read_tokens = 0 WHERE id = 'r_early'",
            [],
        )
        .unwrap();
        drop(conn);

        let records = db.usage_series_records_for_window(4_000, 6_000).unwrap();
        assert!(
            records
                .runs
                .iter()
                .any(|r| r.run_id == "r_early" && r.run_model.as_deref() == Some("claude-sonnet-4")),
            "the execution's only run predates `since` and must still be loaded for grouping"
        );
        assert_eq!(records.finished_executions.len(), 1);

        let q = UsageSeriesQuery::builder()
            .since_epoch_s(4_000)
            .until_epoch_s(6_000)
            .generated_at_epoch_s(6_000)
            .capture_start_epoch_s(0)
            .completed_at_start_epoch_s(0)
            .retention_max_age_secs(14 * 86_400)
            .bucket(UsageBucketWidth::Hour)
            .group_by(UsageDimension::Model)
            .filters(vec![
                UsageFilter {
                    dimension: UsageDimension::Model,
                    values: vec!["claude-sonnet-4".into()],
                },
                UsageFilter {
                    dimension: UsageDimension::Coverage,
                    values: vec!["measured".into()],
                },
            ])
            .build();
        let report = build_usage_series_report(&q, &records, None);
        assert_eq!(report.groups, vec!["claude-sonnet-4"]);
        assert_eq!(report.buckets[0].cells[0].executions_completed, 1);
        assert_eq!(
            report.buckets[0].cells[0].measurement.runs_total, 0,
            "the pre-window run is attribution-only"
        );
    }
}
