//! Every pending execution carries an engine-stated wait reason: the
//! scheduler's recorded reason when it has one, `ordinal_chain` for a project
//! task queued behind an earlier sibling, and `awaiting_scheduler` for a
//! runnable row the scheduler has not stamped yet.

use super::*;

fn project_with_two_tasks() -> (WorkDb, String, Task, Task) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Ordinal chain".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let make = |name: &str| {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name(name)
                .build(),
        )
        .unwrap()
    };
    let first = make("First");
    let second = make("Second");
    complete_design_for_project(&db, &project.id);
    (db, product.id, first, second)
}

/// The ordinal chain flips every later project task to `waiting_dependency`
/// without a declared prerequisite, and the scheduler never stamps those
/// rows. The engine names the earlier task they wait for.
#[test]
fn a_later_project_task_waits_on_the_ordinal_chain_and_names_its_blocker() {
    let (db, product_id, first, second) = project_with_two_tasks();
    db.reconcile_product_executions(&product_id).unwrap();

    let runtime = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::WaitingDependency));
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("ordinal_chain"));
    let blocker = runtime.dispatch_wait_blocker.expect("the earlier task is named");
    assert_eq!(blocker.work_item_id, first.id);
    assert_eq!(blocker.kind, "task");

    // The head of the chain is runnable and simply unstamped.
    let head = db.get_task_runtime(&first.id).unwrap();
    assert_eq!(head.execution_status, Some(ExecutionStatus::Ready));
    assert_eq!(head.dispatch_wait_reason.as_deref(), Some("awaiting_scheduler"));
    assert!(head.dispatch_wait_blocker.is_none());
}

/// A `ready` or `queued` execution with no scheduler stamp is
/// `awaiting_scheduler`; a stamp the scheduler recorded wins.
#[test]
fn an_unstamped_ready_execution_is_awaiting_the_scheduler() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    let runtime = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("awaiting_scheduler"));

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = 'pool_exhausted' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    let stamped = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(stamped.dispatch_wait_reason.as_deref(), Some("pool_exhausted"));
}

/// Once the earlier task completes, the later one is first in the chain and
/// no longer reports an ordinal-chain wait.
#[test]
fn the_ordinal_chain_wait_ends_when_the_earlier_task_is_done() {
    let (db, product_id, first, second) = project_with_two_tasks();
    db.reconcile_product_executions(&product_id).unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&first.id])
        .unwrap();

    // Still `waiting_dependency` until the next reconcile promotes it: the
    // engine says so rather than leaving the card with no reason.
    let runtime = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::WaitingDependency));
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("awaiting_reconcile"));
    assert!(runtime.dispatch_wait_blocker.is_none());
}

/// A scheduler stamp left over from an earlier `ready` period must not hide
/// the ordinal gate a row came to wait behind: the scheduler never drains
/// `waiting_dependency` rows, so the stale label would last indefinitely.
#[test]
fn a_stale_scheduler_stamp_does_not_hide_an_ordinal_chain_gate() {
    let (db, product_id, first, second) = project_with_two_tasks();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&first.id])
        .unwrap();
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.get_task_runtime(&second.id).unwrap().execution_id.unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = 'pool_exhausted' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    // The earlier sibling is reopened, so the later one is gated again.
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'todo' WHERE id = ?1", [&first.id])
        .unwrap();
    db.reconcile_product_executions(&product_id).unwrap();

    let runtime = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::WaitingDependency));
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("ordinal_chain"));
    assert_eq!(runtime.dispatch_wait_blocker.map(|b| b.work_item_id), Some(first.id));
}

/// The whole-tree fetch resolves the ordinal gate from the tasks it already
/// holds, so the lookups it makes do not grow with the number of earlier
/// siblings: both fetches below make the same per-row lookups.
#[test]
fn the_work_tree_ordinal_gate_costs_the_same_lookups_regardless_of_chain_length() {
    let lookups = |extra_tasks: usize| {
        let (db, product_id, _first, _second) = project_with_two_tasks();
        let project_id = list_projects_for_product(&db.connect().unwrap(), &product_id).unwrap()[0]
            .id
            .clone();
        for index in 0..extra_tasks {
            db.create_task(
                CreateTaskInput::builder()
                    .product_id(product_id.clone())
                    .project_id(project_id.clone())
                    .name(format!("Extra {index}"))
                    .build(),
            )
            .unwrap();
        }
        db.reconcile_product_executions(&product_id).unwrap();
        let conn = db.connect().unwrap();
        let tasks = list_tasks_for_product(&conn, &product_id).unwrap();
        let (_runtimes, queries) = collect_task_runtimes(&conn, &tasks, &[]).unwrap();
        (queries, tasks.len() as u64)
    };
    let (small_queries, small_rows) = lookups(0);
    let (large_queries, large_rows) = lookups(6);
    let per_row_small = small_queries.div_ceil(small_rows);
    let per_row_large = large_queries.div_ceil(large_rows);
    assert!(
        per_row_large <= per_row_small + 1,
        "per-row lookups grew with chain length: {per_row_small} -> {per_row_large}"
    );
}
