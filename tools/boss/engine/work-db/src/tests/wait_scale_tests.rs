//! Naming the wait of cards with nothing pending must cost a board assembly
//! and a scheduler heartbeat a fixed number of statements, however many such
//! cards there are, and must name every one of them exactly as the per-item
//! rules did.
use super::wait_scale_fixture::*;
use super::*;
use crate::board_caches::BoardCaches;
use crate::db_diagnostics::StatementCounter;
use crate::unscheduled_wait::{self, Candidate};
use boss_metrics::population_timing::{PopulationTrace, segment};
use std::collections::BTreeMap;
use std::time::Instant;

/// The status of each id's latest execution, as
/// `query_latest_execution_for_work_item` chooses it.
fn latest_execution_statuses(conn: &Connection, ids: &[&str]) -> Result<HashMap<String, ExecutionStatus>> {
    let mut statuses = HashMap::new();
    for id in ids {
        if let Some(execution) = query_latest_execution_for_work_item(conn, id)? {
            statuses.insert((*id).to_owned(), execution.status);
        }
    }
    Ok(statuses)
}

/// Every task row of the database, on the board or not.
fn all_tasks(conn: &Connection) -> Vec<Task> {
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM tasks ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut tasks = query_tasks(conn, &ids).unwrap();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(tasks.len(), ids.len());
    tasks
}

/// Every task row of the database classified in one call, with the status of
/// its latest execution as the scheduler heartbeat reads it.
fn classify_all(conn: &Connection) -> BTreeMap<String, String> {
    let tasks = all_tasks(conn);
    let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let latest = latest_execution_statuses(conn, &ids).unwrap();
    let candidates: Vec<Candidate<'_>> = tasks
        .iter()
        .map(|task| Candidate {
            task,
            latest: latest.get(&task.id),
        })
        .collect();
    let (classified, _) = unscheduled_wait::classify(conn, &candidates, None).unwrap();
    tasks
        .iter()
        .map(|task| {
            let reason = match classified.get(&task.id) {
                Some(wait) => {
                    let reason = wait.reason.clone().expect("a classified item carries a reason");
                    assert_eq!(
                        wait.label.as_deref(),
                        Some(boss_engine_storage_values::dispatch_wait::dispatch_wait_phrase(&reason).as_str()),
                        "{} carries the phrase of its reason",
                        task.id
                    );
                    reason
                }
                None => "(none)".to_owned(),
            };
            (task.id.clone(), reason)
        })
        .collect()
}

fn census(reasons: &BTreeMap<String, String>) -> BTreeMap<&str, usize> {
    let mut census = BTreeMap::new();
    for reason in reasons.values() {
        *census.entry(reason.as_str()).or_insert(0) += 1;
    }
    census
}

/// Rows that put fixture items into the states the plain seed does not reach.
const VARIANTS_SQL: &str = r#"
-- A review reservation: admission deferred for one in-review owner, and a
-- failed first-generation batch with no settled verdict for another.
INSERT INTO work_attention_items(id, work_item_id, kind, status, title, body_markdown, created_at)
VALUES ('attn_variant_deferred', 'task_ws04108', 'pr_review_admission_deferred', 'open', 't', 'b', '1760000000');
INSERT INTO pr_review_batches(id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url,
                              status, target_sha, updated_at)
VALUES ('batch_variant_failed', 'task_ws04118', 'base', '{}', '1760009000', 'pre_merge', 4119,
        'https://example.invalid/wait-scale/pull/4119', 'failed', 'head-4118', '1760009100');
-- A findings verdict whose revision is still to do.
UPDATE pr_review_verdicts SET revision_task_id = 'task_ws03963' WHERE id = 'verdict_ws04104';
-- A merge-only wait with a live revision in the family, and one with a
-- reviewer still running on a family member.
UPDATE tasks SET status = 'active' WHERE id = 'task_ws04107';
INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at)
VALUES ('exec_variant_reviewer', 'task_ws04127', 'pr_review', 'running', 'https://example.invalid/wait-scale',
        '1760009000');
-- Deliberate parks: a blocked declaration, and a nudge-breaker attention on
-- the latest run.
UPDATE work_executions SET status = 'cancelled', run_done_outcome = 'blocked' WHERE id = 'exec_ws04024_i';
INSERT INTO work_attention_items(id, execution_id, kind, status, title, body_markdown, created_at)
VALUES ('attn_variant_nudge', 'exec_ws04032_i', 'nudge_breaker_tripped', 'open', 't', 'b', '1760000000');
UPDATE work_executions SET status = 'cancelled' WHERE id = 'exec_ws04032_i';
-- A park inherited from the project, and a human-driven item in each lane.
UPDATE projects SET parked_at = '1760000000' WHERE id = 'proj_ws080';
UPDATE tasks SET human_driven = 1 WHERE id IN ('task_ws04040', 'task_ws04140', 'task_ws06470', 'task_ws06510');
-- A tombstoned card, and two revisions whose parent is gone.
UPDATE tasks SET deleted_at = '1760000900' WHERE id = 'task_ws03978';
UPDATE tasks SET parent_task_id = 'task_missing' WHERE id IN ('task_ws03987', 'task_ws04147');
-- Runs parked waiting for review or merge, on items in several statuses.
INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at)
SELECT 'exec_variant_parked_' || id, id, 'task_implementation',
       CASE WHEN id IN ('task_ws00010', 'task_ws04060', 'task_ws04150') THEN 'waiting_review' ELSE 'waiting_merge' END,
       'https://example.invalid/wait-scale', '1760009500'
FROM tasks WHERE id IN ('task_ws00010', 'task_ws04060', 'task_ws04150', 'task_ws00020', 'task_ws04160');
-- An idle active item whose latest review pass review recovery owns, and a
-- revision whose implementation finished.
INSERT INTO work_executions(id, work_item_id, kind, status, repo_remote_url, created_at, finished_at)
VALUES ('exec_variant_dead_review', 'task_ws04048', 'pr_review', 'failed', 'https://example.invalid/wait-scale',
        '1760009000', '1760009100');
UPDATE work_attention_items SET status = 'resolved' WHERE work_item_id = 'task_ws04027';
UPDATE work_executions SET status = 'completed' WHERE id = 'exec_ws04027_i';
-- A queued card with no repo to run in, and one whose own row names a repo.
UPDATE products SET repo_remote_url = NULL WHERE id = 'prod_waitcontrol';
UPDATE tasks SET deferred = 0 WHERE id IN ('task_wc00230', 'task_wc00231');
UPDATE tasks SET repo_remote_url = 'https://example.invalid/override' WHERE id = 'task_wc00231';
-- Critical work an operator pause exempts.
UPDATE tasks SET priority = 'critical' WHERE id = 'task_ws06469';
"#;

/// What [`VARIANTS_SQL`] makes of the rows it touches, with dispatch running.
const VARIANT_REASONS: &[(&str, &str)] = &[
    ("task_wc00230", "repo_unresolved"),
    ("task_wc00231", "unclassified"),
    ("task_ws00010", "awaiting_merge"),
    ("task_ws00020", "awaiting_merge"),
    ("task_ws03960", "parked"),
    ("task_ws03961", "human_driven"),
    ("task_ws03962", "parked"),
    ("task_ws03963", "parked"),
    ("task_ws03978", "(none)"),
    ("task_ws03987", "unclassified"),
    ("task_ws04024", "deliberate_park"),
    ("task_ws04027", "awaiting_review"),
    ("task_ws04032", "deliberate_park"),
    ("task_ws04040", "human_driven"),
    ("task_ws04048", "awaiting_review"),
    ("task_ws04060", "unclassified"),
    ("task_ws04104", "revision_pending"),
    ("task_ws04106", "revision_pending"),
    ("task_ws04107", "awaiting_review"),
    ("task_ws04108", "review_reservation"),
    ("task_ws04118", "review_reservation"),
    ("task_ws04126", "review_batch"),
    ("task_ws04127", "(none)"),
    ("task_ws04140", "human_driven"),
    ("task_ws04147", "unclassified"),
    ("task_ws04150", "human_review"),
    ("task_ws04160", "awaiting_merge"),
    ("task_ws06469", "awaiting_redispatch"),
    ("task_ws06470", "human_driven"),
    ("task_ws06477", "awaiting_redispatch"),
    ("task_ws06510", "human_driven"),
];

/// Every reason below was produced by the per-item classification the
/// set-based one replaced. A verbatim copy of that implementation was run
/// beside it over all 7,305 task rows of the fixture, plain and with the
/// variants under each pause state, and the two agreed on every row; these
/// are the censuses and the per-row reasons it gave.
#[test]
fn every_item_keeps_the_reason_the_per_item_rules_gave_it() {
    let (_dir, db) = open_seeded("wait-scale-reasons");
    let conn = db.connect().unwrap();
    assert_eq!(
        census(&classify_all(&conn)),
        BTreeMap::from([
            ("(none)", 6998),
            ("awaiting_merge", 57),
            ("awaiting_parent_pr", 4),
            ("awaiting_redispatch", 15),
            ("awaiting_review", 15),
            ("deferred", 35),
            ("dispatch_failed", 15),
            ("human_driven", 15),
            ("human_review", 49),
            ("parked", 18),
            ("pr_open", 12),
            ("recovery_attention", 15),
            ("review_batch", 8),
            ("review_complete", 26),
            ("run_ended", 12),
            ("unclassified", 11),
        ]),
        "the plain seed"
    );

    conn.execute_batch(VARIANTS_SQL).unwrap();
    // Whatever the pause, only the items the orphan sweep would redispatch
    // change: eleven of them, one of which is critical main-pool work.
    let unpaused = BTreeMap::from([
        ("(none)", 6997),
        ("awaiting_merge", 52),
        ("awaiting_parent_pr", 3),
        ("awaiting_redispatch", 11),
        ("awaiting_review", 17),
        ("deferred", 32),
        ("deliberate_park", 2),
        ("dispatch_failed", 15),
        ("human_driven", 19),
        ("human_review", 46),
        ("parked", 19),
        ("pr_open", 11),
        ("recovery_attention", 14),
        ("repo_unresolved", 1),
        ("review_batch", 9),
        ("review_complete", 26),
        ("review_reservation", 2),
        ("revision_pending", 2),
        ("run_ended", 12),
        ("unclassified", 15),
    ]);
    for (label, pause, paused, critical) in [
        ("dispatch running", None, 0, "awaiting_redispatch"),
        // An operator pause that leaves reviews running exempts critical
        // main-pool work as well.
        (
            "operator pause",
            Some(("operator", "new_work")),
            10,
            "awaiting_redispatch",
        ),
        (
            "operator pause holding reviews",
            Some(("operator", "all_spawns")),
            11,
            "dispatch_paused",
        ),
        ("breaker pause", Some(("breaker", "all_spawns")), 11, "dispatch_paused"),
    ] {
        conn.execute("DELETE FROM metadata WHERE key LIKE 'dispatch_paused%'", [])
            .unwrap();
        if let Some((origin, scope)) = pause {
            conn.execute(
                "INSERT INTO metadata(key, value) VALUES ('dispatch_paused', '1'), ('dispatch_paused_origin', ?1),
                 ('dispatch_paused_scope', ?2)",
                params![origin, scope],
            )
            .unwrap();
        }
        let reasons = classify_all(&conn);
        let mut expected = unpaused.clone();
        if paused > 0 {
            expected.insert("dispatch_paused", paused);
            match 11 - paused {
                0 => expected.remove("awaiting_redispatch"),
                left => expected.insert("awaiting_redispatch", left),
            };
        }
        assert_eq!(census(&reasons), expected, "{label}");
        for (id, reason) in VARIANT_REASONS {
            let expected = match *id {
                "task_ws06469" => critical,
                "task_ws06477" if paused > 0 => "dispatch_paused",
                _ => reason,
            };
            assert_eq!(reasons[*id], expected, "{label}: {id}");
        }
    }
}

/// Statements one classification of `tasks` runs, by branch.
fn classify_cost(conn: &Connection, tasks: &[&Task]) -> unscheduled_wait::ClassifyCost {
    let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let latest = latest_execution_statuses(conn, &ids).unwrap();
    let candidates: Vec<Candidate<'_>> = tasks
        .iter()
        .map(|task| Candidate {
            task,
            latest: latest.get(&task.id),
        })
        .collect();
    let counter = StatementCounter::install(conn);
    let (_, cost) = unscheduled_wait::classify(conn, &candidates, Some(&counter)).unwrap();
    cost
}

/// The first half of each live group of the large product's cards, by seed
/// number (see `SEED_SQL`): half the cards of every branch, with every kind
/// of card each branch decides on still present.
fn in_first_half_of_its_group(task: &Task) -> bool {
    let Some(n) = task.id.strip_prefix("task_ws").and_then(|n| n.parse::<usize>().ok()) else {
        return false;
    };
    [3960..3990, 4020..4060, 4100..4150, 6425..6445, 6465..6485, 6505..6520]
        .iter()
        .any(|range| range.contains(&n))
}

/// The guard against the regression this fixture reproduces: naming waits
/// one card at a time. Classifying half of the cards and classifying all of
/// them must run the same statements in every branch, and that number is
/// pinned. Before this was set-based the whole set took 1,717 statements.
#[test]
fn classification_statements_do_not_grow_with_the_cards_classified() {
    let (_dir, db) = open_seeded("wait-scale-statements");
    let conn = db.connect().unwrap();
    let tasks: Vec<Task> = all_tasks(&conn)
        .into_iter()
        .filter(|task| task.product_id == PRODUCT)
        .collect();
    let whole: Vec<&Task> = tasks.iter().collect();
    let half: Vec<&Task> = tasks.iter().filter(|task| in_first_half_of_its_group(task)).collect();
    let (whole, half) = (classify_cost(&conn, &whole), classify_cost(&conn, &half));
    println!("wait-scale classify whole={whole:?}");
    println!("wait-scale classify half={half:?}");
    for (branch, whole, half, cards, bound) in [
        ("queued", whole.queued, half.queued, 87, 5),
        ("idle active", whole.idle_active, half.idle_active, 60, 9),
        ("review", whole.review, half.review, 130, 8),
    ] {
        assert_eq!(
            whole.cards, cards,
            "{branch}: cards with nothing pending on the seeded board"
        );
        assert!(
            (whole.cards / 2 - 2..=whole.cards / 2 + 2).contains(&half.cards),
            "{branch}: {} cards is not half of {}",
            half.cards,
            whole.cards
        );
        assert_eq!(
            whole.statements, half.statements,
            "{branch}: {} cards and {} cards must cost the same statements",
            whole.cards, half.cards
        );
        assert_eq!(
            whole.statements, bound,
            "{branch}: statements to classify {} cards",
            whole.cards
        );
    }
}

fn plan_of(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let values: Vec<String> = (1..=2)
        .filter(|i| sql.contains(&format!("?{i}")))
        .map(|_| "[\"task_ws04106\"]".to_owned())
        .collect();
    stmt.query_map(rusqlite::params_from_iter(values), |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The three statements that walked `tasks` or `work_executions` once per
/// card must seek by index now that they are driven from the item ids.
///
/// Before, restricted to one item, their plans read `SEARCH t USING INDEX
/// tasks_pr_url_idx (pr_url>?)` (review admission: every PR-bearing task),
/// `SCAN t` (the orphan sweep's walk: every task) and `SCAN t` plus `SCAN e
/// USING COVERING INDEX work_executions_item_kind_status_finished_idx` (the
/// family probe: every task and every execution).
#[test]
fn classification_statements_seek_instead_of_scanning() {
    let (_dir, db) = open_seeded("wait-scale-plans");
    let conn = db.connect().unwrap();
    let by_primary_key = "USING INDEX sqlite_autoindex_tasks_1 (id=?)";
    // The engine analyzes its database, and the planner chooses differently
    // once it has statistics: both states must seek.
    for statistics in [false, true] {
        if statistics {
            conn.execute_batch("ANALYZE").unwrap();
        }
        assert_seeking_plans(&conn, by_primary_key, statistics);
    }
}

fn assert_seeking_plans(conn: &Connection, by_primary_key: &str, statistics: bool) {
    for (name, sql, table_walks) in [
        (
            "review admission",
            WorkDb::pre_merge_review_admission_candidates_sql(true),
            vec!["tasks_pr_url_idx", "tasks_product_idx", "SCAN t"],
        ),
        (
            "orphan sweep candidates",
            crate::dispatch::orphan_active_candidates_sql(true),
            vec!["SCAN t"],
        ),
        (
            "family work",
            unscheduled_wait::family_work_sql(),
            vec!["SCAN t", "SCAN e"],
        ),
    ] {
        let plan = plan_of(conn, &sql);
        println!(
            "wait-scale plan {name} (statistics={statistics}):\n  {}",
            plan.join("\n  ")
        );
        for walk in table_walks {
            assert!(
                !plan.iter().any(|step| step.contains(walk)),
                "{name} (statistics={statistics}): `{walk}` walks a whole table or index: {plan:#?}"
            );
        }
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH t ") && step.contains(by_primary_key)),
            "{name} (statistics={statistics}): tasks must be sought by primary key: {plan:#?}"
        );
    }
    let plan = plan_of(conn, &unscheduled_wait::family_work_sql());
    assert!(
        plan.iter()
            .any(|step| step.starts_with("SEARCH e USING") && step.contains("(work_item_id=?")),
        "family work (statistics={statistics}): review runs must be sought by item: {plan:#?}"
    );
}

fn percentile(values: &mut [f64], p: usize) -> f64 {
    values.sort_by(f64::total_cmp);
    values[(values.len() * p).div_ceil(100) - 1]
}

/// Board assembly and heartbeat cost on the seeded board. Pins the statement
/// counts; prints the timings (`--test_arg=--nocapture`).
#[test]
fn board_and_heartbeat_statements_on_the_seeded_board() {
    let (_dir, db) = open_seeded("wait-scale-board");
    let samples: usize = std::env::var("BOSS_BENCH_SAMPLES")
        .map(|value| value.parse().expect("BOSS_BENCH_SAMPLES must be an integer"))
        .unwrap_or(3);
    for (product, cards) in [(PRODUCT, BOARD_CARDS), (CONTROL_PRODUCT, CONTROL_CARDS)] {
        let assemble = |caches: Option<&BoardCaches>| {
            let start = Instant::now();
            let mut trace = PopulationTrace::new(product, "wait-scale", None, start);
            let tree = match caches {
                Some(caches) => db.get_work_tree_cached(product, &mut trace, caches),
                None => db.get_work_tree_instrumented(product, &mut trace),
            }
            .unwrap();
            let total = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(tree.tasks.len() + tree.chores.len(), cards);
            (tree, total, trace)
        };
        let counts = |trace: &PopulationTrace, name: &str| trace.segment_counts(name).unwrap();
        let (reference, total, trace) = assemble(None);
        let (ms, _, statements) = counts(&trace, segment::DB_TASK_RUNTIMES);
        println!(
            "wait-scale {product} uncached: total_ms={total:.1} task_runtimes_ms={ms:.1} statements={}",
            statements.unwrap()
        );
        let caches = BoardCaches::default();
        let (cold, total, trace) = assemble(Some(&caches));
        let (ms, _, statements) = counts(&trace, segment::DB_TASK_RUNTIMES);
        println!(
            "wait-scale {product} cold: total_ms={total:.1} task_runtimes_ms={ms:.1} statements={}",
            statements.unwrap()
        );
        let runtimes = |tree: &boss_protocol::WorkTree| serde_json::to_value(&tree.task_runtimes).unwrap();
        assert_eq!(runtimes(&cold), runtimes(&reference), "cold cached runtimes diverged");

        let (mut totals, mut runtime_ms, mut annotation_ms) = (Vec::new(), Vec::new(), Vec::new());
        let mut warm_statements = None;
        for _ in 0..samples {
            let (tree, total, trace) = assemble(Some(&caches));
            assert_eq!(runtimes(&tree), runtimes(&reference), "warm cached runtimes diverged");
            let (ms, _, statements) = counts(&trace, segment::DB_TASK_RUNTIMES);
            let (cache_ms, _, cache_statements) = counts(&trace, segment::DB_TASK_RUNTIMES_CACHE);
            let (compute_ms, compute_cards, compute_statements) = counts(&trace, segment::DB_TASK_RUNTIMES_COMPUTE);
            let (annotate_ms, annotated, annotation_statements) = counts(&trace, segment::DB_TASK_RUNTIMES_ANNOTATION);
            // The phases account for every statement of the segment.
            assert_eq!(
                statements.unwrap(),
                cache_statements.unwrap() + compute_statements.unwrap() + annotation_statements.unwrap()
            );
            assert_eq!(cache_statements, Some(1), "the epoch read");
            let branches: Vec<_> = [
                segment::DB_TASK_RUNTIMES_ANNOTATION_QUEUED,
                segment::DB_TASK_RUNTIMES_ANNOTATION_IDLE_ACTIVE,
                segment::DB_TASK_RUNTIMES_ANNOTATION_REVIEW,
            ]
            .into_iter()
            .map(|name| counts(&trace, name))
            .collect();
            assert_eq!(
                annotated.unwrap(),
                branches.iter().map(|(_, cards, _)| cards.unwrap()).sum::<i64>()
            );
            if warm_statements.is_none() {
                println!(
                    "wait-scale {product} warm phases: cache_ms={cache_ms:.2} compute_ms={compute_ms:.2} \
                     compute_cards={} compute_statements={} annotation_ms={annotate_ms:.2} annotated={} \
                     annotation_statements={} queued={:?} idle_active={:?} review={:?}",
                    compute_cards.unwrap(),
                    compute_statements.unwrap(),
                    annotated.unwrap(),
                    annotation_statements.unwrap(),
                    branches[0],
                    branches[1],
                    branches[2],
                );
            }
            warm_statements = Some((statements.unwrap(), annotation_statements.unwrap(), annotated.unwrap()));
            totals.push(total);
            runtime_ms.push(ms);
            annotation_ms.push(annotate_ms);
        }
        let (statements, annotation_statements, annotated) = warm_statements.unwrap();
        println!(
            "wait-scale {product} warm: samples={samples} total_ms p50={:.1} task_runtimes_ms p50={:.1} \
             annotation_ms p50={:.2} statements={statements} annotation_statements={annotation_statements} \
             annotated={annotated}",
            percentile(&mut totals, 50),
            percentile(&mut runtime_ms, 50),
            percentile(&mut annotation_ms, 50),
        );
        if product == PRODUCT {
            // 277 cards with nothing pending are named with 22 statements;
            // one card at a time they took 1,717.
            assert_eq!(annotated, 277, "cards named on the seeded board");
            assert_eq!(annotation_statements, 22, "statements to name {annotated} cards");
            // The epoch read, three point queries for each of the 60 cards
            // waiting on a dependency (never cached), and the annotation:
            // 203 statements, where naming one card at a time made it 1,898.
            assert_eq!(statements, 1 + 180 + 22);
        } else {
            // 30 cards, 9 statements; one card at a time it was 141.
            assert_eq!((annotated, annotation_statements, statements), (30, 8, 9));
        }
    }

    let mut millis = Vec::new();
    let mut last = None;
    for _ in 0..samples {
        let (waits, cost) = db.list_unexplained_dispatch_waits_measured().unwrap();
        millis.push(cost.millis);
        last = Some((waits.len(), cost));
    }
    let (unexplained, cost) = last.unwrap();
    println!(
        "wait-scale heartbeat: samples={samples} ms p50={:.1} statements={} pending_executions={} \
         unscheduled_items={} classified={:?} unexplained={unexplained}",
        percentile(&mut millis, 50),
        cost.statements,
        cost.pending_executions,
        cost.unscheduled_items,
        cost.classified,
    );
    assert_eq!(unexplained, 11, "the seed's unclassified queued cards");
    // 88 pending executions are projected one by one. The 239 items with
    // nothing pending are read with one statement and named with 22, where
    // one item at a time the whole pass ran 2,606 statements.
    assert_eq!((cost.pending_executions, cost.unscheduled_items), (88, 239));
    assert_eq!(cost.classified.statements(), 22, "{:?}", cost.classified);
    assert!(cost.statements <= 300, "{} statements", cost.statements);
}
