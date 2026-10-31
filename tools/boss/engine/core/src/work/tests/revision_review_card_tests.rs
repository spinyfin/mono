use super::*;
use boss_protocol::ReviewSection;

const PR: &str = "https://github.com/spinyfin/mono/pull/42";

fn completed_revision(db: &WorkDb) -> (String, String, String) {
    let (product, revision, execution) = make_waiting_human_chore(db, "review-card");
    let root = make_in_review_chore(db, &product, PR);
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
            params![revision, root],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = 'revision_implementation' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    }
    db.record_worker_pr_completion(
        &execution,
        PR,
        None,
        None,
        WorkerPrCompletionTarget::PendingReview,
        None,
    )
    .unwrap()
    .unwrap();
    // The hold stays persisted; only the board presents the card as in review.
    assert_eq!(task_status(db, &revision), "active");
    (product, root, revision)
}

fn assert_cards(db: &WorkDb, product: &str, root: &str, revision: &str, section: ReviewSection) {
    let tree = db.get_work_tree(product).unwrap();
    let primary = tree.chores.iter().find(|t| t.id == root).unwrap();
    let card = tree.tasks.iter().find(|t| t.id == revision).unwrap();
    assert!(
        !primary.has_in_progress_revision,
        "delivered implementation is waiting on review"
    );
    assert_eq!(card.status, primary.status, "revision card mirrors the owner's status");
    assert!(matches!(card.status, TaskStatus::InReview | TaskStatus::Blocked));
    assert_eq!(card.blocked_reason, primary.blocked_reason);
    assert_eq!(card.blocked_detail, primary.blocked_detail);
    assert_eq!(card.review_section, Some(section));
    assert_eq!(card.review_section, primary.review_section);
    assert_eq!(card.ready_for_review, primary.ready_for_review);
    assert_eq!(card.pr_url, primary.pr_url);
    assert_eq!(card.ci_required_state, primary.ci_required_state);
    assert_eq!(card.review_required_state, primary.review_required_state);
    assert_eq!(card.ai_reviewing, primary.ai_reviewing);
    assert_eq!(card.ai_review_stage, primary.ai_review_stage);
    assert_eq!(card.ai_review_state, primary.ai_review_state);
    assert_eq!(card.ai_review_badge, primary.ai_review_badge);
    let WorkItem::Task(single) = db.get_work_item_for_display(revision).unwrap() else {
        panic!("expected revision")
    };
    assert_eq!(single.status, card.status);
    assert_eq!(single.blocked_reason, card.blocked_reason);
    assert_eq!(single.blocked_detail, card.blocked_detail);
    assert_eq!(single.review_section, card.review_section);
    let patch = db.get_work_runtime_patch(product, &[revision.to_owned()]).unwrap();
    let patched = patch
        .items
        .iter()
        .find(|i| match &i.item {
            WorkItem::Task(t) => t.id == revision,
            _ => false,
        })
        .unwrap();
    let WorkItem::Task(patched) = &patched.item else {
        unreachable!()
    };
    assert_eq!(
        serde_json::to_value(card).unwrap(),
        serde_json::to_value(patched).unwrap()
    );
}

#[test]
fn completed_revision_shares_primary_review_ci_and_ready_presentation() {
    let db = WorkDb::open(temp_db_path("revision-review-card")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&review.id, "reviewer", "repo", "lease", "workspace", "/tmp/review-card")
        .unwrap();
    // Delivered implementation waits on AI review on both cards.
    assert_cards(&db, &product, &root, &revision, ReviewSection::AiReview);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict', blocked_detail = 'conflicts in a.rs' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::Blocked);
    {
        let conn = db.connect().unwrap();
        // The review passed: the hold lifts and the revision is in_review.
        conn.execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&revision])
            .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&review.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL,
             ci_required_state = 'in_progress', pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    }
    assert_cards(&db, &product, &root, &revision, ReviewSection::Pending);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET ci_required_state = 'success' WHERE id = ?1", [&root])
        .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::Ready);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_mergeable_state = 'conflicting' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::Blocked);
}

#[test]
fn held_revision_stays_active_while_shown_in_review() {
    let db = WorkDb::open(temp_db_path("revision-review-hold")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::AiReview);
    assert_eq!(task_status(&db, &revision), "active");
    db.start_execution_run(
        &review.id,
        "held-reviewer",
        "repo",
        "lease",
        "workspace",
        "/tmp/held-reviewer",
    )
    .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::AiReview);
    assert_eq!(task_status(&db, &revision), "active");
}

#[test]
fn restarted_revision_returns_to_doing() {
    let db = WorkDb::open(temp_db_path("revision-review-restart")).unwrap();
    let (product, _root, revision) = completed_revision(&db);
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(&revision)
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap();
    let tree = db.get_work_tree(&product).unwrap();
    let card = tree.tasks.iter().find(|t| t.id == revision).unwrap();
    assert_eq!(card.status, TaskStatus::Active);
}

#[test]
fn held_revision_keeps_its_status_outside_roots_review_lane() {
    let db = WorkDb::open(temp_db_path("held-root-outside-review")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    for (status, reason) in [
        ("blocked", Some("awaiting_operator_answer")),
        ("active", None),
        ("done", None),
    ] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?2, blocked_reason = ?3 WHERE id = ?1",
                params![root, status, reason],
            )
            .unwrap();
        let tree = db.get_work_tree(&product).unwrap();
        assert_eq!(
            tree.tasks.iter().find(|t| t.id == revision).unwrap().status,
            TaskStatus::Active
        );
        let WorkItem::Task(card) = db.get_work_item_for_display(&revision).unwrap() else {
            panic!("expected revision")
        };
        assert_eq!(card.status, TaskStatus::Active);
    }
}

#[test]
fn held_revision_drop_in_review_is_status_preserving() {
    use boss_engine_board_gesture::{BoardRow, DropResolution, resolve_drop};
    let db = WorkDb::open(temp_db_path("held-reorder")).unwrap();
    let (_, _, revision) = completed_revision(&db);
    let WorkItem::Task(card) = db.get_work_item_for_display(&revision).unwrap() else {
        panic!("expected revision")
    };
    let row = BoardRow {
        status: card.status,
        autostart: card.autostart,
        blocked_reason: card.blocked_reason,
        merge_queue_state: card.merge_queue_state,
    };
    assert!(matches!(
        resolve_drop(
            &row,
            boss_protocol::BoardDropTarget::new(boss_protocol::BoardColumn::Review, None)
        ),
        DropResolution::Reorder { .. }
    ));
    assert_eq!(task_status(&db, &revision), "active");
}

#[test]
fn held_revision_gates_dependents_and_rejects_failed_review() {
    let db = WorkDb::open(temp_db_path("held-safety")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    let next = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&root)
                .description("Next revision")
                .depends_on(vec![revision.clone()])
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    db.get_work_tree(&product).unwrap();
    db.try_unblock_dependency_if_resolved(&next.id).unwrap();
    assert_eq!(task_status(&db, &next.id), "blocked");
    assert!(db.gating_prereqs_for(&next.id).unwrap().contains(&revision));
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &review.id,
        &root,
        &ReviewVerdictInput {
            head_sha: Some("head".into()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: super::super::review_verdicts::REVIEW_GATE_OUTCOME_GAVE_UP,
        },
    )
    .unwrap();
    assert!(
        !db.advance_pending_review_task_to_in_review_with_verdict_source(&revision, &root)
            .unwrap()
    );
    assert_eq!(task_status(&db, &revision), "active");
}

#[test]
fn pending_review_completion_recovers_admission_without_post_effects() {
    let db = WorkDb::open(temp_db_path("held-admission")).unwrap();
    let (_, root, revision) = completed_revision(&db);
    // No post-effects created a reviewer or a deferral marker; age the hold
    // through the recovery grace period without changing lifecycle state.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
            params![
                revision,
                (boss_engine_utils::epoch_time::now_epoch_secs() as u64 - crate::work::REVIEW_BATCH_STALE_SECS - 60)
                    .to_string()
            ],
        )
        .unwrap();
    let candidates = db.list_tasks_awaiting_pre_merge_review_admission().unwrap();
    assert!(
        candidates
            .iter()
            .any(|c| c.task_id == revision && c.cycle_root_id == root)
    );
}

#[test]
fn raw_single_item_read_keeps_durable_status_for_held_revision() {
    let db = WorkDb::open(temp_db_path("held-raw-read")).unwrap();
    let (_product, _root, revision) = completed_revision(&db);
    let WorkItem::Task(raw) = db.get_work_item(&revision).unwrap() else {
        panic!("expected revision")
    };
    assert_eq!(raw.status, TaskStatus::Active, "agent-facing reads stay durable");
    let short_id = raw.short_id.expect("revision has a short id");
    let Some(WorkItem::Task(by_short)) = db.get_work_item_by_short_id(&raw.product_id, short_id).unwrap() else {
        panic!("expected revision by short id")
    };
    assert_eq!(by_short.status, raw.status);
    assert_eq!(by_short.blocked_reason, raw.blocked_reason);
    let WorkItem::Task(display) = db.get_work_item_for_display(&revision).unwrap() else {
        panic!("expected revision")
    };
    assert_eq!(display.status, TaskStatus::InReview);
}

#[test]
fn held_revision_keeps_root_out_of_ready_without_a_queued_review() {
    let db = WorkDb::open(temp_db_path("held-not-ready")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    let tree = db.get_work_tree(&product).unwrap();
    let primary = tree.chores.iter().find(|t| t.id == root).unwrap();
    let card = tree.tasks.iter().find(|t| t.id == revision).unwrap();
    assert!(!primary.ready_for_review);
    assert_ne!(primary.review_section, Some(ReviewSection::Ready));
    assert_ne!(card.review_section, Some(ReviewSection::Ready));
    assert_ne!(primary.review_section, Some(ReviewSection::InRevision));
}

#[test]
fn in_review_revision_under_non_review_root_keeps_pr_facts() {
    let db = WorkDb::open(temp_db_path("in-review-blocked-root")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = NULL WHERE id = ?1",
            [&revision],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    }
    let tree = db.get_work_tree(&product).unwrap();
    let card = tree.tasks.iter().find(|t| t.id == revision).unwrap();
    assert_eq!(card.status, TaskStatus::InReview);
    assert_eq!(card.pr_url.as_deref(), Some(PR));
}
