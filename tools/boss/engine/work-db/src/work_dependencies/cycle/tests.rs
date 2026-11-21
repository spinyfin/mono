use super::*;
use crate::work_dependencies::{EdgeInsertOutcome, RELATION_MERGE_ORDER, insert_edge, query_edge};

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT DEFAULT 'chore', parent_task_id TEXT);
         CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE work_item_dependencies (
            dependent_id TEXT, prerequisite_id TEXT, relation TEXT, created_at TEXT,
            PRIMARY KEY (dependent_id, prerequisite_id, relation));
         INSERT INTO tasks (id, name) VALUES ('a', 'Schema'), ('b', 'Engine'), ('c', 'CLI');
         INSERT INTO projects VALUES ('d', 'Release');",
    )
    .unwrap();
    conn
}

fn add(conn: &Connection, dependent: &str, prerequisite: &str) {
    insert_edge(conn, dependent, prerequisite, RELATION_BLOCKS, "1").unwrap();
}

fn assert_cycle(conn: &Connection, dependent: &str, prerequisite: &str, ids: &[&str], names: &[&str]) {
    let err = insert_edge(conn, dependent, prerequisite, RELATION_BLOCKS, "2").unwrap_err();
    let cycle = err.downcast_ref::<DependencyCycleError>().expect("typed cycle error");
    assert_eq!(cycle.path.iter().map(|step| step.id.as_str()).collect::<Vec<_>>(), ids);
    assert_eq!(
        cycle.path.iter().map(|step| step.name.as_str()).collect::<Vec<_>>(),
        names
    );
    assert!(cycle.path.iter().all(|step| step.relation == RELATION_BLOCKS));
    assert!(
        query_edge(conn, dependent, prerequisite, RELATION_BLOCKS)
            .unwrap()
            .is_none()
    );
    assert!(conn.is_autocommit(), "failed insert must release its savepoint");
}

#[test]
fn refuses_direct_cycle_at_low_level_insert() {
    let conn = fixture();
    add(&conn, "a", "b");
    assert_cycle(&conn, "b", "a", &["b", "a"], &["Engine", "Schema"]);
}

#[test]
fn refuses_long_cycle_with_full_path_including_project() {
    let conn = fixture();
    add(&conn, "a", "b");
    add(&conn, "b", "c");
    add(&conn, "c", "d");
    assert_cycle(
        &conn,
        "d",
        "a",
        &["d", "a", "b", "c"],
        &["Release", "Schema", "Engine", "CLI"],
    );
}

#[test]
fn refuses_self_edge_for_every_relation() {
    let conn = fixture();
    assert_cycle(&conn, "a", "a", &["a"], &["Schema"]);
    let err = insert_edge(&conn, "a", "a", RELATION_MERGE_ORDER, "1").unwrap_err();
    let cycle = err.downcast_ref::<DependencyCycleError>().unwrap();
    assert_eq!(cycle.path[0].relation, RELATION_MERGE_ORDER);
}

#[test]
fn accepts_diamond_and_idempotent_readd() {
    let conn = fixture();
    for (dependent, prerequisite) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")] {
        add(&conn, dependent, prerequisite);
    }
    let (_, outcome) = insert_edge(&conn, "a", "b", RELATION_BLOCKS, "2").unwrap();
    assert_eq!(outcome, EdgeInsertOutcome::AlreadyExists);
}

#[test]
fn soft_pairings_remain_independent_of_gating_cycles_in_either_order() {
    for soft_first in [true, false] {
        let conn = fixture();
        if soft_first {
            insert_edge(&conn, "a", "b", RELATION_MERGE_ORDER, "1").unwrap();
        }
        add(&conn, "b", "a");
        if !soft_first {
            insert_edge(&conn, "a", "b", RELATION_MERGE_ORDER, "1").unwrap();
        }
        assert_cycle(&conn, "a", "b", &["a", "b"], &["Schema", "Engine"]);
    }
}

#[test]
fn walk_terminates_with_an_unrelated_legacy_cycle() {
    let conn = fixture();
    conn.execute_batch(
        "INSERT INTO work_item_dependencies VALUES ('b', 'c', 'blocks', '1'), ('c', 'b', 'blocks', '1');",
    )
    .unwrap();
    add(&conn, "a", "b");
    assert_cycle(&conn, "b", "a", &["b", "a"], &["Engine", "Schema"]);
}

fn revision(conn: &Connection, id: &str, parent: &str) {
    conn.execute(
        "INSERT INTO tasks (id, name, kind, parent_task_id) VALUES (?1, ?1, 'revision', ?2)",
        [id, parent],
    )
    .unwrap();
}

#[test]
fn refuses_revision_pr_cycle_in_both_edge_orders_and_through_nested_parents() {
    for reverse in [false, true] {
        let conn = fixture();
        revision(&conn, "review", "a");
        revision(&conn, "revision", "review");
        // The ordinary same-PR ordering remains legal.
        add(&conn, "revision", "review");
        let (first, second) = if reverse {
            (("b", "a"), ("revision", "b"))
        } else {
            (("revision", "b"), ("b", "a"))
        };
        add(&conn, first.0, first.1);
        let err = insert_edge(&conn, second.0, second.1, RELATION_BLOCKS, "2").unwrap_err();
        let cycle = err.downcast_ref::<DependencyCycleError>().unwrap();
        assert!(cycle.path.iter().any(|step| step.id == "a"));
        assert!(cycle.path.iter().any(|step| step.id == "b"));
        assert!(
            query_edge(&conn, second.0, second.1, RELATION_BLOCKS)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn follows_prerequisite_revision_parents_and_transitive_external_dependencies() {
    let conn = fixture();
    revision(&conn, "revision", "a");
    revision(&conn, "other_revision", "c");
    add(&conn, "revision", "b");
    add(&conn, "b", "other_revision");
    let err = insert_edge(&conn, "c", "a", RELATION_BLOCKS, "2").unwrap_err();
    assert!(
        err.downcast_ref::<DependencyCycleError>()
            .unwrap()
            .path
            .iter()
            .any(|step| step.relation == "revision_parent")
    );
}

#[test]
fn legacy_cycle_members_agree_between_dispatch_and_progression() {
    let conn = fixture();
    revision(&conn, "revision", "a");
    revision(&conn, "later", "a");
    conn.execute_batch(
        "INSERT INTO work_item_dependencies VALUES
         ('revision', 'b', 'blocks', '1'), ('b', 'a', 'blocks', '1'),
         ('later', 'revision', 'blocks', '1'), ('c', 'a', 'blocks', '1');",
    )
    .unwrap();
    let mut stmt = conn.prepare(&members_sql()).unwrap();
    let members = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (id, cyclic) in [
        ("a", true),
        ("b", true),
        ("revision", true),
        ("later", false),
        ("c", false),
    ] {
        assert_eq!(prerequisites(&conn, id).unwrap().0, cyclic, "dispatch: {id}");
        assert_eq!(members.iter().any(|member| member == id), cyclic, "progression: {id}");
        assert_eq!(
            prerequisites(&conn, id).unwrap().0 && !prerequisites(&conn, id).unwrap().1.is_empty(),
            cyclic,
            "gates: {id}"
        );
    }
    // A soft relation to the PR root cannot create the same loop.
    conn.execute(
        "UPDATE work_item_dependencies SET relation = 'merge_order' WHERE dependent_id = 'b'",
        [],
    )
    .unwrap();
    assert!(!prerequisites(&conn, "revision").unwrap().0);
    assert!(!prerequisites(&conn, "a").unwrap().0);
}
