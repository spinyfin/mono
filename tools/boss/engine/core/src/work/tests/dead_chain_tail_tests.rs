use super::*;

/// Park a revision the way a finished failed run does: terminal execution,
/// `blocked` with a halt reason, no live writer left.
fn park_dead_writer(db: &WorkDb, revision_id: &str, reason: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions
         SET status = 'failed'
         WHERE work_item_id = ?1
           AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
        [revision_id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks
         SET status = 'blocked', blocked_reason = ?2, autostart = 0
         WHERE id = ?1",
        rusqlite::params![revision_id, reason],
    )
    .unwrap();
}

fn parent_review_section(db: &WorkDb, product_id: &str, parent_id: &str) -> Option<boss_protocol::ReviewSection> {
    db.get_work_tree(product_id)
        .unwrap()
        .chores
        .iter()
        .find(|c| c.id == parent_id)
        .expect("parent chore must be in the work tree")
        .review_section
}

fn ready_revision_execution(db: &WorkDb, revision_id: &str) -> WorkExecution {
    let executions = db.list_executions(Some(revision_id)).unwrap();
    let live: Vec<_> = executions.iter().filter(|e| !e.status.is_terminal()).collect();
    assert_eq!(
        live.len(),
        1,
        "expected one live revision execution, got {executions:?}"
    );
    assert_eq!(live[0].kind, ExecutionKind::RevisionImplementation);
    assert_eq!(live[0].status, ExecutionStatus::Ready);
    live[0].clone()
}

/// Mutual-block shape: parent `in_review` with green CI, hidden revision
/// `blocked: worker_failed` with no live execution, later revision would
/// previously be born `blocked: dependency` on the dead one and pin the
/// parent in Review ▸ Blocked. The dead row is not a live writer, so the
/// later revision dispatches and the parent leaves Blocked.
#[test]
fn later_revision_dispatches_past_dead_worker_failed_chain_tail() {
    let db = WorkDb::open(temp_db_path("dead-tail-worker-failed")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-worker-failed");
    let pr_url = "https://github.com/spinyfin/mono/pull/6805";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks
             SET ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            [&parent_id],
        )
        .unwrap();

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let dead = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    park_dead_writer(&db, &dead.id, "worker_failed");

    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::Blocked),
        "a lone worker_failed revision still marks the parent Review ▸ Blocked before a later writer exists"
    );

    let later = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    assert_eq!(
        later.status,
        TaskStatus::Todo,
        "a later revision must not be gated on a dead worker_failed sibling"
    );
    assert!(
        db.gating_prereqs_for(&later.id).unwrap().is_empty(),
        "no chain-tail edge onto the dead sibling"
    );
    let _ = ready_revision_execution(&db, &later.id);

    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::InRevision),
        "a dispatched later revision must take the parent out of Review ▸ Blocked"
    );
}

/// Same skip for the other terminal halt the chain-tail used to treat as a
/// live writer: `spawning_attempt_retired_unsuccessfully` with no live
/// execution.
#[test]
fn later_revision_dispatches_past_retired_spawning_attempt_chain_tail() {
    let db = WorkDb::open(temp_db_path("dead-tail-retired-spawn")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-retired-spawn");
    let pr_url = "https://github.com/spinyfin/mono/pull/6806";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let dead = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    park_dead_writer(&db, &dead.id, "spawning_attempt_retired_unsuccessfully");

    let later = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    assert_eq!(later.status, TaskStatus::Todo);
    assert!(db.gating_prereqs_for(&later.id).unwrap().is_empty());
    let _ = ready_revision_execution(&db, &later.id);
}

/// A halted revision that still holds a non-terminal execution remains a
/// live writer, so the next revision still serialises behind it.
#[test]
fn chain_tail_still_gates_on_worker_failed_revision_with_live_execution() {
    let db = WorkDb::open(temp_db_path("dead-tail-live-exec")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-live-exec");
    let pr_url = "https://github.com/spinyfin/mono/pull/6807";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let halted = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
            [&halted.id],
        )
        .unwrap();
    let live = db.list_executions(Some(&halted.id)).unwrap();
    assert!(
        live.iter().any(|e| !e.status.is_terminal()),
        "precondition: the halted row still has a live execution"
    );

    let later = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    assert_eq!(later.status, TaskStatus::Blocked);
    assert_eq!(later.blocked_reason.as_deref(), Some("dependency"));
    assert_eq!(db.gating_prereqs_for(&later.id).unwrap(), vec![halted.id]);
}
