use super::*;

use boss_engine_storage_values::dispatch_wait::*;
use std::collections::BTreeSet;

// Every waiting state must name why it is waiting. These tests enumerate the
// states the engine can leave a work item in while it is not running and
// assert each one classifies to a named reason; the one state that has no
// name is asserted to be reported as unclassified, never given a default.

fn db() -> WorkDb {
    WorkDb::open(temp_db_path("queued-wait")).unwrap()
}

fn pending(db: &WorkDb, work_item_id: &str, kind: ExecutionKind, status: ExecutionStatus) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(kind)
            .status(status)
            .build(),
    )
    .unwrap()
}

fn ready_chore(db: &WorkDb, product_id: &str, name: &str) -> (Task, WorkExecution) {
    let chore = create_test_chore(db, product_id, name);
    let execution = pending(
        db,
        &chore.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    (chore, execution)
}

/// A project whose design is done and whose two tasks are still to do.
fn project_with_two_tasks(db: &WorkDb, product_id: &str, autostart: bool) -> (Task, Task) {
    let project = db
        .create_project(CreateProjectInput {
            product_id: product_id.to_owned(),
            name: "Ordered project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let task = |name: &str| {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project.id.clone())
                .name(name)
                .autostart(autostart)
                .build(),
        )
        .unwrap()
    };
    let (first, second) = (task("First"), task("Second"));
    complete_design_for_project(db, &project.id);
    (first, second)
}

fn set_status(db: &WorkDb, id: &str, status: &str) {
    db.update_work_item(
        id,
        WorkItemPatch {
            status: Some(status.to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
}

fn execution_status(db: &WorkDb, work_item_id: &str) -> ExecutionStatus {
    db.list_executions(Some(work_item_id)).unwrap()[0].status.clone()
}

/// The classified wait for `work_item_id`, asserted to be the same answer on
/// the single-item read `bossctl dispatch diagnose` uses and on the board's
/// work-tree projection.
fn wait_of(db: &WorkDb, product_id: &str, work_item_id: &str) -> TaskRuntime {
    let runtime = db.get_task_runtime(work_item_id).unwrap();
    let tree = db.get_work_tree(product_id).unwrap();
    let on_board = tree
        .task_runtimes
        .iter()
        .find(|runtime| runtime.work_item_id == work_item_id)
        .unwrap_or_else(|| panic!("{work_item_id} is missing from the board"));
    assert_eq!(
        (&on_board.dispatch_wait_reason, &on_board.dispatch_wait_label),
        (&runtime.dispatch_wait_reason, &runtime.dispatch_wait_label),
        "the board and the single-item read disagree about why {work_item_id} is waiting"
    );
    runtime
}

/// Asserts `runtime` carries `code` with a phrase that is the registered one
/// (or, for a wait behind another item, one that names it), and returns the
/// code so the caller can record that the state is covered.
fn assert_named(state: &str, runtime: &TaskRuntime, code: &str) -> String {
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some(code), "{state}");
    let label = runtime
        .dispatch_wait_label
        .as_deref()
        .unwrap_or_else(|| panic!("{state}: a reason with no phrase"));
    assert!(!label.trim().is_empty(), "{state}");
    assert!(!label.to_lowercase().contains("unknown"), "{state}: {label}");
    assert_ne!(code, DISPATCH_WAIT_REASON_UNCLASSIFIED, "{state}");
    if runtime.dispatch_wait_blocker.is_none() {
        assert_eq!(label, dispatch_wait_phrase(code), "{state}");
    }
    code.to_owned()
}

#[test]
fn every_waiting_state_yields_a_named_reason() {
    let db = db();
    let product = create_test_product(&db);
    let pid = product.id.as_str();
    let mut covered = BTreeSet::new();

    // ── A `ready` execution ───────────────────────────────────────────────
    let (chore, execution) = ready_chore(&db, pid, "ready, not yet reached by a scheduler pass");
    covered.insert(assert_named(
        "ready with no hold",
        &wait_of(&db, pid, &chore.id),
        DISPATCH_WAIT_REASON_AWAITING_DISPATCH,
    ));

    // Every hold the scheduler records under a code.
    for hold in [
        DISPATCH_WAIT_REASON_POOL_EXHAUSTED,
        DISPATCH_WAIT_REASON_DISPATCH_PAUSED,
        DISPATCH_WAIT_REASON_AUTOMATION_PAUSED,
        DISPATCH_WAIT_REASON_FOCUS_MODE,
        DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE,
        DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION,
        DISPATCH_WAIT_REASON_PREFLIGHT_HOLD,
        DISPATCH_WAIT_REASON_CHAIN_SERIALIZED,
        DISPATCH_WAIT_REASON_CHAIN_SERIALIZED_REVIEW_HELD,
        DISPATCH_WAIT_REASON_INTERACTIVE_CAP,
    ] {
        db.set_dispatch_wait_reason(&execution.id, hold).unwrap();
        covered.insert(assert_named(hold, &wait_of(&db, pid, &chore.id), hold));
    }

    // Holds the scheduler records as a sentence are shown as written.
    for sentence in [
        interactive_cap_wait_reason(3, 0),
        interactive_cap_wait_reason(2, 9),
        WAITING_FOR_WORKSPACE_REASON.to_owned(),
        "blocked by T41 'Earlier review' on repo#42 (revisions on the same PR run one at a time)".to_owned(),
    ] {
        db.set_dispatch_wait_reason(&execution.id, &sentence).unwrap();
        let runtime = wait_of(&db, pid, &chore.id);
        assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some(sentence.as_str()));
        assert_eq!(runtime.dispatch_wait_label.as_deref(), Some(sentence.as_str()));
    }

    // A chain hold names the live writer it is behind.
    let writer = create_test_chore(&db, pid, "live writer on the same PR");
    pending(
        &db,
        &writer.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Running,
    );
    db.set_dispatch_wait_with_blocker(&execution.id, "blocked by an earlier review", Some(&writer.id))
        .unwrap();
    let held = wait_of(&db, pid, &chore.id);
    assert_eq!(
        held.dispatch_wait_label.as_deref(),
        Some("blocked by an earlier review")
    );
    assert_eq!(held.dispatch_wait_blocker.unwrap().work_item_id, writer.id);

    // A dispatch deadline still ahead.
    let future = (boss_engine_utils::epoch_time::now_epoch_secs() + 3600).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = ?2 WHERE id = ?1",
            params![execution.id, future],
        )
        .unwrap();
    let deadline = wait_of(&db, pid, &chore.id);
    assert_eq!(deadline.dispatch_not_before.as_deref(), Some(future.as_str()));
    covered.insert(assert_named(
        "future deadline",
        &deadline,
        DISPATCH_WAIT_REASON_NOT_BEFORE,
    ));

    // ── A `waiting_dependency` execution ──────────────────────────────────
    let prerequisite = create_test_chore(&db, pid, "prerequisite");
    let dependent = create_test_chore(&db, pid, "gated by a prerequisite");
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prerequisite.id.clone(),
        relation: None,
    })
    .unwrap();
    db.reconcile_product_executions(pid).unwrap();
    assert_eq!(execution_status(&db, &dependent.id), ExecutionStatus::WaitingDependency);
    let gated = wait_of(&db, pid, &dependent.id);
    covered.insert(assert_named(
        "unmet prerequisite",
        &gated,
        DISPATCH_WAIT_REASON_WAITING_DEPENDENCY,
    ));
    assert_eq!(
        gated.dispatch_wait_label.as_deref(),
        Some(format!("waiting for T{}", prerequisite.short_id.unwrap()).as_str())
    );
    assert_eq!(gated.dispatch_wait_blocker.unwrap().work_item_id, prerequisite.id);

    let (first, second) = project_with_two_tasks(&db, pid, true);
    db.reconcile_product_executions(pid).unwrap();
    assert_eq!(execution_status(&db, &second.id), ExecutionStatus::WaitingDependency);
    let ordered = wait_of(&db, pid, &second.id);
    covered.insert(assert_named(
        "later task of a project",
        &ordered,
        DISPATCH_WAIT_REASON_PROJECT_ORDER,
    ));
    assert_eq!(
        ordered.dispatch_wait_label.as_deref(),
        Some(
            format!(
                "waiting for T{}, an earlier task in its project",
                first.short_id.unwrap()
            )
            .as_str()
        )
    );
    assert_eq!(ordered.dispatch_wait_blocker.unwrap().work_item_id, first.id);

    let manual = create_test_chore_manual(&db, pid, "automatic start off, execution left behind");
    pending(
        &db,
        &manual.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::WaitingDependency,
    );
    covered.insert(assert_named(
        "automatic start off",
        &db.get_task_runtime(&manual.id).unwrap(),
        DISPATCH_WAIT_REASON_AUTOSTART_OFF,
    ));

    // ── Queued on the board (`todo`, automatic start on) with no execution ─
    let chore_with = |name: &str, human_driven: bool, deferred: bool| {
        db.create_chore(
            CreateChoreInput::builder()
                .product_id(pid)
                .name(name)
                .human_driven(human_driven)
                .deferred(deferred)
                .build(),
        )
        .unwrap()
    };
    // Marking a row human-driven switches automatic start off; switching it
    // back on queues the card without making an agent eligible for it.
    let human = chore_with("a human does this one", true, false);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&human.id])
        .unwrap();
    covered.insert(assert_named(
        "human-driven",
        &wait_of(&db, pid, &human.id),
        DISPATCH_WAIT_REASON_HUMAN_DRIVEN,
    ));

    let deferred = chore_with("future scope", false, true);
    covered.insert(assert_named(
        "deferred",
        &wait_of(&db, pid, &deferred.id),
        DISPATCH_WAIT_REASON_DEFERRED,
    ));

    let parked = create_test_chore(&db, pid, "parked by its owner");
    db.park_work_item_as_actor(&parked.id, true, Some("not now"), "human")
        .unwrap();
    covered.insert(assert_named(
        "parked",
        &db.get_task_runtime(&parked.id).unwrap(),
        DISPATCH_WAIT_REASON_PARKED,
    ));

    let finished = create_test_chore(&db, pid, "its only run was cancelled before it started");
    pending(
        &db,
        &finished.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Cancelled,
    );
    covered.insert(assert_named(
        "last run ended",
        &wait_of(&db, pid, &finished.id),
        DISPATCH_WAIT_REASON_RUN_ENDED,
    ));

    let abandoned_started = create_test_chore(&db, pid, "abandoned after a worker started");
    let abandoned_run = pending(
        &db,
        &abandoned_started.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Abandoned,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = '1' WHERE id = ?1",
            [&abandoned_run.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "abandoned after it started",
        &wait_of(&db, pid, &abandoned_started.id),
        DISPATCH_WAIT_REASON_RUN_ENDED,
    ));

    let delivered = create_test_chore(&db, pid, "pull request already open");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/test/repo/pull/7' WHERE id = ?1",
            [&delivered.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "pull request open",
        &wait_of(&db, pid, &delivered.id),
        DISPATCH_WAIT_REASON_PR_OPEN,
    ));

    // Creation refuses a card with no repository, so the state only arises
    // when the product's default is removed afterwards.
    let repoless = create_test_product_named(&db, "Repository removed");
    let unresolved = create_test_chore(&db, repoless.id.clone(), "nowhere to run");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE products SET repo_remote_url = NULL WHERE id = ?1",
            [&repoless.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "no repository",
        &wait_of(&db, &repoless.id, &unresolved.id),
        DISPATCH_WAIT_REASON_REPO_UNRESOLVED,
    ));

    let root = make_in_review_chore(&db, pid, "https://github.com/test/repo/pull/42");
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root), &checker).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute("DELETE FROM work_executions WHERE work_item_id = ?1", [&revision.id])
            .unwrap();
        conn.execute("UPDATE tasks SET pr_url = NULL WHERE id = ?1", [&root])
            .unwrap();
    }
    assert_eq!((revision.status.clone(), revision.autostart), (TaskStatus::Todo, true));
    covered.insert(assert_named(
        "revision whose parent has no pull request",
        &db.get_task_runtime(&revision.id).unwrap(),
        DISPATCH_WAIT_REASON_AWAITING_PARENT_PR,
    ));

    // ── An `active` item with no run pending or live ──────────────────────
    let idle = |name: &str| {
        let chore = create_test_chore(&db, pid, name);
        set_status(&db, &chore.id, "active");
        chore
    };
    let finished = idle("active, its last run ended");
    let run = pending(
        &db,
        &finished.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    set_execution_status(&db, &run.id, "completed");
    covered.insert(assert_named(
        "active, worker ended, waiting for the orphan sweep",
        &wait_of(&db, pid, &finished.id),
        DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
    ));
    let never_run = idle("active, never ran");
    assert_named(
        "active with no execution",
        &wait_of(&db, pid, &never_run.id),
        DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
    );

    let failed = idle("active, held by a dispatch failure");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_reason = 'churn_guard' WHERE id = ?1",
            [&failed.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "active, held by the churn guard",
        &wait_of(&db, pid, &failed.id),
        DISPATCH_WAIT_REASON_DISPATCH_FAILED,
    ));

    let recovering = idle("active, worker recovery gave up");
    insert_recovery_attention(&db, &recovering.id);
    covered.insert(assert_named(
        "active, recovery attention item open",
        &wait_of(&db, pid, &recovering.id),
        DISPATCH_WAIT_REASON_RECOVERY_ATTENTION,
    ));

    let delivered = idle("active, pull request delivered");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/test/repo/pull/7' WHERE id = ?1",
            [&delivered.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "active, pull request with review handling",
        &wait_of(&db, pid, &delivered.id),
        DISPATCH_WAIT_REASON_AWAITING_REVIEW,
    ));

    for (status, reason) in [
        (ExecutionStatus::WaitingReview, DISPATCH_WAIT_REASON_REVIEW_RESERVATION),
        (ExecutionStatus::WaitingMerge, DISPATCH_WAIT_REASON_AWAITING_MERGE),
    ] {
        let item = idle(reason);
        if status == ExecutionStatus::WaitingReview {
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET pr_url = 'https://github.com/test/repo/pull/7', updated_at = '1' WHERE id = ?1",
                    [&item.id],
                )
                .unwrap();
        }
        pending(&db, &item.id, ExecutionKind::ChoreImplementation, status);
        covered.insert(assert_named(reason, &wait_of(&db, pid, &item.id), reason));
    }
    // Only the merge is left, but the PR conflicts and the conflict is being
    // held until a merge is requested: the card must say so, not "waiting for
    // its pull request to merge".
    let conflicted = idle("merge wait, conflict deferred");
    pending(
        &db,
        &conflicted.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::WaitingMerge,
    );
    db.record_conflict_deferral(
        ConflictResolutionInsertInput::builder()
            .product_id(pid)
            .work_item_id(conflicted.id.clone())
            .pr_url("https://github.com/test/repo/pull/9")
            .pr_number(9)
            .head_branch("feature")
            .base_branch("main")
            .head_sha_before("head-9")
            .build(),
    )
    .unwrap();
    covered.insert(assert_named(
        "merge wait with a deferred conflict",
        &wait_of(&db, pid, &conflicted.id),
        DISPATCH_WAIT_REASON_CONFLICT_DEFERRED,
    ));
    let batch = batch_held_item(&db, pid);
    set_status(&db, &batch.id, "in_review");
    covered.insert(assert_named(
        "review batch",
        &wait_of(&db, pid, &batch.id),
        DISPATCH_WAIT_REASON_REVIEW_BATCH,
    ));
    let parked = idle("worker deliberately stopped");
    let run = pending(
        &db,
        &parked.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Abandoned,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'blocked' WHERE id = ?1",
            [&run.id],
        )
        .unwrap();
    covered.insert(assert_named(
        "deliberate park",
        &wait_of(&db, pid, &parked.id),
        DISPATCH_WAIT_REASON_DELIBERATE_PARK,
    ));

    let (refused, attempt) = ready_chore(&db, pid, "workspace positioning refused");
    db.record_workspace_positioning_refusal(
        &attempt.id,
        "worker",
        None,
        "Cube command failed: {\n  \"error\": \"Preserve commit abc123 before positioning def456; inspect the repository\",\n  \"error_kind\": \"workspace_positioning_refused\"\n}",
        "Preserve commit abc123 before positioning def456; inspect the repository",
    )
    .unwrap();
    let wait = wait_of(&db, pid, &refused.id);
    assert_eq!(
        wait.dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED),
    );
    assert_eq!(
        wait.dispatch_wait_label.as_deref(),
        Some("Workspace blocked: Preserve commit abc123 before positioning def456; inspect the repository")
    );
    covered.insert(DISPATCH_WAIT_REASON_WORKSPACE_POSITIONING_REFUSED.to_owned());

    // Every reason in the vocabulary is reachable from a state above, so a
    // reason cannot be added without a state that produces it.
    covered.extend(review_states_use_admission_and_current_head_evidence_on_every_surface());
    let vocabulary: BTreeSet<String> = DISPATCH_WAIT_REASON_CODES
        .iter()
        .map(|(code, _)| (*code).to_owned())
        .filter(|code| code != DISPATCH_WAIT_REASON_UNCLASSIFIED)
        .collect();
    assert_eq!(covered, vocabulary);
}

/// A pending execution abandoned by deferral never started, so the queued
/// card is not "its last run ended". Until reconcile mints a replacement it
/// is awaiting dispatch, unless another hold still applies.
#[test]
fn a_never_started_abandoned_queued_card_is_not_run_ended() {
    let db = db();
    let product = create_test_product(&db);
    let pid = product.id.as_str();
    let chore = create_test_chore(&db, pid, "deferred then approved");

    db.reconcile_product_executions(pid).unwrap();
    assert_eq!(
        wait_of(&db, pid, &chore.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH)
    );

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        wait_of(&db, pid, &chore.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_DEFERRED)
    );

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let remintable = wait_of(&db, pid, &chore.id);
    assert_eq!(
        remintable.dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH),
        "an abandoned never-started row is about to be reminted, not run_ended"
    );
    assert_eq!(
        remintable.dispatch_wait_label.as_deref(),
        Some(dispatch_wait_phrase(DISPATCH_WAIT_REASON_AWAITING_DISPATCH).as_str()),
    );

    db.park_work_item_as_actor(&chore.id, true, Some("not now"), "human")
        .unwrap();
    assert_eq!(
        db.get_task_runtime(&chore.id).unwrap().dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_PARKED),
        "a real hold still wins over awaiting dispatch"
    );

    db.park_work_item_as_actor(&chore.id, false, None, "human").unwrap();
    assert_eq!(
        wait_of(&db, pid, &chore.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH),
        "clearing the hold restores awaiting dispatch until reconcile mints"
    );

    db.reconcile_product_executions(pid).unwrap();
    assert_eq!(
        wait_of(&db, pid, &chore.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH)
    );
}

#[test]
fn a_wait_nothing_explains_is_unclassified_and_never_given_a_default() {
    let db = db();
    let product = create_test_product(&db);
    let pid = product.id.as_str();
    let unclassified = |state: &str, runtime: TaskRuntime| {
        assert_eq!(
            runtime.dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_UNCLASSIFIED),
            "{state}"
        );
        let label = runtime.dispatch_wait_label.unwrap();
        assert!(
            label.starts_with(&dispatch_wait_phrase(DISPATCH_WAIT_REASON_UNCLASSIFIED)),
            "{state}: {label}"
        );
        assert!(runtime.dispatch_wait_blocker.is_none(), "{state}");
    };

    // An active item whose execution sits off the scheduler's walk with no
    // prerequisite and no project order behind it. A hold recorded while the
    // row was `ready` says nothing about it now and is not reused.
    let stranded = create_test_chore(&db, pid, "active, parked off the ready walk");
    set_status(&db, &stranded.id, "active");
    let execution = pending(
        &db,
        &stranded.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    db.set_dispatch_wait_reason(&execution.id, DISPATCH_WAIT_REASON_POOL_EXHAUSTED)
        .unwrap();
    db.downgrade_ready_to_waiting_dependency(&execution.id).unwrap();
    unclassified(
        "active with an ungated waiting_dependency row",
        wait_of(&db, pid, &stranded.id),
    );

    // Nothing in the engine walks a `queued` row.
    let queued = create_test_chore(&db, pid, "legacy queued status");
    pending(
        &db,
        &queued.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Queued,
    );
    unclassified("queued with no hold", wait_of(&db, pid, &queued.id));

    // The board queues this card, yet no execution exists and nothing the
    // reconciler checks would have withheld one.
    let unminted = create_test_chore(&db, pid, "queued on the board, nothing minted");
    unclassified("no execution and no refusal", wait_of(&db, pid, &unminted.id));

    // A hold recorded under a code nobody gave a phrase is not shown as an
    // identifier.
    let (coded, coded_execution) = ready_chore(&db, pid, "held under an unnamed code");
    db.set_dispatch_wait_reason(&coded_execution.id, "some_new_hold")
        .unwrap();
    let runtime = wait_of(&db, pid, &coded.id);
    assert!(
        runtime
            .dispatch_wait_label
            .as_deref()
            .unwrap()
            .contains("some_new_hold")
    );
    unclassified("unnamed hold code", runtime);

    // The heartbeat is handed exactly these, plus the one `ready` row no pass
    // has explained yet.
    let (fresh, _) = ready_chore(&db, pid, "ready and about to be picked up");
    let reported: BTreeSet<(String, String)> = db
        .list_unexplained_dispatch_waits()
        .unwrap()
        .into_iter()
        .map(|wait| (wait.work_item_id, wait.reason))
        .collect();
    let expected: BTreeSet<(String, String)> = [
        (stranded.id.clone(), DISPATCH_WAIT_REASON_UNCLASSIFIED),
        (queued.id.clone(), DISPATCH_WAIT_REASON_UNCLASSIFIED),
        (unminted.id.clone(), DISPATCH_WAIT_REASON_UNCLASSIFIED),
        (coded.id.clone(), DISPATCH_WAIT_REASON_UNCLASSIFIED),
        (fresh.id.clone(), DISPATCH_WAIT_REASON_AWAITING_DISPATCH),
    ]
    .into_iter()
    .map(|(id, reason)| (id, reason.to_owned()))
    .collect();
    assert_eq!(reported, expected);
}

#[test]
fn a_running_claimed_or_finished_item_carries_no_wait() {
    let db = db();
    let product = create_test_product(&db);
    let (chore, execution) = ready_chore(&db, &product.id, "moves through its lifecycle");
    set_status(&db, &chore.id, "active");
    db.set_dispatch_wait_reason(&execution.id, DISPATCH_WAIT_REASON_POOL_EXHAUSTED)
        .unwrap();
    for status in ["claimed", "running", "waiting_human", "completed"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![execution.id, status],
            )
            .unwrap();
        let runtime = wait_of(&db, &product.id, &chore.id);
        if status == "completed" {
            // An active item whose run ended waits for the orphan sweep.
            assert_eq!(
                runtime.dispatch_wait_reason.as_deref(),
                Some(DISPATCH_WAIT_REASON_AWAITING_REDISPATCH)
            );
            continue;
        }
        assert!(
            runtime.dispatch_wait_reason.is_none() && runtime.dispatch_wait_label.is_none(),
            "{status} is not a wait on dispatch: {runtime:?}"
        );
    }
    assert!(db.list_unexplained_dispatch_waits().unwrap().is_empty());
}

fn set_execution_status(db: &WorkDb, execution_id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            params![execution_id, status],
        )
        .unwrap();
}

fn insert_recovery_attention(db: &WorkDb, work_item_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_attention_items (id, work_item_id, kind, status, title, body_markdown, created_at)
             VALUES (?1, ?2, ?3, 'open', 'recovery', 'recovery gave up', '1')",
            params![
                format!("attn_{work_item_id}"),
                work_item_id,
                ATTENTION_KIND_RECOVERY_EXHAUSTED
            ],
        )
        .unwrap();
}

/// A producer held by a live review batch at its current head.
fn batch_held_item(db: &WorkDb, product_id: &str) -> Task {
    let chore = create_test_chore(db, product_id, "active, held by a review batch");
    set_status(db, &chore.id, "active");
    let run = pending(
        db,
        &chore.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed', finished_at = '5', pr_head_after = 'head' WHERE id = ?1",
        [&run.id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pr_review_batches(id,cycle_root_id,base_sha,classification_json,created_at,phase,pr_number,pr_url,status,target_sha,updated_at)
         VALUES ('batch-1',?1,'base','{}','1','pre_merge',7,'https://github.com/test/repo/pull/7','collecting','head','1')",
        [&chore.id],
    )
    .unwrap();
    chore
}

#[test]
fn a_batch_held_active_item_is_named_and_not_reported() {
    let db = db();
    let product = create_test_product(&db);
    let chore = batch_held_item(&db, &product.id);
    assert_named(
        "batch hold",
        &wait_of(&db, &product.id, &chore.id),
        DISPATCH_WAIT_REASON_AWAITING_REVIEW,
    );
    assert!(db.list_unexplained_dispatch_waits().unwrap().is_empty());
}

#[test]
fn an_active_item_awaiting_the_orphan_sweep_is_named_and_not_reported() {
    let db = db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "active, its worker ended");
    set_status(&db, &chore.id, "active");
    assert_eq!(
        wait_of(&db, &product.id, &chore.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_REDISPATCH)
    );
    assert!(db.list_unexplained_dispatch_waits().unwrap().is_empty());
}

/// The stuck report carries the board's reason for every kind of item the
/// board explains, with no execution pending as well as with one.
#[test]
fn the_stuck_report_names_the_boards_reason_for_items_with_nothing_pending() {
    let db = db();
    let product = create_test_product(&db);
    let pid = product.id.as_str();

    let deferred = create_test_chore(&db, pid, "deferred");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET deferred = 1 WHERE id = ?1", [&deferred.id])
        .unwrap();

    let ended = create_test_chore(&db, pid, "todo, its run ended");
    let run = pending(
        &db,
        &ended.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    set_execution_status(&db, &run.id, "cancelled");

    let repoless = create_test_product_named(&db, "Repository removed");
    let unresolved = create_test_chore(&db, repoless.id.clone(), "no repository");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE products SET repo_remote_url = NULL WHERE id = ?1",
            [&repoless.id],
        )
        .unwrap();

    let batch_held = batch_held_item(&db, pid);
    let redispatch = create_test_chore(&db, pid, "active, worker ended");
    set_status(&db, &redispatch.id, "active");

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let report = db.stuck_report(0, now, &|_| false).unwrap();
    for (item, product_id, expected) in [
        (&deferred, pid, DISPATCH_WAIT_REASON_DEFERRED),
        (&ended, pid, DISPATCH_WAIT_REASON_RUN_ENDED),
        (&unresolved, repoless.id.as_str(), DISPATCH_WAIT_REASON_REPO_UNRESOLVED),
        (&batch_held, pid, DISPATCH_WAIT_REASON_AWAITING_REVIEW),
        (&redispatch, pid, DISPATCH_WAIT_REASON_AWAITING_REDISPATCH),
    ] {
        let on_board = wait_of(&db, product_id, &item.id);
        assert_eq!(
            on_board.dispatch_wait_reason.as_deref(),
            Some(expected),
            "{}",
            item.name
        );
        let row = report
            .rows
            .iter()
            .find(|row| row.work_item_id == item.id)
            .unwrap_or_else(|| panic!("{} is missing from the stuck report", item.name));
        assert_eq!(row.wait_reason, on_board.dispatch_wait_reason, "{}", item.name);
        assert_eq!(row.wait_label, on_board.dispatch_wait_label, "{}", item.name);
    }
}

#[test]
fn the_stuck_report_names_the_same_reason_as_the_board() {
    let db = db();
    let product = create_test_product(&db);
    let (_, second) = project_with_two_tasks(&db, &product.id, true);
    db.reconcile_product_executions(&product.id).unwrap();
    let on_board = wait_of(&db, &product.id, &second.id);
    assert_eq!(
        on_board.dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_PROJECT_ORDER)
    );

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let report = db.stuck_report(0, now, &|_| false).unwrap();
    let row = report
        .rows
        .iter()
        .find(|row| row.work_item_id == second.id)
        .expect("a task held off the ready walk is in the stuck report");
    assert_eq!(
        row.classification,
        format!("S20_dispatch_wait:{DISPATCH_WAIT_REASON_PROJECT_ORDER}")
    );
}

// ── The transition-dispatch gap ──────────────────────────────────────────
//
// Moving a later task of a project to Doing mints a `ready` execution and
// reports it dispatched. The reconcile in the same request leaves that row
// `ready`: an item that is already `active` has been started and is not the
// project chain's to hold, so no row lands in `waiting_dependency`, which no
// scheduler pass reads.

/// What moving a card to Doing does in the engine, in the order the
/// `UpdateWorkItem` handler does it: flip the status, mint the execution,
/// then reconcile the product.
fn move_to_doing(db: &WorkDb, product_id: &str, task_id: &str) -> WorkExecution {
    set_status(db, task_id, "active");
    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(task_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    db.reconcile_product_executions(product_id).unwrap();
    execution
}

#[test]
fn a_task_moved_to_doing_stays_queued_for_a_worker_through_the_reconcile() {
    let db = db();
    let product = create_test_product(&db);
    // Planner-style tasks: nothing starts until a person moves the card.
    let (first, second) = project_with_two_tasks(&db, &product.id, false);

    let first_execution = move_to_doing(&db, &product.id, &first.id);
    assert_eq!(first_execution.status, ExecutionStatus::Ready);
    let second_execution = move_to_doing(&db, &product.id, &second.id);
    assert_eq!(second_execution.status, ExecutionStatus::Ready);

    // No sweep has run. The request for a worker is still in the queue the
    // scheduler drains, for both tasks.
    let ready: Vec<String> = db
        .list_ready_executions()
        .unwrap()
        .into_iter()
        .map(|execution| execution.id)
        .collect();
    assert!(ready.contains(&first_execution.id), "{ready:?}");
    assert!(
        ready.contains(&second_execution.id),
        "the later task's execution left the ready queue after the transition reported it \
         dispatched; ready={ready:?}, status={:?}",
        db.get_execution(&second_execution.id).unwrap().status
    );
    assert_eq!(
        db.get_task_runtime(&second.id).unwrap().dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH)
    );
    // Nor is there anything for the orphan sweep to find later: backdate both
    // cards past its age floor and it still has no candidate.
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET updated_at = '1' WHERE status = 'active'", [])
        .unwrap();
    assert!(db.list_orphan_active_candidates(90).unwrap().is_empty());

    // Later reconciles (any card edit in the product runs one) leave it alone.
    db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(
        db.get_execution(&second_execution.id).unwrap().status,
        ExecutionStatus::Ready
    );
}

#[test]
fn moving_a_chain_held_task_to_doing_releases_its_execution() {
    let db = db();
    let product = create_test_product(&db);
    let (first, second) = project_with_two_tasks(&db, &product.id, true);
    db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(execution_status(&db, &first.id), ExecutionStatus::Ready);
    assert_eq!(execution_status(&db, &second.id), ExecutionStatus::WaitingDependency);

    // The held row already owns the dispatch slot, so the handler mints
    // nothing: the status flip and the reconcile are all that happen.
    set_status(&db, &second.id, "active");
    db.reconcile_product_executions(&product.id).unwrap();

    assert_eq!(execution_status(&db, &second.id), ExecutionStatus::Ready);
    assert_eq!(db.list_executions(Some(&second.id)).unwrap().len(), 1);
    assert_eq!(execution_status(&db, &first.id), ExecutionStatus::Ready);
}

#[test]
fn the_project_chain_still_holds_a_task_nobody_started() {
    let db = db();
    let product = create_test_product(&db);
    let (first, second) = project_with_two_tasks(&db, &product.id, true);
    db.reconcile_product_executions(&product.id).unwrap();

    // The first task starting does not release the second...
    set_status(&db, &first.id, "active");
    db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(execution_status(&db, &second.id), ExecutionStatus::WaitingDependency);
    assert_eq!(
        db.get_task_runtime(&second.id).unwrap().dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_PROJECT_ORDER)
    );

    // ...and a declared prerequisite still gates an active task: the chain
    // exemption is about ordinal order only.
    let third = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(first.project_id.clone().unwrap())
                .name("Third")
                .build(),
        )
        .unwrap();
    set_status(&db, &third.id, "active");
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES (?1, ?2, 'blocks', '1')",
            params![third.id, second.id],
        )
        .unwrap();
    db.reconcile_product_executions(&product.id).unwrap();
    assert_eq!(execution_status(&db, &third.id), ExecutionStatus::WaitingDependency);
    assert_eq!(
        db.get_task_runtime(&third.id).unwrap().dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_WAITING_DEPENDENCY)
    );
}

#[test]
fn idle_admission_holds_and_review_waits_agree_across_surfaces() {
    let db = db();
    let product = create_test_product(&db);
    let ordinary = create_test_chore(&db, &product.id, "paused");
    let critical = create_test_chore(&db, &product.id, "critical");
    for item in [&ordinary, &critical] {
        set_status(&db, &item.id, "active");
    }
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET priority = 'critical' WHERE id = ?1", [&critical.id])
        .unwrap();
    db.connect().unwrap().execute_batch(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES ('dispatch_paused','1'),('dispatch_paused_origin','operator');").unwrap();
    let parked = create_test_chore(&db, &product.id, "deliberate park");
    set_status(&db, &parked.id, "active");
    let run = pending(
        &db,
        &parked.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Abandoned,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'blocked' WHERE id = ?1",
            [&run.id],
        )
        .unwrap();
    let mut cases = vec![
        (ordinary, DISPATCH_WAIT_REASON_DISPATCH_PAUSED),
        (critical, DISPATCH_WAIT_REASON_AWAITING_REDISPATCH),
        (parked, DISPATCH_WAIT_REASON_DELIBERATE_PARK),
    ];
    for (status, reason) in [
        (ExecutionStatus::WaitingReview, DISPATCH_WAIT_REASON_UNCLASSIFIED),
        (ExecutionStatus::WaitingMerge, DISPATCH_WAIT_REASON_AWAITING_MERGE),
    ] {
        let item = create_test_chore(&db, &product.id, reason);
        set_status(&db, &item.id, "in_review");
        pending(&db, &item.id, ExecutionKind::ChoreImplementation, status);
        cases.push((item, reason));
    }
    let unexplained = create_test_chore(&db, &product.id, "review without a PR");
    set_status(&db, &unexplained.id, "in_review");
    cases.push((unexplained.clone(), DISPATCH_WAIT_REASON_UNCLASSIFIED));
    let batch = batch_held_item(&db, &product.id);
    set_status(&db, &batch.id, "in_review");
    cases.push((batch, DISPATCH_WAIT_REASON_REVIEW_BATCH));
    let report = db
        .stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs(), &|_| false)
        .unwrap();
    for (item, reason) in cases {
        let runtime = wait_of(&db, &product.id, &item.id);
        assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some(reason), "{}", item.name);
        let row = report.rows.iter().find(|row| row.work_item_id == item.id).unwrap();
        assert_eq!(row.wait_reason, runtime.dispatch_wait_reason);
        assert_eq!(row.wait_label, runtime.dispatch_wait_label);
    }
    assert!(
        db.list_unexplained_dispatch_waits()
            .unwrap()
            .iter()
            .any(|wait| wait.work_item_id == unexplained.id && wait.reason == DISPATCH_WAIT_REASON_UNCLASSIFIED)
    );
}

#[test]
fn an_idle_active_item_outside_the_sweeps_clock_is_an_anomaly() {
    let db = db();
    let product = create_test_product(&db);
    let item = create_test_chore(&db, &product.id, "invalid update clock");
    set_status(&db, &item.id, "active");
    let run = pending(&db, &item.id, ExecutionKind::PrReview, ExecutionStatus::Completed);
    db.connect().unwrap().execute(
        "INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,head_sha,findings_count,revision_warranted,gate_outcome,created_at)
         VALUES ('productive',?1,?2,'head',0,0,'completed_clean','1')",
        params![run.id, item.id],
    ).unwrap();
    // No legitimate hold explains this row: its invalid timestamp excludes
    // it even from the candidate walk with the broadest possible cutoff.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET updated_at = '9223372036854775807' WHERE id = ?1",
            [&item.id],
        )
        .unwrap();
    assert_eq!(
        wait_of(&db, &product.id, &item.id).dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_UNCLASSIFIED)
    );
    assert!(
        db.list_unexplained_dispatch_waits()
            .unwrap()
            .iter()
            .any(|wait| wait.work_item_id == item.id && wait.reason == DISPATCH_WAIT_REASON_UNCLASSIFIED)
    );
}

fn review_states_use_admission_and_current_head_evidence_on_every_surface() -> BTreeSet<String> {
    let db = db();
    let product = create_test_product(&db);
    let mut cases = Vec::new();
    for (name, expected) in [
        ("clean", DISPATCH_WAIT_REASON_REVIEW_COMPLETE),
        ("revision", DISPATCH_WAIT_REASON_REVISION_PENDING),
        ("findings", DISPATCH_WAIT_REASON_HUMAN_REVIEW),
        ("changes", DISPATCH_WAIT_REASON_HUMAN_REVIEW),
        ("cycle", DISPATCH_WAIT_REASON_HUMAN_REVIEW),
        ("admission", DISPATCH_WAIT_REASON_REVIEW_RESERVATION),
        ("parked", DISPATCH_WAIT_REASON_PARKED),
        ("human", DISPATCH_WAIT_REASON_HUMAN_DRIVEN),
        ("fallback", DISPATCH_WAIT_REASON_AWAITING_MERGE),
        ("unexplained", DISPATCH_WAIT_REASON_UNCLASSIFIED),
    ] {
        let item = create_test_chore(&db, &product.id, name);
        set_status(&db, &item.id, "in_review");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = 'https://github.com/test/repo/pull/7', pr_head_sha = 'head'
             WHERE id = ?1",
                [&item.id],
            )
            .unwrap();
        match name {
            "unexplained" => {
                db.connect()
                    .unwrap()
                    .execute("UPDATE tasks SET pr_url = NULL WHERE id = ?1", [&item.id])
                    .unwrap();
            }
            "clean" | "revision" | "findings" => {
                let run = pending(&db, &item.id, ExecutionKind::PrReview, ExecutionStatus::Completed);
                let revision = if name == "revision" {
                    let child = create_test_chore(&db, &product.id, "findings revision");
                    db.connect()
                        .unwrap()
                        .execute(
                            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = 'active' WHERE id = ?1",
                            params![child.id, item.id],
                        )
                        .unwrap();
                    Some(child.id)
                } else {
                    None
                };
                db.connect().unwrap().execute(
                    "INSERT INTO pr_review_batches(id,cycle_root_id,base_sha,classification_json,created_at,phase,pr_number,pr_url,status,target_sha,updated_at)
                     VALUES (?1,?2,'base','{}','1','pre_merge',7,'https://github.com/test/repo/pull/7','completed','head','1')",
                    params![format!("batch-{name}"), item.id],
                ).unwrap();
                db.connect().unwrap().execute(
                    "INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,head_sha,findings_count,revision_warranted,gate_outcome,revision_task_id,created_at,batch_id)
                     VALUES (?1,?2,?3,'head',?4,?4,?5,?6,'1',?7)",
                    params![format!("verdict-{name}"), run.id, item.id, i64::from(name != "clean"),
                        if name != "clean" { "completed_with_findings" } else { "completed_clean" },
                        revision, format!("batch-{name}")],
                ).unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "INSERT INTO work_attention_items(id,work_item_id,kind,status,title,body_markdown,created_at)
                     VALUES (?1,?2,'pr_review_admission_deferred','open','admission','admission','1')",
                        params![format!("old-admission-{name}"), item.id],
                    )
                    .unwrap();
            }
            "changes" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET review_required_state = 'changes_requested' WHERE id = ?1",
                        [&item.id],
                    )
                    .unwrap();
            }
            "cycle" | "admission" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "INSERT INTO work_attention_items(id,work_item_id,kind,status,title,body_markdown,created_at)
                     VALUES (?1,?2,?3,'open','review','review','1')",
                        params![
                            format!("attention-{name}"),
                            item.id,
                            if name == "cycle" {
                                "pr_review_cycle_bound"
                            } else {
                                "pr_review_admission_deferred"
                            }
                        ],
                    )
                    .unwrap();
            }
            "parked" => {
                db.connect()
                    .unwrap()
                    .execute("UPDATE tasks SET parked_at = '1' WHERE id = ?1", [&item.id])
                    .unwrap();
            }
            "human" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET human_driven = 1, pr_url = NULL WHERE id = ?1",
                        [&item.id],
                    )
                    .unwrap();
            }
            _ => {}
        }
        cases.push((item.id, expected));
    }
    let report = db
        .stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs(), &|_| false)
        .unwrap();
    let anomalies = db.list_unexplained_dispatch_waits().unwrap();
    let mut covered = BTreeSet::new();
    for (id, expected) in cases {
        if expected != DISPATCH_WAIT_REASON_UNCLASSIFIED {
            covered.insert(expected.to_owned());
        }
        let runtime = wait_of(&db, &product.id, &id);
        assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some(expected), "{id}");
        let row = report.rows.iter().find(|row| row.work_item_id == id).unwrap();
        assert_eq!(row.wait_reason, runtime.dispatch_wait_reason);
        assert_eq!(row.wait_label, runtime.dispatch_wait_label);
        assert_eq!(
            anomalies.iter().any(|wait| wait.work_item_id == id),
            expected == DISPATCH_WAIT_REASON_UNCLASSIFIED,
            "{id}"
        );
    }
    covered
}

#[test]
fn excluded_review_kinds_wait_for_merge_without_false_attention() {
    let db = db();
    let product = create_test_product(&db);
    for kind in ["design", "investigation", "design_postmortem"] {
        let item = create_test_chore(&db, &product.id, kind);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = ?2, status = 'in_review',
             pr_url = 'https://github.com/test/repo/pull/7', review_required_state = 'unknown' WHERE id = ?1",
                params![item.id, kind],
            )
            .unwrap();
        assert_named(
            kind,
            &wait_of(&db, &product.id, &item.id),
            DISPATCH_WAIT_REASON_AWAITING_MERGE,
        );
        assert!(
            !db.list_unexplained_dispatch_waits()
                .unwrap()
                .iter()
                .any(|wait| wait.work_item_id == item.id)
        );
        // An outstanding reviewer must prevent the merge-only fallback.
        pending(&db, &item.id, ExecutionKind::PrReview, ExecutionStatus::WaitingReview);
        assert_eq!(
            db.get_task_runtime(&item.id).unwrap().dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_REVIEW_BATCH)
        );
    }
}

#[test]
fn merge_only_wait_requires_the_whole_revision_family_to_be_idle() {
    let db = db();
    let product = create_test_product(&db);
    let root = make_in_review_chore(&db, &product.id, "https://github.com/test/repo/pull/8");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET kind = 'design' WHERE id = ?1", [&root])
        .unwrap();
    let child = create_test_chore(&db, &product.id, "pending revision");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = 'active' WHERE id = ?1",
            params![child.id, root],
        )
        .unwrap();
    for status in ["todo", "active", "blocked"] {
        set_status(&db, &child.id, status);
        assert_eq!(
            db.get_task_runtime(&root).unwrap().dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_REVISION_PENDING),
            "{status}"
        );
        assert!(
            !db.list_unexplained_dispatch_waits()
                .unwrap()
                .iter()
                .any(|wait| wait.work_item_id == root)
        );
    }
    set_status(&db, &child.id, "in_review");
    pending(&db, &child.id, ExecutionKind::PrReview, ExecutionStatus::Running);
    assert_eq!(
        db.get_task_runtime(&root).unwrap().dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_REVIEW_BATCH)
    );
}
