//! Steps 2 to 7 of the sequence, a test per behaviour.
//!
//! Steps 3 and 4 refresh the PR once and route what the refresh finds into the
//! paths the engine already has: the merged and closed-unmerged paths, conflict
//! resolution, CI remediation and the AI review. Steps 2, 5, 6 and 7 clear stale
//! halt stamps, re-derive the item's execution without fighting live work or a
//! recorded hold, re-enter its revisions into chain serialization, and tell
//! progression to look at it.

use boss_work_db::WorkerFailureBasis;
use boss_work_db::parking_reconcile::{HaltStamp, KeptHold};

use super::*;

// ── steps 3 and 4: the PR ──────────────────────────────────────────────────

fn open_conflicting() -> PrLifecycleState {
    PrLifecycleState::Open(OpenPrStatus::conflict_only())
}

#[tokio::test]
async fn a_pr_that_merged_while_parked_takes_the_merged_path_to_done() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7201";
    let chore = f.chore_in_review("merged", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Merged, "base-1", "head-1");

    let outcome = f.unpark_and_reconcile(&chore).await;

    assert_eq!(f.task(&chore).status, TaskStatus::Done);
    let refresh = outcome.get(Step::RefreshPr).expect("step 3 ran");
    assert_eq!((refresh.kind, refresh.code), (StepKind::Ok, "merged"));
    for step in [Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
        let record = outcome.get(step).expect("recorded");
        assert_eq!(
            (record.kind, record.code),
            (StepKind::Skipped, "pr_not_open"),
            "{step:?}"
        );
    }
    assert!(
        f.live_or_ready(&chore).is_empty(),
        "a finished item is given no execution"
    );
}

#[tokio::test]
async fn a_pr_closed_while_parked_takes_the_closed_unmerged_path() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7202";
    let chore = f.chore_in_review("closed", pr);
    f.park(&chore);
    f.probe.set(pr, PrLifecycleState::ClosedUnmerged);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let refresh = outcome.get(Step::RefreshPr).expect("step 3 ran");
    assert_eq!((refresh.kind, refresh.code), (StepKind::Ok, "closed_unmerged"));
    assert_eq!(
        f.task(&chore).status,
        TaskStatus::Done,
        "the same status the merge poller's own closed-unmerged handling leaves"
    );
    assert_eq!(outcome.get(Step::AiReview).unwrap().code, "pr_not_open");
}

#[tokio::test]
async fn a_conflicting_pr_enters_the_conflict_resolution_path() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7203";
    let chore = f.chore_in_review("conflicting", pr);
    f.park(&chore);
    f.probe.set_with_base_head(pr, open_conflicting(), "base-1", "head-1");
    assert!(f.db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none());

    let outcome = f.unpark_and_reconcile(&chore).await;

    let conflict = outcome.get(Step::ConflictProbe).expect("step 4 ran");
    assert_eq!((conflict.kind, conflict.code), (StepKind::Ok, "conflict_routed"));
    assert!(
        f.db.active_conflict_resolution_for_work_item(&chore).unwrap().is_some(),
        "the conflict has a resolution attempt, exactly as if the poller had found it"
    );
    let ci = outcome.get(Step::CiProbe).unwrap();
    assert_eq!((ci.kind, ci.code), (StepKind::Skipped, "conflict_first"));
    let review = outcome.get(Step::AiReview).unwrap();
    assert_eq!((review.kind, review.code), (StepKind::Skipped, "fix_in_flight"));
    assert!(
        f.db.list_executions(Some(&chore))
            .unwrap()
            .iter()
            .all(|e| e.kind != boss_protocol::ExecutionKind::PrReview),
        "no review of a head that is about to change"
    );
}

#[tokio::test]
async fn with_the_mechanical_rebase_rung_on_the_conflict_goes_to_the_poller_remediator() {
    let ladder = Arc::new(RecordingLadder::default());
    let f = Fixture::with_ladder(ladder.clone());
    let pr = "https://github.com/foo/bar/pull/7212";
    let chore = f.chore_in_review("ladder", pr);
    f.park(&chore);
    f.probe.set_with_base_head(pr, open_conflicting(), "base-1", "head-1");

    let outcome = f.unpark_and_reconcile(&chore).await;

    let conflict = outcome.get(Step::ConflictProbe).unwrap();
    assert_eq!((conflict.kind, conflict.code), (StepKind::Ok, "conflict_routed"));
    let mut handed_off = false;
    for _ in 0..500 {
        if ladder.seen.lock().unwrap().as_slice() == [pr.to_owned()] {
            handed_off = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        handed_off,
        "the rebase rung runs first, through the same bounded queue the poller uses"
    );
    let attempt = f.db.active_conflict_resolution_for_work_item(&chore).unwrap();
    assert!(
        attempt.is_some(),
        "and the durable attempt row exists, so a lost hand-off is retried"
    );
}

#[tokio::test]
async fn a_conflict_already_being_resolved_is_reported_not_routed_again() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7204";
    let chore = f.chore_in_review("conflict-in-flight", pr);
    f.probe.set_with_base_head(pr, open_conflicting(), "base-1", "head-1");
    // The poller finds the conflict before the park and a worker takes the fix.
    reconcile_one(
        &f.db,
        f.probe.as_ref(),
        f.env.publisher.as_ref(),
        None,
        Some(f.env.completion.as_ref()),
        Some(f.env.host.as_ref()),
        pr,
    )
    .await;
    let attempt =
        f.db.active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .expect("attempt exists");
    let revision = attempt
        .revision_task_id
        .clone()
        .expect("the attempt has its fix revision");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running', started_at = '1' WHERE work_item_id = ?1",
            [&revision],
        )
        .unwrap();
    f.park(&chore);
    assert_eq!(
        f.live_or_ready(&revision).len(),
        1,
        "setup: parking leaves a live worker alone"
    );

    let outcome = f.unpark_and_reconcile(&chore).await;

    let conflict = outcome.get(Step::ConflictProbe).unwrap();
    assert_eq!(
        (conflict.kind, conflict.code),
        (StepKind::Ok, "conflict_in_flight"),
        "{conflict:?}"
    );
    assert_eq!(
        f.db.active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .map(|a| a.id),
        Some(attempt.id),
        "no second attempt: progression must not put a second writer on a PR that has one"
    );
    assert_eq!(
        f.live_or_ready(&revision).len(),
        1,
        "and no second execution on the revision"
    );
}

#[tokio::test]
async fn red_ci_enters_remediation_with_the_attempt_budget_reset() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7205";
    let chore = f.chore_in_review("red", pr);
    // Attempts spent before the park exhaust the default budget of 3.
    f.db.connect()
        .unwrap()
        .execute("UPDATE tasks SET ci_attempts_used = 3 WHERE id = ?1", [&chore])
        .unwrap();
    f.park(&chore);
    f.probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));

    let outcome = f.unpark_and_reconcile(&chore).await;

    let ci = outcome.get(Step::CiProbe).expect("step 4 ran");
    assert_eq!((ci.kind, ci.code), (StepKind::Ok, "ci_remediation_routed"));
    assert!(ci.detail.contains("reset from 3"), "{}", ci.detail);
    assert_eq!(
        f.db.list_ci_remediations(None, &[], Some(&chore), None).unwrap().len(),
        1
    );
    let task = f.task(&chore);
    assert_eq!(
        (task.status, task.blocked_reason.as_deref()),
        (TaskStatus::InReview, None),
        "with the stale evidence cleared the budget is not exhausted"
    );
    assert_eq!(
        f.db.get_ci_attempts_used(&chore).unwrap(),
        1,
        "the new attempt is the first one counted"
    );
    let conflict = outcome.get(Step::ConflictProbe).unwrap();
    assert_eq!(conflict.code, "clear");
    assert_eq!(outcome.get(Step::AiReview).unwrap().code, "fix_in_flight");
}

#[tokio::test]
async fn an_item_parked_with_its_ci_budget_exhausted_is_re_armed_and_remediated() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7206";
    let chore = f.chore_in_review("exhausted", pr);
    f.db.connect()
        .unwrap()
        .execute("UPDATE tasks SET ci_attempts_used = 3 WHERE id = ?1", [&chore])
        .unwrap();
    f.probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    let before = reconcile_one(&f.db, f.probe.as_ref(), f.env.publisher.as_ref(), None, None, None, pr)
        .await
        .0;
    assert_eq!(before.ci_flagged, 1);
    let task = f.task(&chore);
    assert_eq!(
        (task.status, task.blocked_reason.as_deref()),
        (TaskStatus::Blocked, Some("ci_failure_exhausted")),
        "setup: the budget ran out before the park"
    );
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let ci = outcome.get(Step::CiProbe).unwrap();
    assert_eq!(ci.code, "ci_remediation_routed", "{ci:?}");
    assert!(
        ci.detail.contains("re-armed from ci_failure_exhausted"),
        "{}",
        ci.detail
    );
    let task = f.task(&chore);
    assert_eq!(task.blocked_reason, None, "no longer blocked on the exhausted budget");
    assert_eq!(
        f.db.list_ci_remediations(None, &[], Some(&chore), None).unwrap().len(),
        1
    );
}

#[tokio::test]
async fn a_clean_open_pr_is_left_alone_and_gets_an_ai_review_it_missed() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7207";
    let chore = f.chore_in_review("clean", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");

    let outcome = f.unpark_and_reconcile(&chore).await;

    assert_eq!(outcome.get(Step::RefreshPr).unwrap().code, "open");
    assert_eq!(outcome.get(Step::ConflictProbe).unwrap().code, "clear");
    assert_eq!(outcome.get(Step::CiProbe).unwrap().code, "clear");
    let review = outcome.get(Step::AiReview).unwrap();
    assert_eq!((review.kind, review.code), (StepKind::Ok, "scheduled"), "{review:?}");
    let reviews: Vec<_> =
        f.db.list_executions(Some(&chore))
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == boss_protocol::ExecutionKind::PrReview && !e.status.is_terminal())
            .collect();
    assert_eq!(reviews.len(), 1, "one review of the current head is queued");
    assert_eq!(f.task(&chore).status, TaskStatus::InReview);
}

#[tokio::test]
async fn a_pr_already_reviewed_at_its_head_is_not_reviewed_again() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7208";
    let chore = f.chore_in_review("reviewed", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 1, last_reviewed_sha = 'head-1' WHERE id = ?1",
            [&chore],
        )
        .unwrap();

    let outcome = f.unpark_and_reconcile(&chore).await;

    let review = outcome.get(Step::AiReview).unwrap();
    assert_eq!((review.kind, review.code), (StepKind::Ok, "current"), "{review:?}");
    assert!(f.live_or_ready(&chore).is_empty());
}

#[tokio::test]
async fn a_pr_that_used_its_review_cycles_is_left_to_a_human() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7209";
    let chore = f.chore_in_review("cycled", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-2");
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 3, last_reviewed_sha = 'head-1' WHERE id = ?1",
            [&chore],
        )
        .unwrap();

    let outcome = f.unpark_and_reconcile(&chore).await;

    let review = outcome.get(Step::AiReview).unwrap();
    assert_eq!(
        (review.kind, review.code),
        (StepKind::Skipped, "cycle_bound"),
        "{review:?}"
    );
    assert!(f.live_or_ready(&chore).is_empty());
}

#[tokio::test]
async fn an_item_with_no_pr_has_no_pr_steps() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "no-pr").id;
    let chore = create_test_chore_manual(&f.db, &product, "unstarted").id;
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    for step in [Step::RefreshPr, Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
        let record = outcome.get(step).unwrap();
        assert_eq!((record.kind, record.code), (StepKind::Skipped, "no_pr"), "{step:?}");
    }
}

#[tokio::test]
async fn a_pr_the_probe_cannot_observe_is_recorded_as_an_error_and_the_sequence_continues() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7210";
    let chore = f.chore_in_review("gh-down", pr);
    f.park(&chore);
    f.probe.set_err(pr, "gh: HTTP 502");

    let outcome = f.unpark_and_reconcile(&chore).await;

    let refresh = outcome.get(Step::RefreshPr).unwrap();
    assert_eq!(
        (refresh.kind, refresh.code),
        (StepKind::Error, "unobserved"),
        "{refresh:?}"
    );
    assert!(outcome.has_error());
    for step in [Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
        assert_eq!(outcome.get(step).unwrap().kind, StepKind::Skipped, "{step:?}");
    }
    // The later steps still ran: step 7 flagged the item for progression.
    assert_eq!(outcome.get(Step::ProgressionGrace).unwrap().code, "stamped");
    assert!(f.db.progression_grace_waived_at(&chore).unwrap().is_some());
    assert_eq!(
        f.task(&chore).status,
        TaskStatus::InReview,
        "the item stays where it was"
    );
}

#[tokio::test]
async fn a_revision_follows_its_parents_pr_and_is_not_probed_itself() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7211";
    let chore = f.chore_in_review("parent", pr);
    let revision =
        f.db.create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Address a finding")
                .build(),
            &crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open),
        )
        .unwrap();
    f.park(&chore);
    f.unpark(&chore);

    let outcome = reconcile_unparked_item(&f.env, &revision.id, GithubAccess::Allowed).await;

    for step in [Step::RefreshPr, Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
        let record = outcome.get(step).unwrap();
        assert_eq!(
            (record.kind, record.code),
            (StepKind::Skipped, "follows_parent"),
            "{step:?}"
        );
    }
    assert!(
        outcome.get(Step::Revisions).is_none(),
        "only a chain root re-enters its revisions"
    );
}

// ── steps 2, 5, 6 and 7: the item ──────────────────────────────────────────

fn stamp_halt(f: &Fixture, id: &str, reason: &str) {
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_reason = ?2, dispatch_failed_error = 'boom', dispatch_failed_at = '1'
             WHERE id = ?1",
            rusqlite::params![id, reason],
        )
        .unwrap();
}

// ── step 5: todo ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_todo_item_the_park_cancelled_is_minted_a_ready_execution_again() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "todo").id;
    let chore = create_test_chore(&f.db, &product, "autostarted").id;
    f.db.reconcile_item_execution(&chore).unwrap();
    assert_eq!(f.live_or_ready(&chore).len(), 1, "setup: the row has a ready execution");
    f.park(&chore);
    assert!(f.live_or_ready(&chore).is_empty(), "park cancels the pending execution");

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::ReconcileExecution).expect("step 5 ran");
    assert_eq!((record.kind, record.code), (StepKind::Ok, "minted"), "{record:?}");
    let live = f.live_or_ready(&chore);
    assert_eq!(live.len(), 1, "exactly one execution: {live:?}");
    assert_eq!(live[0].status, ExecutionStatus::Ready);
    assert_eq!(live[0].kind, boss_protocol::ExecutionKind::ChoreImplementation);
}

#[tokio::test]
async fn a_todo_item_waiting_on_a_prerequisite_is_minted_a_waiting_dependency_row() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "todo-dependency").id;
    let prerequisite = create_test_chore_manual(&f.db, &product, "prerequisite").id;
    let dependent = create_test_chore(&f.db, &product, "dependent").id;
    f.db.add_dependency(crate::work::AddDependencyInput {
        dependent: dependent.clone(),
        prerequisite,
        relation: None,
    })
    .unwrap();
    f.db.reconcile_item_execution(&dependent).unwrap();
    f.park(&dependent);

    let outcome = f.unpark_and_reconcile(&dependent).await;

    assert_eq!(outcome.get(Step::ReconcileExecution).unwrap().code, "minted");
    let live = f.live_or_ready(&dependent);
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].status,
        ExecutionStatus::WaitingDependency,
        "minted exactly as for a fresh row, so the gate still holds it back"
    );
}

#[tokio::test]
async fn a_todo_item_with_autostart_off_is_not_minted_an_execution() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "manual").id;
    let chore = create_test_chore_manual(&f.db, &product, "manual").id;
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    assert_eq!(outcome.get(Step::ReconcileExecution).unwrap().code, "nothing_to_mint");
    assert!(f.executions(&chore).is_empty());
}

// ── step 5: active ─────────────────────────────────────────────────────────

#[tokio::test]
async fn an_active_item_with_no_worker_is_redispatched_by_the_orphan_sweep() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "active").id;
    let chore = create_active_chore(&f.db, &product, "was in doing");
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::ReconcileExecution).unwrap();
    assert_eq!((record.kind, record.code), (StepKind::Ok, "redispatched"), "{record:?}");
    let live = f.live_or_ready(&chore);
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].status, ExecutionStatus::Ready);
}

#[tokio::test]
async fn the_churn_guard_counts_only_executions_since_the_unpark() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "churn").id;
    let chore = create_active_chore(&f.db, &product, "churned before the park");
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    // Three dead runs inside the window and in an unbroken streak: enough to
    // trip the guard on the next redispatch if they still counted.
    for age in [600, 500, 400] {
        f.db.insert_terminal_execution_for_test(&chore, "chore_implementation", "abandoned", now - age)
            .unwrap();
    }
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::ReconcileExecution).unwrap();
    assert_eq!(
        (record.kind, record.code),
        (StepKind::Ok, "redispatched"),
        "what died before the owner decided to try again is not evidence against the first attempt after it: {record:?}"
    );
    assert_eq!(f.task(&chore).dispatch_failed_reason, None);
}

#[tokio::test]
async fn the_churn_guard_still_trips_on_executions_that_die_after_the_unpark() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "churn-after").id;
    let chore = create_active_chore(&f.db, &product, "churns again");
    f.park(&chore);
    f.unpark(&chore);
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    for age in [0, 1, 2] {
        f.db.insert_terminal_execution_for_test(&chore, "chore_implementation", "abandoned", now + 2 - age)
            .unwrap();
    }

    let outcome = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    let record = outcome.get(Step::ReconcileExecution).unwrap();
    assert_eq!(
        (record.kind, record.code),
        (StepKind::Skipped, "churn_guard"),
        "{record:?}"
    );
    assert_eq!(f.task(&chore).dispatch_failed_reason.as_deref(), Some("churn_guard"));
}

// ── step 5: holds ──────────────────────────────────────────────────────────

#[tokio::test]
async fn every_worker_failure_reaches_step_7_without_a_step_5_redispatch() {
    for basis in [
        WorkerFailureBasis::OperatorNo,
        WorkerFailureBasis::DeclaredBlocked,
        WorkerFailureBasis::NudgeBreaker,
        WorkerFailureBasis::DeliveredWithoutPr,
        WorkerFailureBasis::ForeignPrBinding,
    ] {
        let f = Fixture::new();
        let chore = f.worker_failed(basis.as_str(), basis);
        f.db.connect()
            .unwrap()
            .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&chore])
            .unwrap();
        f.park(&chore);
        let executions_before = f.executions(&chore).len();

        let report = f.unpark(&chore);
        let outcome = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

        let hold = report.rows[0].kept_hold.clone().expect("the failure is kept");
        let operator_refused = basis == WorkerFailureBasis::OperatorNo;
        assert_eq!(
            hold.is_human_hold(),
            operator_refused,
            "{basis:?}: only the owner's No is a human hold"
        );
        let step5 = outcome.get(Step::ReconcileExecution).unwrap();
        assert_eq!(
            (step5.kind, step5.code),
            (StepKind::Skipped, "held"),
            "{basis:?}: {step5:?}"
        );
        assert_eq!(
            f.executions(&chore).len(),
            executions_before,
            "{basis:?}: nothing was minted"
        );
        let task = f.task(&chore);
        assert_eq!(
            (task.status, task.blocked_reason.as_deref()),
            (TaskStatus::Blocked, Some("worker_failed")),
            "{basis:?}: the failure evidence is retained until progression clears it"
        );
        assert_eq!(
            outcome.get(Step::ProgressionGrace).unwrap().code,
            "stamped",
            "{basis:?}: every held item is handed to progression"
        );
        assert!(f.db.progression_grace_waived_at(&chore).unwrap().is_some(), "{basis:?}");
    }
}

#[tokio::test]
async fn a_stale_churn_guard_stamp_is_cleared_while_the_operators_refusal_is_kept() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "stamps").id;
    let churned = create_test_chore_manual(&f.db, &product, "churned").id;
    stamp_halt(&f, &churned, "churn_guard");
    let refused = f.worker_failed("refused", WorkerFailureBasis::OperatorNo);
    f.park(&churned);
    f.park(&refused);

    let churned_report = f.unpark(&churned);
    let refused_report = f.unpark(&refused);

    assert_eq!(
        churned_report.rows[0].halt_stamp,
        HaltStamp::Cleared {
            reason: "churn_guard".into()
        }
    );
    assert_eq!(f.task(&churned).dispatch_failed_reason, None);
    assert_eq!(refused_report.rows[0].kept_hold, Some(KeptHold::OperatorRefusal));
    assert_eq!(f.task(&refused).blocked_reason.as_deref(), Some("worker_failed"));
}

#[tokio::test]
async fn an_operator_question_stays_open_and_is_not_redispatched() {
    let f = Fixture::new();
    let chore = f.worker_failed("question", WorkerFailureBasis::DeclaredBlocked);
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
            [&chore],
        )
        .unwrap();
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let step5 = outcome.get(Step::ReconcileExecution).unwrap();
    assert_eq!((step5.kind, step5.code), (StepKind::Skipped, "held"));
    assert!(step5.detail.contains("awaiting_operator_answer"), "{}", step5.detail);
}

// ── step 5: in_review ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_in_review_item_has_no_implementation_execution_to_derive() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7301";
    let chore = f.chore_in_review("in-review", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::ReconcileExecution).unwrap();
    assert_eq!((record.kind, record.code), (StepKind::Skipped, "no_execution_state"));
}

// ── step 6: revisions ──────────────────────────────────────────────────────

#[tokio::test]
async fn revisions_parked_with_the_parent_re_enter_chain_serialization() {
    let f = Fixture::new();
    let (project, tasks) = f.project_with_open_prs(1);
    let parent = tasks[0].clone();
    f.probe.set_with_base_head(
        &f.pr_url(&parent),
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-1",
        "head-1",
    );
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let revision = |description: &str| {
        f.db.create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(parent.clone())
                .description(description)
                .build(),
            &checker,
        )
        .unwrap()
        .id
    };
    let first = revision("first finding");
    let second = revision("second finding");
    let status_of = |id: &str| f.live_or_ready(id).into_iter().map(|e| e.status).collect::<Vec<_>>();
    assert_eq!(
        status_of(&first),
        vec![ExecutionStatus::Ready],
        "setup: the head of the chain is ready"
    );
    assert_eq!(
        status_of(&second),
        vec![ExecutionStatus::WaitingDependency],
        "setup: the tail queues"
    );

    f.db.park_work_item_as_actor(&project, true, None, HUMAN).unwrap();
    assert!(f.live_or_ready(&first).is_empty() && f.live_or_ready(&second).is_empty());
    assert!(
        f.task(&first).parked_at.is_some(),
        "the revisions were parked with the parent's project"
    );

    let (_item, report) = f.db.park_work_item_reporting(&project, false, None, HUMAN).unwrap();
    let job = UnparkJob::for_report(&project, &report);
    assert_eq!(
        job.items,
        vec![parent.clone()],
        "revisions are reconciled with their chain root, not as items"
    );
    let outcome = reconcile_unparked_item(&f.env, &parent, GithubAccess::Allowed).await;

    let record = outcome.get(Step::Revisions).expect("step 6 ran");
    assert_eq!((record.kind, record.code), (StepKind::Ok, "re_entered"), "{record:?}");
    assert_eq!(status_of(&first), vec![ExecutionStatus::Ready]);
    assert_eq!(
        status_of(&second),
        vec![ExecutionStatus::WaitingDependency],
        "the chain is serialized exactly as before the park: one writer on the PR"
    );
    for id in [&first, &second] {
        assert!(f.db.progression_grace_waived_at(id).unwrap().is_some());
        let steps = f.event_steps(id).await;
        assert!(steps.contains(&"reconcile_execution".into()), "{steps:?}");
        assert!(steps.contains(&"progression_grace".into()), "{steps:?}");
        assert!(f.parking_lines(id).iter().any(|line| line.contains("grace")));
    }
    assert!(f.task(&first).parked_at.is_none() && f.task(&second).parked_at.is_none());
}

#[tokio::test]
async fn an_item_without_revisions_records_that_step_6_had_nothing_to_do() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "no-revisions").id;
    let chore = create_test_chore_manual(&f.db, &product, "plain").id;
    f.park(&chore);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::Revisions).unwrap();
    assert_eq!((record.kind, record.code), (StepKind::Skipped, "no_revisions"));
}

// ── step 7 ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn step_7_stamps_the_grace_waiver_on_every_open_item() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "grace").id;
    let chore = create_test_chore_manual(&f.db, &product, "open").id;
    f.park(&chore);
    assert_eq!(f.db.progression_grace_waived_at(&chore).unwrap(), None);

    let outcome = f.unpark_and_reconcile(&chore).await;

    let record = outcome.get(Step::ProgressionGrace).unwrap();
    assert_eq!((record.kind, record.code), (StepKind::Ok, "stamped"));
    assert!(f.db.progression_grace_waived_at(&chore).unwrap().is_some());
}
