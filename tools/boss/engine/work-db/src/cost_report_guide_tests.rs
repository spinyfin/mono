//! Runs of executions that do not bind to a task: review guides (a source
//! comparison), answer agents (a comment) and automation triage (an
//! automation). Every such run is fetched; a review guide's is also
//! attributed to the task that owns the PR it explains.

use super::*;
use crate::test_support::{
    create_active_chore, create_product, create_ready_chore_execution, open_db, seed_review_guide_series,
};
use boss_engine_storage_values::RunCostSnapshot;

const REPO: &str = "https://github.com/acme/widget.git";

/// A task with one review guide series, as `(task id, comparison id)`.
fn task_with_guide_series(db: &WorkDb, name: &str) -> (String, String) {
    let product = create_product(db);
    let task = create_active_chore(db, &product, name);
    let (_series, comparison) = seed_review_guide_series(db, &task);
    (task, comparison)
}

/// Dispatches a review guide for `comparison` the way the engine does: the
/// execution binds to the comparison, a run starts, and the run's usage (if
/// any) is written by the same snapshot writer the transcript capture uses.
/// Returns `(execution id, run id)`.
fn completed_guide_run(db: &WorkDb, comparison: &str, usage: Option<RunCostSnapshot>) -> (String, String) {
    let execution = db.create_pr_review_guide_execution(comparison, REPO).unwrap();
    assert_eq!(execution.work_item_id, comparison, "a guide binds to its comparison");
    let (_, run) = db
        .start_execution_run(&execution.id, "guide-1", "widget", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    if let Some(snapshot) = usage {
        assert!(db.set_run_cost_snapshot(&execution.id, snapshot).unwrap());
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_runs SET status = 'completed' WHERE id = ?1",
        params![run.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed', finished_at = created_at WHERE id = ?1",
        params![execution.id],
    )
    .unwrap();
    (execution.id, run.id)
}

fn guide_usage() -> RunCostSnapshot {
    RunCostSnapshot::builder()
        .model("gpt-6-astra")
        .input_tokens(40_000)
        .output_tokens(1_800)
        .cache_creation_tokens(0)
        .cache_read_tokens(30_000)
        .build()
}

/// An execution bound to something that is not a task, with one measured run.
fn insert_unbound_run(db: &WorkDb, run_id: &str, work_item_id: &str, kind: &str, created: i64) {
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, finished_at, driver)
         VALUES (?1, ?2, ?3, 'completed', ?4, ?5, ?5, 'claude')",
        params![format!("exec_{run_id}"), work_item_id, kind, REPO, created.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, input_tokens, output_tokens)
         VALUES (?1, ?2, 'worker', 'completed', ?3, 5, 7)",
        params![run_id, format!("exec_{run_id}"), created.to_string()],
    )
    .unwrap();
}

fn set_run_created_at(db: &WorkDb, run_id: &str, created: i64) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = ?2 WHERE id = ?1",
            params![run_id, created.to_string()],
        )
        .unwrap();
}

fn query_plan(db: &WorkDb, sql: &str) -> String {
    db.connect()
        .unwrap()
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n")
}

#[test]
fn a_completed_review_guide_run_is_in_the_window_and_task_cost_records_with_its_tokens() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let implementation = create_ready_chore_execution(&db, &task);
    db.start_execution_run(&implementation.id, "worker-1", "widget", "lease-0", "ws-0", "/tmp/ws-0")
        .unwrap();
    let (guide_execution, guide_run) = completed_guide_run(&db, &comparison, Some(guide_usage()));

    let window = db.cost_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(window.len(), 2, "the implementation run and the guide run");
    let guide = window
        .iter()
        .find(|record| record.run_id == guide_run)
        .expect("the guide run is in the window");
    assert_eq!(guide.execution_id, guide_execution);
    assert_eq!(guide.execution_kind, "pr_review_guide");
    assert_eq!(guide.execution_status, "completed");
    assert_eq!(guide.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(
        (
            guide.input_tokens,
            guide.output_tokens,
            guide.cache_creation_tokens,
            guide.cache_read_tokens
        ),
        (Some(40_000), Some(1_800), Some(0), Some(30_000))
    );
    assert_eq!(
        guide.work_item_id, task,
        "the spend belongs to the task that owns the PR"
    );
    assert_eq!(guide.work_item_name.as_deref(), Some("Fix cursor flicker"));

    let for_task = db.cost_records_for_work_item(&task).unwrap();
    let kinds: Vec<&str> = for_task.iter().map(|record| record.execution_kind.as_str()).collect();
    assert_eq!(kinds, ["chore_implementation", "pr_review_guide"]);
    assert_eq!(for_task[1], *guide, "both fetches project the guide run identically");
}

#[test]
fn a_review_guide_run_with_no_recorded_usage_is_fetched_with_null_tokens() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let (_, run) = completed_guide_run(&db, &comparison, None);

    for records in [
        db.cost_records_for_window(0, i64::MAX).unwrap(),
        db.cost_records_for_work_item(&task).unwrap(),
    ] {
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].run_id, run);
        assert_eq!(records[0].execution_kind, "pr_review_guide");
        assert_eq!(
            (
                records[0].input_tokens,
                records[0].output_tokens,
                records[0].cache_creation_tokens,
                records[0].cache_read_tokens
            ),
            (None, None, None, None),
            "unknown usage stays NULL, never zero"
        );
    }
}

#[test]
fn runs_of_executions_with_no_task_are_in_the_window_cost_records() {
    let (_dir, db) = open_db();
    insert_unbound_run(&db, "run_answer", "comment_1", "answer_agent", 100);
    insert_unbound_run(&db, "run_triage", "automation_1", "automation_triage", 101);
    // A guide whose comparison row is gone (source retention pruned it).
    insert_unbound_run(&db, "run_orphan_guide", "prgc_pruned", "pr_review_guide", 102);

    let records = db.cost_records_for_window(0, 1_000).unwrap();
    let rows: Vec<(&str, &str, &str)> = records
        .iter()
        .map(|record| {
            (
                record.run_id.as_str(),
                record.execution_kind.as_str(),
                record.work_item_id.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("run_answer", "answer_agent", "comment_1"),
            ("run_triage", "automation_triage", "automation_1"),
            ("run_orphan_guide", "pr_review_guide", "prgc_pruned"),
        ],
        "a run with no task keeps what its execution binds to"
    );
    for record in &records {
        assert_eq!(record.work_item_name, None, "a run with no task has no task name");
        assert_eq!((record.input_tokens, record.output_tokens), (Some(5), Some(7)));
    }
    assert!(records.iter().all(|record| record.work_item_short_id.is_none()));
}

#[test]
fn another_tasks_review_guide_is_not_charged_to_this_task() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let other = create_active_chore(&db, &create_product(&db), "Unrelated");
    let (_series, other_comparison) = crate::test_support::seed_review_guide_series_for_pr(
        &db,
        &other,
        "https://github.com/acme/widget/pull/10",
        "base",
        "head",
    );
    let (_, run) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    let (_, other_run) = completed_guide_run(&db, &other_comparison, Some(guide_usage()));

    let mine = db.cost_records_for_work_item(&task).unwrap();
    assert_eq!(
        mine.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        [run.as_str()]
    );
    let theirs = db.cost_records_for_work_item(&other).unwrap();
    assert_eq!(
        theirs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        [other_run.as_str()]
    );
}

#[test]
fn usage_records_name_the_owning_task_of_a_review_guide_run_and_count_it_for_that_task() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let (_, in_window) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    let (_, before_window) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    set_run_created_at(&db, &in_window, 1_500);
    set_run_created_at(&db, &before_window, 500);
    insert_unbound_run(&db, "run_orphan_guide", "prgc_pruned", "pr_review_guide", 1_600);

    let records = db.usage_records_for_window(1_000, 2_000).unwrap();
    let runs: Vec<(&str, &str, Option<&str>)> = records
        .runs
        .iter()
        .map(|run| {
            (
                run.run_id.as_str(),
                run.work_item_id.as_str(),
                run.owning_task_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        [
            (in_window.as_str(), comparison.as_str(), Some(task.as_str())),
            ("run_orphan_guide", "prgc_pruned", None),
        ]
    );
    assert_eq!(records.runs[0].input_tokens, Some(40_000));

    assert_eq!(records.tasks.len(), 1, "the owning task, and nothing for the orphan");
    assert_eq!(records.tasks[0].task_id, task);
    assert_eq!(
        records.tasks[0].run_count_all_time, 2,
        "both guide runs count toward the task's all-time total"
    );
}

#[test]
fn usage_series_records_pull_a_completed_tasks_review_guide_runs_from_outside_the_window() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let (old_execution, before_window) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    let (late_execution, after_window) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    set_run_created_at(&db, &before_window, 500);
    set_run_created_at(&db, &after_window, 3_000);
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'done', created_at = '400', completed_at = '1800' WHERE id = ?1",
            params![task],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '500', finished_at = '600' WHERE id = ?1",
            params![old_execution],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '3000', finished_at = '3100' WHERE id = ?1",
            params![late_execution],
        )
        .unwrap();
    }

    // The task completed inside the window; neither guide run was created in it.
    let records = db.usage_series_records_for_window(1_000, 2_000).unwrap();
    let mut run_ids: Vec<&str> = records.runs.iter().map(|run| run.run_id.as_str()).collect();
    run_ids.sort_unstable();
    let mut expected = [before_window.as_str(), after_window.as_str()];
    expected.sort_unstable();
    assert_eq!(run_ids, expected, "the completed task's cost includes its guides");
    assert!(
        records
            .runs
            .iter()
            .all(|run| run.owning_task_id.as_deref() == Some(task.as_str()))
    );

    // A window that contains the guide execution's finish reports it as a
    // finished execution owned by the task.
    let records = db.usage_series_records_for_window(0, 1_000).unwrap();
    let finished: Vec<(&str, &str, Option<&str>)> = records
        .finished_executions
        .iter()
        .map(|exec| {
            (
                exec.execution_id.as_str(),
                exec.execution_kind.as_str(),
                exec.owning_task_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        finished,
        [(old_execution.as_str(), "pr_review_guide", Some(task.as_str()))]
    );
    assert!(records.tasks.iter().any(|t| t.task_id == task));
}

/// The owning task is stored on the guide's execution, so deleting the
/// comparison (what source retention does to aged and surplus comparisons)
/// must not move the run's spend.
#[test]
fn a_review_guide_runs_task_attribution_survives_source_retention() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let (_, run) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', completed_at = '1000' WHERE id = ?1",
            params![task],
        )
        .unwrap();
    let before_cost = db.cost_records_for_work_item(&task).unwrap();
    let before_usage = db.usage_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(before_cost.len(), 1);

    // Source retention at its most aggressive: the comparison row is gone.
    {
        let conn = db.connect().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "DELETE FROM pr_review_guide_source_comparisons WHERE id = ?1",
            params![comparison],
        )
        .unwrap();
    }

    assert_eq!(db.cost_records_for_work_item(&task).unwrap(), before_cost);
    let after_usage = db.usage_records_for_window(0, i64::MAX).unwrap();
    assert_eq!(after_usage.runs, before_usage.runs);
    assert_eq!(after_usage.runs[0].run_id, run);
    assert_eq!(after_usage.runs[0].owning_task_id.as_deref(), Some(task.as_str()));
    assert_eq!(after_usage.tasks[0].run_count_all_time, 1);
    let series = db.usage_series_records_for_window(0, i64::MAX).unwrap();
    assert!(
        series
            .runs
            .iter()
            .any(|r| r.run_id == run && r.owning_task_id.as_deref() == Some(task.as_str()))
    );
}

/// A database from before the column: the migration fills the owner for
/// guide executions whose comparison still exists.
#[test]
fn the_owner_migration_backfills_guide_executions_from_their_comparisons() {
    let (_dir, db) = open_db();
    let (task, comparison) = task_with_guide_series(&db, "Fix cursor flicker");
    let (execution, _) = completed_guide_run(&db, &comparison, Some(guide_usage()));
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET owner_task_id = NULL WHERE id = ?1",
        params![execution],
    )
    .unwrap();
    migrate_execution_owner_task(&conn).unwrap();
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner_task_id FROM work_executions WHERE id = ?1",
            params![execution],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner.as_deref(), Some(task.as_str()));
}

/// Plan lines that walk a whole base table, by the aliases the fetches use.
fn full_table_scans(plan: &str) -> Vec<&str> {
    const ALIASES: [&str; 3] = ["we", "wr", "t"];
    plan.lines()
        .map(str::trim)
        .filter(|line| {
            line.strip_prefix("SCAN ")
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|alias| ALIASES.contains(&alias))
        })
        .collect()
}

/// The owner lookup must stay a primary-key probe per run, and the reverse
/// (a task's guide comparisons) an index seek: neither may walk a table.
#[test]
fn the_guide_owner_joins_and_the_per_task_fetches_seek_indexes() {
    let (_dir, db) = open_db();
    let source = cost_record_source();

    let window = query_plan(
        &db,
        &format!(
            "SELECT {COST_RECORD_COLUMNS} FROM {source}
             WHERE CAST(wr.created_at AS INTEGER) >= 1 AND CAST(wr.created_at AS INTEGER) < 2"
        ),
    );
    assert!(
        window.contains("work_runs_created_at_epoch_idx"),
        "the window read must use the expression index, plan was:\n{window}"
    );

    let owned = execution_owned_by_any_sql("'[\"t\"]'");
    let for_task = query_plan(
        &db,
        &format!("SELECT {COST_RECORD_COLUMNS} FROM {source} WHERE {owned}"),
    );
    let task_columns = usage_task_columns();
    let task_run_count = query_plan(
        &db,
        &format!(
            "SELECT {task_columns} FROM tasks t LEFT JOIN products p ON p.id = t.product_id LEFT JOIN projects pr ON pr.id = t.project_id WHERE t.id = 't'"
        ),
    );
    for (name, plan) in [
        ("window", &window),
        ("per-task", &for_task),
        ("task run count", &task_run_count),
    ] {
        let scans = full_table_scans(plan);
        assert!(
            scans.is_empty(),
            "the {name} fetch walks a table: {scans:?}; plan was:\n{plan}"
        );
    }
}
