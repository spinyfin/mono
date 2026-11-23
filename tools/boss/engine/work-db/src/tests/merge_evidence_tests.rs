use super::*;

const PR: &str = "https://github.com/spinyfin/mono/pull/7";
const OTHER_PR: &str = "https://github.com/spinyfin/mono/pull/8";

fn fixture(db: &WorkDb) -> String {
    let product = make_revision_product(db, "merge evidence");
    make_in_review_chore(db, &product, PR)
}

fn journal_count(db: &WorkDb, root: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM board_changes WHERE item_id = ?1 AND source = 'pr_merge_evidence'",
            [root],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn manual_completion_and_closed_unmerged_are_not_merge_evidence() {
    let db = WorkDb::open(temp_db_path("manual-completion")).unwrap();
    let root = fixture(&db);
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
    let closed = fixture(&db);
    db.mark_chore_pr_closed_unmerged(&closed, PR).unwrap();
    assert!(db.get_merge_evidence(&closed).unwrap().is_none());
}

#[test]
fn terminal_observations_are_durable_idempotent_and_journaled() {
    let db = WorkDb::open(temp_db_path("terminal-merge")).unwrap();
    for status in ["done", "archived"] {
        let root = fixture(&db);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![root, status])
            .unwrap();
        assert!(db.mark_chore_pr_merged(&root, PR).unwrap().is_none());
        let evidence = db.get_merge_evidence(&root).unwrap().unwrap();
        assert_eq!(evidence.root_task_id, root);
        assert_eq!(evidence.canonical_pr_url, PR);
        assert!(!evidence.observed_at.is_empty());
        assert_eq!(journal_count(&db, &root), 1);
        assert!(db.mark_chore_pr_merged(&root, PR).unwrap().is_none());
        assert_eq!(db.get_merge_evidence(&root).unwrap(), Some(evidence));
        assert_eq!(journal_count(&db, &root), 1);
        assert_eq!(task_status(&db, &root), status);
    }
}

#[test]
fn human_driven_merge_records_evidence_without_completing_the_task() {
    let db = WorkDb::open(temp_db_path("human-merge")).unwrap();
    let root = fixture(&db);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET human_driven = 1 WHERE id = ?1", [&root])
        .unwrap();
    db.set_task_merge_queue_state(&root, Some("queued"), Some("{}"))
        .unwrap();
    let observation = db.observe_chore_pr_merged(&root, PR).unwrap();
    assert!(observation.completed_task.is_none());
    assert!(observation.changed);
    assert_eq!(observation.cleared_queue_state.as_deref(), Some("queued"));
    assert!(db.get_merge_evidence(&root).unwrap().is_some());
    assert_eq!(task_status(&db, &root), "in_review");
    assert_eq!(journal_count(&db, &root), 1);
}

#[test]
fn evidence_follows_the_root_and_current_canonical_binding() {
    let db = WorkDb::open(temp_db_path("binding")).unwrap();
    let product = make_revision_product(&db, "binding");
    let root = make_in_review_chore(&db, &product, PR);
    let revision = insert_revision_row(&db, &product, &root);
    let nested = insert_revision_row(&db, &product, &revision);
    db.mark_chore_pr_merged(&root, PR).unwrap();
    let original = db.get_merge_evidence(&root).unwrap();
    assert_eq!(db.get_merge_evidence(&nested).unwrap(), original);
    for binding in [Some(OTHER_PR), None, Some("not a PR")] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?2, status = 'in_review' WHERE id = ?1",
                params![root, binding],
            )
            .unwrap();
        assert!(db.get_merge_evidence(&nested).unwrap().is_none());
        assert!(db.mark_chore_pr_merged(&root, PR).unwrap().is_none());
        assert!(db.get_merge_evidence(&nested).unwrap().is_none());
        assert_eq!(task_status(&db, &root), "in_review");
    }
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![root, OTHER_PR])
        .unwrap();
    assert!(db.mark_chore_pr_merged(&root, PR).unwrap().is_none());
    assert_eq!(task_status(&db, &root), "in_review");
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            params![root, "https://github.com/SpinyFin/Mono/pull/007/files#diff"],
        )
        .unwrap();
    assert_eq!(db.get_merge_evidence(&nested).unwrap(), original);
    db.mark_chore_pr_merged(&root, PR).unwrap();
    assert_eq!(db.get_merge_evidence(&root).unwrap(), original);
    assert_eq!(journal_count(&db, &root), 1);
}

#[test]
fn merge_evidence_and_journal_roll_back_with_the_merge_transition() {
    let db = WorkDb::open(temp_db_path("rollback-merge")).unwrap();
    let root = fixture(&db);
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_completion BEFORE UPDATE OF status ON tasks
         WHEN NEW.status = 'done' BEGIN SELECT RAISE(ABORT, 'completion rejected'); END;",
        )
        .unwrap();
    assert!(
        db.mark_chore_pr_merged(&root, PR)
            .unwrap_err()
            .to_string()
            .contains("completion rejected")
    );
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
    assert_eq!(journal_count(&db, &root), 0);
    assert_eq!(task_status(&db, &root), "in_review");
}

#[test]
fn binding_change_and_projection_invalidation_share_the_transaction() {
    let db = WorkDb::open(temp_db_path("binding-transaction")).unwrap();
    let root = fixture(&db);
    db.mark_chore_pr_merged(&root, PR).unwrap();
    let mut conn = db.connect().unwrap();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let before: i64 = tx
        .query_row("SELECT MAX(sequence) FROM board_changes", [], |r| r.get(0))
        .unwrap();
    tx.execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![root, OTHER_PR])
        .unwrap();
    assert!(merge_evidence::read_current(&tx, &root).unwrap().is_none());
    let changed: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM board_changes WHERE sequence > ?1 AND item_id = ?2)",
            params![before, root],
            |r| r.get(0),
        )
        .unwrap();
    assert!(changed);
    tx.rollback().unwrap();
    let after: i64 = conn
        .query_row("SELECT MAX(sequence) FROM board_changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    drop(conn);
    assert!(db.get_merge_evidence(&root).unwrap().is_some());
}

#[test]
fn upgrade_installs_empty_evidence_without_guessing_terminal_history() {
    let (dir, path) = disk_db_path("merge-evidence-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let root = fixture(&db);
    db.mark_chore_pr_merged(&root, PR).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute_batch("DROP TABLE pr_merge_evidence;").unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE id = ?1",
            [merge_evidence::MIGRATION_ID],
        )
        .unwrap();
    }
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
    assert_eq!(task_status(&db, &root), "done");
    db.mark_chore_pr_merged(&root, PR).unwrap();
    // A later trigger refresh must retain evidence's invalidation coverage.
    board_projection_schema::refine_triggers(&db.connect().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        let mut names: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'trigger' AND name LIKE 'board_changes_pr_merge_evidence_%'
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                "board_changes_pr_merge_evidence_DELETE",
                "board_changes_pr_merge_evidence_INSERT",
                "board_changes_pr_merge_evidence_UPDATE",
            ]
        );
        conn.execute("DELETE FROM pr_merge_evidence WHERE root_task_id = ?1", [&root])
            .unwrap();
    }
    assert_eq!(journal_count(&db, &root), 3);
    db.mark_chore_pr_merged(&root, PR).unwrap();
    assert_eq!(journal_count(&db, &root), 4);
    let observed = db.get_merge_evidence(&root).unwrap().unwrap();
    drop(db);
    let reopened = WorkDb::open(path).unwrap();
    assert_eq!(reopened.get_merge_evidence(&root).unwrap(), Some(observed));
    drop(dir);
}

#[tokio::test]
async fn worker_merge_observation_records_root_even_after_execution_is_terminal() {
    let db = WorkDb::open(temp_db_path("worker-merge")).unwrap();
    let root = fixture(&db);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.record_worker_pr_completion(&execution.id, PR, None, None, WorkerPrCompletionTarget::InReview, None)
        .unwrap();
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::WorkItemChanged,
    ));
    assert!(
        db.record_worker_pr_completion(&execution.id, PR, None, None, WorkerPrCompletionTarget::Done, None)
            .unwrap()
            .is_none()
    );
    assert!(db.get_merge_evidence(&root).unwrap().is_some());
    assert_eq!(journal_count(&db, &root), 1);
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, boss_event_bus::Event::WorkItemChanged { task_id, .. } if task_id == root));
}

#[test]
fn worker_observing_merge_on_a_revision_records_the_bound_root() {
    let db = WorkDb::open(temp_db_path("revision-merge")).unwrap();
    let product = make_revision_product(&db, "revision merge");
    let root = make_in_review_chore(&db, &product, PR);
    let revision = insert_revision_row(&db, &product, &root);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.record_worker_pr_completion(&execution.id, PR, None, None, WorkerPrCompletionTarget::Done, None)
        .unwrap()
        .unwrap();
    let evidence = db.get_merge_evidence(&revision).unwrap().unwrap();
    assert_eq!(evidence.root_task_id, root);
    assert_eq!(evidence.canonical_pr_url, PR);
    assert_eq!(journal_count(&db, &root), 1);
    assert_eq!(journal_count(&db, &revision), 0);
}

#[test]
fn deleted_and_missing_roots_do_not_acquire_evidence() {
    let db = WorkDb::open(temp_db_path("deleted-merge")).unwrap();
    let root = fixture(&db);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET deleted_at = 'deleted' WHERE id = ?1", [&root])
        .unwrap();
    for id in [&root, "missing"] {
        assert!(db.mark_chore_pr_merged(id, PR).unwrap().is_none());
        assert!(db.get_merge_evidence(id).unwrap().is_none());
        assert_eq!(journal_count(&db, id), 0);
    }
}

#[test]
fn worker_closed_unmerged_completion_does_not_record_a_merge() {
    let db = WorkDb::open(temp_db_path("closed-worker")).unwrap();
    let root = fixture(&db);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.record_worker_pr_completion(
        &execution.id,
        PR,
        None,
        None,
        WorkerPrCompletionTarget::DoneUnmerged,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(task_status(&db, &root), "done");
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
}

#[test]
fn worker_superseded_pr_completion_records_completed_without_merge_evidence() {
    let db = WorkDb::open(temp_db_path("superseded-worker")).unwrap();
    let root = fixture(&db);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.record_worker_superseded_pr_completion(&execution.id, PR, OTHER_PR, Some("Closed #2927 as superseded by #2977"))
        .unwrap()
        .unwrap();
    let updated = db.get_execution(&execution.id).unwrap();
    assert_eq!(updated.status, ExecutionStatus::Completed);
    assert_eq!(updated.superseded_by_pr.as_deref(), Some(OTHER_PR));
    assert_eq!(task_status(&db, &root), "done");
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
}

#[test]
fn stale_worker_merge_cannot_replace_a_new_pr_binding() {
    let db = WorkDb::open(temp_db_path("stale-worker-merge")).unwrap();
    let root = fixture(&db);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![root, OTHER_PR])
        .unwrap();
    let completion = db
        .record_worker_pr_completion(&execution.id, PR, None, None, WorkerPrCompletionTarget::Done, None)
        .unwrap()
        .unwrap();
    assert!(completion.binding_mismatch);
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(task_status(&db, &root), "in_review");
    assert!(db.get_merge_evidence(&root).unwrap().is_none());
}

#[test]
fn blank_pr_url_is_unbound_for_a_worker_merge() {
    let db = WorkDb::open(temp_db_path("blank-worker-merge")).unwrap();
    for (label, bound) in [("empty", ""), ("whitespace", "   ")] {
        let root = fixture(&db);
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![root, bound])
            .unwrap();
        let completion = db
            .record_worker_pr_completion(&execution.id, PR, None, None, WorkerPrCompletionTarget::Done, None)
            .unwrap()
            .unwrap();
        assert!(
            !completion.binding_mismatch,
            "{label}: a blank binding must be treated as unbound"
        );
        assert_eq!(task_status(&db, &root), "done", "{label}");
        let WorkItem::Chore(task) = completion.work_item else {
            panic!("{label}: expected a chore");
        };
        assert_eq!(task.pr_url.as_deref(), Some(PR), "{label}");
        assert!(db.get_merge_evidence(&root).unwrap().is_some(), "{label}");
    }
}
