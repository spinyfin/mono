use std::collections::HashMap;

use boss_protocol::{CreateExecutionInput, DispatchPauseScope, ExecutionKind, ExecutionStatus};
use rusqlite::params;

use super::*;
use crate::coordinator::DispatchPauseOrigin;
use crate::test_support::{create_test_product, open_db};

/// Raw-insert a task so a test controls `kind`, `created_via` and `priority`
/// without standing up a revision chain (same shape as the dispatch-class tests).
fn insert_task(db: &WorkDb, product_id: &str, id: &str, kind: &str, created_via: &str, priority: &str) {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
             VALUES (?1, ?2, NULL, ?3, 'Raw', '', 'active', NULL, NULL, NULL, '1', '1', 1, ?4, ?5)",
            params![id, product_id, kind, priority, created_via],
        )
        .unwrap();
}

/// A `ready` execution whose `created_at` is pinned, so ordering never depends
/// on the clock.
fn ready_execution(db: &WorkDb, task_id: &str, created_at: &str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?1 WHERE id = ?2",
            params![created_at, execution.id],
        )
        .unwrap();
    execution.id
}

fn pools(entries: &[(&str, DispatchPool)]) -> HashMap<String, DispatchPool> {
    entries.iter().map(|(id, pool)| ((*id).to_owned(), *pool)).collect()
}

fn classify(
    db: &WorkDb,
    pause: Option<&DispatchPause>,
    automation_paused: bool,
    pool_by_task: &HashMap<String, DispatchPool>,
) -> BTreeMap<String, QueuedDispatch> {
    classify_with_chain_blocked(db, pause, automation_paused, pool_by_task, &[])
}

fn classify_with_chain_blocked(
    db: &WorkDb,
    pause: Option<&DispatchPause>,
    automation_paused: bool,
    pool_by_task: &HashMap<String, DispatchPool>,
    chain_blocked_tasks: &[&str],
) -> BTreeMap<String, QueuedDispatch> {
    let pool_of = |execution: &WorkExecution| {
        pool_by_task
            .get(&execution.work_item_id)
            .copied()
            .unwrap_or(DispatchPool::Main)
    };
    classify_queued_dispatch(
        db,
        &DispatchGate {
            pause,
            automation_paused,
            pool_of: &pool_of,
            chain_blocked: &|execution| chain_blocked_tasks.contains(&execution.work_item_id.as_str()),
        },
    )
}

fn operator_pause(scope: DispatchPauseScope) -> DispatchPause {
    DispatchPause {
        origin: DispatchPauseOrigin::Operator,
        scope,
        since_epoch_s: 1,
        reason: "test".into(),
    }
}

fn ranked(entries: &BTreeMap<String, QueuedDispatch>) -> Vec<String> {
    let mut ids: Vec<_> = entries
        .iter()
        .filter_map(|(id, entry)| entry.position.map(|position| (position, id.clone())))
        .collect();
    ids.sort();
    ids.into_iter().map(|(_, id)| id).collect()
}

/// Critical, each revision class and plain rows, plus a task whose retry
/// produced a newer execution than a task created after it.
#[test]
fn positions_follow_list_ready_executions_across_classes_and_retries() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    insert_task(&db, &product, "task_plain_old", "chore", "cli", "medium");
    insert_task(&db, &product, "task_plain_new", "chore", "cli", "medium");
    insert_task(&db, &product, "task_retried", "chore", "cli", "medium");
    insert_task(&db, &product, "task_other_revision", "revision", "bossctl", "medium");
    insert_task(
        &db,
        &product,
        "task_review_revision",
        "revision",
        "pr_review:exec_1",
        "medium",
    );
    insert_task(&db, &product, "task_ci_fix", "revision", "ci-fix:cir_1", "medium");
    insert_task(
        &db,
        &product,
        "task_conflict_fix",
        "revision",
        "merge-conflict:crz_1",
        "medium",
    );
    insert_task(&db, &product, "task_critical_plain", "chore", "cli", "critical");

    // `retried` is the oldest task: its first attempt failed and the retry
    // created a new, newer execution — later than `plain_new`'s.
    let first_attempt = ready_execution(&db, "task_retried", "1000");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
            [&first_attempt],
        )
        .unwrap();
    ready_execution(&db, "task_plain_old", "1001");
    ready_execution(&db, "task_plain_new", "1002");
    ready_execution(&db, "task_retried", "1003");
    ready_execution(&db, "task_other_revision", "1004");
    ready_execution(&db, "task_review_revision", "1005");
    ready_execution(&db, "task_ci_fix", "1006");
    ready_execution(&db, "task_conflict_fix", "1007");
    ready_execution(&db, "task_critical_plain", "1008");

    let entries = classify(&db, None, false, &HashMap::new());

    let dispatcher_order: Vec<String> = db
        .list_ready_executions()
        .unwrap()
        .into_iter()
        .map(|execution| execution.work_item_id)
        .collect();
    assert_eq!(
        ranked(&entries),
        dispatcher_order,
        "board order is the dispatcher's order"
    );
    assert_eq!(
        dispatcher_order,
        [
            "task_critical_plain",
            "task_conflict_fix",
            "task_ci_fix",
            "task_review_revision",
            "task_other_revision",
            "task_plain_old",
            "task_plain_new",
            "task_retried",
        ],
        "critical first, then revision classes, then plain rows by execution age",
    );
    let positions: Vec<_> = dispatcher_order
        .iter()
        .map(|id| entries[id].position.unwrap())
        .collect();
    assert_eq!(positions, (0..8).collect::<Vec<_>>());
    assert!(entries["task_critical_plain"].next_up);
    assert_eq!(entries.values().filter(|entry| entry.next_up).count(), 1);
}

#[test]
fn rows_the_dispatcher_will_not_take_have_no_position() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    insert_task(&db, &product, "task_runnable", "chore", "cli", "medium");
    insert_task(&db, &product, "task_backoff", "chore", "cli", "medium");
    insert_task(&db, &product, "task_dependent", "chore", "cli", "medium");
    ready_execution(&db, "task_runnable", "1000");
    let backoff = ready_execution(&db, "task_backoff", "1001");
    let dependent = ready_execution(&db, "task_dependent", "1002");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = '99999999999' WHERE id = ?1",
            [&backoff],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'waiting_dependency' WHERE id = ?1",
            [&dependent],
        )
        .unwrap();

    let pools = pools(&[
        ("task_backoff", DispatchPool::Automation),
        ("task_dependent", DispatchPool::Review),
    ]);
    let entries = classify(&db, None, false, &pools);

    assert_eq!(
        entries.keys().collect::<Vec<_>>(),
        ["task_backoff", "task_dependent", "task_runnable"]
    );
    assert_eq!(entries["task_runnable"].position, Some(0));
    assert_eq!(entries["task_backoff"].pool, DispatchPool::Automation);
    assert_eq!(entries["task_dependent"].pool, DispatchPool::Review);
    for id in ["task_backoff", "task_dependent"] {
        assert_eq!(entries[id].position, None, "{id} is not on the ready walk");
        assert!(!entries[id].next_up);
    }
}

#[test]
fn a_ready_revision_behind_a_live_chain_writer_is_not_ranked_or_next_up() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    // The revision sorts ahead of the plain chore in the dispatcher's order.
    insert_task(&db, &product, "task_revision", "revision", "bossctl", "medium");
    insert_task(&db, &product, "task_runnable", "chore", "cli", "medium");
    ready_execution(&db, "task_revision", "1000");
    ready_execution(&db, "task_runnable", "1001");

    let entries = classify_with_chain_blocked(&db, None, false, &HashMap::new(), &["task_revision"]);

    assert_eq!(entries["task_revision"].position, None);
    assert!(!entries["task_revision"].next_up);
    assert_eq!(entries["task_runnable"].position, Some(0));
    assert!(entries["task_runnable"].next_up);
}

#[test]
fn only_the_first_runnable_main_pool_card_is_next_up() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    for (index, id) in [
        "task_review_first",
        "task_auto_second",
        "task_main_third",
        "task_main_fourth",
    ]
    .iter()
    .enumerate()
    {
        insert_task(&db, &product, id, "chore", "cli", "medium");
        ready_execution(&db, id, &format!("{}", 1000 + index));
    }
    let pools = pools(&[
        ("task_review_first", DispatchPool::Review),
        ("task_auto_second", DispatchPool::Automation),
    ]);

    let entries = classify(&db, None, false, &pools);

    assert_eq!(entries["task_review_first"].pool, DispatchPool::Review);
    assert_eq!(entries["task_auto_second"].pool, DispatchPool::Automation);
    assert_eq!(
        entries["task_main_third"].position,
        Some(2),
        "positions interleave pools"
    );
    let next_up: Vec<_> = entries.iter().filter(|(_, e)| e.next_up).map(|(id, _)| id).collect();
    assert_eq!(
        next_up,
        ["task_main_third"],
        "review and automation cards never claim next-up"
    );
}

#[test]
fn a_new_work_pause_holds_main_and_automation_but_not_reviews_or_critical_rows() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    insert_task(&db, &product, "task_main", "chore", "cli", "medium");
    insert_task(&db, &product, "task_review", "chore", "cli", "medium");
    insert_task(&db, &product, "task_critical", "chore", "cli", "critical");
    ready_execution(&db, "task_main", "1000");
    ready_execution(&db, "task_review", "1001");
    ready_execution(&db, "task_critical", "1002");
    let pools = pools(&[("task_review", DispatchPool::Review)]);

    let entries = classify(&db, Some(&operator_pause(DispatchPauseScope::NewWork)), false, &pools);

    assert_eq!(entries["task_main"].position, None);
    assert_eq!(
        entries["task_review"].position,
        Some(1),
        "held rows do not consume a rank"
    );
    assert_eq!(entries["task_critical"].position, Some(0));
    assert!(entries["task_critical"].next_up);
    assert!(!entries["task_main"].next_up);

    let all_spawns = classify(&db, Some(&operator_pause(DispatchPauseScope::AllSpawns)), false, &pools);
    assert!(
        all_spawns
            .values()
            .all(|entry| entry.position.is_none() && !entry.next_up)
    );
}

#[test]
fn automation_pause_holds_only_automation_rows() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db).id;
    insert_task(&db, &product, "task_auto", "chore", "cli", "medium");
    insert_task(&db, &product, "task_main", "chore", "cli", "medium");
    ready_execution(&db, "task_auto", "1000");
    ready_execution(&db, "task_main", "1001");
    let pools = pools(&[("task_auto", DispatchPool::Automation)]);

    let entries = classify(&db, None, true, &pools);

    assert_eq!(entries["task_auto"].position, None);
    assert_eq!(entries["task_main"].position, Some(0));
    assert!(entries["task_main"].next_up);
}

/// Backing off the first queued card through a direct mutation reorders the
/// dispatcher's queue without any other wait reason changing; the stored order
/// must follow without a worker-slot event.
#[tokio::test]
async fn direct_mutation_invalidation_refreshes_the_queued_order() {
    let (state, _temp) = crate::app::tests::test_server_state();
    let product = create_test_product(&state.work_db);
    let ready = |name: &str, created_at: &str| {
        let task = crate::test_support::create_test_chore(&state.work_db, &product.id, name);
        let execution = state
            .work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&task.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![created_at, execution.id],
            )
            .unwrap();
        (task.id, execution.id)
    };
    let (first_task, first_execution) = ready("first", "1000");
    let (second_task, _) = ready("second", "1001");
    assert!(state.refresh_queued_dispatch());
    assert_eq!(
        state.published_queued_dispatch.lock().unwrap()[&first_task].position,
        Some(0)
    );
    assert_eq!(
        state.published_queued_dispatch.lock().unwrap()[&second_task].position,
        Some(1)
    );

    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = '99999999999' WHERE id = ?1",
            [&first_execution],
        )
        .unwrap();
    crate::app::handler_helpers::publish_work_invalidation(
        &state,
        "session",
        "request",
        vec![],
        "test_mutation",
        Some(product.id.clone()),
        vec![first_task.clone()],
    )
    .await;

    // The kick the mutation issues may also dispatch the second card, so
    // assert on the card that left the queue's front.
    for _ in 0..100 {
        let position = state
            .published_queued_dispatch
            .lock()
            .unwrap()
            .get(&first_task)
            .map(|entry| entry.position);
        if position == Some(None) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "queued order was not republished after the direct mutation: {:?}",
        state.published_queued_dispatch.lock().unwrap()
    );
}

/// An execution of `kind` in `status` on `task_id`, created through the same
/// path the dispatcher's own rows take.
fn execution_in(db: &WorkDb, task_id: &str, kind: ExecutionKind, status: ExecutionStatus, created_at: &str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task_id)
                .kind(kind)
                .status(status)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?1 WHERE id = ?2",
            params![created_at, execution.id],
        )
        .unwrap();
    execution.id
}

/// A chain root with a live execution of `writer_kind`, a ready revision on
/// that chain, and an unrelated ready chore queued behind the revision.
fn chain_fixture(db: &WorkDb, live_kind: ExecutionKind) {
    let product = create_test_product(db).id;
    insert_task(db, &product, "task_parent", "chore", "cli", "medium");
    insert_task(db, &product, "task_revision", "revision", "bossctl", "medium");
    insert_task(db, &product, "task_runnable", "chore", "cli", "medium");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET parent_task_id = 'task_parent' WHERE id = 'task_revision'",
            [],
        )
        .unwrap();
    execution_in(db, "task_parent", live_kind, ExecutionStatus::Running, "900");
    ready_execution(db, "task_revision", "1000");
    ready_execution(db, "task_runnable", "1001");
}

/// Runs the production chain projection through the real coordinator.
#[tokio::test]
async fn a_revision_behind_a_live_chain_writer_is_held_by_the_real_projection() {
    let (state, _temp) = crate::app::tests::test_server_state();
    chain_fixture(&state.work_db, ExecutionKind::ChoreImplementation);

    let entries = state.queued_dispatch_snapshot();

    assert_eq!(entries["task_revision"].position, None);
    assert!(!entries["task_revision"].next_up);
    assert_eq!(entries["task_runnable"].position, Some(0));
    assert!(entries["task_runnable"].next_up);
}

/// A read-only review never holds a revision, so it stays ranked.
#[tokio::test]
async fn a_live_read_only_review_does_not_hold_a_revision_in_the_real_projection() {
    let (state, _temp) = crate::app::tests::test_server_state();
    chain_fixture(&state.work_db, ExecutionKind::PrReview);

    let entries = state.queued_dispatch_snapshot();

    assert_eq!(entries["task_revision"].position, Some(0));
    assert!(entries["task_revision"].next_up);
    assert_eq!(entries["task_runnable"].position, Some(1));
}

/// A refresh blocked behind another must compute after it, so the order it
/// stores reflects writes made while it waited.
#[tokio::test]
async fn a_refresh_waiting_on_another_computes_after_it() {
    let (state, _temp) = crate::app::tests::test_server_state();
    let product = create_test_product(&state.work_db).id;
    insert_task(&state.work_db, &product, "task_first", "chore", "cli", "medium");
    insert_task(&state.work_db, &product, "task_second", "chore", "cli", "medium");
    ready_execution(&state.work_db, "task_first", "1000");
    state.refresh_queued_dispatch();

    let in_progress = state.queued_dispatch_refresh.lock().unwrap();
    let racer = {
        let state = state.clone();
        std::thread::spawn(move || state.refresh_queued_dispatch())
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    ready_execution(&state.work_db, "task_second", "1001");
    drop(in_progress);

    assert!(racer.join().unwrap(), "the waiting refresh saw the later write");
    assert_eq!(
        state.published_queued_dispatch.lock().unwrap()["task_second"].position,
        Some(1)
    );
}

/// A refresh that runs while the drain holds a row `claimed` would omit it, so
/// it keeps the stored order instead; releasing the claim announces nothing, so
/// a retry armed behind the claim must bring the order up to date.
#[tokio::test]
async fn the_order_is_not_replaced_mid_claim_and_recovers_after_release() {
    let (state, _temp) = crate::app::tests::test_server_state();
    let product = create_test_product(&state.work_db).id;
    for id in ["task_first", "task_second", "task_third"] {
        insert_task(&state.work_db, &product, id, "chore", "cli", "medium");
    }
    let first = ready_execution(&state.work_db, "task_first", "1000");
    ready_execution(&state.work_db, "task_second", "1001");
    state.refresh_queued_dispatch();

    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE work_executions SET status = 'claimed' WHERE id = ?1", [&first])
        .unwrap();
    ready_execution(&state.work_db, "task_third", "1002");
    assert!(!state.refresh_queued_dispatch(), "mid-claim computation is discarded");
    {
        let published = state.published_queued_dispatch.lock().unwrap();
        assert_eq!(published["task_first"].position, Some(0), "claimed row keeps its place");
        assert!(!published.contains_key("task_third"));
    }
    // Arms the retry that follows the claim.
    state.republish_queued_dispatch_if_changed().await;

    state.work_db.release_dispatch_claim(&first).unwrap();

    for _ in 0..150 {
        if state
            .published_queued_dispatch
            .lock()
            .unwrap()
            .contains_key("task_third")
        {
            let published = state.published_queued_dispatch.lock().unwrap();
            assert_eq!(published["task_first"].position, Some(0));
            assert_eq!(published["task_third"].position, Some(2));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("order never caught up after the claim was released");
}
