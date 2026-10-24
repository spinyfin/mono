use super::helpers::*;

#[tokio::test]
async fn failure_identity_survives_terminal_attempts_and_repeated_linking() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "failure identity", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(&product, &chore, pr);
    let mac = failure("buildkite/mono/mac-app-build", "FAILURE");
    let bazel = failure("buildkite/mono/bazel-build-test", "FAILURE");
    for (head, check, expected) in [
        ("compile-error", mac.clone(), 1),
        ("compile-error", bazel.clone(), 2),
        ("rebase-head", bazel.clone(), 3),
    ] {
        assert!(
            on_ci_failure_detected(
                &db,
                &publisher,
                &fix_checker(),
                &candidate,
                &probe(pr, head),
                std::slice::from_ref(&check)
            )
            .await
        );
        let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
        let revision = attempt.revision_task_id.as_ref().unwrap();
        assert!(task(&db, revision).name.contains(&check.name));
        db.set_ci_remediation_revision_task_id(&attempt.id, revision).unwrap();
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), expected);
        db.mark_ci_remediation_succeeded(&attempt.id, None).unwrap();
        close_superseded_moot_revision(&db, &chore, Some(revision), "failure handled");
        let transitioned =
            on_ci_failure_detected(&db, &publisher, &fix_checker(), &candidate, &probe(pr, head), &[check]).await;
        assert!(transitioned, "a terminal attempt must not hide a still-red head");
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, TaskStatus::Blocked);
        assert_eq!(
            reason.as_deref(),
            Some(if expected == 3 {
                "ci_failure_exhausted"
            } else {
                "ci_failure"
            })
        );
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), expected);
    }
    // Exhaustion remains sticky on a genuinely new failed head.
    assert!(
        !on_ci_failure_detected(
            &db,
            &publisher,
            &fix_checker(),
            &candidate,
            &probe(pr, "still-broken"),
            &[bazel]
        )
        .await
    );
    assert_eq!(chore_state(&db, &chore).1.as_deref(), Some("ci_failure_exhausted"));
}

#[tokio::test]
async fn refused_revision_does_not_charge_budget() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "closed PR", pr);
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Merged);
    on_ci_failure_detected(
        &db,
        &RecordingPublisher::default(),
        &checker,
        &candidate(&product, &chore, pr),
        &probe(pr, "head"),
        &one_failure(),
    )
    .await;
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}

#[tokio::test]
async fn green_fix_and_rebase_heads_do_not_mint_stale_mac_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "green heads", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(&product, &chore, pr);
    let mac = failure("buildkite/mono/mac-app-build", "FAILURE");
    assert!(
        on_ci_failure_detected(
            &db,
            &publisher,
            &fix_checker(),
            &candidate,
            &probe(pr, "failing-head"),
            std::slice::from_ref(&mac)
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    db.mark_ci_remediation_succeeded(&attempt.id, None).unwrap();
    close_superseded_moot_revision(&db, &chore, attempt.revision_task_id.as_deref(), "fixed");
    for head in ["fix-head", "rebase-head"] {
        let mut current = probe(pr, head);
        current.state = PrLifecycleState::Open(OpenPrStatus::clean());
        assert!(
            !on_ci_failure_detected(
                &db,
                &publisher,
                &fix_checker(),
                &candidate,
                &current,
                std::slice::from_ref(&mac)
            )
            .await
        );
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    }
}

#[tokio::test]
async fn prior_fix_does_not_suppress_a_budget_free_infrastructure_retrigger() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "infrastructure retrigger", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(&product, &chore, pr);
    assert!(
        on_ci_failure_detected(
            &db,
            &publisher,
            &fix_checker(),
            &candidate,
            &probe(pr, "head"),
            &one_failure()
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    db.mark_ci_remediation_succeeded(&attempt.id, None).unwrap();
    close_superseded_moot_revision(&db, &chore, attempt.revision_task_id.as_deref(), "fixed");
    assert!(
        on_ci_failure_detected(
            &db,
            &publisher,
            &fix_checker(),
            &candidate,
            &probe(pr, "head"),
            &[failure("ci/test", "STARTUP_FAILURE")]
        )
        .await
    );
    assert_eq!(
        db.active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .unwrap()
            .attempt_kind,
        "retrigger"
    );
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

#[tokio::test]
async fn multiple_failures_select_one_unattempted_check_after_prior_revision_finishes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "multiple failures", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(&product, &chore, pr);
    let failures = [
        failure("buildkite/mono/mac-app-build", "FAILURE"),
        failure("buildkite/mono/bazel-build-test", "FAILURE"),
    ];
    for (index, selected) in failures.iter().enumerate() {
        assert!(
            on_ci_failure_detected(
                &db,
                &publisher,
                &fix_checker(),
                &candidate,
                &probe(pr, "head"),
                &failures
            )
            .await
        );
        let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
        let revision = attempt.revision_task_id.as_deref().unwrap();
        assert!(task(&db, revision).name.contains(&selected.name));
        assert!(!task(&db, revision).name.contains(&failures[1 - index].name));
        assert_eq!(
            attempt.failed_checks,
            encode_failed_checks(std::slice::from_ref(selected))
        );
        assert!(
            !on_ci_failure_detected(
                &db,
                &publisher,
                &fix_checker(),
                &candidate,
                &probe(pr, "head"),
                &failures
            )
            .await
        );
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), (index + 1) as i64);
        db.mark_ci_remediation_succeeded(&attempt.id, None).unwrap();
        close_superseded_moot_revision(&db, &chore, Some(revision), "handled");
    }
}

#[tokio::test]
async fn mixed_infrastructure_and_real_failures_select_the_real_failure() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "mixed failures", pr);
    let real = failure("ci/build", "FAILURE");
    assert!(
        on_ci_failure_detected(
            &db,
            &RecordingPublisher::default(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head"),
            &[failure("ci/start", "STARTUP_FAILURE"), real.clone()]
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    assert_eq!(attempt.attempt_kind, "fix");
    assert_eq!(attempt.failed_checks, encode_failed_checks(std::slice::from_ref(&real)));
    let title = task(&db, attempt.revision_task_id.as_deref().unwrap()).name;
    assert!(title.contains(&real.name));
    assert!(!title.contains("ci/start"));
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

#[tokio::test]
async fn retrigger_remains_once_per_head_when_first_failure_changes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3277";
    let (product, chore) = make_in_review(&db, "retrigger identity", pr);
    let publisher = RecordingPublisher::default();
    let candidate = candidate(&product, &chore, pr);
    let failures = [failure("ci/a", "STARTUP_FAILURE"), failure("ci/b", "STARTUP_FAILURE")];
    assert!(
        on_ci_failure_detected(
            &db,
            &publisher,
            &fix_checker(),
            &candidate,
            &probe(pr, "head"),
            &failures
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    db.mark_ci_remediation_retriggered(&attempt.id).unwrap();
    on_ci_failure_detected(
        &db,
        &publisher,
        &fix_checker(),
        &candidate,
        &probe(pr, "head"),
        &failures[1..],
    )
    .await;
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}
