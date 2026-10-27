//! Regression tests for the "engine re-dispatches a revision after its
//! execution already completed" incident.
//!
//! The sequence under test: a revision worker pushes and is finalised with
//! `WorkerPrCompletionTarget::PendingReview`, which leaves the revision row
//! `active` with its `revision_implementation` execution `completed` while
//! the reviewer batch (keyed on the chain root) runs. Every automatic
//! re-dispatcher that reads "active + latest execution terminal" as orphaned
//! work must leave that row alone; only an explicit start re-opens it. A
//! re-opened prerequisite revision must also keep gating its dependents until
//! its re-run terminates (the dependency inversion half of the incident).

use super::*;

/// Product with a resolvable repo (the revision reconciler refuses to mint
/// when it cannot resolve one), an `in_review` chain root with an open PR,
/// and one revision created against it.
fn seed_delivered_revision(label: &str) -> (WorkDb, String, String, String, &'static str) {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product = create_test_product_with_repo(&db, &format!("Prod-{label}"), Some("git@github.com:foo/bar.git"));
    let pr_url = "https://github.com/spinyfin/mono/pull/3257";
    let root_id = make_in_review_chore(&db, &product.id, pr_url);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();
    (db, product.id, root_id, revision.id, pr_url)
}

/// Drive one implementation run of `revision_id` to the point where its
/// worker has pushed and the engine finalised it with the review hold.
fn run_revision_to_pending_review_hold(db: &WorkDb, revision_id: &str, pr_url: &str, head: &str) -> String {
    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id)
                .explicit_start(true)
                .created_via("test:explicit_start")
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    db.start_execution_run(
        &exec.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    assert_eq!(
        task_status(db, revision_id),
        "active",
        "run start moves the revision to Doing"
    );
    db.record_worker_pr_completion(
        &exec.id,
        pr_url,
        Some(head),
        None,
        WorkerPrCompletionTarget::PendingReview,
        None,
    )
    .unwrap()
    .expect("completion must apply to a live execution");
    exec.id
}

/// The incident sequence, step for step: a completed revision held for
/// review is `active` with a terminal execution and no live execution of
/// its own (the reviewer runs against the chain root). The product-wide
/// reconciler used to read that as "needs dispatch" and mint a fresh
/// `revision_implementation` that redid the landed work.
#[test]
fn product_reconcile_does_not_remint_a_delivered_revision_held_for_review() {
    let (db, product_id, _root_id, revision_id, pr_url) = seed_delivered_revision("delivered-hold-reconcile");
    let exec_id = run_revision_to_pending_review_hold(&db, &revision_id, pr_url, "5f5f46ab");

    // Hold state: row still `active`, execution `completed`, nothing live.
    assert_eq!(task_status(&db, &revision_id), "active");
    assert_eq!(
        executions_for(&db, &revision_id),
        vec![(exec_id.clone(), "completed".to_owned())]
    );

    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert!(
        result.created.is_empty(),
        "reconcile must not mint a new execution for a delivered revision; created={:?}",
        result.created.iter().map(|e| (&e.id, &e.kind)).collect::<Vec<_>>()
    );
    assert_eq!(
        executions_for(&db, &revision_id),
        vec![(exec_id.clone(), "completed".to_owned())],
        "the completed execution must remain the revision's only execution"
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "the hold must be left in place"
    );

    // The verdict releases the hold; the row rests in `in_review` and a
    // later reconcile still leaves it alone.
    assert!(db.advance_held_pending_review_task_to_in_review(&revision_id).unwrap());
    assert_eq!(task_status(&db, &revision_id), "in_review");
    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert!(result.created.is_empty());
    assert_eq!(executions_for(&db, &revision_id).len(), 1);
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

/// The startup rehydration, the on-worker-release rescan and the orphan
/// sweep's candidate query are the other three "active row, terminal
/// execution" re-dispatchers. None may touch a delivered revision.
#[test]
fn startup_rescan_and_orphan_paths_skip_a_delivered_revision() {
    let (db, _product_id, _root_id, revision_id, pr_url) = seed_delivered_revision("delivered-hold-sweeps");
    run_revision_to_pending_review_hold(&db, &revision_id, pr_url, "5f5f46ab");
    let before = executions_for(&db, &revision_id);

    assert!(
        db.reconcile_active_dispatch(|_| false).unwrap().is_empty(),
        "startup reconcile must not redispatch a delivered revision"
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&revision_id])
        .unwrap();
    assert!(
        db.rescan_active_dispatch().unwrap().is_empty(),
        "on-release rescan must not redispatch a delivered revision"
    );
    // Age the row past any min-age guard; the batch-based exclusion does
    // not apply here (no pre-merge batch row exists), so this exercises the
    // delivery exclusion itself.
    db.force_updated_at_for_test(&revision_id, boss_engine_utils::epoch_time::now_epoch_secs() - 600)
        .unwrap();
    assert!(
        db.list_orphan_active_candidates(0).unwrap().is_empty(),
        "orphan sweep must not treat a delivered revision as an orphan"
    );
    assert_eq!(executions_for(&db, &revision_id), before);
}

/// What legitimately re-opens a delivered revision: an explicit start. It
/// must still mint, and the run start may still pull the row to Doing.
#[test]
fn explicit_start_still_reopens_a_delivered_revision() {
    let (db, _product_id, _root_id, revision_id, pr_url) = seed_delivered_revision("delivered-explicit-start");
    run_revision_to_pending_review_hold(&db, &revision_id, pr_url, "5f5f46ab");
    assert!(db.advance_held_pending_review_task_to_in_review(&revision_id).unwrap());

    let reopened = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .created_via("frontend:request_execution")
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(reopened.status, ExecutionStatus::Ready);
    assert_eq!(reopened.created_via.as_deref(), Some("frontend:request_execution"));
    db.start_execution_run(
        &reopened.id,
        "worker-2",
        "mono",
        "lease-2",
        "mono-agent-002",
        "/tmp/mono-agent-002",
    )
    .unwrap();
    assert_eq!(task_status(&db, &revision_id), "active");
}

/// A revision whose latest execution ended any way other than `completed`
/// did not deliver: the ordinary recovery mint must still happen for it.
#[test]
fn reconcile_still_remints_a_revision_whose_worker_died() {
    let (db, product_id, _root_id, revision_id, _pr_url) = seed_delivered_revision("dead-worker-remint");
    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .created_via("test:explicit_start")
                .build(),
            |_| false,
        )
        .unwrap();
    db.start_execution_run(
        &exec.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    db.mark_execution_orphaned(&exec.id, "worker process gone").unwrap();
    assert_eq!(task_status(&db, &revision_id), "active");

    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(
        result.created.len(),
        1,
        "a dead worker's revision is re-minted as before"
    );
    let created = &result.created[0];
    assert_eq!(created.work_item_id, revision_id);
    assert_eq!(created.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(created.created_via.as_deref(), Some("reconcile_revision_execution"));
}

/// Dependency inversion: a dependent revision must not run while its
/// same-chain prerequisite has been re-opened and is queued behind it. The
/// prerequisite's `in_review` status describes what it delivered *before*
/// the re-open, so it stops satisfying the edge until the re-run terminates.
#[test]
fn reopened_prerequisite_revision_keeps_gating_its_dependent() {
    let (db, product_id, root_id, prereq_id, pr_url) = seed_delivered_revision("reopened-prereq-gates");
    run_revision_to_pending_review_hold(&db, &prereq_id, pr_url, "5f5f46ab");
    assert!(db.advance_held_pending_review_task_to_in_review(&prereq_id).unwrap());

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let dependent = db.create_revision(revision_input(&root_id), &checker).unwrap();
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prereq_id.clone(),
        relation: None,
    })
    .unwrap();
    assert!(
        db.gating_prereqs_for(&dependent.id).unwrap().is_empty(),
        "a same-chain in_review prerequisite satisfies a revision dependent"
    );

    // Re-open the prerequisite (explicit start) — its execution is queued.
    let reopened = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(&prereq_id)
                .explicit_start(true)
                .created_via("test:explicit_start")
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(reopened.status, ExecutionStatus::Ready);
    assert_eq!(
        task_status(&db, &prereq_id),
        "in_review",
        "queueing alone does not move the row"
    );
    assert_eq!(
        db.gating_prereqs_for(&dependent.id).unwrap(),
        vec![prereq_id.clone()],
        "a re-opened prerequisite must gate its dependent until the re-run terminates"
    );

    // The reconciler sees the gate and parks the dependent's execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let dependent_exec = db
        .list_executions(Some(&dependent.id))
        .unwrap()
        .into_iter()
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)))
        .expect("dependent gets an execution");
    assert_eq!(dependent_exec.status, ExecutionStatus::WaitingDependency);

    // The re-run lands: the gate clears and the dependent is promoted.
    db.start_execution_run(
        &reopened.id,
        "worker-2",
        "mono",
        "lease-2",
        "mono-agent-002",
        "/tmp/mono-agent-002",
    )
    .unwrap();
    db.record_worker_pr_completion(
        &reopened.id,
        pr_url,
        Some("6a6a57bc"),
        None,
        WorkerPrCompletionTarget::InReview,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(task_status(&db, &prereq_id), "in_review");
    assert!(db.gating_prereqs_for(&dependent.id).unwrap().is_empty());
    db.reconcile_product_executions(&product_id).unwrap();
    let dependent_exec = db.get_execution(&dependent_exec.id).unwrap();
    assert_eq!(dependent_exec.status, ExecutionStatus::Ready);
}

/// Every execution row names the code path that minted it.
#[test]
fn execution_rows_carry_creation_provenance() {
    let (db, product_id, _root_id, revision_id, _pr_url) = seed_delivered_revision("exec-provenance");

    // Creating a revision already queues its initial implementation.
    let initial = db.list_executions(Some(&revision_id)).unwrap().pop().unwrap();
    assert_eq!(initial.created_via.as_deref(), Some("reconcile_revision_execution"));
    db.mark_execution_orphaned(&initial.id, "initial worker lost").unwrap();

    let requested = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .created_via("orphan_sweep")
                .build(),
        )
        .unwrap();
    assert_eq!(requested.created_via.as_deref(), Some("orphan_sweep"));
    let listed = db.list_executions(Some(&revision_id)).unwrap();
    let listed_request = listed.iter().find(|row| row.id == requested.id).unwrap();
    assert_eq!(listed_request.created_via.as_deref(), Some("orphan_sweep"));
    let reused = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .created_via("different_requester")
                .build(),
        )
        .unwrap();
    assert_eq!(reused.id, requested.id);
    assert_eq!(reused.created_via.as_deref(), Some("orphan_sweep"));

    // A request that does not say falls back to the generic label, never NULL.
    db.mark_execution_orphaned(&requested.id, "test").unwrap();
    let generic = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&revision_id).build())
        .unwrap();
    assert_eq!(generic.created_via.as_deref(), Some("request_execution"));

    // Reconcile-minted rows name the reconciler.
    db.mark_execution_orphaned(&generic.id, "test").unwrap();
    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(result.created.len(), 1);
    assert_eq!(
        result.created[0].created_via.as_deref(),
        Some("reconcile_revision_execution")
    );
}

/// Engine-initiated status moves stamp `status_reason`, every move lands in
/// the transition ledger with its actor, and a human move clears the reason.
#[test]
fn status_moves_record_reason_and_transition_ledger() {
    let (db, _product_id, _root_id, revision_id, pr_url) = seed_delivered_revision("status-ledger");
    assert!(db.list_task_status_transitions(&revision_id).unwrap().is_empty());

    run_revision_to_pending_review_hold(&db, &revision_id, pr_url, "5f5f46ab");
    let WorkItem::Task(held) = db.get_work_item(&revision_id).unwrap() else {
        panic!("revision is a task row");
    };
    assert_eq!(held.status, TaskStatus::Active);
    assert_eq!(held.status_reason.as_deref(), Some("pr_completion:pending_review_hold"));

    assert!(db.advance_held_pending_review_task_to_in_review(&revision_id).unwrap());
    let WorkItem::Task(released) = db.get_work_item(&revision_id).unwrap() else {
        panic!("revision is a task row");
    };
    assert_eq!(released.status, TaskStatus::InReview);
    assert_eq!(
        released.status_reason.as_deref(),
        Some("review_hold_released:no_reviewer_enqueued")
    );

    let transitions = db.list_task_status_transitions(&revision_id).unwrap();
    let summary: Vec<(String, String, String, Option<String>)> = transitions
        .iter()
        .map(|t| {
            (
                t.from_status.clone(),
                t.to_status.clone(),
                t.actor.clone(),
                t.reason.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "todo".to_owned(),
                "active".to_owned(),
                "engine".to_owned(),
                Some("execution_run_started".to_owned())
            ),
            (
                "active".to_owned(),
                "in_review".to_owned(),
                "engine".to_owned(),
                Some("review_hold_released:no_reviewer_enqueued".to_owned())
            ),
        ]
    );
    assert!(
        transitions.windows(2).all(|w| w[0].id < w[1].id),
        "ledger ids are chronological"
    );

    // A human move clears the engine's reason and is recorded as such.
    db.update_work_item(
        &revision_id,
        WorkItemPatch {
            status: Some("todo".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let WorkItem::Task(moved) = db.get_work_item(&revision_id).unwrap() else {
        panic!("revision is a task row");
    };
    assert_eq!(moved.status, TaskStatus::Todo);
    assert_eq!(moved.status_reason, None);
    let last = db.list_task_status_transitions(&revision_id).unwrap().pop().unwrap();
    assert_eq!(
        (last.from_status.as_str(), last.to_status.as_str()),
        ("in_review", "todo")
    );
    assert_eq!(last.actor, "human");
    assert_eq!(last.reason, None);
}
