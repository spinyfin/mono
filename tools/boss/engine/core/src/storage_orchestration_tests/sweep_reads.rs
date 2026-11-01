//! Cost baseline for the sweeps' narrow reads against the full per-id
//! projection, on a synthetic database shaped like a busy engine. The sweeps
//! (`envelope_watch`, `terminal_work_sweep`, `dead_pane_sweep`) drive the
//! reads; the fixture is bulk-seeded through an explicit raw connection
//! because building it through the create/update APIs would be the thing
//! being timed.

use std::collections::HashMap;
use std::time::Instant;

use boss_protocol::{EffortLevel, RequestExecutionInput, WorkItem, WorkItemPatch};
use rusqlite::{Connection, params};

use super::raw;
use crate::test_support::{create_active_chore, create_product, create_spawned_execution, open_db};
use crate::work::WorkDb;

fn set_effort(db: &WorkDb, id: &str, effort: EffortLevel) {
    db.update_work_item(
        id,
        WorkItemPatch {
            effort_level: Some(effort.as_str().to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

/// Copy one `tasks` / `work_executions` row into a new row with a few
/// columns overridden — the fixture builder for the baseline below. Going
/// through the create/update APIs for hundreds of rows is itself quadratic
/// (every update republishes the full projection), which would make the
/// fixture, not the reads, the thing being timed.
fn clone_row(conn: &Connection, table: &str, template_id: &str, overrides: &[(&str, &str)]) {
    let columns: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let select: Vec<String> = columns
        .iter()
        .map(|column| match overrides.iter().find(|(name, _)| name == column) {
            Some((_, value)) => format!("'{}'", value.replace('\'', "''")),
            None => column.clone(),
        })
        .collect();
    conn.execute(
        &format!(
            "INSERT INTO {table} ({}) SELECT {} FROM {table} WHERE id = ?1",
            columns.join(", "),
            select.join(", ")
        ),
        params![template_id],
    )
    .unwrap();
}

/// Not a correctness test: compares the per-id full `get_work_item`
/// projection against the narrow sweep reads on a synthetic DB shaped like
/// a busy engine (review cycles with revisions and verdicts, which is what
/// makes the projection's badge query expensive). It asserts only that the
/// narrow reads agree with the full projection; timings are printed (run
/// with `--nocapture`) and never asserted, so the test cannot flake on a
/// slow host.
#[test]
fn sweep_cost_baseline_full_projection_vs_narrow_reads() {
    const ROOTS: usize = 1500;
    const REVISIONS_PER_ROOT: usize = 3;
    /// The full projection is slow enough on this fixture that timing it
    /// over every root would push the test past its shard budget; a sample
    /// gives the per-id figure.
    const FULL_SAMPLE: usize = 20;
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let template = create_active_chore(&db, &product, "template");
    set_effort(&db, &template, EffortLevel::Medium);
    let template_execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(template.clone()).build())
        .unwrap();
    let fixture_started = Instant::now();
    let mut roots = Vec::with_capacity(ROOTS);
    {
        let conn = raw(&db);
        for i in 0..ROOTS {
            let root = format!("task_root{i:04}");
            let pr_url = format!("https://github.com/o/r/pull/{i}");
            let head = format!("head{i:04}");
            clone_row(
                &conn,
                "tasks",
                &template,
                &[
                    ("id", &root),
                    ("name", &format!("root {i}")),
                    ("short_id", &format!("{}", 1000 + i)),
                    ("status", "in_review"),
                    ("pr_url", &pr_url),
                    ("pr_head_sha", &head),
                ],
            );
            let mut parent = root.clone();
            for r in 0..REVISIONS_PER_ROOT {
                let revision = format!("task_root{i:04}rev{r}");
                clone_row(
                    &conn,
                    "tasks",
                    &template,
                    &[
                        ("id", &revision),
                        ("name", &format!("root {i} revision {r}")),
                        ("short_id", &format!("{}", 100_000 + i * 10 + r)),
                        ("kind", "revision"),
                        ("parent_task_id", &parent),
                        ("status", "done"),
                    ],
                );
                let execution = format!("exec_root{i:04}rev{r}");
                clone_row(
                    &conn,
                    "work_executions",
                    &template_execution.id,
                    &[("id", &execution), ("work_item_id", &revision), ("status", "completed")],
                );
                conn.execute(
                    "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, head_sha, findings_count,
                        revision_warranted, gate_outcome, revision_task_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, 2, 1, 'completed_with_findings', ?5, ?6)",
                    params![
                        format!("prv_{i}_{r}"),
                        execution,
                        root,
                        head,
                        revision,
                        format!("{}", 1_700_000_000 + i * 10 + r),
                    ],
                )
                .unwrap();
                parent = revision;
            }
            roots.push(root);
        }
    }
    eprintln!("sweep_cost_baseline: fixture built in {:?}", fixture_started.elapsed());
    let states = crate::live_worker_state::LiveWorkerStateRegistry::new();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let mut executions = Vec::new();
    for (slot, id) in roots.iter().take(8).enumerate() {
        let spawn_item = create_active_chore(&db, &product, &format!("slot {slot}"));
        let execution = create_spawned_execution(&db, &spawn_item, 2_000_000_000);
        // Model the post-publication state: the live execution is bound to
        // an in-review root, which cannot accept a new implementation run.
        raw(&db)
            .execute(
                "UPDATE work_executions SET work_item_id = ?1 WHERE id = ?2",
                params![id, execution],
            )
            .unwrap();
        states.register_spawn(
            slot as u8,
            &execution,
            "test-model",
            2_000_000_000,
            Some(boss_protocol::WorkItemBinding {
                work_item_id: id.clone(),
                work_item_name: "measurement".into(),
                execution_id: execution.clone(),
            }),
        );
        states.set_activity_for_test(slot as u8, boss_protocol::WorkerActivity::Working);
        executions.push(execution);
    }
    for pass in 0..3 {
        let before = Instant::now();
        for (execution, id) in executions.iter().zip(&roots) {
            db.get_execution(execution).unwrap();
            db.get_work_item(id).unwrap();
        }
        eprintln!(
            "pass {pass}: old per-slot execution + full task reads (8 slots): {:?}",
            before.elapsed()
        );
        let outcome = crate::envelope_watch::run_one_pass(
            &db,
            &states,
            &crate::envelope_watch::EnvelopeThresholds::default(),
            now,
        );
        assert_eq!(outcome.within_envelope, 8);
        crate::terminal_work_sweep::gather_reap_candidates(
            &db,
            &states,
            &crate::teardown_registry::TeardownRegistry::new(),
        );
        for prefix in ["envelope_watch", "terminal_work_sweep"] {
            assert_eq!(
                db.metrics_registry().gauge_value(&format!("{prefix}.items_per_pass")),
                Some(8)
            );
            eprintln!(
                "pass {pass}: {prefix}.pass_duration_ms={:?} items_per_pass=8",
                db.metrics_registry().gauge_value(&format!("{prefix}.pass_duration_ms"))
            );
        }
    }
    for execution in &executions {
        db.mark_execution_orphaned(execution, "measurement").unwrap();
    }
    for pass in 0..3 {
        let before = Instant::now();
        let conn = raw(&db);
        let mut stmt = conn
            .prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at, pre_start_failure_count, dispatch_not_before,
                    pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count,
                    allow_dirty, branch_naming, dispatch_wait_reason, dispatch_wait_since,
                    driver_runtime_state, driver, model, effort_level, pr_head_after, last_error
             FROM work_executions
             WHERE status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND EXISTS (
                   SELECT 1 FROM work_runs r
                   WHERE r.id = (SELECT latest.id FROM work_runs latest
                       WHERE latest.execution_id = work_executions.id
                       ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1)
                     AND r.host_id = 'local' AND r.shell_pid > 0
                     AND CAST(COALESCE(r.finished_at, r.created_at) AS INTEGER) >= ?1)
             ORDER BY created_at ASC, id ASC",
            )
            .unwrap();
        // Read every column of every row, as the full-row scan does; the
        // mapper that builds the owned execution is storage-private.
        let rows: Vec<String> = stmt
            .query_map([now - crate::work::REDISPATCH_PID_TRUST_SECS], |row| {
                for column in 0..row.as_ref().column_count() {
                    row.get_ref(column)?;
                }
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 8);
        drop(stmt);
        drop(conn);
        for execution in rows {
            // The pid read the host probe starts from; no OS probe in these tests.
            db.latest_local_shell_pid_for_execution_within(&execution, crate::work::REDISPATCH_PID_TRUST_SECS, now)
                .unwrap();
        }
        eprintln!(
            "pass {pass}: terminal full-row scan + probes (8 rows): {:?}",
            before.elapsed()
        );
        assert!(crate::dead_pane_sweep::gather_live_terminal_candidates(&db, now).is_empty());
        assert_eq!(
            db.metrics_registry().gauge_value("dead_pane_terminal.items_per_pass"),
            Some(8)
        );
        eprintln!(
            "pass {pass}: dead_pane_terminal.pass_duration_ms={:?} items_per_pass=8",
            db.metrics_registry().gauge_value("dead_pane_terminal.pass_duration_ms")
        );
    }

    // Full projection: one `get_work_item` per id, for comparison with the narrow batched and single-row reads below.
    let full_started = Instant::now();
    let mut full_efforts = HashMap::new();
    let mut full_closed = HashMap::new();
    for id in &roots[..FULL_SAMPLE] {
        match db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                if let Some(effort) = task.effort_level {
                    full_efforts.insert(id.clone(), effort);
                }
                full_closed.insert(id.clone(), task.status.is_terminal());
            }
            _ => panic!("task expected"),
        }
    }
    let full_elapsed = full_started.elapsed();

    // Narrow batched reads: one query per pass for each field.
    let narrow_started = Instant::now();
    let efforts = db.task_effort_levels(&roots).unwrap();
    let closed = db.task_closed_states(&roots).unwrap();
    let narrow_elapsed = narrow_started.elapsed();

    // Narrow single-row reads: the per-slot shape the timer path keeps.
    let single_started = Instant::now();
    for id in &roots {
        db.task_effort_level(id).unwrap();
        // The public per-slot closed check; for a task it is the narrow
        // single-row status read.
        db.is_bound_work_item_closed(id).unwrap();
    }
    let single_elapsed = single_started.elapsed();

    assert_eq!(efforts.len(), ROOTS);
    assert_eq!(closed.len(), ROOTS);
    for id in &roots[..FULL_SAMPLE] {
        assert_eq!(efforts.get(id), full_efforts.get(id), "{id}");
        assert_eq!(closed.get(id), full_closed.get(id), "{id}");
    }
    eprintln!(
        "sweep_cost_baseline: {ROOTS} in-review roots x {REVISIONS_PER_ROOT} revisions + verdicts\n\
         \x20 full get_work_item per id ({FULL_SAMPLE} sampled): {:?} total, {:?} per id\n\
         \x20 narrow batched (effort + closed, all {ROOTS}): {:?} total\n\
         \x20 narrow single-row per id (effort + closed, all {ROOTS}): {:?} total, {:?} per id",
        full_elapsed,
        full_elapsed / FULL_SAMPLE as u32,
        narrow_elapsed,
        single_elapsed,
        single_elapsed / ROOTS as u32,
    );
}
