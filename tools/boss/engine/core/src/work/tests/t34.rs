use super::*;

// ── Explicit start of an engine-spawned revision whose attempt retired ───
//
// Contract: the dispatcher and `reconcile_revision_execution` agree on what
// "settled" means for a CI-fix / merge-conflict revision. The reconciler
// treats a retired `ci_remediations` / `conflict_resolutions` attempt as a
// spent fix vehicle: it abandons pending executions and halts re-dispatch.
// An explicit start (kanban drag-to-Doing, `bossctl work start`, an
// affirmative answer to the revision's question) re-opens the retired
// attempt (back to `pending`) and lifts the block the gate left behind, so
// the run proceeds. Automatic recovery never re-opens a retired attempt. A
// start that cannot be honoured (parent PR already merged/closed) is refused
// with a reason, and the settled card carries a `blocked_detail` naming the
// resume path.

fn blocked_fields(db: &WorkDb, task_id: &str) -> (Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn start_run(db: &WorkDb, execution_id: &str, worker: &str) {
    db.start_execution_run(
        execution_id,
        worker,
        "mono",
        &format!("lease-{worker}"),
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
}

fn fixture_timeout_question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Raise the fixture timeout from 30s to 60s?".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
        explanation: "The failing check is a fixture timeout, not a product regression.".into(),
    }
}

/// The incident shape: a CI-fix revision's worker finds its attempt already
/// terminal, declares blocked with a Yes/No question, and the owner answers
/// Yes. The resume must actually run — the attempt is re-opened, the minted
/// execution survives the next reconcile tick, and a worker can pick it up.
#[test]
fn ci_fix_revision_question_answered_yes_resumes_after_attempt_retired() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::t34::ci_fix_revision_question_answered_yes_resumes_after_attempt_retired",
    );
    let db = WorkDb::open(temp_db_path("ci-fix-question-yes-resume")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-q-yes");
    let pr_url = "https://github.com/spinyfin/mono/pull/3142";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3142);

    // Normal dispatch while the attempt is live.
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id);
    assert_eq!(first.len(), 1, "{first:?}");
    let first_exec = first[0].0.clone();
    start_run(&db, &first_exec, "worker-1");
    assert_eq!(task_status(&db, &revision_id), "active");

    // The attempt retires underneath the live worker — whose own
    // `engine ci mark-failed` then reports it already terminal.
    db.mark_ci_remediation_failed(&rem_id, "retired_underneath_worker")
        .unwrap()
        .expect("first retirement flips the row");
    assert!(
        db.mark_ci_remediation_failed(&rem_id, "worker_classification")
            .unwrap()
            .is_none(),
        "the worker's own mark-failed must observe an already-terminal attempt",
    );

    // The worker declares blocked with a question.
    let question = fixture_timeout_question();
    crate::work::pr_flow::insert_question(&db.connect().unwrap(), &first_exec, "prop_test", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&first_exec, "cannot complete without a decision", &question)
        .unwrap()
        .expect("live execution parks the revision");
    assert_eq!(task_status(&db, &revision_id), "blocked");
    assert_eq!(
        blocked_fields(&db, &revision_id).0.as_deref(),
        Some("awaiting_operator_answer")
    );

    // A reconcile tick while the question is open leaves the parked row alone.
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(executions_for(&db, &revision_id).len(), 1);
    assert_eq!(
        blocked_fields(&db, &revision_id).0.as_deref(),
        Some("awaiting_operator_answer"),
        "an open question must not be clobbered by the retired-attempt gate",
    );

    // The owner answers Yes.
    let outcome = db
        .answer_operator_question(&revision_id, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap();
    assert!(outcome.minted_execution, "Yes must mint the resume execution");
    let after_answer = executions_for(&db, &revision_id);
    assert_eq!(after_answer.len(), 2, "{after_answer:?}");
    let (resume_exec, resume_status) = after_answer[1].clone();
    assert_eq!(resume_status, "ready");
    let attempt = db.get_ci_remediation(&rem_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "pending",
        "answering Yes must re-open the retired attempt so the reconciler agrees the run may proceed",
    );
    assert_eq!(attempt.failure_reason, None);
    assert_eq!(attempt.finished_at, None);

    // The reconcile tick that used to abandon the resume in the same instant.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_reconcile = executions_for(&db, &revision_id);
    assert_eq!(
        after_reconcile.get(1),
        Some(&(resume_exec.clone(), "ready".to_owned())),
        "the resume execution must survive reconcile: {after_reconcile:?}",
    );
    assert_eq!(after_reconcile.len(), 2, "{after_reconcile:?}");
    assert_eq!(task_status(&db, &revision_id), "todo");
    assert_eq!(blocked_fields(&db, &revision_id), (None, None));

    // A worker picks the resume up and the row stays in Doing through the
    // next tick instead of bouncing.
    start_run(&db, &resume_exec, "worker-2");
    assert_eq!(task_status(&db, &revision_id), "active");
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(executions_for(&db, &revision_id).len(), 2);
    assert_eq!(task_status(&db, &revision_id), "active");
    assert_eq!(blocked_fields(&db, &revision_id), (None, None));
}

/// `bossctl work start` shape: the row is parked `blocked` by the gate (the
/// CLI does not flip kanban status first). The settled card must say why and
/// how to resume; the explicit start must re-open the attempt, lift the
/// block, and produce a run that the next reconcile does not abandon.
#[test]
fn manual_start_of_ci_fix_revision_with_retired_attempt_reopens_it() {
    let db = WorkDb::open(temp_db_path("ci-fix-manual-start-retired")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-manual");
    let pr_url = "https://github.com/spinyfin/mono/pull/3143";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3143);

    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(executions_for(&db, &revision_id).len(), 1);
    db.mark_ci_remediation_failed(&rem_id, "ran out of attempts").unwrap();

    // The gate settles the row — visibly.
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(task_status(&db, &revision_id), "blocked");
    let (reason, detail) = blocked_fields(&db, &revision_id);
    assert_eq!(reason.as_deref(), Some("spawning_attempt_retired_unsuccessfully"));
    let detail = detail.expect("the settled card must explain itself");
    assert!(
        detail.contains("CI-fix attempt") && detail.contains("failed: ran out of attempts"),
        "detail must name what ended: {detail}",
    );
    assert!(
        detail.contains("drag the card to Doing") && detail.contains("bossctl work start"),
        "detail must name the resume path: {detail}",
    );
    let settled = executions_for(&db, &revision_id);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].1, "abandoned");

    // Explicit start on the blocked row.
    let exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    assert_eq!(exec.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(exec.pr_url.as_deref(), Some(pr_url));
    let attempt = db.get_ci_remediation(&rem_id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending", "explicit start must re-open the attempt");
    assert_eq!(attempt.failure_reason, None);
    assert_eq!(attempt.finished_at, None);
    assert_eq!(
        task_status(&db, &revision_id),
        "todo",
        "the retired-attempt block is lifted"
    );
    assert_eq!(blocked_fields(&db, &revision_id), (None, None));

    // The scheduler picks it up; the next tick must not abandon or re-block.
    start_run(&db, &exec.id, "worker-1");
    assert_eq!(task_status(&db, &revision_id), "active");
    db.reconcile_product_executions(&product_id).unwrap();
    let after = executions_for(&db, &revision_id);
    assert_eq!(after.len(), 2, "no duplicate execution: {after:?}");
    assert_eq!(after[1].1, "running", "{after:?}");
    assert_eq!(task_status(&db, &revision_id), "active");
    assert_eq!(blocked_fields(&db, &revision_id), (None, None));
}

/// Kanban drag-to-Doing shape, including the production race: the app sends
/// UpdateWorkItem(status=active) first, the engine's invalidation tick
/// reconciles (the gate re-blocks the row because the attempt is still
/// terminal), and only then does RequestExecution arrive. The start must
/// still win: attempt re-opened, row unblocked, execution survives.
#[test]
fn kanban_drag_of_ci_fix_revision_with_retired_attempt_wins_the_reconcile_race() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::t34::kanban_drag_of_ci_fix_revision_with_retired_attempt_wins_the_reconcile_race",
    );
    let db = WorkDb::open(temp_db_path("ci-fix-drag-retired")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-drag");
    let pr_url = "https://github.com/spinyfin/mono/pull/3144";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3144);

    db.reconcile_product_executions(&product_id).unwrap();
    db.mark_ci_remediation_failed(&rem_id, "ran out of attempts").unwrap();
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(task_status(&db, &revision_id), "blocked");

    // Drag: status flip, then the invalidation tick, then RequestExecution.
    db.update_work_item(
        &revision_id,
        WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(
        task_status(&db, &revision_id),
        "blocked",
        "precondition: the tick between the two RPCs re-blocks the row (attempt still terminal)",
    );
    let exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    assert_eq!(db.get_ci_remediation(&rem_id).unwrap().unwrap().status, "pending");
    assert_eq!(task_status(&db, &revision_id), "todo");

    db.reconcile_product_executions(&product_id).unwrap();
    let after = executions_for(&db, &revision_id);
    assert!(
        after.iter().any(|(id, status)| id == &exec.id && status == "ready"),
        "the dragged start must survive the next tick: {after:?}",
    );
    assert_ne!(task_status(&db, &revision_id), "blocked");
}

/// Sibling for the merge-conflict arm: the re-open contract is keyed off the
/// `created_via` prefix, so a `conflict_resolutions` attempt gets the same
/// treatment as a `ci_remediations` one.
#[test]
fn manual_start_of_merge_conflict_revision_with_retired_attempt_reopens_it() {
    let db = WorkDb::open(temp_db_path("crz-manual-start-retired")).unwrap();
    let product_id = make_revision_product(&db, "crz-manual");
    let pr_url = "https://github.com/spinyfin/mono/pull/3145";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 3145,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    db.reconcile_product_executions(&product_id).unwrap();
    db.mark_conflict_resolution_failed(&crz.id, "resolver gave up").unwrap();
    db.reconcile_product_executions(&product_id).unwrap();
    assert_eq!(task_status(&db, &revision_id), "blocked");
    let (reason, detail) = blocked_fields(&db, &revision_id);
    assert_eq!(reason.as_deref(), Some("spawning_attempt_retired_unsuccessfully"));
    let detail = detail.unwrap();
    assert!(
        detail.contains("merge-conflict attempt") && detail.contains("failed: resolver gave up"),
        "{detail}"
    );

    let exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    let attempt = db.get_conflict_resolution(&crz.id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.failure_reason, None);
    assert_eq!(attempt.finished_at, None);
    assert_eq!(task_status(&db, &revision_id), "todo");

    start_run(&db, &exec.id, "worker-1");
    db.reconcile_product_executions(&product_id).unwrap();
    let after = executions_for(&db, &revision_id);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[1].1, "running", "{after:?}");
    assert_eq!(task_status(&db, &revision_id), "active");
}

/// Guard: an explicit start while the attempt is still live is unchanged —
/// nothing to re-open, attempt untouched, one execution.
#[test]
fn manual_start_of_ci_fix_revision_with_active_attempt_leaves_it_alone() {
    let db = WorkDb::open(temp_db_path("ci-fix-manual-start-active")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-manual-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/3146";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3146);

    let exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    let attempt = db.get_ci_remediation(&rem_id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending");
    assert_eq!(executions_for(&db, &revision_id).len(), 1);
}

/// A start that cannot be honoured is refused with the reason, not accepted
/// and then archived by the next tick: once the chain root's PR has merged
/// (chain root `done`), `request_execution` on the revision errors and mints
/// nothing.
#[test]
fn manual_start_of_ci_fix_revision_refused_when_parent_pr_closed() {
    let db = WorkDb::open(temp_db_path("ci-fix-manual-start-parent-closed")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-parent-closed");
    let pr_url = "https://github.com/spinyfin/mono/pull/3147";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (_rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3147);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            rusqlite::params![chore_id],
        )
        .unwrap();

    let err = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .expect_err("start of a revision whose parent PR closed must be refused");
    let message = err.to_string();
    assert!(
        message.contains(&format!("cannot start {revision_id}")) && message.contains(&chore_id),
        "refusal must name the item and its chain root: {message}",
    );
    assert!(
        message.contains("merged") && message.contains("closed"),
        "refusal must say why: {message}",
    );
    assert!(
        executions_for(&db, &revision_id).is_empty(),
        "a refused start must not strand an execution",
    );
}

/// An `active` CI-fix revision whose worker is gone and whose spawning
/// attempt retired underneath it (failed or succeeded), with no reconcile
/// tick in between — the live-worker-lost shape the automatic recovery
/// paths see. Returns `(product_id, rem_id, revision_id, dead_execution_id)`.
fn active_revision_with_retired_attempt(db: &WorkDb, label: &str, succeeded: bool) -> (String, String, String, String) {
    let product_id = make_revision_product(db, label);
    let pr_url = "https://github.com/spinyfin/mono/pull/3148";
    let chore_id = make_in_review_chore(db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(db, &product_id, &chore_id, pr_url, 3148);

    db.reconcile_product_executions(&product_id).unwrap();
    let exec_id = executions_for(db, &revision_id)[0].0.clone();
    start_run(db, &exec_id, "worker-1");
    assert_eq!(task_status(db, &revision_id), "active", "{label}: precondition");
    // The rescan only honours rows whose single-shot `autostart` is armed.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET autostart = 1 WHERE id = ?1",
            rusqlite::params![revision_id],
        )
        .unwrap();
    if succeeded {
        db.mark_ci_remediation_succeeded(&rem_id, Some("sha-after")).unwrap();
    } else {
        db.mark_ci_remediation_failed(&rem_id, "ran out of attempts").unwrap();
    }
    db.mark_execution_orphaned(&exec_id, "worker died").unwrap();
    (product_id, rem_id, revision_id, exec_id)
}

/// Shared post-conditions for the automatic-recovery negatives: the attempt
/// ledger is still retired exactly as it was, and any block the gate left
/// behind is intact. A regression that set `explicit_start` on an automatic
/// caller would flip the attempt to `pending`.
fn assert_attempt_still_retired(db: &WorkDb, rem_id: &str, revision_id: &str, name: &str, succeeded: bool) {
    let attempt = db.get_ci_remediation(rem_id).unwrap().unwrap();
    assert_eq!(
        attempt.status,
        if succeeded { "succeeded" } else { "failed" },
        "{name}: automatic recovery must not re-open the attempt",
    );
    assert!(attempt.finished_at.is_some(), "{name}: finished_at must stay set");
    assert!(
        !executions_for(db, revision_id)
            .iter()
            .any(|(_, status)| matches!(status.as_str(), "running")),
        "{name}: no run may be live for a retired attempt",
    );
}

/// After a reconcile tick the revision is settled, not running: a failed
/// attempt parks it `blocked` with the retired-attempt reason intact; a
/// succeeded one is moved out of Doing by the reconciler (its fix landed).
fn assert_revision_settled(db: &WorkDb, revision_id: &str, name: &str, succeeded: bool) {
    let status = task_status(db, revision_id);
    assert!(
        !matches!(status.as_str(), "active" | "todo"),
        "{name}: a retired attempt must not leave the revision runnable, got {status}",
    );
    if !succeeded {
        assert_eq!(status, "blocked", "{name}");
        assert_eq!(
            blocked_fields(db, revision_id).0.as_deref(),
            Some("spawning_attempt_retired_unsuccessfully"),
            "{name}: the retired-attempt block must stay intact",
        );
    }
    assert!(
        !executions_for(db, revision_id)
            .iter()
            .any(|(_, status)| matches!(status.as_str(), "ready" | "running")),
        "{name}: no runnable execution may survive for a retired attempt",
    );
}

#[test]
fn reconcile_active_dispatch_does_not_reopen_retired_attempt() {
    for succeeded in [false, true] {
        let name = if succeeded { "succeeded" } else { "failed" };
        let db = WorkDb::open(temp_db_path(&format!("ci-fix-reconcile-active-{name}"))).unwrap();
        let (product_id, rem_id, revision_id, _exec) =
            active_revision_with_retired_attempt(&db, &format!("rad-{name}"), succeeded);

        // Startup rehydration: no worker is live.
        db.reconcile_active_dispatch(|_| false).unwrap();
        assert_attempt_still_retired(&db, &rem_id, &revision_id, name, succeeded);

        // The revision reconciler then settles the row rather than running it.
        db.reconcile_product_executions(&product_id).unwrap();
        assert_attempt_still_retired(&db, &rem_id, &revision_id, name, succeeded);
        assert_revision_settled(&db, &revision_id, name, succeeded);
    }
}

#[tokio::test]
async fn settling_retired_revision_publishes_its_board_change() {
    use boss_event_bus::{Event, EventKind, TopicFilter};
    let db = WorkDb::open(temp_db_path("retired-revision-board-change")).unwrap();
    let (product_id, _rem_id, revision_id, _execution_id) =
        active_revision_with_retired_attempt(&db, "board-change", true);
    let mut sub = db.event_bus().subscribe(TopicFilter::kind(EventKind::WorkItemChanged));
    db.reconcile_product_executions(&product_id).unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        Event::WorkItemChanged {
            product_id,
            task_id: revision_id.clone()
        }
    );
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

#[test]
fn rescan_active_dispatch_does_not_reopen_retired_attempt() {
    for succeeded in [false, true] {
        let name = if succeeded { "succeeded" } else { "failed" };
        let db = WorkDb::open(temp_db_path(&format!("ci-fix-rescan-active-{name}"))).unwrap();
        let (product_id, rem_id, revision_id, _exec) =
            active_revision_with_retired_attempt(&db, &format!("rsc-{name}"), succeeded);

        db.rescan_active_dispatch().unwrap();
        assert_attempt_still_retired(&db, &rem_id, &revision_id, name, succeeded);

        db.reconcile_product_executions(&product_id).unwrap();
        assert_attempt_still_retired(&db, &rem_id, &revision_id, name, succeeded);
        assert_revision_settled(&db, &revision_id, name, succeeded);
    }
}

/// Direct `request_execution` (what the dead-pid sweep issues) on a
/// settled, blocked revision: not explicit, so the attempt stays retired and
/// the block stays intact.
#[test]
fn automatic_request_execution_does_not_reopen_retired_attempt() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::t34::automatic_request_execution_does_not_reopen_retired_attempt",
    );
    for succeeded in [false, true] {
        let name = if succeeded { "succeeded" } else { "failed" };
        let db = WorkDb::open(temp_db_path(&format!("ci-fix-auto-no-reopen-{name}"))).unwrap();
        let (product_id, rem_id, revision_id, _exec) =
            active_revision_with_retired_attempt(&db, &format!("auto-{name}"), succeeded);
        db.reconcile_product_executions(&product_id).unwrap();
        assert_revision_settled(&db, &revision_id, name, succeeded);
        let blocked_before = blocked_fields(&db, &revision_id);
        let status_before = task_status(&db, &revision_id);

        let _ = db.request_execution(RequestExecutionInput::builder().work_item_id(&revision_id).build());
        assert_attempt_still_retired(&db, &rem_id, &revision_id, name, succeeded);
        assert_eq!(
            blocked_fields(&db, &revision_id),
            blocked_before,
            "{name}: a non-explicit request must not lift the retired-attempt block",
        );
        assert_eq!(
            task_status(&db, &revision_id),
            status_before,
            "{name}: a non-explicit request must not move the settled row",
        );
    }
}

/// An explicit start is a human override: it re-opens even a `succeeded`
/// attempt.
#[test]
fn explicit_start_reopens_succeeded_attempt() {
    let db = WorkDb::open(temp_db_path("ci-fix-explicit-succeeded")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-explicit-succ");
    let pr_url = "https://github.com/spinyfin/mono/pull/3149";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 3149);

    db.reconcile_product_executions(&product_id).unwrap();
    db.mark_ci_remediation_succeeded(&rem_id, Some("sha-after")).unwrap();
    db.reconcile_product_executions(&product_id).unwrap();

    let exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    assert_eq!(db.get_ci_remediation(&rem_id).unwrap().unwrap().status, "pending");
}
