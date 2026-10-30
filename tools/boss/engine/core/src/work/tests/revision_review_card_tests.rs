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
    assert_eq!(task_status(db, &revision), "in_review");
    (product, root, revision)
}

fn assert_cards(db: &WorkDb, product: &str, root: &str, revision: &str, section: ReviewSection) {
    let tree = db.get_work_tree(product).unwrap();
    let primary = tree.chores.iter().find(|t| t.id == root).unwrap();
    let card = tree.tasks.iter().find(|t| t.id == revision).unwrap();
    assert_eq!(card.status, TaskStatus::InReview);
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
    assert!(!primary.has_in_progress_revision);
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
    assert_cards(&db, &product, &root, &revision, ReviewSection::AiReview);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = ?1",
            [&root],
        )
        .unwrap();
    assert_cards(&db, &product, &root, &revision, ReviewSection::Blocked);
    {
        let conn = db.connect().unwrap();
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
fn reconciliation_recovers_legacy_completed_revision_without_redispatch() {
    let db = WorkDb::open(temp_db_path("revision-review-recovery")).unwrap();
    let (product, root, revision) = completed_revision(&db);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision])
        .unwrap();
    let before = db.list_executions(Some(&revision)).unwrap().len();
    assert!(db.reconcile_active_dispatch(|_| false).unwrap().is_empty());
    assert_eq!(db.list_executions(Some(&revision)).unwrap().len(), before);
    assert_cards(&db, &product, &root, &revision, ReviewSection::Pending);
}
