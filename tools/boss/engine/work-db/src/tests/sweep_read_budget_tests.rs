//! Read budgets for the engine's recurring database reads.
//!
//! Every read below runs on a timer (the scheduler beat, a once-a-minute
//! sweep, the merge poller) whether or not anything changed. Each one looks
//! for a handful of live rows, so what it reads must follow the number of
//! live rows, not the size of the history behind them. A read that walks a
//! whole table costs one page fetch per page of that table on every tick,
//! and with twenty of them the engine re-reads its database several times a
//! minute while idle.
//!
//! The fixture is the wait-scale board grown to the payload sizes that make
//! a walk expensive: descriptions, PR bodies and proposal payloads long
//! enough to spill onto overflow pages, which a walk has to follow whenever
//! it reads a column stored after them.
//!
//! ```sh
//! bazel test //tools/boss/engine/work-db:work_db_test --test_filter=sweep_read_budget \
//!   --test_output=all --test_arg=--nocapture
//! ```
use super::wait_scale_fixture::open_seeded;
use super::*;
use crate::db_diagnostics::read_volume::{Measured, measure};
use std::collections::HashMap;

/// Grows the seeded rows to production payload sizes and adds the history
/// tables the board fixture leaves empty.
const PAYLOAD_SQL: &str = r#"
CREATE TEMP TABLE fill AS SELECT printf('%.1000c', 'x') AS k1;
INSERT INTO worker_proposals(id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at,
                             decided_at)
SELECT 'prop_' || e.id, e.id, e.work_item_id,
       CASE WHEN e.kind = 'pr_review' THEN 'review_verdict' ELSE 'run_done' END,
       CASE WHEN e.kind = 'pr_review' THEN '{"findings":"' || k1 || k1 || k1 || k1 || k1 || k1 || '"}'
            ELSE '{"summary":"' || k1 || '"}' END,
       'idem_' || e.id, 'applied', e.created_at, e.created_at
FROM work_executions e, fill;
UPDATE worker_proposals SET state = 'proposed', decided_at = NULL
WHERE kind = 'review_verdict' AND rowid % 2000 = 7;
UPDATE work_executions SET driver_runtime_state = '{"session":"' || substr((SELECT k1 FROM fill), 1, 280) || '"}';
UPDATE work_executions SET pr_body_before = (SELECT k1 || k1 || k1 FROM fill) WHERE kind LIKE '%implementation';
UPDATE work_runs SET result_summary = (SELECT substr(k1, 1, 800) FROM fill),
                     tmux_pane_observation = (SELECT substr(k1, 1, 500) FROM fill);
UPDATE tasks SET description = description || (SELECT k1 || k1 || k1 || k1 || k1 || k1 FROM fill) WHERE rowid % 3 = 0;
UPDATE work_runs SET host_id = 'anaplian', remote_pid = 1000 + rowid
WHERE execution_id IN (SELECT id FROM work_executions WHERE status IN ('completed', 'failed'));
UPDATE work_runs SET host_id = 'anaplian', remote_pid = 2000 + rowid
WHERE execution_id IN (SELECT id FROM work_executions WHERE status = 'running');
UPDATE work_executions SET cube_lease_id = 'lease_' || id
WHERE id IN (
    SELECT id FROM work_executions
    WHERE status = 'completed' AND id IN (SELECT execution_id FROM work_runs WHERE host_id = 'anaplian')
    LIMIT 3
);
UPDATE work_runs SET persona_lease_active = 1, persona = 'Data' || rowid
WHERE execution_id IN (SELECT id FROM work_executions WHERE cube_lease_id IS NOT NULL);
WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 1999)
INSERT INTO work_comments(id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status,
                          plain_text_projection_version, created_at, updated_at)
SELECT printf('cmt_%05d', n), 'pr_doc', 'doc' || (n % 300), 'v1', '{}', (SELECT k1 FROM fill), 'human', 'resolved', 1,
       '1760000000', '1760000000' FROM s;
WITH RECURSIVE g(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < 499)
INSERT INTO pr_review_guide_source_series(id, root_task_id, canonical_pr_url, created_at, updated_at)
SELECT printf('gser_%04d', n), printf('task_g_%04d', n), printf('https://github.com/o/r/pull/%d', n + 1),
       '1760000000', '1760000000' FROM g;
WITH RECURSIVE g(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < 499)
INSERT INTO pr_review_guide_source_comparisons(id, series_id, observation_sequence, observed_base_sha, merge_base_sha,
                                               head_sha, trigger, packet_hash, complete, captured_at)
SELECT printf('gcmp_%04d', n), printf('gser_%04d', n), 1, 'base', 'base', printf('%040x', n), 'manual', 'hash', 1,
       CASE WHEN n % 5 = 0 THEN '1700000000' ELSE '1900000000' END FROM g;
WITH RECURSIVE g(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < 499)
INSERT INTO pr_review_guide_attempts(id, series_id, comparison_id, request_epoch, ordinal, status, prompt_version, created_at)
SELECT printf('gatt_%04d', n), printf('gser_%04d', n), printf('gcmp_%04d', n), 1, 1, 'succeeded', 'v1',
       CASE WHEN n % 5 = 0 THEN '1700000000' ELSE '1900000000' END FROM g;
WITH RECURSIVE g(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < 499)
INSERT INTO pr_review_guide_versions(id, series_id, comparison_id, attempt_id, markdown, raw_output, content_hash,
                                     prompt_version, generated_at)
SELECT printf('gver_%04d', n), printf('gser_%04d', n), printf('gcmp_%04d', n), printf('gatt_%04d', n),
       'md', 'raw', 'ch', 'v1', CASE WHEN n % 5 = 0 THEN '1700000000' ELSE '1900000000' END FROM g;
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 499)
INSERT INTO pr_review_guide_updates(attempt_id, previous_version_id)
SELECT printf('gatt_%04d', n), printf('gver_%04d', n - 1) FROM g;
DROP TABLE fill;
"#;

/// Pages a recurring read may fetch. The fixture's `tasks` table alone is
/// over 11,000 pages and `worker_proposals` nearly 20,000, so a walk of
/// either fails by an order of magnitude; an indexed read of the live rows
/// stays well inside it.
const READ_BUDGET_PAGES: u64 = 2500;
/// Hourly scans that intentionally read the full execution stock (e.g.
/// driver-home retention classifying live vs terminal `driver_runtime_state`).
const HOURLY_STOCK_BUDGET_PAGES: u64 = 10_000;

struct Fixture {
    _dir: tempfile::TempDir,
    db: WorkDb,
    table_pages: HashMap<String, u64>,
}

/// The planner statistics a database can carry. Plans must hold under each:
/// which index a statement gets is otherwise decided by numbers nobody
/// reviews.
#[derive(Clone, Copy)]
enum Statistics {
    /// A database that has never been analyzed.
    None,
    /// Analyzed after the indexes existed, as a new install is.
    Fresh,
    /// Analyzed by an SQLite without `STAT4`, such as the system `sqlite3`:
    /// per-index averages only, so every status looks equally common and a
    /// list of live statuses is estimated at most of the table.
    AveragesOnly,
    /// Analyzed before the indexes existed; the migration then analyzed only
    /// the indexes it created. What an existing install has after upgrading.
    Upgraded,
}

fn fixture(label: &str, statistics: Statistics) -> Fixture {
    let (dir, db) = open_seeded(label);
    let table_pages = {
        let conn = db.connect().unwrap();
        conn.execute_batch(&format!("BEGIN; {PAYLOAD_SQL} COMMIT;")).unwrap();
        match statistics {
            Statistics::None => conn
                .execute_batch("DELETE FROM sqlite_stat1; DELETE FROM sqlite_stat4")
                .unwrap(),
            Statistics::Fresh => conn.execute_batch("ANALYZE").unwrap(),
            Statistics::AveragesOnly => conn.execute_batch("ANALYZE; DELETE FROM sqlite_stat4").unwrap(),
            Statistics::Upgraded => {
                let created: Vec<String> = crate::recurring_read_indexes::INDEXES_SQL
                    .split("CREATE INDEX IF NOT EXISTS ")
                    .skip(1)
                    .map(|statement| statement.split_whitespace().next().unwrap().to_owned())
                    .collect();
                assert_eq!(created.len(), 7, "{created:?}");
                for index in &created {
                    conn.execute_batch(&format!("DROP INDEX {index}")).unwrap();
                }
                conn.execute_batch("ANALYZE").unwrap();
                crate::recurring_read_indexes::migrate(&conn).unwrap();
            }
        }
        let mut stmt = conn
            .prepare("SELECT name, count(*) FROM dbstat WHERE name IN ('tasks', 'work_executions', 'work_runs', 'worker_proposals') GROUP BY name")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<HashMap<_, _>>>()
            .unwrap()
    };
    // A fresh attachment: the statistics above are read when a connection
    // first prepares a statement, and its page cache starts empty.
    let path = db.path().to_path_buf();
    drop(db);
    let db = WorkDb::open_existing(path).unwrap();
    Fixture {
        _dir: dir,
        db,
        table_pages,
    }
}

type Read = (&'static str, &'static str, u64, fn(&WorkDb));

/// The recurring reads, with the timer that drives each and its page budget.
fn recurring_reads() -> Vec<Read> {
    vec![
        ("review verdict applier", "10 s", READ_BUDGET_PAGES, |db| {
            db.list_proposed_review_verdicts().unwrap();
        }),
        ("scheduler beat: unexplained waits", "15 s", READ_BUDGET_PAGES, |db| {
            db.list_unexplained_dispatch_waits().unwrap();
        }),
        ("scheduler beat: queued displays", "15 s", READ_BUDGET_PAGES, |db| {
            db.queued_displays(boss_engine_utils::epoch_time::now_epoch_secs(), &HashMap::new())
                .unwrap();
        }),
        ("scheduler beat: ready executions", "15 s", READ_BUDGET_PAGES, |db| {
            db.list_ready_executions().unwrap();
        }),
        (
            "dependency unblock: blocked candidates",
            "30 s",
            READ_BUDGET_PAGES,
            |db| {
                db.list_dependency_blocked_candidates().unwrap();
            },
        ),
        ("dependency unblock: stuck autostart", "30 s", READ_BUDGET_PAGES, |db| {
            db.promote_todo_autostart_stuck_executions().unwrap();
        }),
        ("merge poller: pending merge check", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_chores_pending_merge_check().unwrap();
        }),
        (
            "merge poller: blocked on merge conflict",
            "60 s",
            READ_BUDGET_PAGES,
            |db| {
                db.list_chores_blocked_on_merge_conflict().unwrap();
            },
        ),
        ("merge poller: blocked on CI failure", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_chores_blocked_on_ci_failure().unwrap();
        }),
        ("merge poller: blocked with PR", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_chores_blocked_other_with_pr().unwrap();
        }),
        ("merge poller: stranded remediation", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_chores_stranded_blocked_remediation().unwrap();
        }),
        ("merge poller: held active with PR", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_chores_held_active_with_pr().unwrap();
        }),
        (
            "merge poller: orphaned revision chains",
            "60 s",
            READ_BUDGET_PAGES,
            |db| {
                db.list_orphaned_in_review_revision_chains().unwrap();
            },
        ),
        ("merge poller: merges in flight", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_work_item_ids_with_pending_merge().unwrap();
        }),
        ("merge poller: merge queue members", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_work_item_ids_in_merge_queue().unwrap();
        }),
        ("merge poller: review admission", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_tasks_awaiting_pre_merge_review_admission().unwrap();
        }),
        ("merge poller: pending PR detection", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_executions_pending_pr_detection().unwrap();
        }),
        ("merge poller: late PR detection", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_recently_terminal_executions_pending_pr_detection(3600).unwrap();
        }),
        ("merge poller: stalled reviewer", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_tasks_with_stalled_reviewer(600).unwrap();
        }),
        ("orphan sweep", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_orphan_active_candidates(300).unwrap();
        }),
        ("review recovery", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_dead_pr_review_candidates().unwrap();
        }),
        ("pane-death sweep: live shells", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_non_terminal_executions_with_local_shell_pid().unwrap();
        }),
        ("pane-death sweep: recent terminal", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_recent_terminal_execution_refs_with_local_shell_pid(
                3600,
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )
            .unwrap();
        }),
        ("lease release", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_terminal_executions_holding_lease().unwrap();
        }),
        ("lost workspace sweep", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_non_terminal_executions_with_workspace().unwrap();
        }),
        ("answer-agent completion", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_live_answer_agent_executions().unwrap();
        }),
        ("stranded answering", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_answering_comments().unwrap();
        }),
        ("remote lease reconcile: live runs", "60 s", READ_BUDGET_PAGES, |db| {
            db.list_live_remote_runs().unwrap();
        }),
        (
            "remote lease reconcile: cleanup runs",
            "60 s",
            READ_BUDGET_PAGES,
            |db| {
                db.terminal_remote_cleanup_runs().unwrap();
            },
        ),
        ("attention reconcile", "300 s", READ_BUDGET_PAGES, |db| {
            db.reconcile_stale_attention_signals().unwrap();
        }),
        (
            "abandoned branch sweep: pushed branches",
            "300 s",
            READ_BUDGET_PAGES,
            |db| {
                db.list_abandoned_pushed_branch_candidates(600, 86400).unwrap();
            },
        ),
        ("abandoned branch sweep: bookmarks", "300 s", READ_BUDGET_PAGES, |db| {
            db.terminal_bookmark_executions(600, 86400).unwrap();
        }),
        ("preserved-work cleanup: superseded", "300 s", READ_BUDGET_PAGES, |db| {
            db.list_superseded_preserved(50, &[]).unwrap();
        }),
        (
            "preserved-work cleanup: closed chain",
            "300 s",
            READ_BUDGET_PAGES,
            |db| {
                db.list_closed_chain_preserved(14 * 24 * 60 * 60, 50, &[]).unwrap();
            },
        ),
        ("positioning-hold recheck", "300 s", READ_BUDGET_PAGES, |db| {
            db.list_positioning_holds().unwrap();
        }),
        ("PR binding repair", "600 s", READ_BUDGET_PAGES, |db| {
            db.list_pr_binding_repair_candidates().unwrap();
        }),
        ("proposal expiry", "3600 s", READ_BUDGET_PAGES, |db| {
            db.expire_stale_in_flight_proposals().unwrap();
        }),
        ("execution retention", "3600 s", READ_BUDGET_PAGES, |db| {
            db.prune_terminal_executions(
                crate::ExecutionRetentionPolicy::default(),
                boss_engine_utils::epoch_time::now_epoch_secs(),
                false,
            )
            .unwrap();
        }),
        ("review-guide source retention", "3600 s", READ_BUDGET_PAGES, |db| {
            db.prune_pr_review_guide_sources(crate::review_guide_sources::SourceRetentionPolicy::default())
                .unwrap();
        }),
        (
            "driver-home retention: transcript backfill",
            "3600 s",
            READ_BUDGET_PAGES,
            |db| {
                db.backfill_durable_transcript_paths().unwrap();
            },
        ),
        (
            "driver-home retention: runtime states",
            "3600 s",
            HOURLY_STOCK_BUDGET_PAGES,
            |db| {
                db.list_executions_with_driver_runtime_state().unwrap();
            },
        ),
        ("startup: stranded executions", "start", READ_BUDGET_PAGES, |db| {
            db.abandon_stranded_executions_on_closed_work_items().unwrap();
        }),
    ]
}

fn measure_all(fixture: &Fixture) -> Vec<(&'static str, &'static str, u64, Measured)> {
    // The first statement on a connection reads the whole schema.
    fixture.db.get_metadata("warm").unwrap();
    recurring_reads()
        .into_iter()
        .map(|(name, every, budget, read)| {
            let ((), measured) = measure(|| read(&fixture.db));
            (name, every, budget, measured)
        })
        .collect()
}

fn report(label: &str, fixture: &Fixture, results: &[(&'static str, &'static str, u64, Measured)]) {
    let mut tables: Vec<_> = fixture.table_pages.iter().collect();
    tables.sort();
    println!("sweep_read_budget {label}: table pages {tables:?}");
    for (name, every, budget, measured) in results {
        let top = measured
            .top_statement()
            .map_or_else(String::new, |top| format!(" top=[{:.90}]", top.sql));
        println!(
            "sweep_read_budget {label}: pages={:<6} budget={:<6} full_scan_steps={:<6} statements={:<4} every={every:<5} {name}{top}",
            measured.pages, budget, measured.full_scan_steps, measured.statements
        );
    }
    let total: u64 = results.iter().map(|(_, _, _, measured)| measured.pages).sum();
    println!("sweep_read_budget {label}: one pass of every read = {total} pages");
}

fn assert_remote_reconcile_covers_history_and_live_tail(label: &str, db: &WorkDb) {
    let conn = db.connect().unwrap();
    let remote_runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_runs WHERE host_id != 'local'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let running: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE status = 'running'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let cleanup_holders: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE cube_lease_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    assert!(
        remote_runs > 1000,
        "{label}: fixture must hold a large settled remote history, got {remote_runs} remote runs"
    );
    assert!(
        running > 0 && running < 200,
        "{label}: fixture must hold a small live remote tail, got {running} running executions"
    );
    let live = db.list_live_remote_runs().unwrap();
    assert_eq!(
        live.len(),
        running as usize,
        "{label}: live remote reconcile must return the running tail, not settled history ({remote_runs} remote runs)"
    );
    assert!(
        live.iter().all(|handle| handle.host_id != "local"),
        "{label}: live remote handles must all be remote"
    );
    let cleanup = db.terminal_remote_cleanup_runs().unwrap();
    assert_eq!(
        cleanup.len(),
        cleanup_holders as usize,
        "{label}: terminal remote cleanup must return the resource-holding tail"
    );
}

fn assert_within_budget(label: &str, results: &[(&'static str, &'static str, u64, Measured)]) {
    let over: Vec<String> = results
        .iter()
        .filter(|(_, _, budget, measured)| measured.pages > *budget)
        .map(|(name, _, budget, measured)| {
            format!(
                "{name}: {} pages (budget {budget}), top statement {:?}",
                measured.pages,
                measured.top_statement().map(|top| top.sql.as_str()).unwrap_or("")
            )
        })
        .collect();
    assert!(
        over.is_empty(),
        "{label}: recurring reads over budget (a table walk):\n{}",
        over.join("\n")
    );
}

#[test]
fn sweep_read_budget_holds_with_fresh_statistics() {
    let fixture = fixture("sweep-read-budget-fresh", Statistics::Fresh);
    let results = measure_all(&fixture);
    report("fresh", &fixture, &results);
    // The budget only means something while a walk is far over it.
    for table in ["tasks", "work_executions", "work_runs", "worker_proposals"] {
        assert!(
            fixture.table_pages[table] > 2 * READ_BUDGET_PAGES,
            "{table} is too small to fail a walk: {:?}",
            fixture.table_pages
        );
    }
    assert_within_budget("fresh", &results);
    assert_remote_reconcile_covers_history_and_live_tail("fresh", &fixture.db);
    assert_postmortem_plans("fresh", &fixture.db);
}

#[test]
fn sweep_read_budget_holds_without_statistics() {
    let fixture = fixture("sweep-read-budget-none", Statistics::None);
    let results = measure_all(&fixture);
    report("none", &fixture, &results);
    assert_within_budget("none", &results);
    assert_remote_reconcile_covers_history_and_live_tail("none", &fixture.db);
    assert_postmortem_plans("none", &fixture.db);
}

#[test]
fn sweep_read_budget_holds_with_average_only_statistics() {
    let fixture = fixture("sweep-read-budget-averages", Statistics::AveragesOnly);
    let results = measure_all(&fixture);
    report("averages", &fixture, &results);
    assert_within_budget("averages", &results);
    assert_remote_reconcile_covers_history_and_live_tail("averages", &fixture.db);
    assert_postmortem_plans("averages", &fixture.db);
}

#[test]
fn sweep_read_budget_holds_with_statistics_from_before_the_upgrade() {
    let fixture = fixture("sweep-read-budget-upgraded", Statistics::Upgraded);
    let results = measure_all(&fixture);
    report("upgraded", &fixture, &results);
    assert_within_budget("upgraded", &results);
    assert_remote_reconcile_covers_history_and_live_tail("upgraded", &fixture.db);
    assert_postmortem_plans("upgraded", &fixture.db);
}

fn plan_of(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let bindings: Vec<&str> = if sql.contains("?2") {
        vec!["proj_ws000", "0"]
    } else {
        vec!["proj_ws000"]
    };
    stmt.query_map(rusqlite::params_from_iter(bindings), |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Project-scoped postmortem reads must seek `tasks_project_idx` under every
/// statistics regime. `tasks_status_idx` would otherwise be a candidate for
/// `status = 'done'` and walk every done task instead of one project.
fn assert_postmortem_plans(label: &str, db: &WorkDb) {
    let conn = db.connect().unwrap();
    for (name, sql) in [
        ("latest", crate::project_postmortem::POSTMORTEM_LATEST_SQL),
        ("live open", crate::project_postmortem::POSTMORTEM_LIVE_OPEN_SQL),
        ("open count", crate::project_postmortem::POSTMORTEM_OPEN_COUNT_SQL),
        ("done work", crate::project_postmortem::POSTMORTEM_DONE_WORK_SQL),
        (
            "trigger exists",
            crate::project_postmortem::POSTMORTEM_TRIGGER_DONE_EXISTS_SQL,
        ),
    ] {
        let plan = plan_of(&conn, sql);
        println!("postmortem plan {name} ({label}): {plan:?}");
        assert!(
            plan.iter().any(|step| step.contains("tasks_project_idx")),
            "{name} ({label}) must seek tasks_project_idx: {plan:#?}"
        );
        assert!(
            plan.iter().all(|step| !step.contains("tasks_status_idx")),
            "{name} ({label}) must not use tasks_status_idx: {plan:#?}"
        );
    }
}
