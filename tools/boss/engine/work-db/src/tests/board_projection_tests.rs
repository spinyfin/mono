//! Complete serialized projection parity, including fields outside the cards.
use super::*;
use crate::{
    board_caches::BoardCaches,
    board_projection::{CachedProjection as Projection, Refresh},
};
use boss_metrics::population_timing::PopulationTrace;

fn check(db: &WorkDb, product: &str, model: &mut Option<Projection>, now: i64) -> Refresh {
    let (tree, refresh) = db
        .refresh_board_projection(
            product,
            model,
            &mut PopulationTrace::disabled(),
            &BoardCaches::default(),
            now,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&tree).unwrap(),
        serde_json::to_value(db.get_work_tree_at(product, now).unwrap()).unwrap()
    );
    if let Some(model) = model {
        let actual = serde_json::to_vec(&tree).unwrap().len();
        assert!((actual..=actual + 6).contains(&model.retained_bytes));
    }
    refresh
}

#[test]
fn board_projection_evicts_oversized_content_without_truncating_the_response() {
    let db = WorkDb::open(temp_db_path("projection-size")).unwrap();
    let product = make_revision_product(&db, "size");
    let task = create_test_chore_manual(&db, product.clone(), "large card").id;
    let mut model = None;
    check(&db, &product, &mut model, 100);
    let large = "x".repeat(32 * 1024 * 1024);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET description = ?2 WHERE id = ?1", params![task, large])
        .unwrap();
    drop(large);
    for expected in [Refresh::Incremental(1), Refresh::Snapshot("cold")] {
        let (tree, refresh) = db
            .refresh_board_projection(
                &product,
                &mut model,
                &mut PopulationTrace::disabled(),
                &BoardCaches::default(),
                100,
            )
            .unwrap();
        assert_eq!(refresh, expected);
        assert_eq!(tree.chores[0].description.len(), 32 * 1024 * 1024);
        assert!(model.is_none());
    }
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET description = 'small again' WHERE id = ?1", [&task])
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("cold"));
    assert!(model.is_some());
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
}

#[test]
fn board_projection_runtime_and_structural_sequence_matches_every_field() {
    let db = WorkDb::open(temp_db_path("projection")).unwrap();
    let product = make_revision_product(&db, "projection");
    let other = make_revision_product(&db, "other");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/7");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET name = 'Source family' WHERE id = ?1", [&root])
        .unwrap();
    let sibling = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/8");
    let revision = insert_revision_row(&db, &product, &root);
    let nested = insert_revision_row(&db, &product, &revision);
    let mut model = None;
    let mut other_model = None;
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("cold"));
    check(&db, &other, &mut other_model, 100);
    for sql in [
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        "UPDATE tasks SET ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        "UPDATE tasks SET status = 'done' WHERE id = ?1",
        "UPDATE tasks SET status = 'archived' WHERE id = ?1",
        "UPDATE tasks SET status = 'todo', ordinal = 3 WHERE id = ?1",
    ] {
        db.connect().unwrap().execute(sql, [&revision]).unwrap();
        assert!(
            matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)),
            "{sql}"
        );
    }
    // Reparent a nested chain: both old/new roots and all descendants change.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1",
            params![revision, sibling],
        )
        .unwrap();
    assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    // Hard deletion removes the only persisted edge back to the old root.
    db.connect()
        .unwrap()
        .execute("DELETE FROM tasks WHERE id = ?1", [&nested])
        .unwrap();
    check(&db, &product, &mut model, 100);
    // A new task and a move into a different lane/product are incremental too.
    let task = create_test_chore_manual(&db, product.clone(), "new").id;
    check(&db, &product, &mut model, 100);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'project_task', ordinal = 0 WHERE id = ?1",
            [&task],
        )
        .unwrap();
    check(&db, &product, &mut model, 100);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET product_id = ?2 WHERE id = ?1", params![task, other])
        .unwrap();
    check(&db, &product, &mut model, 100);
    check(&db, &other, &mut other_model, 100);

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    check(&db, &product, &mut model, 100);
    db.connect().unwrap().execute("INSERT INTO worker_proposals(id, execution_id, kind, payload_json, idempotency_key, created_at) VALUES ('unscoped', ?1, 'test', '{}', 'unscoped', '1')", [&execution.id]).unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    for status in ["ready", "claimed", "running", "waiting_human", "completed", "abandoned"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![execution.id, status],
            )
            .unwrap();
        assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    }
    // Deadlines must expire even without a write or invalidation.
    db.connect().unwrap().execute("UPDATE work_executions SET status = 'ready', dispatch_not_before = '150', pre_start_failure_count = 1 WHERE id = ?1", [&execution.id]).unwrap();
    for now in [100, 149, 150, 151] {
        check(&db, &product, &mut model, now);
    }
    // A run's existing runtime epoch is sufficient; no transport dependency.
    db.connect().unwrap().execute("INSERT INTO work_runs(id,execution_id,agent_id,status,created_at) VALUES ('projection-run',?1,'synthetic','active','1')", [&execution.id]).unwrap();
    check(&db, &product, &mut model, 151);
    for sql in [
        "UPDATE work_runs SET status = 'completed' WHERE id = 'projection-run'",
        "DELETE FROM work_runs WHERE id = 'projection-run'",
    ] {
        db.connect().unwrap().execute(sql, []).unwrap();
        check(&db, &product, &mut model, 151);
    }
}

#[test]
fn board_projection_recovers_gaps_broad_changes_and_bounded_closures() {
    let db = WorkDb::open(temp_db_path("projection-recovery")).unwrap();
    let product = make_revision_product(&db, "projection");
    let root = create_test_chore_manual(&db, product.clone(), "root").id;
    let mut model = None;
    check(&db, &product, &mut model, 100);
    db.connect()
        .unwrap()
        .execute("UPDATE products SET name = 'changed' WHERE id = ?1", [&product])
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("broad_input"));
    db.set_metadata("unrelated_scheduler_watermark", "100").unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    db.set_metadata("pr_review_verdicts_since", "100").unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("broad_input"));
    {
        let conn = db.connect().unwrap();
        for i in 0..=crate::board_projection_schema::JOURNAL_LIMIT {
            conn.execute("UPDATE tasks SET name = ?2 WHERE id = ?1", params![root, i.to_string()])
                .unwrap();
        }
        let count: u64 = conn
            .query_row("SELECT COUNT(*) FROM board_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, crate::board_projection_schema::JOURNAL_LIMIT);
    }
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("journal_gap"));
    for _ in 0..boss_board_projection::MAX_AFFECTED {
        insert_revision_row(&db, &product, &root);
    }
    assert_eq!(
        check(&db, &product, &mut model, 100),
        Refresh::Snapshot("affected_limit")
    );
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    // Restart/eviction simply starts from a fresh snapshot.
    model = None;
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("cold"));
}

#[test]
fn board_projection_external_commit_and_rollback_are_snapshot_consistent() {
    let (_dir, path) = disk_db_path("projection-external");
    let db = WorkDb::open(path).unwrap();
    let product = make_revision_product(&db, "projection");
    let task = create_test_chore_manual(&db, product.clone(), "before").id;
    let mut model = None;
    check(&db, &product, &mut model, 100);
    let writer = Connection::open(db.path()).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    writer
        .execute("UPDATE tasks SET name = 'after' WHERE id = ?1", [&task])
        .unwrap();
    assert_eq!(
        check(&db.reader().unwrap(), &product, &mut model, 100),
        Refresh::Incremental(0)
    );
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    writer
        .execute("UPDATE tasks SET name = 'after' WHERE id = ?1", [&task])
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(1));
    assert_eq!(model.unwrap().tree().chores[0].name, "after");
}

#[test]
fn board_projection_cross_product_dependencies_and_chain_holds_follow_their_inputs() {
    let db = WorkDb::open(temp_db_path("projection-related")).unwrap();
    let product = make_revision_product(&db, "dependent");
    let other = make_revision_product(&db, "prerequisite");
    let dependent = create_test_chore_manual(&db, product.clone(), "dependent").id;
    let prerequisite = create_test_chore_manual(&db, other, "prerequisite").id;
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&dependent)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingDependency)
                .build(),
        )
        .unwrap();
    let mut model = None;
    check(&db, &product, &mut model, 100);
    db.connect().unwrap().execute("INSERT INTO work_item_dependencies(dependent_id, prerequisite_id, relation, created_at) VALUES (?1,?2,'blocks','1')", params![dependent, prerequisite]).unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("broad_input"));
    assert_eq!(
        model.as_ref().unwrap().tree().task_runtimes[0]
            .dispatch_wait_reason
            .as_deref(),
        Some("waiting_dependency")
    );
    for status in ["done", "active"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                params![prerequisite, status],
            )
            .unwrap();
        assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    }
    let blocker = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&prerequisite)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    db.connect().unwrap().execute("UPDATE work_executions SET status = 'ready', dispatch_wait_reason = 'chain hold', dispatch_wait_blocker_id = ?2 WHERE id = ?1", params![execution.id, prerequisite]).unwrap();
    check(&db, &product, &mut model, 100);
    for status in ["completed", "running", "abandoned"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![blocker.id, status],
            )
            .unwrap();
        check(&db, &product, &mut model, 100);
    }
    db.connect()
        .unwrap()
        .execute(
            "DELETE FROM work_item_dependencies WHERE dependent_id = ?1",
            [&dependent],
        )
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("broad_input"));
}

#[test]
fn board_projection_review_evidence_and_metadata_sequences_match_complete_projection() {
    let db = WorkDb::open(temp_db_path("projection-evidence")).unwrap();
    let product = make_revision_product(&db, "evidence");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/7");
    let revision = insert_revision_row(&db, &product, &root);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let mut model = None;
    check(&db, &product, &mut model, 100);
    db.connect().unwrap().execute("INSERT INTO pr_review_batches(id,cycle_root_id,base_sha,classification_json,created_at,phase,pr_number,pr_url,status,target_sha,updated_at) VALUES ('projection-batch',?1,'base','{}','1','pre_merge',7,'https://github.com/spinyfin/mono/pull/7','collecting','head','1')", [&root]).unwrap();
    db.connect().unwrap().execute("INSERT INTO pr_review_batch_members(id,batch_id,attempt,created_at,provider_effort,requested_driver,resolved_model,role,status,updated_at,execution_id) VALUES ('projection-member','projection-batch',1,'1','medium','claude','synthetic','supervisor','pending','1',?1)", [&execution.id]).unwrap();
    assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    assert_eq!(
        model
            .as_ref()
            .unwrap()
            .tree()
            .chores
            .iter()
            .find(|t| t.id == root)
            .unwrap()
            .ai_review_stage
            .as_deref(),
        Some("consolidation")
    );
    db.connect().unwrap().execute("UPDATE pr_review_batch_members SET status = 'reported', updated_at = '2', terminal_at = '2' WHERE id = 'projection-member'", []).unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    db.connect()
        .unwrap()
        .execute("DELETE FROM pr_review_batch_members WHERE id = 'projection-member'", [])
        .unwrap();
    assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    assert_eq!(
        model
            .as_ref()
            .unwrap()
            .tree()
            .chores
            .iter()
            .find(|t| t.id == root)
            .unwrap()
            .ai_review_stage
            .as_deref(),
        Some("reviewers")
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head' WHERE id = ?1", [&root])
        .unwrap();
    db.connect().unwrap().execute("INSERT INTO pr_review_verdicts(id, execution_id, work_item_id, head_sha, gate_outcome, created_at) VALUES ('verdict',?1,?2,'head','completed_clean','1')", params![execution.id, root]).unwrap();
    assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    db.connect().unwrap().execute("INSERT INTO work_attachments(id,execution_id,work_item_id,content_digest,media_type,pixel_width,pixel_height,size_bytes,source_name,created_at) VALUES ('attachment',?1,?2,'digest','png',1,1,1,'image','1')", params![execution.id, revision]).unwrap();
    assert!(matches!(check(&db, &product, &mut model, 100), Refresh::Incremental(_)));
    assert!(
        model
            .as_ref()
            .unwrap()
            .tree()
            .chores
            .iter()
            .find(|t| t.id == root)
            .unwrap()
            .has_attachments
    );
    db.connect()
        .unwrap()
        .execute("DELETE FROM work_attachments WHERE id = 'attachment'", [])
        .unwrap();
    check(&db, &product, &mut model, 100);
    db.connect()
        .unwrap()
        .execute("DELETE FROM pr_review_verdicts WHERE id = 'verdict'", [])
        .unwrap();
    check(&db, &product, &mut model, 100);
    // Non-card state is recovered too, including product alert membership.
    db.connect().unwrap().execute("INSERT INTO work_attention_items(id,work_item_id,kind,status,title,body_markdown,created_at) VALUES ('alert',?1,'test','open','alert','body','1')", [&product]).unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("broad_input"));
    assert_eq!(model.as_ref().unwrap().tree().attention_items.len(), 1);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_attention_items SET status = 'resolved', resolved_at = '2' WHERE id = 'alert'",
            [],
        )
        .unwrap();
    check(&db, &product, &mut model, 100);
}

#[test]
fn board_projection_large_ready_backlog_stays_incremental() {
    let db = WorkDb::open(temp_db_path("projection-ready-backlog")).unwrap();
    let product = make_revision_product(&db, "backlog");
    for i in 0..=boss_board_projection::MAX_AFFECTED {
        let task = create_test_chore_manual(&db, product.clone(), format!("ready {i}")).id;
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    }
    let mut model = None;
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Snapshot("cold"));
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));
    assert_eq!(check(&db, &product, &mut model, 101), Refresh::Incremental(0));
}

#[test]
fn board_projection_review_guide_changes_are_card_scoped() {
    let (_dir, path) = disk_db_path("projection-guide");
    let db = WorkDb::open(path).unwrap();
    let product = make_revision_product(&db, "guide");
    let other = make_revision_product(&db, "unrelated");
    let root = make_in_review_chore(&db, &product, "https://github.com/acme/widget/pull/9");
    let (series, comparison) = crate::test_support::seed_review_guide_series(&db, &root);
    let mut model = None;
    let mut unrelated = None;
    check(&db, &product, &mut model, 100);
    check(&db, &other, &mut unrelated, 100);
    let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(1));
    assert_eq!(check(&db, &other, &mut unrelated, 100), Refresh::Incremental(0));
    db.publish_pr_review_guide_version(&attempt.id, "# Guide", "raw")
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(1));
    assert_eq!(check(&db, &other, &mut unrelated, 100), Refresh::Incremental(0));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_guide_attempts SET retries = retries + 1 WHERE id = ?1",
            [&attempt.id],
        )
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(0));

    let revision = insert_revision_row(&db, &product, &root);
    let PrSourceCapturePersistOutcome::Stored(capture) = db
        .persist_pr_review_guide_source_capture(
            &root,
            2,
            PrSourceCaptureTrigger::Completion,
            &crate::test_support::review_guide_source_packet("base", "new-head"),
        )
        .unwrap()
    else {
        panic!("capture must persist");
    };
    let held = db
        .create_automatic_pr_review_guide_attempt(&series, &capture.comparison_id)
        .unwrap();
    check(&db, &product, &mut model, 100);
    check(&db, &other, &mut unrelated, 100);
    assert!(
        model.as_ref().unwrap().tree().chores[0]
            .review_guide_update_status
            .is_none()
    );
    let pr = "https://github.com/acme/widget/pull/9";
    db.record_review_guide_revision_head(&root, pr, "new-head", &revision, 2)
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(2));
    assert_eq!(check(&db, &other, &mut unrelated, 100), Refresh::Incremental(0));
    assert_eq!(
        model.as_ref().unwrap().tree().chores[0].review_guide_update_status,
        Some(format!("Updating after revision {revision}..."))
    );
    let conn = db.connect().unwrap();
    let prompt: String = conn
        .query_row(
            "SELECT prompt_version FROM pr_review_guide_attempts WHERE id = ?1",
            [&held.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prompt, boss_review_guide::UPDATE_PROMPT_VERSION);
    drop(conn);
    // Exercise the joined status inputs independently of attribution so one
    // working trigger cannot conceal a missing trigger on another table.
    for (sql, id) in [
        (
            "UPDATE pr_review_guide_source_series SET request_epoch = request_epoch + 1 WHERE id = ?1",
            &series,
        ),
        (
            "UPDATE pr_review_guide_source_series SET request_epoch = request_epoch - 1 WHERE id = ?1",
            &series,
        ),
        (
            "UPDATE pr_review_guide_attempts SET request_epoch = request_epoch + 1 WHERE id = ?1",
            &held.id,
        ),
        (
            "UPDATE pr_review_guide_attempts SET request_epoch = request_epoch - 1 WHERE id = ?1",
            &held.id,
        ),
        (
            "UPDATE pr_review_guide_source_comparisons SET head_sha = 'different' WHERE id = ?1",
            &capture.comparison_id,
        ),
        (
            "UPDATE pr_review_guide_source_comparisons SET head_sha = 'new-head' WHERE id = ?1",
            &capture.comparison_id,
        ),
        ("DELETE FROM pr_review_guide_updates WHERE attempt_id = ?1", &held.id),
    ] {
        db.connect().unwrap().execute(sql, [id]).unwrap();
        assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(2));
        assert_eq!(check(&db, &other, &mut unrelated, 100), Refresh::Incremental(0));
    }
    db.fail_pr_review_guide_attempt(&held.id, "generation failed").unwrap();
    db.record_pr_review_guide_source_capture_failure(&root, pr, 3, "capture failed")
        .unwrap();
    check(&db, &product, &mut model, 100);
    check(&db, &other, &mut unrelated, 100);
    db.record_review_guide_revision_head(&root, pr, "failed-head", &revision, 3)
        .unwrap();
    assert_eq!(check(&db, &product, &mut model, 100), Refresh::Incremental(2));
    assert_eq!(check(&db, &other, &mut unrelated, 100), Refresh::Incremental(0));
    assert_eq!(
        model.as_ref().unwrap().tree().chores[0].review_guide_update_status,
        Some(format!(
            "Update after revision {revision} failed; previous guide retained"
        ))
    );
}

#[test]
fn board_projection_task_updates_do_not_duplicate_journal_ids() {
    let db = WorkDb::open(temp_db_path("projection-journal-ids")).unwrap();
    let product = make_revision_product(&db, "journal");
    let root = create_test_chore_manual(&db, product.clone(), "root").id;
    let child = insert_revision_row(&db, &product, &root);
    let conn = db.connect().unwrap();
    conn.execute("DELETE FROM board_changes", []).unwrap();
    conn.execute("UPDATE tasks SET name = 'renamed' WHERE id = ?1", [&child])
        .unwrap();
    let ids: Vec<String> = collect_rows(
        conn.prepare("SELECT item_id FROM board_changes")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&root));
    assert!(ids.contains(&child));
}

#[test]
fn board_projection_snapshot_preserves_cached_runtime_expiry() {
    let db = WorkDb::open(temp_db_path("projection-cached-expiry")).unwrap();
    let product = make_revision_product(&db, "expiry");
    let root = create_test_chore_manual(&db, product.clone(), "ready").id;
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = '150', pre_start_failure_count = 1 WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    let cache = BoardCaches::default();
    db.get_work_tree_cached_at(&product, &mut PopulationTrace::disabled(), &cache, 100)
        .unwrap();
    let mut model = None;
    db.refresh_board_projection(&product, &mut model, &mut PopulationTrace::disabled(), &cache, 100)
        .unwrap();
    assert!(cache.runtimes.stats().hits > 0);
    assert_eq!(check(&db, &product, &mut model, 149), Refresh::Incremental(0));
    assert_eq!(check(&db, &product, &mut model, 150), Refresh::Incremental(1));
    assert_eq!(check(&db, &product, &mut model, 151), Refresh::Incremental(0));
}

#[test]
fn board_projection_upgrade_replaces_broad_triggers_and_preserves_watermark() {
    let (_dir, path) = disk_db_path("projection-trigger-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = make_revision_product(&db, "upgrade");
    let root = create_test_chore_manual(&db, product, "root").id;
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "DROP TRIGGER board_changes_tasks_UPDATE;
        CREATE TRIGGER board_changes_tasks_UPDATE AFTER UPDATE ON tasks BEGIN
            INSERT INTO board_changes(source) VALUES ('tasks');
        END;
        DROP TRIGGER board_changes_pr_review_guide_revision_heads_INSERT;
        DROP TABLE schema_migrations;
        INSERT OR REPLACE INTO metadata VALUES ('schema_version', '57');",
    )
    .unwrap();
    let before: u64 = conn
        .query_row("SELECT MAX(sequence) FROM board_changes", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    drop(db);
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    let after: u64 = conn
        .query_row("SELECT MAX(sequence) FROM board_changes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before, after);
    conn.execute("UPDATE tasks SET name = 'changed' WHERE id = ?1", [&root])
        .unwrap();
    let item: String = conn
        .query_row(
            "SELECT item_id FROM board_changes WHERE sequence > ?1",
            [before],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(item, root);
    conn.execute(
        "INSERT INTO pr_review_guide_revision_heads VALUES (?1, 'pr', 'head', 'revision', 1)",
        [&root],
    )
    .unwrap();
    let item: String = conn
        .query_row(
            "SELECT item_id FROM board_changes ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(item, root);
}
