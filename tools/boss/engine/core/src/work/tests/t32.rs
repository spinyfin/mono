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
//! These tests pin all four behaviours the design calls out:
//!   1. normal item behind a dependency → clears → auto-dispatches (guard),
//!   2. deferred item behind a dependency → clears → `todo`, no execution,
//!   3. explicit approval dispatches the deferred item and clears the flag,
//!   4. the same suppression on the `reconcile_revision_execution` branch.

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
