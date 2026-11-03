use super::*;

#[test]
fn review_verdict_apply_records_cycle_root_advance() {
    let (db, product) = memory_db();
    let root = create_test_chore_manual(&db, product, "Review target");
    let pr = "https://github.com/example/repo/pull/42";
    set_pr_url(&db, &root.id, pr);
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let classification = ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["engine".to_owned()])
        .additions(1)
        .deletions(0)
        .build();
    let (batch, _) = db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(&root.id)
                .base_sha("base-sha")
                .classification(classification)
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url(pr)
                .target_sha("head-sha")
                .build(),
            &[ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("test-model")
                .role(ReviewBatchMemberRole::Supervisor)
                .status(ReviewBatchMemberStatus::Pending)
                .maybe_execution_id(Some(supervisor.id.clone()))
                .build()],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    let payload = format!(
        r#"{{"batch_id":"{}","verdict":{{"batch_id":"{}","pr_url":"{pr}","target_sha":"head-sha","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#,
        batch.id, batch.id
    );
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &payload,
            idempotency_key: "verdict-ledger",
        })
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&supervisor.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'head-sha', ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    crate::test_support::apply_review_verdict(
        &db,
        &outcome.proposal.id,
        &FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha"),
    )
    .unwrap();
    assert_latest(
        &db,
        &root.id,
        "status",
        "advance_cycle_root_to_in_review_in_tx",
        "engine",
    );
}

#[test]
fn review_verdict_apply_records_deletion_signoff_hold() {
    let (db, product) = memory_db();
    let root = create_test_chore_manual(&db, product.clone(), "Deletion signoff target");
    let pr = "https://github.com/example/repo/pull/42";
    set_pr_url(&db, &root.id, pr);
    db.mark_chore_blocked_merge_conflict(&root.id, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product,
            work_item_id: root.id.clone(),
            pr_url: pr.into(),
            pr_number: 42,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha".into()),
            head_sha_before: Some("head-before".into()),
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("head-sha"))
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL, pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root.id, pr],
        )
        .unwrap();
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let classification = ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["engine".to_owned()])
        .additions(1)
        .deletions(0)
        .build();
    let (batch, _) = db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(&root.id)
                .base_sha("base-sha")
                .classification(classification)
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url(pr)
                .target_sha("head-sha")
                .build(),
            &[ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("test-model")
                .role(ReviewBatchMemberRole::Supervisor)
                .status(ReviewBatchMemberStatus::Pending)
                .maybe_execution_id(Some(supervisor.id.clone()))
                .build()],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    let payload = format!(
        r#"{{"batch_id":"{}","verdict":{{"batch_id":"{}","pr_url":"{pr}","target_sha":"head-sha","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#,
        batch.id, batch.id
    );
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &payload,
            idempotency_key: "verdict-signoff",
        })
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&supervisor.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'head-sha', ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    struct DeletingPrChecker {
        inner: FakePrStateChecker,
        deletions: Vec<String>,
    }
    impl PrStateChecker for DeletingPrChecker {
        fn check(&self, pr_url: &str) -> anyhow::Result<PrOpenState> {
            self.inner.check(pr_url)
        }
        fn inspect(&self, pr_url: &str) -> anyhow::Result<crate::work::PrInspect> {
            self.inner.inspect(pr_url)
        }
        fn merged_parent_deletions(
            &self,
            _repo_slug: &str,
            _head_before: &str,
            _base_sha: &str,
            _head_after: &str,
        ) -> Vec<String> {
            self.deletions.clone()
        }
    }
    crate::test_support::apply_review_verdict(
        &db,
        &outcome.proposal.id,
        &DeletingPrChecker {
            inner: FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha"),
            deletions: vec!["`src/lib.rs` — added by a merged parent, removed by this resolution".to_owned()],
        },
    )
    .unwrap();
    assert_latest(
        &db,
        &root.id,
        "status",
        "hold_cycle_root_for_deletion_signoff",
        "engine",
    );
    assert_latest(
        &db,
        &root.id,
        "blocked_reason",
        "hold_cycle_root_for_deletion_signoff",
        "engine",
    );
}
