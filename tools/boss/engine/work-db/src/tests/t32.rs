//! Deferred / future-scope classification (`tasks.deferred`).
//!
//! A deferred item is auto-unblocked and kept schedulable like any other
//! work item, but the engine must never mint a `ready` execution for it on
//! any automatic path — normal reconcile, the dependency auto-unblock
//! cascade, or a revision reconcile. It runs only after a human explicitly
//! approves it (an explicit `RequestExecution`, which clears the flag). The
//! suppression lives at the mint point (`reconcile_work_item_execution` /
//! `reconcile_revision_execution`), never in `task_accepts_execution` —
//! the cascade bypasses that gate and calls the reconcile helpers directly.
//!
//! These tests pin the behaviours the design calls out:
//!   1. normal item behind a dependency → clears → auto-dispatches (guard),
//!   2. deferred item behind a dependency → clears → `todo`, no execution,
//!   3. explicit approval dispatches the deferred item and clears the flag,
//!   4. the same suppression on the `reconcile_revision_execution` branch,
//!   5. undeferring remints a pending execution that deferral abandoned,
//!      including a gated project task whose prerequisites then clear,
//!   6. an execution that actually ran is not reminted automatically.

use super::*;

/// Read the raw `deferred` column for a task id.
fn deferred_flag(db: &WorkDb, id: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT deferred FROM tasks WHERE id = ?1", params![id], |row| {
            row.get(0)
        })
        .unwrap()
}

/// `true` when `work_item_id` has at least one `ready` execution.
fn has_ready(db: &WorkDb, work_item_id: &str) -> bool {
    db.list_executions(Some(work_item_id))
        .unwrap()
        .iter()
        .any(|e| e.status == ExecutionStatus::Ready)
}

fn status_of(db: &WorkDb, id: &str) -> TaskStatus {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
        other => panic!("expected chore/task, got {other:?}"),
    }
}

/// Regression guard: a **normal** (non-deferred) item gated by a dependency
/// still auto-dispatches the moment its prereq clears. This is the wanted
/// behaviour the deferred classification must not disturb.
#[test]
fn normal_item_behind_dependency_auto_dispatches_on_unblock() {
    let db = WorkDb::open(temp_db_path("deferred-guard-normal")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-normal");
    let gate = create_test_chore(&db, product.id.clone(), "gate");
    let dependent = create_test_chore(&db, product.id.clone(), "dependent");

    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: gate.id.clone(),
        relation: Some(RELATION_BLOCKS.to_owned()),
    })
    .unwrap();
    assert_eq!(
        status_of(&db, &dependent.id),
        TaskStatus::Blocked,
        "new gate blocks the dependent"
    );

    // Clear the gate — the cascade must unblock and mint a ready execution.
    db.update_work_item(
        &gate.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        status_of(&db, &dependent.id),
        TaskStatus::Todo,
        "dependent unblocks to todo"
    );
    assert!(
        has_ready(&db, &dependent.id),
        "a normal dependent must auto-dispatch (ready execution) when its gate clears",
    );
}

/// A **deferred** item gated by a dependency is still unblocked to `todo`
/// and stays visible/schedulable — but no execution is minted, so nothing
/// dispatches until a human approves it.
#[test]
fn deferred_item_behind_dependency_unblocks_but_mints_no_execution() {
    let db = WorkDb::open(temp_db_path("deferred-gated")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-gated");
    let gate = create_test_chore(&db, product.id.clone(), "gate");
    let dependent = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("deferred dependent")
                .deferred(true)
                .build(),
        )
        .unwrap();

    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: gate.id.clone(),
        relation: Some(RELATION_BLOCKS.to_owned()),
    })
    .unwrap();
    assert_eq!(status_of(&db, &dependent.id), TaskStatus::Blocked);

    // Clear the gate — the cascade must still unblock the row...
    db.update_work_item(
        &gate.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        status_of(&db, &dependent.id),
        TaskStatus::Todo,
        "the unblock itself is never suppressed — the deferred row still reaches todo",
    );
    assert!(
        db.list_executions(Some(&dependent.id)).unwrap().is_empty(),
        "a deferred item must not get any execution minted on the auto-unblock cascade",
    );
    assert_eq!(
        deferred_flag(&db, &dependent.id),
        1,
        "the classification survives the unblock"
    );
}

/// A deferred item with no dependency is created straight into `todo`, but
/// the ordinary reconcile pass mints nothing for it.
#[test]
fn deferred_item_is_not_dispatched_by_normal_reconcile() {
    let db = WorkDb::open(temp_db_path("deferred-reconcile")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-reconcile");
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("future work")
                .deferred(true)
                .build(),
        )
        .unwrap();

    db.reconcile_product_executions(&product.id).unwrap();

    assert!(
        db.list_executions(Some(&chore.id)).unwrap().is_empty(),
        "the normal reconcile path must not mint an execution for a deferred item",
    );
}

/// Explicit approval (an operator `RequestExecution` — `bossctl work start`
/// / a kanban drag) dispatches the deferred item AND clears the `deferred`
/// classification, so subsequent reconciles treat it as ordinary work.
#[test]
fn approving_deferred_item_dispatches_and_clears_flag() {
    let db = WorkDb::open(temp_db_path("deferred-approve")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-approve");
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("future work")
                .deferred(true)
                .build(),
        )
        .unwrap();

    // Sanity: auto-reconcile leaves it parked.
    db.reconcile_product_executions(&product.id).unwrap();
    assert!(!has_ready(&db, &chore.id), "deferred item is parked before approval");

    // Approve via explicit RequestExecution.
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    assert!(
        has_ready(&db, &chore.id),
        "an approved deferred item must receive a ready execution",
    );
    assert_eq!(
        deferred_flag(&db, &chore.id),
        0,
        "explicit approval clears the deferred classification (single-shot, like autostart)",
    );
}

/// Re-classifying an already-queued item as deferred (`--deferred true`)
/// immediately pulls its pending execution out of the dispatch pool, so a
/// row that was `ready` can't slip through before the reconcile gate runs.
#[test]
fn marking_queued_item_deferred_abandons_its_pending_execution() {
    let db = WorkDb::open(temp_db_path("deferred-requeue")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-requeue");
    let chore = create_test_chore(&db, product.id.clone(), "queued work");

    // Normal reconcile mints a ready execution (the item is autostart).
    db.reconcile_product_executions(&product.id).unwrap();
    assert!(has_ready(&db, &chore.id), "precondition: the item is queued (ready)");

    // Mark it deferred after it is already queued.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        !has_ready(&db, &chore.id),
        "marking a queued item deferred must abandon its pending ready execution",
    );
    // ...and a subsequent reconcile must not re-mint one.
    db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        !has_ready(&db, &chore.id),
        "the reconcile gate keeps the newly-deferred item parked"
    );
}

/// Companion to [`marking_queued_item_deferred_abandons_its_pending_execution`]:
/// the abandon it exercises must also publish `ExecutionTerminal` for the
/// abandoned row, same as every other terminal-write producer.
#[tokio::test]
async fn marking_queued_item_deferred_publishes_execution_terminal() {
    let db = WorkDb::open(temp_db_path("deferred-requeue-publishes-terminal")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-requeue-terminal");
    let chore = create_test_chore(&db, product.id.clone(), "queued work");

    db.reconcile_product_executions(&product.id).unwrap();
    let ready = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|e| e.status == ExecutionStatus::Ready)
        .expect("precondition: the item is queued (ready)");

    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionTerminal,
    ));

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();

    let event = sub
        .recv()
        .await
        .expect("ExecutionTerminal published after abandoning the pending execution");
    assert_eq!(
        event,
        Event::ExecutionTerminal {
            execution_id: ready.id.clone(),
            task_id: chore.id.clone(),
            host_id: "local".to_owned(),
            pool_claim: None,
        }
    );
}

/// The primary non-start approval affordance: `boss task update --deferred
/// false` clears the classification directly (no `RequestExecution`), and a
/// subsequent reconcile then mints a `ready` execution for the now-ordinary
/// autostart item.
#[test]
fn clearing_deferred_via_update_then_reconcile_dispatches() {
    let db = WorkDb::open(temp_db_path("deferred-update-approve")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-update-approve");
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("future work")
                .deferred(true)
                .build(),
        )
        .unwrap();

    // Sanity: auto-reconcile leaves it parked while still deferred.
    db.reconcile_product_executions(&product.id).unwrap();
    assert!(!has_ready(&db, &chore.id), "deferred item is parked before approval");

    // Approve via `--deferred false` (the update path), not RequestExecution.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(false),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        deferred_flag(&db, &chore.id),
        0,
        "`--deferred false` clears the deferred column directly",
    );
    assert!(
        !has_ready(&db, &chore.id),
        "the update itself does not mint an execution — only reconcile does",
    );

    db.reconcile_product_executions(&product.id).unwrap();

    assert!(
        has_ready(&db, &chore.id),
        "reconcile mints a ready execution for the now-approved autostart item",
    );
}

/// The same suppression on the revision branch: a deferred revision whose
/// chain root has an open PR is not auto-dispatched by
/// `reconcile_revision_execution`.
#[test]
fn deferred_revision_is_not_auto_dispatched() {
    let db = WorkDb::open(temp_db_path("deferred-revision")).unwrap();
    let product_id = make_revision_product(&db, "deferred-rev");
    let pr_url = "https://github.com/spinyfin/mono/pull/4242";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("A future-scope revision.")
                .build(),
            &checker,
        )
        .unwrap();

    // `create_revision` auto-reconciles at creation (before we can mark the
    // row deferred), so it mints a ready execution up front. To isolate the
    // `reconcile_revision_execution` gate we mark the revision future-scope
    // and drop that create-time execution, then re-reconcile: the gate must
    // refuse to re-mint. (There is no create-input surface for `deferred` on
    // revisions; the column is set the same way the update path writes it.)
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET deferred = 1 WHERE id = ?1", params![revision.id])
            .unwrap();
        conn.execute(
            "DELETE FROM work_executions WHERE work_item_id = ?1",
            params![revision.id],
        )
        .unwrap();
    }
    assert!(
        !has_ready(&db, &revision.id),
        "precondition: no execution before re-reconcile"
    );

    db.reconcile_product_executions(&product_id).unwrap();

    assert!(
        !has_ready(&db, &revision.id),
        "a deferred revision must not receive a ready execution from reconcile_revision_execution",
    );
}

fn latest_status(db: &WorkDb, work_item_id: &str) -> ExecutionStatus {
    db.list_executions(Some(work_item_id)).unwrap()[0].status.clone()
}

/// Deferral abandons a pending execution. Clearing deferred must remint
/// once the item is runnable again — otherwise the abandoned row strands it
/// as "last run ended" with no run ever having started.
#[test]
fn undeferring_remints_an_abandoned_never_started_execution() {
    let db = WorkDb::open(temp_db_path("deferred-undefer-remint")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-undefer-remint");
    let chore = create_test_chore(&db, product.id.clone(), "queued work");

    db.reconcile_product_executions(&product.id).unwrap();
    let first = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("precondition: the item is queued (ready)");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(latest_status(&db, &chore.id), ExecutionStatus::Abandoned);

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    db.reconcile_product_executions(&product.id).unwrap();

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    let ready = executions
        .iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("undeferring must mint a replacement ready execution");
    assert_ne!(ready.id, first.id, "the replacement must be a new execution");
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Abandoned)
            .count(),
        1,
        "the abandoned pending row stays in history"
    );
}

/// A project task deferred while waiting on a prerequisite remints and
/// dispatches once both the deferral and the prerequisite clear.
#[test]
fn undeferring_a_gated_project_task_remints_once_prerequisites_are_done() {
    let db = WorkDb::open(temp_db_path("deferred-project-task-remint")).unwrap();
    let product = create_test_product_named(&db, "Boss-deferred-project-task-remint");
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Ordered project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let gate = create_test_chore(&db, product.id.clone(), "gate");
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("gated project task")
                .autostart(true)
                .build(),
        )
        .unwrap();
    complete_design_for_project(&db, &project.id);
    db.add_dependency(AddDependencyInput {
        dependent: task.id.clone(),
        prerequisite: gate.id.clone(),
        relation: Some(RELATION_BLOCKS.to_owned()),
    })
    .unwrap();

    db.reconcile_product_executions(&product.id).unwrap();
    let first = db
        .list_executions(Some(&task.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::WaitingDependency)
        .expect("precondition: the gated task has a waiting_dependency execution");

    db.update_work_item(
        &task.id,
        WorkItemPatch {
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(latest_status(&db, &task.id), ExecutionStatus::Abandoned);

    db.update_work_item(
        &task.id,
        WorkItemPatch {
            deferred: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    db.update_work_item(
        &gate.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    let executions = db.list_executions(Some(&task.id)).unwrap();
    let ready = executions
        .iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("clearing the gate after undefer must mint a ready execution");
    assert_ne!(ready.id, first.id);
    assert_eq!(status_of(&db, &task.id), TaskStatus::Todo);
}

/// Completing a run must not auto-remint; the card stays "start it again".
#[test]
fn a_finished_run_is_not_reminted_by_reconcile() {
    let db = WorkDb::open(temp_db_path("finished-run-no-remint")).unwrap();
    let product = create_test_product_named(&db, "Boss-finished-run-no-remint");
    let chore = create_test_chore(&db, product.id.clone(), "already ran");

    db.reconcile_product_executions(&product.id).unwrap();
    let first = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("precondition: the item is queued (ready)");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', started_at = '1', finished_at = '2' WHERE id = ?1",
            params![first.id],
        )
        .unwrap();

    db.reconcile_product_executions(&product.id).unwrap();

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 1, "a finished run must not be reminted");
    assert_eq!(executions[0].id, first.id);
    assert_eq!(executions[0].status, ExecutionStatus::Completed);
}

/// An abandoned execution that actually started is a finished attempt, not
/// a withdrawn pending row, so reconcile must not remint it.
#[test]
fn an_abandoned_execution_that_started_is_not_reminted() {
    let db = WorkDb::open(temp_db_path("abandoned-started-no-remint")).unwrap();
    let product = create_test_product_named(&db, "Boss-abandoned-started-no-remint");
    let chore = create_test_chore(&db, product.id.clone(), "ran then abandoned");

    db.reconcile_product_executions(&product.id).unwrap();
    let first = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("precondition: the item is queued (ready)");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned', started_at = '1', finished_at = '2' WHERE id = ?1",
            params![first.id],
        )
        .unwrap();

    db.reconcile_product_executions(&product.id).unwrap();

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "an abandoned execution that started must not be reminted"
    );
    assert_eq!(executions[0].id, first.id);
    assert_eq!(executions[0].status, ExecutionStatus::Abandoned);
}

/// A `work_runs` row is evidence the execution ran even when `started_at`
/// was never stamped, so it must not be reminted as a never-started abandon.
#[test]
fn an_abandoned_execution_with_a_run_is_not_reminted() {
    let db = WorkDb::open(temp_db_path("abandoned-with-run-no-remint")).unwrap();
    let product = create_test_product_named(&db, "Boss-abandoned-with-run-no-remint");
    let chore = create_test_chore(&db, product.id.clone(), "has a run row");

    db.reconcile_product_executions(&product.id).unwrap();
    let first = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("precondition: the item is queued (ready)");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            params![first.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, host_id)
             VALUES ('run_abandoned_evidence', ?1, 'synthetic', 'completed', '1', 'local')",
            params![first.id],
        )
        .unwrap();
    }

    db.reconcile_product_executions(&product.id).unwrap();

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "an abandoned execution with a work_runs row must not be reminted"
    );
    assert_eq!(executions[0].id, first.id);
}
