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

fn revision_row(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected a task row, got {other:?}"),
    }
}

/// Create two revisions while the first is still a live writer so the later
/// one is born with a real `blocks` edge and `blocked: dependency`.
fn later_revision_blocked_on_live_writer(db: &WorkDb, parent_id: &str) -> (Task, Task) {
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let first = db.create_revision(revision_input(parent_id), &checker).unwrap();
    let later = db.create_revision(revision_input(parent_id), &checker).unwrap();
    assert_eq!(later.status, TaskStatus::Blocked);
    assert_eq!(later.blocked_reason.as_deref(), Some("dependency"));
    assert_eq!(db.gating_prereqs_for(&later.id).unwrap(), vec![first.id.clone()]);
    (first, later)
}

/// A later revision is not gated on a dead worker_failed sibling, so it
/// dispatches and the parent leaves Review ▸ Blocked.
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

/// A later revision is not gated on a `spawning_attempt_retired_unsuccessfully`
/// sibling with no live execution, so it dispatches.
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

/// Mutual-block shape: parent `in_review` with green CI, a later revision
/// already `blocked: dependency` on a sibling that then dead-halts as
/// `worker_failed`. The persisted edge stops gating, the unblock path
/// dispatches the later revision, and the parent leaves Review ▸ Blocked.
#[test]
fn already_blocked_later_revision_unblocks_when_sibling_worker_fails() {
    let db = WorkDb::open(temp_db_path("dead-tail-recover-worker-failed")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-recover-worker-failed");
    let parent_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/6808");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks
             SET ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            [&parent_id],
        )
        .unwrap();

    let (first, later) = later_revision_blocked_on_live_writer(&db, &parent_id);
    let first_exec = ready_revision_execution(&db, &first.id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            [&first_exec.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&first.id])
        .unwrap();
    db.record_worker_failure(&first_exec.id, "worker exited before producing a commit")
        .unwrap();

    assert!(
        db.gating_prereqs_for(&later.id).unwrap().is_empty(),
        "a dead worker_failed sibling must not keep gating a same-chain later revision"
    );
    let later = revision_row(&db, &later.id);
    assert_eq!(later.status, TaskStatus::Todo);
    assert!(later.blocked_reason.is_none());
    let _ = ready_revision_execution(&db, &later.id);
    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::InRevision),
        "a dispatched later revision must take the parent out of Review ▸ Blocked"
    );
}

/// Same recovery for `spawning_attempt_retired_unsuccessfully`: park the
/// live writer after the later revision is already `blocked: dependency`,
/// then the dep-unblock path makes the later revision dispatchable.
#[test]
fn already_blocked_later_revision_unblocks_when_sibling_spawn_retires() {
    let db = WorkDb::open(temp_db_path("dead-tail-recover-retired-spawn")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-recover-retired-spawn");
    let parent_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/6809");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks
             SET ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            [&parent_id],
        )
        .unwrap();

    let (first, later) = later_revision_blocked_on_live_writer(&db, &parent_id);
    park_dead_writer(&db, &first.id, "spawning_attempt_retired_unsuccessfully");
    assert!(db.gating_prereqs_for(&later.id).unwrap().is_empty());
    assert!(
        db.try_unblock_dependency_if_resolved(&later.id).unwrap(),
        "dep-unblock must release a later revision gated only on a dead-halted sibling"
    );
    let later = revision_row(&db, &later.id);
    assert_eq!(later.status, TaskStatus::Todo);
    let _ = ready_revision_execution(&db, &later.id);
    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::InRevision)
    );
}

/// A sibling that is `blocked: worker_failed` but still holds a live
/// execution remains a live writer, so an already-gated later revision
/// stays `blocked: dependency`.
#[test]
fn already_blocked_later_revision_stays_gated_while_sibling_execution_is_live() {
    let db = WorkDb::open(temp_db_path("dead-tail-recover-live-exec")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-recover-live-exec");
    let parent_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/6810");

    let (first, later) = later_revision_blocked_on_live_writer(&db, &parent_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
    assert!(
        db.list_executions(Some(&first.id))
            .unwrap()
            .iter()
            .any(|e| !e.status.is_terminal()),
        "precondition: the halted row still has a live execution"
    );
    assert_eq!(db.gating_prereqs_for(&later.id).unwrap(), vec![first.id]);
    assert!(!db.try_unblock_dependency_if_resolved(&later.id).unwrap());
    let later = revision_row(&db, &later.id);
    assert_eq!(later.status, TaskStatus::Blocked);
    assert_eq!(later.blocked_reason.as_deref(), Some("dependency"));
}

/// A dead writer is skipped even when the parent PR head is red. The later
/// revision dispatches; the parent stays in Review ▸ Blocked because of
/// the CI failure itself.
#[test]
fn later_revision_dispatches_past_dead_writer_when_parent_ci_is_red() {
    let db = WorkDb::open(temp_db_path("dead-tail-red-ci")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-red-ci");
    let parent_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/6811");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET ci_required_state = 'fail' WHERE id = ?1",
            [&parent_id],
        )
        .unwrap();

    let (first, later) = later_revision_blocked_on_live_writer(&db, &parent_id);
    park_dead_writer(&db, &first.id, "worker_failed");
    assert!(db.gating_prereqs_for(&later.id).unwrap().is_empty());
    assert!(db.try_unblock_dependency_if_resolved(&later.id).unwrap());
    let _ = ready_revision_execution(&db, &later.id);
    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::Blocked),
        "red CI still places the parent in Review ▸ Blocked"
    );
}

/// A dead writer is skipped when the parent has no recorded CI state.
#[test]
fn later_revision_dispatches_past_dead_writer_when_parent_ci_is_unknown() {
    let db = WorkDb::open(temp_db_path("dead-tail-unknown-ci")).unwrap();
    let product_id = make_revision_product(&db, "dead-tail-unknown-ci");
    let parent_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/6812");

    let (first, later) = later_revision_blocked_on_live_writer(&db, &parent_id);
    park_dead_writer(&db, &first.id, "worker_failed");
    assert!(db.gating_prereqs_for(&later.id).unwrap().is_empty());
    assert!(db.try_unblock_dependency_if_resolved(&later.id).unwrap());
    let _ = ready_revision_execution(&db, &later.id);
    assert_eq!(
        parent_review_section(&db, &product_id, &parent_id),
        Some(boss_protocol::ReviewSection::InRevision)
    );
}
