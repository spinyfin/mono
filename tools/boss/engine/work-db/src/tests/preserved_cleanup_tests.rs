use super::*;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, PreservedScope, PreservedTip};

const DAY: i64 = 24 * 60 * 60;

fn sql(db: &WorkDb, statement: &str, parameters: impl rusqlite::Params) {
    db.connect().unwrap().execute(statement, parameters).unwrap();
}

/// A finished implementation execution of `work_item_id` with a live record and
/// a preserved tip.
fn preserved_execution(db: &WorkDb, work_item_id: &str, finished_secs_ago: i64) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&execution.id, "worker", "mono", "lease", "workspace", "/workspace")
        .unwrap();
    db.record_execution_bookmark(&ExecutionBookmark {
        execution_id: execution.id.clone(),
        repo_path: "/tmp/shared".into(),
        host_id: "local".into(),
        base_commit: None,
        pr_head_before: None,
    })
    .unwrap();
    db.record_preserved_tip(
        &execution.id,
        &PreservedTip {
            change_id: "k".repeat(32),
            commit_id: "a".repeat(40),
            count: 1,
        },
    )
    .unwrap();
    db.mark_execution_orphaned(&execution.id, "worker terminated").unwrap();
    sql(
        db,
        "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![execution.id, finished_secs_ago],
    );
    execution.id
}

fn restaged(db: &WorkDb, successor: &str, predecessor: &str) {
    db.record_bookmark_recovery(successor, predecessor, true).unwrap();
}

fn publish(db: &WorkDb, execution_id: &str) {
    sql(
        db,
        "UPDATE work_executions SET pr_head_before = ?3, pr_head_after = ?2 WHERE id = ?1",
        params![execution_id, "b".repeat(40), "a".repeat(40)],
    );
}

fn ids(cleanups: &[PreservedCleanup]) -> Vec<String> {
    let mut ids: Vec<_> = cleanups.iter().map(|cleanup| cleanup.execution.id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn a_published_successor_supersedes_every_restaged_predecessor_in_its_lineage() {
    let db = WorkDb::open(temp_db_path("superseded-preserved")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "restaged twice");
    let first = preserved_execution(&db, &chore.id, 10);
    let second = preserved_execution(&db, &chore.id, 9);
    let third = preserved_execution(&db, &chore.id, 8);
    restaged(&db, &second, &first);
    restaged(&db, &third, &second);

    assert!(
        db.list_superseded_preserved(10, &[]).unwrap().is_empty(),
        "nothing has published yet"
    );
    publish(&db, &third);
    let cleanups = db.list_superseded_preserved(10, &[]).unwrap();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(
        ids(&cleanups),
        expected,
        "the published execution's own tip is not listed"
    );
    for cleanup in &cleanups {
        assert_eq!(
            cleanup.reason,
            PreservedCleanupReason::Superseded {
                published_by: third.clone()
            }
        );
        assert_eq!(cleanup.scope, PreservedScope::Tip);
    }
    assert_eq!(
        db.list_superseded_preserved(1, &[]).unwrap().len(),
        1,
        "bounded by the limit"
    );

    db.finish_preserved_cleanup(&first, PreservedScope::Tip, &[]).unwrap();
    assert_eq!(ids(&db.list_superseded_preserved(10, &[]).unwrap()), vec![second]);
}

#[test]
fn a_successor_that_did_not_restage_the_work_supersedes_nothing() {
    let db = WorkDb::open(temp_db_path("unrelated-published")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "fresh start");
    let abandoned = preserved_execution(&db, &chore.id, 10);
    let fresh = preserved_execution(&db, &chore.id, 9);
    publish(&db, &fresh);
    assert!(db.list_superseded_preserved(10, &[]).unwrap().is_empty());
    assert!(db.preserved_tip(&abandoned).unwrap().is_some());
}

#[test]
fn a_restage_cycle_in_the_record_cannot_loop_the_listing() {
    let db = WorkDb::open(temp_db_path("restage-cycle")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "corrupt lineage");
    let a = preserved_execution(&db, &chore.id, 10);
    let b = preserved_execution(&db, &chore.id, 9);
    restaged(&db, &a, &b);
    restaged(&db, &b, &a);
    publish(&db, &a);
    let cleanups = db.list_superseded_preserved(10, &[]).unwrap();
    assert_eq!(ids(&cleanups), vec![b]);
}

#[test]
fn a_closed_chain_lists_its_tip_only_after_the_retention_period() {
    let db = WorkDb::open(temp_db_path("closed-chain-preserved")).unwrap();
    let product = create_test_product(&db);
    let open = create_test_chore_manual(&db, product.id.clone(), "still open");
    let recent = create_test_chore_manual(&db, product.id.clone(), "closed recently");
    let old = create_test_chore_manual(&db, product.id.clone(), "closed long ago");
    let archived = create_test_chore_manual(&db, product.id, "archived long ago");
    let kept = preserved_execution(&db, &open.id, 100 * DAY);
    let _recent = preserved_execution(&db, &recent.id, 100 * DAY);
    let done = preserved_execution(&db, &old.id, 100 * DAY);
    let gone = preserved_execution(&db, &archived.id, 100 * DAY);
    sql(
        &db,
        "UPDATE tasks SET status = 'done', completed_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![recent.id, DAY],
    );
    sql(
        &db,
        "UPDATE tasks SET status = 'done', completed_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![old.id, 30 * DAY],
    );
    sql(
        &db,
        "UPDATE tasks SET status = 'archived', completed_at = NULL,
             archived_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![archived.id, 30 * DAY],
    );

    let cleanups = db
        .list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 10, &[])
        .unwrap();
    let mut expected = vec![done.clone(), gone.clone()];
    expected.sort();
    assert_eq!(ids(&cleanups), expected);
    assert!(
        !ids(&cleanups).contains(&kept),
        "an open item keeps its tip however old"
    );
    for cleanup in &cleanups {
        assert!(matches!(
            cleanup.reason,
            PreservedCleanupReason::ClosedChain { closed_at_epoch } if closed_at_epoch > 0
        ));
        assert_eq!(cleanup.scope, PreservedScope::TipAndArchives);
    }
    assert_eq!(
        db.list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 1, &[])
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.list_closed_chain_preserved(60 * DAY, 10, &[]).unwrap().is_empty(),
        "a longer retention period holds everything back"
    );
}

#[test]
fn a_revision_is_judged_by_its_chain_root() {
    let db = WorkDb::open(temp_db_path("closed-chain-revision")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id.clone(), "root");
    let revision = create_test_chore_manual(&db, product.id, "revision");
    sql(
        &db,
        "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
        params![revision.id, root.id],
    );
    let tip = preserved_execution(&db, &revision.id, 100 * DAY);
    assert!(
        db.list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 10, &[])
            .unwrap()
            .is_empty()
    );
    sql(
        &db,
        "UPDATE tasks SET status = 'done', completed_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![root.id, 30 * DAY],
    );
    assert_eq!(
        ids(&db
            .list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 10, &[])
            .unwrap()),
        vec![tip]
    );
}

#[test]
fn an_open_inspection_request_keeps_the_archives_but_not_the_tip() {
    use boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
    let db = WorkDb::open(temp_db_path("closed-chain-inspection")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "needs inspection");
    let tip = preserved_execution(&db, &chore.id, 100 * DAY);
    sql(
        &db,
        "UPDATE tasks SET status = 'done', completed_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
        params![chore.id, 30 * DAY],
    );
    db.upsert_execution_attention(&tip, PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND, "Inspect", "body")
        .unwrap();
    let cleanups = db
        .list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 10, &[])
        .unwrap();
    assert_eq!(ids(&cleanups), vec![tip]);
    assert_eq!(cleanups[0].scope, PreservedScope::Tip);
}

#[test]
fn finishing_a_cleanup_clears_the_tip_and_resolves_only_the_named_attention() {
    use boss_engine_storage_values::attention_kinds::{
        ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND, PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND,
    };
    let db = WorkDb::open(temp_db_path("finish-preserved")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "finish");
    let tip = preserved_execution(&db, &chore.id, DAY);
    for kind in [
        ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND,
        PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND,
    ] {
        db.upsert_execution_attention(&tip, kind, "title", "body").unwrap();
    }
    db.finish_preserved_cleanup(
        &tip,
        PreservedScope::Tip,
        &[ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND],
    )
    .unwrap();
    assert!(db.preserved_tip(&tip).unwrap().is_none());
    let status = |kind: &str| {
        db.list_attention_items(&tip)
            .unwrap()
            .into_iter()
            .find(|item| item.kind == kind)
            .unwrap()
            .status
    };
    assert_eq!(status(ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND), "resolved");
    assert_eq!(status(PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND), "open");
    // The record that says where the store is outlives the tip.
    assert!(db.preservation_source(&tip).unwrap().is_some());
}

#[test]
fn retiring_the_prune_state_drops_its_columns_and_resolves_its_open_items() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE execution_bookmarks (
             execution_id TEXT PRIMARY KEY, repo_path TEXT NOT NULL, host_id TEXT NOT NULL,
             prune_checked_at INTEGER, prune_outcome TEXT, has_recovery_head INTEGER);
         INSERT INTO execution_bookmarks VALUES ('e', '/repo', 'local', 1, 'unproven', 1);
         CREATE TABLE work_attention_items (id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL, resolved_at TEXT);
         INSERT INTO work_attention_items VALUES ('open', 'execution_bookmark_prune_unproven', 'open', NULL);
         INSERT INTO work_attention_items VALUES ('other', 'preserved_work_archived', 'open', NULL);",
    )
    .unwrap();
    for _ in 0..2 {
        crate::execution_bookmarks::migrate_retire_prune_state(&conn).unwrap();
    }
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('execution_bookmarks')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(columns, ["execution_id", "repo_path", "host_id"]);
    let status = |id: &str| -> (String, Option<String>) {
        conn.query_row(
            "SELECT status, resolved_at FROM work_attention_items WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let (resolved, at) = status("open");
    assert_eq!(resolved, "resolved");
    assert!(at.is_some());
    assert_eq!(status("other"), ("open".to_owned(), None));
}

#[test]
fn a_metadata_only_successor_keeps_its_predecessors_tip() {
    let db = WorkDb::open(temp_db_path("metadata-only-preserved")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "metadata only");
    let prior = preserved_execution(&db, &chore.id, DAY);
    let successor = preserved_execution(&db, &chore.id, 1);
    restaged(&db, &successor, &prior);
    publish(&db, &successor);
    sql(
        &db,
        "UPDATE work_executions SET pr_head_before = pr_head_after WHERE id = ?1",
        [&successor],
    );
    assert!(db.list_superseded_preserved(10, &[]).unwrap().is_empty());
    assert!(db.preserved_tip(&prior).unwrap().is_some());
    sql(
        &db,
        "UPDATE work_executions SET pr_head_after = 'new-head' WHERE id = ?1",
        [&successor],
    );
    assert_eq!(
        ids(&db.list_superseded_preserved(10, &[]).unwrap()),
        vec![prior.clone()]
    );
    assert!(db.list_superseded_preserved(1, &[prior]).unwrap().is_empty());
}

#[test]
fn a_successor_without_a_head_baseline_does_not_count_as_published() {
    let db = WorkDb::open(temp_db_path("null-baseline-preserved")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "no baseline");
    let prior = preserved_execution(&db, &chore.id, DAY);
    let successor = preserved_execution(&db, &chore.id, 1);
    restaged(&db, &successor, &prior);
    sql(
        &db,
        "UPDATE work_executions SET pr_head_before = NULL, pr_head_after = 'new-head' WHERE id = ?1",
        [&successor],
    );
    assert!(db.list_superseded_preserved(10, &[]).unwrap().is_empty());
    assert!(db.preserved_tip(&prior).unwrap().is_some());
}

#[test]
fn superseded_listing_resumes_after_the_cursor() {
    let db = WorkDb::open(temp_db_path("superseded-cursor")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "cursor");
    let first = preserved_execution(&db, &chore.id, 10);
    let second = preserved_execution(&db, &chore.id, 9);
    let third = preserved_execution(&db, &chore.id, 8);
    restaged(&db, &second, &first);
    restaged(&db, &third, &second);
    publish(&db, &third);
    let page = db.list_superseded_preserved_after(1, &[], None).unwrap();
    assert_eq!(ids(&page), vec![first.clone()]);
    let page = db.list_superseded_preserved_after(1, &[], Some(&first)).unwrap();
    assert_eq!(ids(&page), vec![second.clone()]);
    assert!(
        db.list_superseded_preserved_after(1, &[], Some(&second))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn archive_cleanup_migration_is_idempotent_and_flags_only_records_that_may_hold_references() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE execution_bookmarks (execution_id TEXT PRIMARY KEY);
         CREATE TABLE work_executions (id TEXT PRIMARY KEY, preserved_commit_id TEXT);
         CREATE TABLE work_attention_items (id TEXT PRIMARY KEY, execution_id TEXT, kind TEXT);
         INSERT INTO execution_bookmarks VALUES ('tip'), ('archived'), ('inspect'), ('plain');
         INSERT INTO work_executions VALUES ('tip', 'abc'), ('archived', NULL), ('inspect', NULL), ('plain', NULL);
         INSERT INTO work_attention_items VALUES
            ('a1', 'archived', 'preserved_work_archived'),
            ('a2', 'inspect', 'preserved_work_needs_inspection'),
            ('a3', 'plain', 'something_else');",
    )
    .unwrap();
    for _ in 0..2 {
        crate::preserved_cleanup::migrate_archive_cleanup(&conn).unwrap();
    }
    conn.execute("INSERT INTO execution_bookmarks (execution_id) VALUES ('new')", [])
        .unwrap();
    let mut stmt = conn
        .prepare("SELECT execution_id FROM execution_bookmarks WHERE archive_cleanup_pending = 1 ORDER BY 1")
        .unwrap();
    let pending: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(pending, ["archived", "inspect", "tip"]);
}
