//! What every step leaves behind: a `[parking]` line on the item and an
//! `unpark_reconcile` dispatch event, with the step name and outcome. And the
//! sequence's two promises: a failing step does not stop the ones after it, and
//! running it again is harmless.

use super::*;

/// Every step name the sequence can record, in the order it records them for an
/// item with an open PR.
const PR_ITEM_STEPS: [&str; 9] = [
    "clear_park",
    "clear_halt_stamps",
    "refresh_pr",
    "conflict_probe",
    "ci_probe",
    "ai_review",
    "reconcile_execution",
    "revisions",
    "progression_grace",
];

#[test]
fn step_names_are_stable() {
    let names = [
        Step::ClearPark,
        Step::ClearHaltStamps,
        Step::RefreshPr,
        Step::ConflictProbe,
        Step::CiProbe,
        Step::AiReview,
        Step::ReconcileExecution,
        Step::Revisions,
        Step::ProgressionGrace,
    ]
    .map(Step::name);
    assert_eq!(names, PR_ITEM_STEPS, "the audit trail and the events share these names");
}

#[tokio::test]
async fn every_step_appends_a_parking_line_and_emits_an_event() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7401";
    let chore = f.chore_in_review("traced", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");

    let report = f.unpark(&chore);
    record_released_rows(&f.env, HUMAN, &report).await;
    reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    let lines = f.parking_lines(&chore);
    assert_eq!(lines.len(), PR_ITEM_STEPS.len(), "one line per step: {lines:#?}");
    for (line, step) in lines.iter().zip(PR_ITEM_STEPS) {
        assert!(
            line.starts_with("[parking] epoch ") && line.contains(&format!(": {step}: ")) && line.ends_with('.'),
            "expected a `[parking]` line for {step}, got {line:?}"
        );
    }
    assert_eq!(
        f.event_steps(&chore).await,
        PR_ITEM_STEPS,
        "one event per step, in order"
    );
}

#[tokio::test]
async fn each_event_carries_the_step_the_outcome_and_the_work_item() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7402";
    let chore = f.chore_in_review("event-shape", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Merged, "base-1", "head-1");

    let report = f.unpark(&chore);
    record_released_rows(&f.env, HUMAN, &report).await;
    reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    let events = f.unpark_events(&chore).await;
    let by_step = |step: &str| {
        events
            .iter()
            .find(|event| event.details["step"] == step)
            .unwrap_or_else(|| panic!("no {step} event"))
    };
    let refresh = by_step("refresh_pr");
    assert_eq!(refresh.stage, "unpark_reconcile");
    assert_eq!(refresh.outcome, "ok");
    assert_eq!(refresh.work_item_id.as_deref(), Some(chore.as_str()));
    assert_eq!(refresh.details["outcome"], "merged");
    assert!(
        refresh.details["detail"].as_str().unwrap().contains("merged"),
        "{:?}",
        refresh.details
    );
    assert_eq!(by_step("clear_park").details["outcome"], "unparked");
    assert_eq!(by_step("clear_halt_stamps").details["outcome"], "nothing_to_clear");
    let skipped = by_step("conflict_probe");
    assert_eq!(
        skipped.outcome, "skipped",
        "a step whose precondition did not hold says so"
    );
    assert_eq!(skipped.details["outcome"], "pr_not_open");
}

#[tokio::test]
async fn the_trail_names_what_step_2_cleared_and_what_it_kept() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "step-2-trail").id;
    let churned = create_test_chore_manual(&f.db, &product, "churned").id;
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_reason = 'churn_guard', dispatch_failed_error = 'x', dispatch_failed_at = '1' WHERE id = ?1",
            [&churned],
        )
        .unwrap();
    let failed = f.worker_failed("declared", boss_work_db::WorkerFailureBasis::DeclaredBlocked);
    f.park(&churned);
    f.park(&failed);

    for id in [&churned, &failed] {
        let report = f.unpark(id);
        record_released_rows(&f.env, HUMAN, &report).await;
    }

    let cleared = f.parking_lines(&churned);
    assert_eq!(cleared.len(), 2, "{cleared:#?}");
    assert!(cleared[0].contains("clear_park: un-parked by human"), "{}", cleared[0]);
    assert!(
        cleared[1].contains("clear_halt_stamps: cleared the `churn_guard` dispatch-failure stamp"),
        "{}",
        cleared[1]
    );
    let kept = f.parking_lines(&failed);
    assert!(
        kept[1].contains("kept `worker_failed` (recorded as `declared_blocked`) as failure evidence"),
        "{}",
        kept[1]
    );
    let events = f.unpark_events(&failed).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].details["step"], "clear_halt_stamps");
    assert!(
        events[1].details["detail"]
            .as_str()
            .unwrap()
            .contains("failure evidence"),
        "the event carries the same sentence as the audit line: {:?}",
        events[1].details
    );
}

#[tokio::test]
async fn the_trail_does_not_leak_into_the_brief_a_worker_reads() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "brief").id;
    let chore = create_test_chore_manual(&f.db, &product, "brief").id;
    f.db.update_work_item(
        &chore,
        WorkItemPatch {
            description: Some("Do the thing.".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    f.park(&chore);

    f.unpark_and_reconcile(&chore).await;

    let description = f.description(&chore);
    assert!(description.contains("[parking]"), "{description}");
    assert_eq!(
        boss_engine_storage_values::audit_text::strip_engine_audit_lines(&description),
        "Do the thing."
    );
}

#[tokio::test]
async fn a_failing_step_is_recorded_and_the_later_steps_still_run() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7403";
    let chore = f.chore_in_review("fails", pr);
    f.park(&chore);
    f.probe.set_err(pr, "gh: HTTP 502");

    let report = f.unpark(&chore);
    record_released_rows(&f.env, HUMAN, &report).await;
    let outcome = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    assert!(outcome.has_error());
    let steps: Vec<_> = outcome.records.iter().map(|r| r.step.name()).collect();
    assert_eq!(
        steps,
        [
            "refresh_pr",
            "conflict_probe",
            "ci_probe",
            "ai_review",
            "reconcile_execution",
            "revisions",
            "progression_grace"
        ],
        "the failure did not stop the sequence"
    );
    let failed = f.unpark_events(&chore).await;
    let error_event = failed
        .iter()
        .find(|event| event.outcome == "error")
        .expect("the failure is an error event");
    assert_eq!(error_event.details["step"], "refresh_pr");
    assert_eq!(error_event.details["outcome"], "unobserved");
    let lines = f.parking_lines(&chore);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("refresh_pr: ") && line.contains("did not observe an open PR")),
        "{lines:#?}"
    );
    assert!(
        lines.last().unwrap().contains("progression_grace"),
        "the last step still left its line"
    );
    assert!(
        f.task(&chore).parked_at.is_none(),
        "the item stays un-parked whatever fails"
    );
}

#[tokio::test]
async fn an_unreadable_item_is_recorded_not_panicked_on() {
    let f = Fixture::new();

    let outcome = reconcile_unparked_item(&f.env, "task_does_not_exist", GithubAccess::Allowed).await;

    assert_eq!(outcome.records.len(), 1);
    assert_eq!(
        (outcome.records[0].kind, outcome.records[0].code),
        (StepKind::Error, "item_unreadable")
    );
    let events = f.unpark_events("task_does_not_exist").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, "error");
}

#[tokio::test]
async fn a_finished_item_is_skipped_whole() {
    let f = Fixture::new();
    let product = create_test_product_named(&f.db, "finished").id;
    let chore = create_test_chore_manual(&f.db, &product, "done already").id;
    f.db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&chore])
        .unwrap();

    let outcome = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    assert_eq!(outcome.records.len(), 1);
    assert_eq!(
        (outcome.records[0].kind, outcome.records[0].code),
        (StepKind::Skipped, "finished")
    );
}

#[tokio::test]
async fn re_running_the_sequence_on_an_unparked_item_performs_steps_3_to_7_only() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7404";
    let chore = f.chore_in_review("rerun", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    let first = f.unpark(&chore);
    assert_eq!(first.rows.len(), 1);

    // Un-parking again clears nothing, so there are no database steps to trace.
    let second = f.unpark(&chore);
    assert_eq!(second, UnparkReport::default(), "steps 1 and 2 have nothing to do");
    let job = UnparkJob::for_report(&chore, &second);
    assert_eq!(
        job.items,
        vec![chore.clone()],
        "but the item itself is still reconciled"
    );
    record_released_rows(&f.env, HUMAN, &second).await;
    assert!(
        f.parking_lines(&chore).is_empty(),
        "no phantom clear_park line for a park that was not there"
    );

    let outcome = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    let steps: Vec<_> = outcome.records.iter().map(|r| r.step.name()).collect();
    assert_eq!(
        steps,
        [
            "refresh_pr",
            "conflict_probe",
            "ci_probe",
            "ai_review",
            "reconcile_execution",
            "revisions",
            "progression_grace"
        ],
        "steps 3 to 7 only"
    );
}

#[tokio::test]
async fn the_sequence_is_idempotent() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7405";
    let chore = f.chore_in_review("idempotent", pr);
    f.park(&chore);
    f.probe
        .set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    f.unpark(&chore);

    let reviews = |f: &Fixture| {
        f.executions(&chore)
            .iter()
            .filter(|execution| execution.kind == boss_protocol::ExecutionKind::PrReview)
            .map(|execution| execution.id.clone())
            .collect::<Vec<_>>()
    };

    let first = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;
    let after_first = reviews(&f);
    let second = reconcile_unparked_item(&f.env, &chore, GithubAccess::Allowed).await;

    assert_eq!(first.get(Step::AiReview).unwrap().code, "scheduled");
    assert_eq!(second.get(Step::AiReview).unwrap().code, "scheduled");
    assert_eq!(after_first.len(), 1);
    assert_eq!(
        reviews(&f),
        after_first,
        "the second run reuses the queued review instead of adding one"
    );
    assert_eq!(f.task(&chore).status, TaskStatus::InReview);
    assert!(f.db.progression_grace_waived_at(&chore).unwrap().is_some());
}

#[tokio::test]
async fn a_rate_limited_item_skips_its_github_steps_and_still_runs_the_rest() {
    let f = Fixture::new();
    let pr = "https://github.com/foo/bar/pull/7406";
    let chore = f.chore_in_review("deferred", pr);
    f.park(&chore);
    f.unpark(&chore);

    let outcome = reconcile_unparked_item(
        &f.env,
        &chore,
        GithubAccess::Deferred {
            remaining: 120,
            reserve: 2_000,
        },
    )
    .await;

    for step in [Step::RefreshPr, Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
        let record = outcome.get(step).unwrap();
        assert_eq!(
            (record.kind, record.code),
            (StepKind::Skipped, "rate_limited"),
            "{step:?}"
        );
        assert!(
            record.detail.contains("120") && record.detail.contains("2000"),
            "{}",
            record.detail
        );
    }
    assert_eq!(
        outcome.get(Step::ProgressionGrace).unwrap().code,
        "stamped",
        "steps 5 to 7 still run"
    );
    assert!(!outcome.has_error(), "a deferral is not a failure");
}
