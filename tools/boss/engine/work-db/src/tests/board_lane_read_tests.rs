//! A complete board read orders its lanes after reading them. Ordering in SQL
//! made SQLite sort every card's description through a sorter that keeps
//! about 2 MB in memory and writes the rest to a temporary file, so the read
//! stalled whenever the disk was busy. These tests pin both halves of the
//! replacement: SQLite does no sort, and the order is the one it produced.
use super::*;
use crate::board_patch::{Lane, read_lane};
use boss_metrics::population_timing::{PopulationTrace, segment};

/// The SQL `ORDER BY` equivalent of each lane's board order.
/// Tests compare `task_order` and `chore_order` against it to detect divergence.
fn sql_order(lane: Lane) -> &'static str {
    match lane {
        Lane::Tasks => "COALESCE(ordinal, 0) ASC, created_at ASC, id ASC",
        Lane::Chores => "created_at ASC, id ASC",
    }
}

fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(["any-product"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Run `sql` to completion for `product` and return how many sorts SQLite
/// performed doing it.
fn sorts_run(conn: &Connection, sql: &str, product: &str) -> i32 {
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt
        .query_map([product], |row| row.get::<_, String>(0))
        .unwrap()
        .count();
    assert!(rows > 0, "the probe must read rows to mean anything");
    stmt.get_status(rusqlite::StatementStatus::Sort)
}

fn assert_no_sqlite_sort(conn: &Connection, product: &str, context: &str) {
    for lane in [Lane::Tasks, Lane::Chores] {
        let sql = lane.query();
        let steps = plan(conn, &sql);
        assert!(
            !steps.iter().any(|step| step.contains("TEMP B-TREE")),
            "{context}: the {lane:?} lane read must not sort in SQLite: {steps:#?}"
        );
        assert_eq!(sorts_run(conn, &sql, product), 0, "{context}: {lane:?} lane");

        // The same read with its former ORDER BY, to prove both probes see a
        // sort on this SQLite build when there is one.
        let sorted = format!("{sql} ORDER BY {}", sql_order(lane));
        let steps = plan(conn, &sorted);
        assert!(
            steps.iter().any(|step| step.contains("TEMP B-TREE FOR ORDER BY")),
            "{context}: expected the ordered {lane:?} read to need a temporary b-tree: {steps:#?}"
        );
        assert!(
            sorts_run(conn, &sorted, product) >= 1,
            "{context}: ordered {lane:?} lane"
        );
    }
}

fn insert_card(conn: &Connection, product: &str, id: &str, kind: &str, ordinal: Option<i64>, created_at: &str) {
    conn.execute(
        "INSERT INTO tasks(id, product_id, kind, name, status, ordinal, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?1, 'todo', ?4, ?5, ?5)",
        params![id, product, kind, ordinal, created_at],
    )
    .unwrap();
}

#[test]
fn lane_reads_do_not_sort_in_sqlite() {
    // A freshly initialised database, planned from the statistics startup
    // leaves behind.
    let db = WorkDb::open(temp_db_path("lane-plan")).unwrap();
    let product = make_revision_product(&db, "lane-plan");
    {
        let conn = db.connect().unwrap();
        insert_card(&conn, &product, "plan-task", "project_task", Some(1), "1");
        insert_card(&conn, &product, "plan-chore", "chore", None, "1");
        assert_no_sqlite_sort(&conn, &product, "fresh database");
    }
}

#[cfg(boss_scale_tests)]
mod board_lane_scale_tests {
    use super::*;

    #[test]
    fn lane_reads_do_not_sort_in_sqlite_with_production_statistics() {
        // The production-cardinality fixture, before and after a full ANALYZE:
        // the planner picks different access paths for the lanes once it has
        // real statistics, and none of them may bring a sort back.
        let (_dir, db) = crate::tests::work_tree_scale_tests::open_seeded("lane-plan");
        let conn = db.connect().unwrap();
        insert_card(&conn, "scale-product-0", "plan-chore", "chore", None, "1");
        assert_no_sqlite_sort(&conn, "scale-product-0", "scale fixture");
        conn.execute_batch("ANALYZE").unwrap();
        assert_no_sqlite_sort(&conn, "scale-product-0", "scale fixture with full statistics");
    }
}

#[test]
fn lanes_keep_the_order_the_sql_sort_produced() {
    let db = WorkDb::open(temp_db_path("lane-order")).unwrap();
    let product = make_revision_product(&db, "lane-order");
    let other = make_revision_product(&db, "lane-order-other");
    {
        let conn = db.connect().unwrap();
        // Ordinals: absent sorts as 0, between negative and positive; ties
        // fall through to creation time and then id. Creation times and ids
        // compare as text, byte by byte: "10" before "9", upper case before
        // lower, and a non-ASCII id after every ASCII one.
        let tasks: &[(&str, &str, Option<i64>, &str)] = &[
            ("task-b", "project_task", Some(2), "5"),
            ("task-a", "project_task", Some(2), "5"),
            ("task-B", "project_task", Some(2), "5"),
            ("task-é", "project_task", Some(2), "5"),
            ("task-late", "project_task", Some(2), "9"),
            ("task-ten", "project_task", Some(2), "10"),
            ("task-none", "project_task", None, "7"),
            ("task-zero", "design", Some(0), "7"),
            ("task-zero-early", "investigation", Some(0), "3"),
            ("task-negative", "project_task", Some(-4), "8"),
            ("task-first", "project_task", Some(1), "2026-11-17T23:57:04Z"),
            ("task-dated", "investigation", None, "2026-11-17T23:39:14Z"),
            ("task-postmortem", "design_postmortem", Some(40), "1"),
        ];
        for (id, kind, ordinal, created_at) in tasks {
            insert_card(&conn, &product, id, kind, *ordinal, created_at);
        }
        // Chores ignore ordinals entirely.
        let chores: &[(&str, &str, Option<i64>, &str)] = &[
            ("chore-b", "chore", Some(1), "5"),
            ("chore-a", "followup", Some(9), "5"),
            ("chore-B", "chore", None, "5"),
            ("chore-é", "chore", None, "5"),
            ("chore-ten", "chore", Some(-3), "10"),
            ("chore-nine", "followup", None, "9"),
            ("chore-dated", "chore", None, "2026-11-17T23:39:14Z"),
        ];
        for (id, kind, ordinal, created_at) in chores {
            insert_card(&conn, &product, id, kind, *ordinal, created_at);
        }
        // Rows the board never shows must stay out of both lanes.
        insert_card(&conn, &product, "gone-archived", "chore", None, "1");
        insert_card(&conn, &product, "gone-deleted", "project_task", Some(0), "1");
        insert_card(&conn, &other, "gone-elsewhere", "chore", None, "1");
        conn.execute_batch(
            "UPDATE tasks SET status = 'archived' WHERE id = 'gone-archived';
             UPDATE tasks SET deleted_at = '2' WHERE id = 'gone-deleted';",
        )
        .unwrap();
    }

    let ids = |cards: &[Task]| cards.iter().map(|card| card.id.clone()).collect::<Vec<_>>();
    let tree = db.get_work_tree(&product).unwrap();
    let conn = db.connect().unwrap();
    for (lane, in_tree, expected_len) in [(Lane::Tasks, &tree.tasks, 13), (Lane::Chores, &tree.chores, 7)] {
        let reference: Vec<String> = conn
            .prepare(&format!("{} ORDER BY {}", lane.query(), sql_order(lane)))
            .unwrap()
            .query_map([&product], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(reference.len(), expected_len, "{lane:?}: {reference:?}");
        assert_eq!(ids(&read_lane(&conn, &product, lane).unwrap()), reference, "{lane:?}");
        assert_eq!(ids(in_tree), reference, "{lane:?} lane of the assembled tree");
    }
    // Spell the order out too, so a wrong ORDER BY in the reference cannot
    // make the comparison above vacuous.
    assert_eq!(
        ids(&tree.tasks),
        [
            "task-negative",
            "task-dated",
            "task-zero-early",
            "task-none",
            "task-zero",
            "task-first",
            "task-ten",
            "task-B",
            "task-a",
            "task-b",
            "task-é",
            "task-late",
            "task-postmortem",
        ]
    );
    assert_eq!(
        ids(&tree.chores),
        [
            "chore-ten",
            "chore-dated",
            "chore-B",
            "chore-a",
            "chore-b",
            "chore-é",
            "chore-nine"
        ]
    );
}

#[test]
fn a_complete_read_times_the_snapshot_and_each_lane_with_cpu_time() {
    let db = WorkDb::open(temp_db_path("lane-timing")).unwrap();
    let product = make_revision_product(&db, "lane-timing");
    create_test_chore_manual(&db, product.clone(), "a card");

    let caches = crate::board_caches::BoardCaches::default();
    let mut model = None;
    let mut trace = PopulationTrace::new(product.clone(), "req-cold", None, std::time::Instant::now());
    db.refresh_board_projection(&product, &mut model, &mut trace, &caches, 100)
        .unwrap();
    for stage in [segment::DB_SNAPSHOT, segment::DB_TASKS, segment::DB_CHORES] {
        let (wall_ms, _, _) = trace
            .segment_counts(stage)
            .unwrap_or_else(|| panic!("{stage} must be recorded"));
        let cpu_ms = trace
            .segment_cpu_ms(stage)
            .unwrap_or_else(|| panic!("{stage} must carry cpu_ms"));
        assert!(wall_ms >= 0.0 && cpu_ms >= 0.0, "{stage}: wall {wall_ms}, cpu {cpu_ms}");
    }

    // A warm refresh runs none of the lane reads, but it still takes the
    // connection and pins a snapshot, and a wait there must be visible too.
    let mut trace = PopulationTrace::new(product.clone(), "req-warm", None, std::time::Instant::now());
    let (_, refresh) = db
        .refresh_board_projection(&product, &mut model, &mut trace, &caches, 100)
        .unwrap();
    assert!(
        matches!(refresh, crate::board_projection::Refresh::Incremental(_)),
        "{refresh:?}"
    );
    assert!(trace.segment_cpu_ms(segment::DB_SNAPSHOT).is_some());
    assert!(trace.segment_counts(segment::DB_TASKS).is_none());
}
