use super::*;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, LegacyArchive, LegacyCleanupReport};

fn sql(db: &WorkDb, statement: &str, parameters: impl rusqlite::Params) {
    db.connect().unwrap().execute(statement, parameters).unwrap();
}

/// A started execution with a recovery record in `repo_path` on `host_id`.
fn recorded_execution(db: &WorkDb, work_item_id: &str, host_id: &str, repo_path: &str, cube_repo: &str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&execution.id, "worker", cube_repo, "lease", "workspace", "/workspace")
        .unwrap();
    db.record_execution_bookmark(&ExecutionBookmark {
        execution_id: execution.id.clone(),
        repo_path: repo_path.into(),
        host_id: host_id.into(),
        base_commit: None,
        pr_head_before: None,
    })
    .unwrap();
    db.mark_execution_orphaned(&execution.id, "worker terminated").unwrap();
    execution.id
}

/// Forget the migration ran, as an engine that predates it would have left the
/// database, then open it again so the upgrade runs.
fn reapply_migration(path: &Path) -> WorkDb {
    {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("DROP TABLE legacy_store_cleanups").unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE id LIKE '%legacy_store_cleanup'",
            [],
        )
        .unwrap();
    }
    WorkDb::open(path.to_path_buf()).unwrap()
}

fn report(archived: &[&str], forgotten: usize, kept: usize) -> LegacyCleanupReport {
    LegacyCleanupReport {
        found: forgotten + kept,
        archived: archived
            .iter()
            .map(|execution_id| LegacyArchive {
                execution_id: (*execution_id).to_owned(),
                source: format!("boss-recovery/{execution_id}"),
                archive: format!("preserved-archive/{execution_id}/{}", "a".repeat(40)),
                commit_id: "a".repeat(40),
            })
            .collect(),
        forgotten,
        kept: vec!["boss/exec_live".to_owned(); kept],
        operations: 2,
    }
}

#[test]
fn the_upgrade_lists_each_store_the_engine_ever_used_once() {
    let (_dir, path) = disk_db_path("legacy-store-cleanup-seed");
    let db = WorkDb::open(path.clone()).unwrap();
    assert!(db.pending_legacy_store_cleanups(0).unwrap().is_empty(), "fresh install");
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "dispatched twice");
    recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    recorded_execution(&db, &chore.id, "remote", "/stores/mono", "mono-remote");
    recorded_execution(&db, &chore.id, "local", "/stores/other", "other");
    drop(db);

    let db = reapply_migration(&path);
    let pending = db.pending_legacy_store_cleanups(0).unwrap();
    let listed: Vec<_> = pending
        .iter()
        .map(|store| {
            (
                store.host_id.as_str(),
                store.repo_path.to_str().unwrap(),
                store.cube_repo_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            ("local", "/stores/mono", Some("mono")),
            ("local", "/stores/other", Some("other")),
            ("remote", "/stores/mono", Some("mono-remote")),
        ]
    );

    // A store the engine starts using afterwards holds none of these names and
    // is never listed.
    recorded_execution(&db, &chore.id, "local", "/stores/new", "new");
    drop(db);
    let db = WorkDb::open(path).unwrap();
    assert_eq!(db.pending_legacy_store_cleanups(0).unwrap().len(), 3);
}

#[test]
fn a_failed_attempt_waits_before_the_store_is_offered_again() {
    let db = WorkDb::open(temp_db_path("legacy-store-cleanup-retry")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "retry");
    recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    sql(
        &db,
        "INSERT OR IGNORE INTO legacy_store_cleanups (host_id, repo_path) VALUES ('local', '/stores/mono')",
        [],
    );
    let store = Path::new("/stores/mono");

    assert_eq!(db.pending_legacy_store_cleanups(3600).unwrap().len(), 1);
    db.begin_legacy_store_cleanup("local", store).unwrap();
    db.fail_legacy_store_cleanup("local", store, "host unreachable")
        .unwrap();
    assert!(
        db.pending_legacy_store_cleanups(3600).unwrap().is_empty(),
        "just attempted"
    );
    sql(
        &db,
        "UPDATE legacy_store_cleanups SET last_attempt_at = CAST(unixepoch('now') - 3601 AS TEXT)",
        [],
    );
    let pending = db.pending_legacy_store_cleanups(3600).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].attempts, 1);
    let status = db.legacy_store_cleanup_status("local", store).unwrap().unwrap();
    assert!(!status.completed);
    assert_eq!(status.last_error.as_deref(), Some("host unreachable"));
}

#[test]
fn finishing_completes_the_store_for_good_and_queues_the_archives_for_cleanup() {
    let db = WorkDb::open(temp_db_path("legacy-store-cleanup-finish")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "archived work");
    let archived = recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    let untouched = recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    sql(
        &db,
        "INSERT OR IGNORE INTO legacy_store_cleanups (host_id, repo_path) VALUES ('local', '/stores/mono')",
        [],
    );
    let store = Path::new("/stores/mono");
    db.begin_legacy_store_cleanup("local", store).unwrap();
    db.fail_legacy_store_cleanup("local", store, "first try failed")
        .unwrap();

    db.finish_legacy_store_cleanup("local", store, &report(&[archived.as_str()], 5, 1))
        .unwrap();
    db.record_legacy_store_gc("local", store, Some("cube repo gc timed out"))
        .unwrap();

    assert!(db.pending_legacy_store_cleanups(0).unwrap().is_empty());
    let status = db.legacy_store_cleanup_status("local", store).unwrap().unwrap();
    assert_eq!(
        status,
        LegacyStoreCleanupStatus::builder()
            .completed(true)
            .attempts(1)
            .archived(1)
            .forgotten(5)
            .kept(1)
            .gc_error("cube repo gc timed out")
            .build()
    );
    let pending_flag = |id: &str| -> i64 {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT archive_cleanup_pending FROM execution_bookmarks WHERE execution_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        pending_flag(&archived),
        1,
        "closed-chain cleanup will forget its archives"
    );
    assert_eq!(pending_flag(&untouched), 0);
}

#[test]
fn pushed_heads_come_from_every_recorded_source_as_lowercase_full_commit_ids() {
    let db = WorkDb::open(temp_db_path("legacy-store-cleanup-heads")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "pushed");
    let execution = recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    sql(
        &db,
        "UPDATE work_executions SET pr_head_before = ?2, pr_head_after = ?3 WHERE id = ?1",
        params![execution, "1".repeat(40), "2".repeat(40)],
    );
    sql(
        &db,
        "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
        params![chore.id, "A".repeat(40)],
    );
    sql(
        &db,
        "INSERT INTO conflict_resolutions
            (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, head_sha_before,
             head_sha_after, status, created_at)
         VALUES ('res', ?1, ?2, 'https://github.com/o/r/pull/1', 1, 'boss/exec', 'main', ?3, ?4, 'resolved', '1')",
        params![product.id, chore.id, "4".repeat(40), "2".repeat(40)],
    );
    // Not full commit ids: an abbreviation and a revset fragment.
    sql(
        &db,
        "INSERT INTO conflict_resolutions
            (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, head_sha_before,
             head_sha_after, status, created_at)
         VALUES ('res2', ?1, ?2, 'https://github.com/o/r/pull/2', 2, 'boss/exec', 'main', 'abc1234', 'main) | all(', 'resolved', '1')",
        params![product.id, chore.id],
    );

    assert_eq!(
        db.pushed_pr_heads().unwrap(),
        vec!["1".repeat(40), "2".repeat(40), "4".repeat(40), "a".repeat(40)],
        "sorted, deduplicated, with the uppercase head lowercased and the fragments dropped"
    );
}

#[test]
fn new_archives_on_an_old_closed_chain_survive_until_inspected() {
    let db = WorkDb::open(temp_db_path("legacy-old-chain-archives")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "old closed chain");
    let execution = recorded_execution(&db, &chore.id, "local", "/stores/mono", "mono");
    sql(
        &db,
        "UPDATE tasks SET status = 'done', completed_at = '1', updated_at = '1' WHERE id = ?1",
        [&chore.id],
    );
    sql(
        &db,
        "INSERT INTO legacy_store_cleanups (host_id, repo_path) VALUES ('local', '/stores/mono')",
        [],
    );
    let archived = report(&[&execution], 1, 0);
    for _ in 0..2 {
        db.finish_legacy_store_cleanup("local", Path::new("/stores/mono"), &archived)
            .unwrap();
    }
    assert!(
        db.list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 100, &[])
            .unwrap()
            .is_empty(),
        "new inspection archives survive even though the chain closed long ago"
    );
    let conn = db.connect().unwrap();
    let (count, body): (i64, String) = conn.query_row(
        "SELECT count(*), body_markdown FROM work_attention_items WHERE execution_id = ?1 AND kind = 'preserved_work_needs_inspection' AND status = 'open'",
        [&execution], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(count, 1, "one deduplicated inspection item per execution");
    assert!(body.contains(&archived.archived[0].archive));
    conn.execute(
        "UPDATE work_attention_items SET status = 'resolved' WHERE execution_id = ?1",
        [&execution],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        db.list_closed_chain_preserved(CLOSED_CHAIN_RETENTION_SECS, 100, &[])
            .unwrap()
            .len(),
        1,
        "human inspection releases the archives"
    );
}
