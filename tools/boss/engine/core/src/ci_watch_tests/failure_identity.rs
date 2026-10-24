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
        assert_eq!(
            transitioned,
            expected == 3,
            "only genuine exhaustion changes the parent"
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
