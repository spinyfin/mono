//! Query layer behind `GetMetricSeries` / `GetMetricCatalog` — projects
//! requested-window `work_executions` and `tasks` rows into the fact types
//! the pure [`crate::metric_series`] module aggregates. SQL filters the
//! window (and, for a series, kind/status) as TEXT against 10-digit,
//! zero-padded epoch bounds so the `work_executions(finished_at, kind)`
//! index is usable; duration and product slug are computed in the
//! projection. The join to `tasks`/`products` is `LEFT`: a
//! `product_design` execution's `work_item_id` is a `prod_` id and an
//! `answer_agent` execution's is a `cmt_` comment id, neither of which
//! exists in `tasks`, so an inner join would silently drop those rows.

use super::*;

use rusqlite::params_from_iter;

use crate::metric_series::{ExecutionFact, TaskFact};
#[cfg(test)]
use crate::metric_series::{SeriesSource, SeriesSpec};

/// 10-digit, zero-padded epoch bound so lexicographic TEXT comparison
/// against `finished_at`/`completed_at` agrees with numeric comparison
/// regardless of the bound's own digit count (e.g. a pre-2001 `--since`).
fn epoch_bound(epoch_s: i64) -> String {
    format!("{:010}", epoch_s.max(0))
}

fn duration_ms(started_at: Option<i64>, finished_at: i64) -> Option<i64> {
    let started = started_at?;
    let delta = finished_at.checked_sub(started)?;
    if delta < 0 {
        return None;
    }
    delta.checked_mul(1_000)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|v| if v.is_empty() { None } else { Some(v) })
}

fn map_execution_fact(row: &Row) -> rusqlite::Result<ExecutionFact> {
    let finished_at_epoch_s: i64 = row.get(2)?;
    let started_at_epoch_s: Option<i64> = row.get(3)?;
    Ok(ExecutionFact {
        finished_at_epoch_s,
        kind: row.get(0)?,
        status: row.get(1)?,
        driver: nonempty(row.get(4)?),
        duration_ms: duration_ms(started_at_epoch_s, finished_at_epoch_s),
        effort_level: nonempty(row.get(6)?),
        model: nonempty(row.get(5)?),
        pr_url: nonempty(row.get(8)?),
        product: nonempty(row.get(9)?),
        repo: nonempty(row.get(7)?),
    })
}

fn map_task_fact(row: &Row) -> rusqlite::Result<TaskFact> {
    let completed_at_epoch_s: i64 = row.get(1)?;
    let created_at_epoch_s: i64 = row.get(2)?;
    Ok(TaskFact {
        completed_at_epoch_s,
        duration_ms: duration_ms(Some(created_at_epoch_s), completed_at_epoch_s).unwrap_or(0),
        kind: row.get(0)?,
        effort_level: nonempty(row.get(3)?),
        product: nonempty(row.get(6)?),
        reasoning: nonempty(row.get(4)?),
        repo: nonempty(row.get(5)?),
    })
}

impl WorkDb {
    /// Window-scoped execution facts for `[since_epoch_s, until_epoch_s)`.
    /// `kinds` / `statuses` are optional IN-list predicates; `require_pr_url`
    /// keeps only rows carrying a non-empty `pr_url`.
    pub fn metric_execution_facts(
        &self,
        since_epoch_s: i64,
        until_epoch_s: i64,
        kinds: Option<&[&str]>,
        statuses: Option<&[&str]>,
        require_pr_url: bool,
    ) -> Result<Vec<ExecutionFact>> {
        let conn = self.connect()?;
        let mut sql = String::from(
            "SELECT
                we.kind,
                we.status,
                CAST(we.finished_at AS INTEGER),
                CAST(we.started_at AS INTEGER),
                we.driver,
                we.model,
                we.effort_level,
                we.repo_remote_url,
                we.pr_url,
                COALESCE(p.slug, p2.slug)
             FROM work_executions we
             LEFT JOIN tasks t ON t.id = we.work_item_id
             LEFT JOIN products p ON p.id = t.product_id
             LEFT JOIN products p2 ON p2.id = we.work_item_id
             WHERE we.finished_at IS NOT NULL
               AND we.finished_at >= ?1
               AND we.finished_at < ?2",
        );
        let mut params: Vec<String> = vec![epoch_bound(since_epoch_s), epoch_bound(until_epoch_s)];
        if let Some(kinds) = kinds.filter(|k| !k.is_empty()) {
            let start = params.len() + 1;
            sql.push_str(" AND we.kind IN (");
            for (i, kind) in kinds.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{}", start + i));
                params.push((*kind).to_owned());
            }
            sql.push(')');
        }
        if let Some(statuses) = statuses.filter(|s| !s.is_empty()) {
            let start = params.len() + 1;
            sql.push_str(" AND we.status IN (");
            for (i, status) in statuses.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{}", start + i));
                params.push((*status).to_owned());
            }
            sql.push(')');
        }
        if require_pr_url {
            sql.push_str(" AND we.pr_url IS NOT NULL AND we.pr_url != ''");
        }
        sql.push_str(" ORDER BY we.finished_at ASC, we.id ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), map_execution_fact)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Window-scoped task facts for `task_lead_time`: tasks with
    /// `completed_at` in `[since_epoch_s, until_epoch_s)`.
    pub fn metric_task_facts(&self, since_epoch_s: i64, until_epoch_s: i64) -> Result<Vec<TaskFact>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT
                t.kind,
                CAST(t.completed_at AS INTEGER),
                CAST(t.created_at AS INTEGER),
                t.effort_level,
                t.reasoning,
                t.repo_remote_url,
                p.slug
             FROM tasks t
             JOIN products p ON p.id = t.product_id
             WHERE t.deleted_at IS NULL
               AND t.completed_at IS NOT NULL
               AND t.completed_at >= ?1
               AND t.completed_at < ?2
             ORDER BY t.completed_at ASC, t.id ASC",
        )?;
        let rows = stmt.query_map(
            params![epoch_bound(since_epoch_s), epoch_bound(until_epoch_s)],
            map_task_fact,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Project the requested window using a catalog series' source predicates.
    #[cfg(test)]
    pub(crate) fn metric_facts_for_spec(
        &self,
        spec: &SeriesSpec,
        since_epoch_s: i64,
        until_epoch_s: i64,
    ) -> Result<SeriesFacts> {
        match spec.source {
            SeriesSource::Executions {
                kinds,
                require_pr_url,
                statuses,
                ..
            } => Ok(SeriesFacts::Executions(self.metric_execution_facts(
                since_epoch_s,
                until_epoch_s,
                kinds,
                statuses,
                require_pr_url,
            )?)),
            SeriesSource::Tasks => Ok(SeriesFacts::Tasks(
                self.metric_task_facts(since_epoch_s, until_epoch_s)?,
            )),
        }
    }
}

/// Discriminated projection so tests can pick the matching builder.
#[cfg(test)]
pub(crate) enum SeriesFacts {
    Executions(Vec<ExecutionFact>),
    Tasks(Vec<TaskFact>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric_series::{
        SERIES_EXECUTION_DURATION, SERIES_EXECUTION_OUTCOMES, SERIES_PRS_GENERATED, SERIES_REVIEW_DURATION,
        SERIES_TASK_LEAD_TIME, series_spec,
    };
    use crate::test_support::{create_product, create_test_chore, open_db};
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};
    use std::time::Instant;

    /// 10-digit epoch so TEXT window comparisons match production rows.
    const T0: i64 = 1_780_000_000;

    fn stamp_execution(
        db: &WorkDb,
        work_item_id: &str,
        kind: ExecutionKind,
        status: ExecutionStatus,
        started_at: i64,
        finished_at: i64,
        pr_url: Option<&str>,
    ) -> String {
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .kind(kind)
                    .status(status)
                    .started_at(started_at.to_string())
                    .finished_at(finished_at.to_string())
                    .maybe_pr_url(pr_url.map(str::to_owned))
                    .build(),
            )
            .unwrap();
        execution.id
    }

    fn set_launch_config(db: &WorkDb, execution_id: &str, driver: &str, model: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET driver = ?1, model = ?2, effort_level = 'medium' WHERE id = ?3",
                params![driver, model, execution_id],
            )
            .unwrap();
    }

    #[test]
    fn execution_facts_filter_by_finished_at_window_as_text() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "windowed");
        let inside_at = 1_780_000_100_i64;
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Completed,
            inside_at - 30,
            inside_at,
            None,
        );
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Completed,
            1_780_000_500,
            1_780_000_600,
            None,
        );

        let inside = db
            .metric_execution_facts(1_780_000_000, 1_780_000_200, None, None, false)
            .unwrap();
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].finished_at_epoch_s, inside_at);
        assert_eq!(inside[0].duration_ms, Some(30_000));
        assert_eq!(inside[0].product.as_deref(), Some("test-product"));

        let outside = db
            .metric_execution_facts(1_780_000_200, 1_780_000_400, None, None, false)
            .unwrap();
        assert!(outside.is_empty());
    }

    #[test]
    fn execution_facts_match_a_sub_10_digit_since_bound_against_a_10_digit_row() {
        // A pre-2001-09-09 --since (e.g. 2001-01-01T00:00:00Z -> 978307200,
        // 9 digits) must still admit a 10-digit-epoch row: unpadded TEXT
        // comparison would put "978307200" > "1780000100" lexicographically
        // (leading '9' > '1'), excluding every real row.
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "nine-digit-bound");
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Completed,
            1_780_000_070,
            1_780_000_100,
            None,
        );

        let facts = db
            .metric_execution_facts(978_307_200, 1_790_000_000, None, None, false)
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].finished_at_epoch_s, 1_780_000_100);
    }

    #[test]
    fn execution_facts_honor_kind_status_and_pr_url_predicates() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "predicates");
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::PrReview,
            ExecutionStatus::Completed,
            T0 + 100,
            T0 + 130,
            None,
        );
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::PrReview,
            ExecutionStatus::Failed,
            T0 + 100,
            T0 + 140,
            None,
        );
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::ChoreImplementation,
            ExecutionStatus::Completed,
            T0 + 100,
            T0 + 150,
            Some("https://github.com/o/r/pull/1"),
        );

        let reviews = db
            .metric_execution_facts(T0, T0 + 1_000, Some(&["pr_review"]), Some(&["completed"]), false)
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].kind, "pr_review");

        let with_pr = db.metric_execution_facts(T0, T0 + 1_000, None, None, true).unwrap();
        assert_eq!(with_pr.len(), 1);
        assert_eq!(with_pr[0].pr_url.as_deref(), Some("https://github.com/o/r/pull/1"));
    }

    #[test]
    fn execution_facts_include_non_task_work_items_with_no_product() {
        // product_design executions carry the product's own `prod_` id as
        // work_item_id, and answer_agent executions carry a `cmt_` comment
        // id; neither exists in `tasks`, so an inner join would drop them.
        // Inserted directly (rather than via `create_execution`, whose
        // repo-remote-url resolution expects a task/chore work item) since
        // the whole point is that these rows have no corresponding task.
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url,
                created_at, started_at, finished_at, branch_naming
             ) VALUES ('exec_prod_design', ?1, 'product_design', 'completed',
                       'https://github.com/test/repo', ?2, ?2, ?3, '{}')",
            params![product_id, (T0 + 100).to_string(), (T0 + 130).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url,
                created_at, started_at, finished_at, branch_naming
             ) VALUES ('exec_answer_agent', 'cmt_does_not_exist', 'answer_agent', 'completed',
                       'https://github.com/test/repo', ?1, ?1, ?2, '{}')",
            params![(T0 + 100).to_string(), (T0 + 140).to_string()],
        )
        .unwrap();
        drop(conn);

        let facts = db.metric_execution_facts(T0, T0 + 1_000, None, None, false).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(
            facts
                .iter()
                .find(|f| f.kind == "product_design")
                .unwrap()
                .product
                .as_deref(),
            Some("test-product")
        );
        assert!(
            facts
                .iter()
                .find(|f| f.kind == "answer_agent")
                .unwrap()
                .product
                .is_none()
        );
    }

    #[test]
    fn execution_facts_project_launch_config_and_skip_negative_duration() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "launch");
        let id = stamp_execution(
            &db,
            &task.id,
            ExecutionKind::TaskImplementation,
            ExecutionStatus::Completed,
            T0 + 500,
            T0 + 400, // finished before started
            None,
        );
        set_launch_config(&db, &id, "claude", "opus");
        let facts = db.metric_execution_facts(T0, T0 + 1_000, None, None, false).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].driver.as_deref(), Some("claude"));
        assert_eq!(facts[0].model.as_deref(), Some("opus"));
        assert_eq!(facts[0].effort_level.as_deref(), Some("medium"));
        assert_eq!(facts[0].duration_ms, None);
    }

    #[test]
    fn task_facts_filter_by_completed_at_and_compute_lead_time() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "lead");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET created_at = '0000000100', completed_at = '0000000250' WHERE id = ?1",
                params![task.id],
            )
            .unwrap();

        let inside = db.metric_task_facts(200, 300).unwrap();
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].duration_ms, 150_000);
        assert_eq!(inside[0].kind, "chore");

        let outside = db.metric_task_facts(0, 200).unwrap();
        assert!(outside.is_empty());
    }

    #[test]
    fn spec_projection_selects_the_matching_source() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "spec");
        stamp_execution(
            &db,
            &task.id,
            ExecutionKind::PrReview,
            ExecutionStatus::Completed,
            T0 + 10,
            T0 + 40,
            None,
        );
        match db
            .metric_facts_for_spec(series_spec(SERIES_REVIEW_DURATION).unwrap(), T0, T0 + 100)
            .unwrap()
        {
            SeriesFacts::Executions(facts) => assert_eq!(facts.len(), 1),
            SeriesFacts::Tasks(_) => panic!("review_duration should project executions"),
        }
        match db
            .metric_facts_for_spec(series_spec(SERIES_TASK_LEAD_TIME).unwrap(), 0, i64::MAX)
            .unwrap()
        {
            SeriesFacts::Tasks(facts) => assert!(facts.len() <= 1),
            SeriesFacts::Executions(_) => panic!("task_lead_time should project tasks"),
        }
        // execution_duration ignores pr_review
        match db
            .metric_facts_for_spec(series_spec(SERIES_EXECUTION_DURATION).unwrap(), T0, T0 + 100)
            .unwrap()
        {
            SeriesFacts::Executions(facts) => assert!(facts.is_empty()),
            SeriesFacts::Tasks(_) => panic!("expected executions"),
        }
        match db
            .metric_facts_for_spec(series_spec(SERIES_EXECUTION_OUTCOMES).unwrap(), T0, T0 + 100)
            .unwrap()
        {
            SeriesFacts::Executions(facts) => assert_eq!(facts.len(), 1),
            SeriesFacts::Tasks(_) => panic!("expected executions"),
        }
        match db
            .metric_facts_for_spec(series_spec(SERIES_PRS_GENERATED).unwrap(), T0, T0 + 100)
            .unwrap()
        {
            SeriesFacts::Executions(facts) => assert!(facts.is_empty()),
            SeriesFacts::Tasks(_) => panic!("expected executions"),
        }
    }

    /// Guards median bucket/percentile aggregation latency for an
    /// 8,000-execution five-month window at day buckets. It intentionally
    /// does not time SQLite projection or assert a p95 full-path budget.
    #[test]
    fn median_query_latency_over_five_month_synthetic_dataset_stays_under_budget() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task = create_test_chore(&db, &product_id, "synth");
        let since = 1_746_316_800_i64; // 2026-04-01T00:00:00Z
        let until = since + 150 * 86_400; // ~5 months
        const N: i64 = 8_000;
        {
            let mut conn = db.connect().unwrap();
            let tx = conn.transaction().unwrap();
            for i in 0..N {
                let finished = since + (i * (until - since)) / N;
                let started = finished - 45;
                let kind = if i % 20 == 0 {
                    "pr_review"
                } else {
                    "chore_implementation"
                };
                let status = if i % 17 == 0 { "failed" } else { "completed" };
                let pr_url: Option<&str> = if i % 11 == 0 {
                    Some("https://github.com/o/r/pull/1")
                } else if i % 13 == 0 {
                    Some("https://github.com/o/r/pull/2")
                } else {
                    None
                };
                tx.execute(
                    "INSERT INTO work_executions (
                        id, work_item_id, kind, status, repo_remote_url,
                        created_at, started_at, finished_at, pr_url, driver, model,
                        branch_naming
                     ) VALUES (?1, ?2, ?3, ?4, 'https://github.com/test/repo',
                               ?5, ?6, ?7, ?8, 'claude', 'opus', '{}')",
                    params![
                        format!("exec_synth_{i}"),
                        task.id,
                        kind,
                        status,
                        started.to_string(),
                        started.to_string(),
                        finished.to_string(),
                        pr_url,
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        // Project the same requested lower bound used by the handler. The
        // timed loop below deliberately measures aggregation only.
        let spec = series_spec(SERIES_EXECUTION_OUTCOMES).unwrap();
        let SeriesFacts::Executions(rows) = db.metric_facts_for_spec(spec, since, until).unwrap() else {
            panic!("execution_outcomes must project executions");
        };
        assert!(
            rows.len() >= 7_000,
            "synthetic five-month set should be thousands of facts, got {}",
            rows.len()
        );
        let query = crate::metric_series::SeriesQuery {
            series: spec.id,
            since_epoch_s: since,
            until_epoch_s: until,
            bucket: Some(crate::metric_series::BucketWidth::Day),
            group_by: spec.default_group_by,
            filters: &[],
        };
        let warm = crate::metric_series::build_execution_series_report(&query, &rows, until).unwrap();
        assert_eq!(warm.bucket_secs, crate::metric_series::BucketWidth::DAY_SECS);
        assert!(!warm.buckets.is_empty());

        let mut samples_ms: Vec<u128> = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            let report = crate::metric_series::build_execution_series_report(&query, &rows, until).unwrap();
            std::hint::black_box(report.buckets.len());
            samples_ms.push(started.elapsed().as_millis());
        }
        samples_ms.sort_unstable();
        // This median assertion bounds repeatable in-process aggregation;
        // it does not claim to measure a p95 or SQLite projection latency.
        let median = samples_ms[samples_ms.len() / 2];
        let in_budget = samples_ms.iter().filter(|ms| **ms <= 150).count();
        assert!(
            median <= 150 && in_budget >= 15,
            "series latency missed the 150 ms budget (median={median} ms, \
             {in_budget}/{} samples in budget); samples_ms={samples_ms:?}",
            samples_ms.len()
        );
    }
}
