use super::*;

fn setup() -> (WorkDb, String) {
    let db = WorkDb::open(temp_db_path("revision-priority")).unwrap();
    let product = make_revision_product(&db, "priority");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/42");
    (db, root)
}

fn revision(db: &WorkDb, root: &str, conflict: bool) -> Task {
    let mut input = revision_input(root);
    input.created_via = Some(if conflict {
        format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}test")
    } else {
        format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", next_id("exec"))
    });
    db.create_revision(input, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap()
}

fn execution(db: &WorkDb, task: &Task) -> WorkExecution {
    db.list_executions(Some(&task.id)).unwrap().remove(0)
}

fn finish(db: &WorkDb, task: &Task) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE work_item_id = ?1",
        [&task.id],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&task.id])
        .unwrap();
}

#[test]
fn conflict_revision_precedes_queued_reviews_and_preserves_remaining_order() {
    let (db, root) = setup();
    let first = revision(&db, &root, false);
    let second = revision(&db, &root, false);
    let conflict = revision(&db, &root, true);
    let later = revision(&db, &root, false);
    assert_eq!(execution(&db, &conflict).status, ExecutionStatus::Ready);
    for task in [&first, &second, &later] {
        assert_eq!(execution(&db, task).status, ExecutionStatus::WaitingDependency);
        assert_eq!(
            db.claim_execution_for_dispatch(&execution(&db, task).id).unwrap(),
            DispatchClaimOutcome::Rejected
        );
    }
    finish(&db, &conflict);
    assert!(db.try_unblock_dependency_if_resolved(&first.id).unwrap());
    assert!(!db.try_unblock_dependency_if_resolved(&second.id).unwrap());
    assert!(!db.try_unblock_dependency_if_resolved(&later.id).unwrap());
    assert_eq!(execution(&db, &first).status, ExecutionStatus::Ready);
    finish(&db, &first);
    assert!(db.try_unblock_dependency_if_resolved(&second.id).unwrap());
    assert!(!db.try_unblock_dependency_if_resolved(&later.id).unwrap());
    finish(&db, &second);
    assert!(db.try_unblock_dependency_if_resolved(&later.id).unwrap());
}

#[test]
fn conflict_revision_waits_for_claimed_or_live_writer_then_runs_next_even_after_failure() {
    for status in ["claimed", "running", "waiting_human"] {
        for outcome in ["completed", "failed"] {
            let (db, root) = setup();
            let writer = revision(&db, &root, false);
            let writer_exec = execution(&db, &writer);
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![writer_exec.id, status],
            )
            .unwrap();
            conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&writer.id])
                .unwrap();
            drop(conn);
            let queued = revision(&db, &root, false);
            let conflict = revision(&db, &root, true);
            assert_eq!(db.get_execution(&writer_exec.id).unwrap().status.as_str(), status);
            assert_eq!(execution(&db, &conflict).status, ExecutionStatus::WaitingDependency);
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![writer_exec.id, outcome],
            )
            .unwrap();
            conn.execute(
                "UPDATE tasks SET status = ?2 WHERE id = ?1",
                params![writer.id, if outcome == "failed" { "blocked" } else { "in_review" }],
            )
            .unwrap();
            drop(conn);
            assert!(db.try_unblock_dependency_if_resolved(&conflict.id).unwrap());
            assert_eq!(execution(&db, &conflict).status, ExecutionStatus::Ready);
            assert_eq!(execution(&db, &queued).status, ExecutionStatus::WaitingDependency);
        }
    }
}

#[test]
fn conflict_revision_bypasses_blocked_and_failed_predecessors_without_retrying_them() {
    for reason in ["dependency", "worker_failed", "deliberate_park"] {
        let (db, root) = setup();
        let earlier = revision(&db, &root, false);
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = ?2 WHERE id = ?1",
            params![earlier.id, reason],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'failed' WHERE work_item_id = ?1",
            [&earlier.id],
        )
        .unwrap();
        drop(conn);
        let conflict = revision(&db, &root, true);
        assert_eq!(execution(&db, &conflict).status, ExecutionStatus::Ready);
        assert_eq!(execution(&db, &earlier).status, ExecutionStatus::Failed);
        let conn = db.connect().unwrap();
        assert_eq!(
            query_task(&conn, &earlier.id)
                .unwrap()
                .unwrap()
                .blocked_reason
                .as_deref(),
            Some(reason)
        );
        assert_eq!(deps::gating_prereqs_for(&conn, &earlier.id).unwrap(), vec![conflict.id]);
    }
}

#[test]
fn newer_conflict_revision_precedes_queued_conflict_revision() {
    let (db, root) = setup();
    let review = revision(&db, &root, false);
    let older = revision(&db, &root, true);
    let newer = revision(&db, &root, true);
    assert_eq!(execution(&db, &newer).status, ExecutionStatus::Ready);
    assert_eq!(execution(&db, &older).status, ExecutionStatus::WaitingDependency);
    assert_eq!(execution(&db, &review).status, ExecutionStatus::WaitingDependency);
    finish(&db, &newer);
    assert!(db.try_unblock_dependency_if_resolved(&older.id).unwrap());
    assert!(!db.try_unblock_dependency_if_resolved(&review.id).unwrap());
}

#[test]
fn conflict_revision_waits_for_root_writer_but_not_read_only_review() {
    for kind in ["chore_implementation", "pr_review"] {
        let (db, root) = setup();
        let root_exec = crate::test_support::create_ready_chore_execution(&db, root.clone());
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = ?2, status = 'running' WHERE id = ?1",
            params![root_exec.id, kind],
        )
        .unwrap();
        drop(conn);
        let conflict = revision(&db, &root, true);
        let expected = if kind == "pr_review" {
            ExecutionStatus::Ready
        } else {
            ExecutionStatus::WaitingDependency
        };
        assert_eq!(execution(&db, &conflict).status, expected);
        assert_eq!(
            db.get_execution(&root_exec.id).unwrap().status,
            ExecutionStatus::Running
        );
    }
}

#[test]
fn conflict_priority_preserves_external_prerequisites_and_other_pr_queues() {
    let (db, root) = setup();
    let product = query_task(&db.connect().unwrap(), &root).unwrap().unwrap().product_id;
    let other_root = make_chore_root(&db, &product, "other PR");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/43' WHERE id = ?1",
            [&other_root],
        )
        .unwrap();
    let other = revision(&db, &other_root, false);
    let mut input = revision_input(&root);
    input.depends_on = vec![other.id.clone()];
    let review = db
        .create_revision(input.clone(), &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    input.created_via = Some(format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}external"));
    let conflict = db
        .create_revision(input, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(execution(&db, &other).status, ExecutionStatus::Ready);
    assert_eq!(execution(&db, &conflict).status, ExecutionStatus::WaitingDependency);
    let conn = db.connect().unwrap();
    let gates = deps::gating_prereqs_for(&conn, &review.id).unwrap();
    assert!(gates.contains(&other.id));
    assert!(gates.contains(&conflict.id));
}

fn live_writer_then_conflict(db: &WorkDb, root: &str) -> (Task, WorkExecution, Task) {
    let writer = revision(db, root, false);
    let writer_exec = execution(db, &writer);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'claimed' WHERE id = ?1",
        [&writer_exec.id],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&writer.id])
        .unwrap();
    drop(conn);
    let conflict = revision(db, root, true);
    (writer, writer_exec, conflict)
}

#[test]
fn released_writer_claim_stays_behind_conflict_revision() {
    let (db, root) = setup();
    let (writer, writer_exec, conflict) = live_writer_then_conflict(&db, &root);
    assert!(db.release_dispatch_claim(&writer_exec.id).unwrap());
    let conn = db.connect().unwrap();
    assert_eq!(
        deps::gating_prereqs_for(&conn, &writer.id).unwrap(),
        vec![conflict.id.clone()]
    );
    assert!(deps::gating_prereqs_for(&conn, &conflict.id).unwrap().is_empty());
    drop(conn);
    // The re-claim must be refused: a claimed row counts as a live writer and
    // would otherwise switch the gate off before dispatch checks it.
    assert_eq!(
        db.claim_execution_for_dispatch(&writer_exec.id).unwrap(),
        DispatchClaimOutcome::Rejected
    );
    assert_eq!(
        db.get_execution(&writer_exec.id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );
    finish(&db, &conflict);
    assert!(db.gating_prereqs_for(&writer.id).unwrap().is_empty());
    assert_eq!(
        db.promote_todo_autostart_stuck_executions().unwrap(),
        vec![writer.id.clone()]
    );
    assert_eq!(
        db.get_execution(&writer_exec.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(
        db.claim_execution_for_dispatch(&writer_exec.id).unwrap(),
        DispatchClaimOutcome::Won
    );
}

#[test]
fn failed_writer_retry_stays_behind_conflict_revision() {
    let (db, root) = setup();
    let (writer, writer_exec, conflict) = live_writer_then_conflict(&db, &root);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
        [&writer_exec.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
        [&writer.id],
    )
    .unwrap();
    assert_eq!(
        deps::gating_prereqs_for(&conn, &writer.id).unwrap(),
        vec![conflict.id.clone()]
    );
    assert!(deps::gating_prereqs_for(&conn, &conflict.id).unwrap().is_empty());
    drop(conn);
    // The real retry API refuses to restart a writer while the fix is in flight.
    let request = || RequestExecutionInput::builder().work_item_id(writer.id.clone()).build();
    assert!(db.request_execution(request()).is_err());
    finish(&db, &conflict);
    let retry = db.request_execution(request()).unwrap();
    assert_eq!(retry.status, ExecutionStatus::Ready);
    assert_eq!(
        db.claim_execution_for_dispatch(&retry.id).unwrap(),
        DispatchClaimOutcome::Won
    );
}

#[test]
fn restarted_in_review_revision_waits_for_conflict_revision() {
    let (db, root) = setup();
    let review = revision(&db, &root, false);
    finish(&db, &review);
    let restart = db
        .request_execution(RequestExecutionInput::builder().work_item_id(review.id.clone()).build())
        .unwrap();
    assert_eq!(restart.status, ExecutionStatus::Ready);
    let conflict = revision(&db, &root, true);
    assert_eq!(
        db.get_execution(&restart.id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );
    assert_eq!(
        db.claim_execution_for_dispatch(&restart.id).unwrap(),
        DispatchClaimOutcome::Rejected
    );
    assert_eq!(db.gating_prereqs_for(&review.id).unwrap(), vec![conflict.id.clone()]);
    finish(&db, &conflict);
    assert!(db.gating_prereqs_for(&review.id).unwrap().is_empty());
    // The task stays `in_review`, so only the writer sweep can wake it.
    assert_eq!(
        db.promote_todo_autostart_stuck_executions().unwrap(),
        vec![review.id.clone()]
    );
    assert_eq!(db.get_execution(&restart.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(
        db.claim_execution_for_dispatch(&restart.id).unwrap(),
        DispatchClaimOutcome::Won
    );
}

#[test]
fn demotion_and_wake_stage_status_events_published_only_on_commit() {
    use boss_event_bus::{Event, EventKind, TopicFilter};
    let (db, root) = setup();
    let review = revision(&db, &root, false);
    let review_exec = execution(&db, &review);
    let mut sub = db
        .event_bus()
        .subscribe(TopicFilter::kind(EventKind::ExecutionStatusChanged));
    let conflict = revision(&db, &root, true);
    let changed = |status: &str| Event::ExecutionStatusChanged {
        execution_id: review_exec.id.clone(),
        task_id: review.id.clone(),
        status: status.to_owned(),
    };
    // Events are staged synchronously, so a single poll sees everything published.
    fn next(sub: &mut boss_event_bus::Subscription) -> Option<Event> {
        let mut recv = std::pin::pin!(sub.recv());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match std::future::Future::poll(recv.as_mut(), &mut cx) {
            std::task::Poll::Ready(event) => event,
            std::task::Poll::Pending => None,
        }
    }
    assert_eq!(next(&mut sub), Some(changed("waiting_dependency")));

    finish(&db, &conflict);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&review.id])
        .unwrap();
    // Rolled back: the staged wake-up must never reach subscribers.
    let mut conn = db.connect().unwrap();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut pending = PendingEvents::new();
    super::super::revision_sequencing::promote_waiting_writers(&mut pending, &tx, None).unwrap();
    assert!(!pending.is_empty());
    drop(tx);
    drop(conn);
    drop(pending);
    assert_eq!(next(&mut sub), None);
    assert_eq!(
        db.get_execution(&review_exec.id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );

    // Committed: published.
    db.promote_todo_autostart_stuck_executions().unwrap();
    assert_eq!(next(&mut sub), Some(changed("ready")));
}

#[test]
fn failed_newer_conflict_revision_does_not_strand_older_conflict_revision() {
    let (db, root) = setup();
    let older = revision(&db, &root, true);
    let newer = revision(&db, &root, true);
    assert_eq!(db.gating_prereqs_for(&older.id).unwrap(), vec![newer.id.clone()]);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'failed' WHERE work_item_id = ?1",
        [&newer.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
        [&newer.id],
    )
    .unwrap();
    drop(conn);
    assert!(db.gating_prereqs_for(&older.id).unwrap().is_empty());
}
