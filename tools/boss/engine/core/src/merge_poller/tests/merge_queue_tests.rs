use super::*;

/// GitHub's GraphQL API returns `reason` in lowercase snake_case
/// ("failed_checks") even though the schema documents the enum as
/// FAILED_CHECKS.  The parser must accept the lowercase form.
#[test]
fn parse_dequeue_event_nodes_accepts_lowercase_failed_checks() {
    // Replay of mono#2692's timeline: the PR head precedes the failed queue
    // synthetic commit in the ordered connection.
    let nodes = serde_json::json!([
        {"__typename": "PullRequestCommit", "commit": {"oid": "0d9dab33152f553bfe6072ccbd6f33fad7dd9d2d"}},
        {"__typename": "RemovedFromMergeQueueEvent", "reason": "failed_checks", "beforeCommit": {"oid": "e005ddece2a840a944f5ffdfa7bad640d54af9c1"}}
    ]);
    let events = parse_dequeue_event_nodes(nodes.as_array().unwrap());
    assert_eq!(events.len(), 1, "lowercase 'failed_checks' must be surfaced");
    assert_eq!(events[0].reason, "failed_checks");
    assert_eq!(
        events[0].pr_head_oid.as_deref(),
        Some("0d9dab33152f553bfe6072ccbd6f33fad7dd9d2d")
    );
    assert_eq!(
        events[0].before_commit_oid.as_deref(),
        Some("e005ddece2a840a944f5ffdfa7bad640d54af9c1")
    );
}

/// The schema-documented uppercase form must also be accepted for
/// forward-compatibility (in case GitHub normalises casing in future).
#[test]
fn parse_dequeue_event_nodes_accepts_uppercase_failed_checks() {
    let nodes = serde_json::json!([
        {"__typename": "HeadRefForcePushedEvent", "afterCommit": {"oid": "forced-head"}},
        {"__typename": "RemovedFromMergeQueueEvent", "reason": "FAILED_CHECKS", "beforeCommit": {"oid": "def456abc789"}}
    ]);
    let events = parse_dequeue_event_nodes(nodes.as_array().unwrap());
    assert_eq!(events.len(), 1, "uppercase 'FAILED_CHECKS' must also be surfaced");
    assert_eq!(events[0].pr_head_oid.as_deref(), Some("forced-head"));
    assert_eq!(events[0].before_commit_oid.as_deref(), Some("def456abc789"));
}

/// Every departure reason must reach the intent reconciler. The CI-remediation
/// path itself filters to `failed_checks`, so manual/conflict removals clear
/// Merging without incorrectly minting a CI revision.
#[test]
fn parse_dequeue_event_nodes_surfaces_all_departure_reasons() {
    let nodes = serde_json::json!([
        {"reason": "dequeued",       "beforeCommit": {"oid": "sha1"}},
        {"reason": "merge_conflict", "beforeCommit": {"oid": "sha2"}},
        {"reason": "queue_cleared",  "beforeCommit": {"oid": "sha3"}},
        {"reason": "failed_checks",  "beforeCommit": {"oid": "sha4"}}
    ]);
    let events = parse_dequeue_event_nodes(nodes.as_array().unwrap());
    assert_eq!(
        events.len(),
        4,
        "every dequeue reason is evidence for intent retirement"
    );
    assert_eq!(events[0].reason, "dequeued");
    assert_eq!(events[1].reason, "merge_conflict");
    assert_eq!(events[2].reason, "queue_cleared");
    assert_eq!(events[3].before_commit_oid.as_deref(), Some("sha4"));
}

/// `beforeCommit` can be null when GitHub omits it. The event must
/// still be returned (with `before_commit_oid = None`) so the caller
/// can decide how to handle it.
#[test]
fn parse_dequeue_event_nodes_handles_null_before_commit() {
    let nodes = serde_json::json!([
        {"reason": "failed_checks", "beforeCommit": null}
    ]);
    let events = parse_dequeue_event_nodes(nodes.as_array().unwrap());
    assert_eq!(events.len(), 1, "null beforeCommit must not drop the event");
    assert!(events[0].before_commit_oid.is_none());
    assert!(events[0].pr_head_oid.is_none());
}

/// The parser snapshots the head at each dequeue rather than applying a
/// later force-push retroactively to an older queue failure.
#[test]
fn parse_dequeue_event_nodes_attributes_each_event_to_its_contemporaneous_head() {
    let nodes = serde_json::json!([
        {"__typename": "PullRequestCommit", "commit": {"oid": "head-before"}},
        {"__typename": "RemovedFromMergeQueueEvent", "reason": "failed_checks", "beforeCommit": {"oid": "queue-1"}},
        {"__typename": "HeadRefForcePushedEvent", "afterCommit": {"oid": "head-after"}},
        {"__typename": "RemovedFromMergeQueueEvent", "reason": "failed_checks", "beforeCommit": {"oid": "queue-2"}}
    ]);
    let events = parse_dequeue_event_nodes(nodes.as_array().unwrap());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].pr_head_oid.as_deref(), Some("head-before"));
    assert_eq!(events[1].pr_head_oid.as_deref(), Some("head-after"));
}

/// An empty nodes array returns an empty vec without panicking.
#[test]
fn parse_dequeue_event_nodes_empty_nodes() {
    let nodes = serde_json::json!([]);
    assert!(parse_dequeue_event_nodes(nodes.as_array().unwrap()).is_empty());
}

// ----- fetch_merge_queue_dequeue_events_batch's query/response plumbing -----
//
// The batch fetcher reuses `build_batch_query` / `walk_batch_response`
// (already covered by the PR-probe batching tests above) with
// `DEQUEUE_EVENTS_FIELDS` as the selection set — these tests pin that
// the fields constant produces the expected query shape and that a
// batched response walks back out to the right per-URL node.

#[test]
fn build_batch_query_with_dequeue_events_fields_requests_timeline_items() {
    let urls = vec!["https://github.com/acme/widgets/pull/1".to_owned()];
    let mut parsed: HashMap<String, (String, String, u64)> = HashMap::new();
    parsed.insert(urls[0].clone(), ("acme".to_owned(), "widgets".to_owned(), 1));

    let (query, alias_map) = build_batch_query(&urls, &parsed, DEQUEUE_EVENTS_FIELDS);

    assert_eq!(alias_map.len(), 1);
    assert!(
        query.contains(boss_gh_telemetry::RATE_LIMIT_SELECTION),
        "quota reading must ride along for free — and must be the FULL block, \
         since `cost` is what attributes the spend to this query rather than \
         leaving it inferable from the gap between two `remaining` readings"
    );
    assert!(query.contains("REMOVED_FROM_MERGE_QUEUE_EVENT"));
    assert!(query.contains("createdAt"));
    assert!(query.contains("PULL_REQUEST_COMMIT"));
    assert!(query.contains("HEAD_REF_FORCE_PUSHED_EVENT"));
    assert!(
        !query.contains("statusCheckRollup"),
        "must not pull in the PR-probe field set"
    );
}

#[test]
fn walk_batch_response_locates_timeline_items_per_pr() {
    let alias_map: BatchAliasMap = vec![(
        "repo0".to_owned(),
        vec![("pr0".to_owned(), "https://github.com/acme/widgets/pull/1".to_owned())],
    )];
    let body = serde_json::json!({
        "data": {
            "rateLimit": { "remaining": 4321 },
            "repo0": {
                "pr0": {
                    "timelineItems": {
                        "nodes": [
                            {"__typename": "PullRequestCommit", "commit": {"oid": "head9"}},
                            {"__typename": "RemovedFromMergeQueueEvent", "reason": "failed_checks", "beforeCommit": {"oid": "sha9"}}
                        ]
                    }
                }
            }
        }
    });
    let walked = walk_batch_response(&body, &alias_map);
    assert_eq!(walked.len(), 1);
    let (_, node) = &walked[0];
    let nodes = node.unwrap()["timelineItems"]["nodes"].as_array().cloned().unwrap();
    let events = parse_dequeue_event_nodes(&nodes);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pr_head_oid.as_deref(), Some("head9"));
    assert_eq!(events[0].before_commit_oid.as_deref(), Some("sha9"));
}

/// GitHub's squash-mode queue commit is ancestry-diverged from the PR head
/// even when it tested that exact head. An unchanged PR-head probe must keep
/// the queue remediation active and must not let the green PR rollup rewrite
/// the work item's combined CI state to success.
#[tokio::test]
async fn unchanged_pr_head_keeps_queue_failure_active_and_visible() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/2692";
    let (product_id, chore) = make_chore_in_review(&db, "C-queue-squash", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };

    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "pr-head-under-test",
            "synthetic-squash-commit",
            &[],
            &[RequiredCheckFailure {
                name: "buildkite/checks".to_owned(),
                conclusion: "FAILURE".to_owned(),
                target_url: "https://buildkite.com/foo/bar/builds/1".to_owned(),
                provider: CiProvider::Buildkite,
                provider_job_id: None,
            }],
        )
        .await
    );

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "pr-head-under-test",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let active = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("unchanged PR head must keep the queue failure active");
    assert_eq!(active.head_sha_at_trigger, "mq:pr-head-under-test");
    assert_eq!(active.before_commit_sha.as_deref(), Some("synthetic-squash-commit"));
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => {
            assert_eq!(task.status, TaskStatus::InReview);
            assert_eq!(task.ci_required_state.as_deref(), Some("fail"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Pre-transition rows stored the synthetic queue SHA in both
/// `head_sha_at_trigger` and `before_commit_sha`. While a linked revision is
/// still live, the reaper must fail closed (keep the attempt pending) even
/// when the current PR head differs — do not re-mint mid-fix.
#[tokio::test]
async fn legacy_queue_attempt_with_live_revision_stays_pending_when_head_advances() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/legacy-reaper";
    let (product_id, chore) = make_chore_in_review(&db, "C-legacy-reaper", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };

    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "original-pr-head",
            "synthetic-legacy-sha",
            &[],
            &[],
        )
        .await
    );
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("rebounce attempt");
    assert!(
        attempt.revision_task_id.is_some(),
        "detection must link a live fix revision"
    );
    // Rewrite to the pre-transition shape: synthetic SHA in both columns.
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute(
        "UPDATE ci_remediations
            SET head_sha_at_trigger = 'synthetic-legacy-sha',
                before_commit_sha = 'synthetic-legacy-sha'
          WHERE id = ?1",
        rusqlite::params![&attempt.id],
    )
    .unwrap();

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "advanced-pr-head",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let still = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("legacy attempt with live revision must stay pending (fail-closed)");
    assert_eq!(still.id, attempt.id);
    assert_eq!(still.status, "pending");
    assert_eq!(still.head_sha_at_trigger, "synthetic-legacy-sha");
}

/// A legacy synthetic-key row must retire after its linked fix revision
/// reaches `in_review`, the normal concluded state while the parent PR stays
/// open. Retiring releases the synthetic failure pin back to the clean
/// PR-head rollup.
#[tokio::test]
async fn legacy_queue_attempt_with_concluded_revision_retires_and_unpins() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/legacy-concluded";
    let (product_id, chore) = make_chore_in_review(&db, "C-legacy-concluded", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };
    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "original-pr-head",
            "synthetic-legacy-sha",
            &[],
            &[],
        )
        .await
    );
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("rebounce attempt");
    let revision_id = attempt.revision_task_id.as_deref().expect("linked revision");
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute(
        "UPDATE ci_remediations
            SET head_sha_at_trigger = 'synthetic-legacy-sha',
                before_commit_sha = 'synthetic-legacy-sha'
          WHERE id = ?1",
        rusqlite::params![&attempt.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        rusqlite::params![revision_id],
    )
    .unwrap();

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "current-pr-head",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert!(attempts.iter().any(|row| {
        row.id == attempt.id
            && row.status == "abandoned"
            && row.failure_reason.as_deref() == Some("merge_queue_trigger_superseded")
    }));
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => assert_eq!(task.ci_required_state.as_deref(), Some("success")),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// The raw PR-head UNIQUE key is shared with ordinary branch-CI attempts.
/// Rebounces namespace their key so both failure surfaces can be recorded.
#[tokio::test]
async fn rebounce_at_a_head_with_branch_ci_remediation_still_mints_attempt() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/rebounce-collision";
    let (product_id, chore) = make_chore_in_review(&db, "C-rebounce-collision", pr);
    db.insert_ci_remediation(crate::work::CiRemediationInsertInput {
        product_id: product_id.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.to_owned(),
        pr_number: 1,
        head_branch: "feature".to_owned(),
        head_sha_at_trigger: "shared-pr-head".to_owned(),
        attempt_kind: "fix".to_owned(),
        consumes_budget: 1,
        failed_checks: "[]".to_owned(),
        failure_kind: "pr_branch_ci".to_owned(),
        before_commit_sha: None,
    })
    .unwrap()
    .expect("branch CI remediation");
    assert!(
        !db.ci_remediation_exists_for_merge_queue_episode(&chore, "shared-pr-head", "synthetic-queue-sha",)
            .unwrap()
    );
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };
    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "shared-pr-head",
            "synthetic-queue-sha",
            &[],
            &[],
        )
        .await
    );
    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert!(attempts.iter().any(|row| {
        row.failure_kind.as_deref() == Some("merge_queue_rebounce") && row.head_sha_at_trigger == "mq:shared-pr-head"
    }));
}

/// Superseded events must not consume a remediation attempt. A same-head
/// event passes the gate and is then able to mint its rebounce attempt.
#[tokio::test]
async fn dequeue_event_head_gate_skips_superseded_and_admits_current_head() {
    let stale = MergeQueueDequeueEvent {
        reason: "failed_checks".to_owned(),
        created_at: Some("2026-01-01T00:00:00Z".to_owned()),
        pr_head_oid: Some("head-1".to_owned()),
        before_commit_oid: Some("queue-1".to_owned()),
    };
    assert!(!dequeue_event_is_for_current_pr_head(&stale, Some("head-2")));

    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/current-head-gate";
    let (product_id, chore) = make_chore_in_review(&db, "C-current-head-gate", pr);
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
    // Mirror the preceding poll pass, which records the live PR head before
    // the rebounce pass consumes timeline events.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1",
            rusqlite::params![&chore],
        )
        .unwrap();
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id: product_id.clone(),
        pr_url: pr.to_owned(),
    };
    let mut stale_events = HashMap::new();
    stale_events.insert(pr.to_owned(), vec![stale.clone()]);
    let mut outcome = SweepOutcome::default();
    check_merge_queue_rebounce(&db, publisher.as_ref(), &candidate, &stale_events, &mut outcome).await;
    assert!(
        db.list_ci_remediations(None, &[], Some(&chore), None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
    let current = MergeQueueDequeueEvent {
        pr_head_oid: Some("head-2".to_owned()),
        ..stale
    };
    assert!(dequeue_event_is_for_current_pr_head(&current, Some("head-2")));
    let mut current_events = HashMap::new();
    current_events.insert(pr.to_owned(), vec![current]);
    check_merge_queue_rebounce(&db, publisher.as_ref(), &candidate, &current_events, &mut outcome).await;
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

/// Empty `failed_checks = "[]"` is an explicitly reachable insert state for
/// rebounce (confirmed failed_checks ejection with no check-run enrichment).
/// The pin path must surface a generic merge-queue failure blob, not `[]`.
#[tokio::test]
async fn queue_failure_detail_falls_back_when_failed_checks_empty() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/empty-checks";
    let (product_id, chore) = make_chore_in_review(&db, "C-empty-checks", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };

    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "pr-head-empty-checks",
            "synthetic-empty-checks",
            &[],
            &[], // lands failed_checks = "[]"
        )
        .await
    );

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "pr-head-empty-checks",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => {
            assert_eq!(task.ci_required_state.as_deref(), Some("fail"));
            let detail = task
                .ci_required_detail
                .as_deref()
                .expect("queue failure must write ci_required_detail");
            let parsed: serde_json::Value = serde_json::from_str(detail).expect("valid JSON");
            assert_eq!(
                parsed,
                serde_json::json!([{"name": "merge queue", "conclusion": "FAILURE"}]),
                "empty failed_checks must fall back to the generic merge-queue blob"
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// When a queue attempt retires after a head advance and the new head's PR
/// rollup is clean, `ci_required_state` must un-pin from `fail` back to the
/// rollup value (success) — the operator-visible half of the pin.
#[tokio::test]
async fn advanced_pr_head_with_clean_rollup_unpins_ci_required_state() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/2650-unpin";
    let (product_id, chore) = make_chore_in_review(&db, "C-queue-unpin", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };

    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            publisher.as_ref(),
            &candidate,
            "original-pr-head",
            "synthetic-queue-commit",
            &[],
            &[RequiredCheckFailure {
                name: "ci/test".to_owned(),
                conclusion: "FAILURE".to_owned(),
                target_url: "https://buildkite.com/foo/bar/builds/1".to_owned(),
                provider: CiProvider::Buildkite,
                provider_job_id: Some("job-old".to_owned()),
            }],
        )
        .await
    );

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "original-pr-head",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => {
            assert_eq!(task.ci_required_state.as_deref(), Some("fail"));
        }
        other => panic!("expected chore, got {other:?}"),
    }

    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus::clean()),
        "base-head",
        "new-pr-head",
    );
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert!(
        attempts
            .iter()
            .any(|a| a.status == "abandoned" && a.failure_reason.as_deref() == Some("merge_queue_trigger_superseded")),
        "queue attempt must be retired after head advance: {attempts:?}"
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(task) => {
            assert_eq!(
                task.ci_required_state.as_deref(),
                Some("success"),
                "retiring the queue attempt must un-pin ci_required_state to the green PR-head rollup"
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Dual-key episode dedup must treat a pre-transition row (synthetic SHA in
/// `head_sha_at_trigger`) as already handled for the same beforeCommit.
#[test]
fn merge_queue_episode_dedup_matches_legacy_synthetic_key() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/dedup-legacy";
    let (product_id, chore) = make_chore_in_review(&db, "C-dedup-legacy", pr);
    db.insert_ci_remediation(crate::work::CiRemediationInsertInput {
        product_id,
        work_item_id: chore.clone(),
        pr_url: pr.to_owned(),
        pr_number: 1,
        head_branch: "feature".to_owned(),
        head_sha_at_trigger: "synthetic-old-key".to_owned(),
        attempt_kind: "fix".to_owned(),
        consumes_budget: 1,
        failed_checks: "[]".to_owned(),
        failure_kind: "merge_queue_rebounce".to_owned(),
        before_commit_sha: Some("synthetic-old-key".to_owned()),
    })
    .unwrap()
    .expect("legacy row");

    assert!(
        db.ci_remediation_exists_for_merge_queue_episode(&chore, "current-pr-head", "synthetic-old-key")
            .unwrap(),
        "legacy synthetic key must match via before_commit_sha / head_sha_at_trigger"
    );
    assert!(
        !db.ci_remediation_exists_for_merge_queue_episode(&chore, "current-pr-head", "other-synthetic")
            .unwrap(),
        "unrelated synthetic must not match"
    );
}

// ----- GitHub API quota throttling -----

#[test]
fn merge_queue_state_str_maps_flags() {
    assert_eq!(merge_queue_state_str(true, false), Some("queued"));
    assert_eq!(merge_queue_state_str(true, true), Some("queued"));
    assert_eq!(merge_queue_state_str(false, true), Some("auto_merge_enabled"));
    assert_eq!(merge_queue_state_str(false, false), None);
}

#[test]
fn merge_queue_detail_json_builds_blob_when_queued_with_sub_state() {
    let probe = probe_with_queue_fields(
        true,
        Some("AWAITING_CHECKS"),
        Some(1),
        Some("2026-07-10T11:54:54Z"),
        false,
        None,
    );
    let json = merge_queue_detail_json(&probe).expect("queued with sub-state → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!({
            "position": 1,
            "state": "AWAITING_CHECKS",
            "enqueued_at": "2026-07-10T11:54:54Z",
            "section_order": 1,
        })
    );
}

#[test]
fn merge_queue_detail_json_none_when_neither_queued_nor_auto_merge() {
    let probe = probe_with_queue_fields(false, None, None, None, false, None);
    assert_eq!(merge_queue_detail_json(&probe), None);
}

#[test]
fn merge_queue_detail_json_uses_position_sentinel_when_queued_but_no_sub_state_reported() {
    // Degenerate case: GitHub reported a non-null mergeQueueEntry but none
    // of the sub-fields (shouldn't happen per schema, but the parser
    // degrades gracefully — `section_order` still needs a value so the
    // Merging section can place the card, so this falls back to the
    // "queued, unknown position" sentinel rather than going `None`.
    let probe = probe_with_queue_fields(true, None, None, None, false, None);
    let json = merge_queue_detail_json(&probe).expect("queued → Some, even with no sub-state");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!({
            "position": null,
            "state": null,
            "enqueued_at": null,
            "section_order": QUEUED_NO_POSITION_SECTION_ORDER,
        })
    );
}

#[test]
fn merge_queue_detail_json_builds_blob_when_auto_merge_enabled_not_queued() {
    let probe = probe_with_queue_fields(false, None, None, None, true, Some("2026-07-10T11:54:54Z"));
    let json = merge_queue_detail_json(&probe).expect("auto-merge armed → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!({
            "position": null,
            "state": null,
            "enqueued_at": null,
            "section_order": 1_783_684_494i64,
        })
    );
}

#[test]
fn merge_queue_detail_json_auto_merge_enabled_falls_back_when_enabled_at_missing() {
    let probe = probe_with_queue_fields(false, None, None, None, true, None);
    let json = merge_queue_detail_json(&probe).expect("auto-merge armed → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed["section_order"],
        serde_json::json!(MERGE_WHEN_READY_UNKNOWN_ENABLED_AT_SECTION_ORDER)
    );
}

#[test]
fn merge_queue_detail_json_queued_takes_precedence_over_auto_merge_for_section_order() {
    // A queued PR also has auto-merge armed on GitHub's side (queueing
    // requires it) — the queue position must win, not the enabledAt
    // epoch, so a queued card never sorts into the merge-when-ready
    // bucket.
    let probe = probe_with_queue_fields(true, Some("QUEUED"), Some(3), None, true, Some("2026-07-10T11:54:54Z"));
    let json = merge_queue_detail_json(&probe).expect("queued → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(parsed["section_order"], serde_json::json!(3));
}

// ── renumber_merge_queue (mono#1997: whole-queue renumbering) ──────────

/// Reproduces the reported bug directly: four members whose stored
/// `merge_queue_detail.position` is duplicated (two cards both `#2`)
/// and missing (a clock-icon-but-no-number card) because each was written in isolation by its own
/// probe. A single `renumber_merge_queue` pass must re-derive a unique,
/// contiguous `1..N` ranking across every currently queued member —
/// ordered by `enqueued_at` — and only touch (and only publish a change
/// event for) the rows whose rank actually moved.
#[tokio::test]
async fn renumber_merge_queue_repairs_duplicate_and_missing_positions() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "Renumber", Some("git@github.com:foo/bar.git"));
    let a = chore_in_review_for_product(&db, &product.id, "A", "https://github.com/foo/bar/pull/1");
    let b = chore_in_review_for_product(&db, &product.id, "B", "https://github.com/foo/bar/pull/2");
    let c = chore_in_review_for_product(&db, &product.id, "C", "https://github.com/foo/bar/pull/3");
    let d = chore_in_review_for_product(&db, &product.id, "D", "https://github.com/foo/bar/pull/4");

    seed_queued(&db, &a, Some(1), "2026-07-15T10:00:00Z", "QUEUED");
    seed_queued(&db, &b, Some(2), "2026-07-15T10:01:00Z", "QUEUED");
    // Duplicate of `b`'s position, enqueued later, failed.
    seed_queued(&db, &c, Some(2), "2026-07-15T10:02:00Z", "UNMERGEABLE");
    // Missing position entirely, enqueued last.
    seed_queued(&db, &d, None, "2026-07-15T10:03:00Z", "QUEUED");

    let publisher = RecordingPublisher::default();
    renumber_merge_queue(&db, &publisher, &product.id).await;

    let (_, a_detail) = merge_queue_columns(&db, &a);
    let (_, b_detail) = merge_queue_columns(&db, &b);
    let (_, c_detail) = merge_queue_columns(&db, &c);
    let (_, d_detail) = merge_queue_columns(&db, &d);
    assert_eq!(a_detail["position"], serde_json::json!(1));
    assert_eq!(b_detail["position"], serde_json::json!(2));
    assert_eq!(
        c_detail["position"],
        serde_json::json!(3),
        "duplicate must resolve to a unique rank"
    );
    assert_eq!(
        d_detail["position"],
        serde_json::json!(4),
        "missing position must be filled in"
    );
    // section_order mirrors position so the kanban's sort key and its
    // displayed badge never disagree.
    assert_eq!(a_detail["section_order"], serde_json::json!(1));
    assert_eq!(d_detail["section_order"], serde_json::json!(4));
    // Non-position fields survive the rewrite untouched.
    assert_eq!(c_detail["state"], serde_json::json!("UNMERGEABLE"));
    assert_eq!(c_detail["enqueued_at"], serde_json::json!("2026-07-15T10:02:00Z"));

    // Only the two rows whose rank actually moved get a change event —
    // `a` and `b` were already correct and must not be touched.
    let renumbered: Vec<String> = publisher
        .events
        .lock()
        .await
        .iter()
        .filter(|(_, _, reason)| reason == "merge_queue_renumbered")
        .map(|(_, task_id, _)| task_id.clone())
        .collect();
    assert_eq!(
        renumbered.iter().collect::<std::collections::HashSet<_>>(),
        [&c, &d].into_iter().collect::<std::collections::HashSet<_>>(),
        "only c (duplicate) and d (missing) should have been rewritten; got {renumbered:?}"
    );
}

/// Membership in the renumbered set is governed only by
/// `merge_queue_state == "queued"` — a sibling that isn't queued (e.g.
/// only Merge-When-Ready armed, or plain `in_review` with no queue
/// sub-state at all) must never be touched or counted against the
/// ranking, regardless of whatever is left in its own `merge_queue_detail`.
#[tokio::test]
async fn renumber_merge_queue_ignores_non_queued_members() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "RenumberIgnore", Some("git@github.com:foo/bar.git"));
    let queued = chore_in_review_for_product(&db, &product.id, "Queued", "https://github.com/foo/bar/pull/1");
    let armed = chore_in_review_for_product(&db, &product.id, "Armed", "https://github.com/foo/bar/pull/2");

    // Wrong position on purpose so a real rewrite is observable.
    seed_queued(&db, &queued, Some(5), "2026-07-15T10:00:00Z", "QUEUED");
    db.update_task_pr_poll_state(
        &armed,
        PrPollStateInput {
            ci_required_state: "success",
            review_required_state: "approved",
            merge_queue_state: Some("auto_merge_enabled"),
            merge_queue_detail: Some(r#"{"position":null,"state":null,"enqueued_at":null}"#),
            ..Default::default()
        },
    )
    .unwrap();
    let armed_detail_before = merge_queue_columns(&db, &armed).1;

    let publisher = RecordingPublisher::default();
    renumber_merge_queue(&db, &publisher, &product.id).await;

    let (_, queued_detail) = merge_queue_columns(&db, &queued);
    assert_eq!(
        queued_detail["position"],
        serde_json::json!(1),
        "the sole queued member must become #1"
    );
    let (armed_state, armed_detail_after) = merge_queue_columns(&db, &armed);
    assert_eq!(armed_state.as_deref(), Some("auto_merge_enabled"));
    assert_eq!(
        armed_detail_after, armed_detail_before,
        "a non-queued member's detail must be left untouched by the queued-only renumbering pass"
    );
}

/// Regression (mono#58-shown-for-4): a terminal (`done`/`archived`) task
/// that still carries `merge_queue_state = 'queued'` — an orphan left
/// behind by a terminal transition that predates clearing merge-queue
/// columns — must be excluded from `list_queued_merge_queue_members`'s
/// membership set entirely, so it can never occupy a rank and inflate
/// the live cards' positions, even before any cleanup pass has run.
#[tokio::test]
async fn renumber_merge_queue_excludes_orphaned_terminal_rows() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "RenumberOrphan", Some("git@github.com:foo/bar.git"));
    let live_a = chore_in_review_for_product(&db, &product.id, "LiveA", "https://github.com/foo/bar/pull/1");
    let live_b = chore_in_review_for_product(&db, &product.id, "LiveB", "https://github.com/foo/bar/pull/2");
    let orphan_done = chore_in_review_for_product(&db, &product.id, "OrphanDone", "https://github.com/foo/bar/pull/3");
    let orphan_archived =
        chore_in_review_for_product(&db, &product.id, "OrphanArchived", "https://github.com/foo/bar/pull/4");

    // Fifty-six-orphans-style setup: the orphans were enqueued earliest,
    // so an unguarded query would rank them #1/#2 and push the two live
    // members to #3/#4 — mirroring the reported #58-for-a-4-deep-queue bug.
    seed_queued(&db, &orphan_done, Some(1), "2026-07-15T09:00:00Z", "QUEUED");
    seed_queued(&db, &orphan_archived, Some(2), "2026-07-15T09:01:00Z", "QUEUED");
    seed_queued(&db, &live_a, Some(3), "2026-07-15T10:00:00Z", "QUEUED");
    seed_queued(&db, &live_b, Some(4), "2026-07-15T10:01:00Z", "QUEUED");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            rusqlite::params![orphan_done],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'archived' WHERE id = ?1",
            rusqlite::params![orphan_archived],
        )
        .unwrap();

    let publisher = RecordingPublisher::default();
    renumber_merge_queue(&db, &publisher, &product.id).await;

    let (_, live_a_detail) = merge_queue_columns(&db, &live_a);
    let (_, live_b_detail) = merge_queue_columns(&db, &live_b);
    assert_eq!(
        live_a_detail["position"],
        serde_json::json!(1),
        "the earliest-enqueued LIVE member must become #1, not #3"
    );
    assert_eq!(
        live_b_detail["position"],
        serde_json::json!(2),
        "the second live member must become #2, not #4"
    );
}

/// End-to-end regression through the real wiring: a mid-queue member
/// leaving the queue (fails, GitHub dequeues it) must immediately
/// renumber its still-queued siblings via `update_pr_poll_state` alone
/// — no separate call to `renumber_merge_queue` — reproducing the
/// acceptance scenario (queue of 3+, cause a mid-queue member to
/// fail/leave, remaining cards get unique positions).
#[tokio::test]
async fn mid_queue_member_leaving_renumbers_remaining_siblings_via_update_pr_poll_state() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "RenumberWiring", Some("git@github.com:foo/bar.git"));
    let t1 = chore_in_review_for_product(&db, &product.id, "T1", "https://github.com/foo/bar/pull/1");
    let t2 = chore_in_review_for_product(&db, &product.id, "T2", "https://github.com/foo/bar/pull/2");
    let t3 = chore_in_review_for_product(&db, &product.id, "T3", "https://github.com/foo/bar/pull/3");

    seed_queued(&db, &t1, Some(1), "2026-07-15T10:00:00Z", "MERGEABLE");
    seed_queued(&db, &t2, Some(2), "2026-07-15T10:01:00Z", "MERGEABLE");
    seed_queued(&db, &t3, Some(3), "2026-07-15T10:02:00Z", "MERGEABLE");

    let publisher = RecordingPublisher::default();
    // T2 fails and GitHub dequeues it: still an open, clean-enough PR,
    // but `in_merge_queue` flips false.
    let left_queue_probe = probe_with_queue_fields(false, None, None, None, false, None);
    let candidate = PendingMergeCheck {
        work_item_id: t2.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/2".to_owned(),
    };
    update_pr_poll_state(&db, &publisher, &candidate, &left_queue_probe, None).await;

    let (t2_state, t2_detail) = merge_queue_columns(&db, &t2);
    assert!(t2_state.is_none(), "T2 must leave the queued set once it's dequeued");
    assert_eq!(t2_detail, serde_json::Value::Null);

    let (_, t1_detail) = merge_queue_columns(&db, &t1);
    let (_, t3_detail) = merge_queue_columns(&db, &t3);
    assert_eq!(
        t1_detail["position"],
        serde_json::json!(1),
        "T1 was already correct and stays #1"
    );
    assert_eq!(
        t3_detail["position"],
        serde_json::json!(2),
        "T3 must shift down to #2 once T2 leaves — no gap, no stale #3"
    );

    let renumbered: std::collections::HashSet<String> = publisher
        .events
        .lock()
        .await
        .iter()
        .filter(|(_, _, reason)| reason == "merge_queue_renumbered")
        .map(|(_, task_id, _)| task_id.clone())
        .collect();
    assert!(
        renumbered.contains(&t3),
        "T3's position change must be broadcast so its card refreshes; events={renumbered:?}"
    );
    assert!(
        !renumbered.contains(&t1),
        "T1's position didn't change and must not generate a redundant event"
    );
}

/// Regression (mono#2023): GitHub keeps auto-merge armed while
/// required checks are red, so a naive write of the raw probe would
/// leave `merge_queue_state = Some("auto_merge_enabled")` right next to
/// `ci_required_state = "fail"` — stranding the card in the macOS
/// kanban's Merging section with a contradictory red-CI chip. The row
/// poll must demote `merge_queue_state`/`merge_queue_detail` to `None`
/// whenever it observes failing required CI, even though GitHub's own
/// probe still reports auto-merge armed.
#[tokio::test]
async fn update_pr_poll_state_demotes_merging_when_required_ci_fails() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "CiDemote", Some("git@github.com:foo/bar.git"));
    let t = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/1");

    let mut probe = probe_with_queue_fields(false, None, None, None, true, Some("2026-07-10T11:54:54Z"));
    probe.state = PrLifecycleState::Open(OpenPrStatus::ci_failing(vec![failure("ci/test", "FAILURE")]));
    let candidate = PendingMergeCheck {
        work_item_id: t.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/1".to_owned(),
    };
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(&db, &publisher, &candidate, &probe, None).await;

    let (state, detail) = merge_queue_columns(&db, &t);
    assert!(
        state.is_none(),
        "merge_queue_state must be demoted to None while required CI is failing, even though GitHub auto-merge is still armed"
    );
    assert_eq!(detail, serde_json::Value::Null);

    let conn = db.connect().unwrap();
    let ci_required_state: Option<String> = conn
        .query_row(
            "SELECT ci_required_state FROM tasks WHERE id = ?1",
            rusqlite::params![t],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ci_required_state.as_deref(),
        Some("fail"),
        "ci_required_state must still be stamped normally — only the lane signal is demoted"
    );
}

/// Companion to the demotion regression above: once required CI recovers
/// to success on a later poll, the card must return to Merging on its
/// own — no separate re-arm path — because the demotion only ever
/// overrode the derived value each poll, never GitHub's own arming.
#[tokio::test]
async fn update_pr_poll_state_restores_merging_once_ci_recovers() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "CiRestore", Some("git@github.com:foo/bar.git"));
    let t = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/1");
    let candidate = PendingMergeCheck {
        work_item_id: t.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/1".to_owned(),
    };
    let publisher = RecordingPublisher::default();

    let mut failing_probe = probe_with_queue_fields(false, None, None, None, true, Some("2026-07-10T11:54:54Z"));
    failing_probe.state = PrLifecycleState::Open(OpenPrStatus::ci_failing(vec![failure("ci/test", "FAILURE")]));
    update_pr_poll_state(&db, &publisher, &candidate, &failing_probe, None).await;
    let (demoted_state, _) = merge_queue_columns(&db, &t);
    assert!(
        demoted_state.is_none(),
        "sanity: card must be demoted while CI is failing"
    );

    // GitHub auto-merge is still armed on the next poll — same probe
    // flags — but required CI now reads clean.
    let recovered_probe = probe_with_queue_fields(false, None, None, None, true, Some("2026-07-10T11:54:54Z"));
    update_pr_poll_state(&db, &publisher, &candidate, &recovered_probe, None).await;

    let (state, detail) = merge_queue_columns(&db, &t);
    assert_eq!(
        state.as_deref(),
        Some("auto_merge_enabled"),
        "card must return to Merging once CI recovers, with no separate re-arm path"
    );
    assert_eq!(detail["section_order"], serde_json::json!(1_783_684_494i64));
}

/// Companion to the demotion regression above, scoped to the `"queued"`
/// bucket: `renumber_merge_queue`'s doc comment (below) promises that
/// membership is governed *only* by `merge_queue_state == "queued"` and
/// that a queued-but-failing member keeps its position — its number must
/// never race between "kept" and "excluded" mid-transition. A PR that is
/// genuinely enqueued in GitHub's merge queue can read a failing
/// required check on its own head (GitHub runs required checks against
/// the merge-queue branch), so the CI-fail override introduced above
/// must NOT touch a `"queued"` row — only `"auto_merge_enabled"`.
#[tokio::test]
async fn update_pr_poll_state_leaves_queued_row_untouched_when_required_ci_fails() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "CiQueuedDemote", Some("git@github.com:foo/bar.git"));
    let t = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/1");

    let mut probe = probe_with_queue_fields(
        true,
        Some("UNMERGEABLE"),
        Some(2),
        Some("2026-07-10T11:54:54Z"),
        false,
        None,
    );
    probe.state = PrLifecycleState::Open(OpenPrStatus::ci_failing(vec![failure("ci/test", "FAILURE")]));
    let candidate = PendingMergeCheck {
        work_item_id: t.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/1".to_owned(),
    };
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(&db, &publisher, &candidate, &probe, None).await;

    let (state, detail) = merge_queue_columns(&db, &t);
    assert_eq!(
        state.as_deref(),
        Some("queued"),
        "a queued row must keep its position through a failing required check — only the \
             auto_merge_enabled bucket is demoted, per renumber_merge_queue's invariant"
    );
    // `renumber_merge_queue` re-derives rank across the whole product's
    // queued set (mono#1997), so the sole queued row here renumbers to
    // position 1 regardless of GitHub's raw position — what matters for
    // this regression is that the row stayed `"queued"` at all rather
    // than being demoted to `None`.
    assert_eq!(detail["position"], serde_json::json!(1));
    assert_eq!(detail["state"], serde_json::json!("UNMERGEABLE"));

    let conn = db.connect().unwrap();
    let ci_required_state: Option<String> = conn
        .query_row(
            "SELECT ci_required_state FROM tasks WHERE id = ?1",
            rusqlite::params![t],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ci_required_state.as_deref(),
        Some("fail"),
        "ci_required_state must still be stamped normally"
    );
}

/// Regression: a `trunk_queue`-mechanism task's `merge_queue_state`
/// column is owned by `app::review::handle_trunk_queue_merge`'s
/// optimistic write after a successful Trunk submit, not by this
/// GitHub-facing poller. GitHub always reports
/// `in_merge_queue=false`/`auto_merge_enabled=false` for such a task
/// (it never actually enters GitHub's native queue), so without the
/// `preserve_merge_queue_state` gate every sweep would immediately wipe
/// the optimistic `"queued"` write back to `None` — bouncing the card
/// out of the Merging lane within one poll interval.
#[tokio::test]
async fn update_pr_poll_state_preserves_trunk_owned_merge_queue_state() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "TrunkPreserve", Some("git@github.com:foo/bar.git"));
    db.set_product_merge_mechanism(&product.id, Some("trunk_queue"))
        .unwrap();
    let t = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/1");

    let detail = serde_json::json!({"source": "trunk", "state": "pending"}).to_string();
    db.set_task_merge_queue_state(&t, Some("queued"), Some(&detail))
        .unwrap();

    // A GitHub probe for a trunk_queue task's PR always reads
    // "not in GitHub's native queue, auto-merge not armed" — this is
    // the shape a real sweep would observe.
    let probe = probe_with_queue_fields(false, None, None, None, false, None);
    let candidate = PendingMergeCheck {
        work_item_id: t.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/1".to_owned(),
    };
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(&db, &publisher, &candidate, &probe, None).await;

    let (state, stored_detail) = merge_queue_columns(&db, &t);
    assert_eq!(
        state.as_deref(),
        Some("queued"),
        "a trunk_queue task's optimistic merge_queue_state must survive a GitHub-facing poll sweep"
    );
    assert_eq!(
        stored_detail["source"],
        serde_json::json!("trunk"),
        "the trunk-owned detail blob must not be overwritten by the GitHub probe's (empty) detail"
    );

    // No whole-queue renumbering pass for a trunk_queue product either
    // — that's a GitHub-native-queue concept and does not apply here.
    let renumbered = publisher
        .events
        .lock()
        .await
        .iter()
        .any(|(_, _, reason)| reason == "merge_queue_renumbered");
    assert!(
        !renumbered,
        "a trunk_queue product's rows must never go through GitHub-native queue renumbering"
    );
}

/// Regression: GitHub can answer the first targeted poll after a successful
/// `gh pr merge --auto --squash` before either `mergeQueueEntry` or
/// `autoMergeRequest` has materialized. The durable per-row intent must keep
/// the card in Merging through that empty observation.
#[tokio::test]
async fn update_pr_poll_state_preserves_github_intent_through_empty_first_probe() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "GitHubIntentPreserve", Some("git@github.com:foo/bar.git"));
    let task = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/1");
    let detail = serde_json::json!({
        "position": null,
        "state": null,
        "enqueued_at": null,
        "section_order": QUEUED_NO_POSITION_SECTION_ORDER,
    })
    .to_string();
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(task.clone())
            .pr_url("https://github.com/foo/bar/pull/1")
            .head_sha("head-1")
            .build(),
        &detail,
    )
    .unwrap();

    let candidate = PendingMergeCheck {
        work_item_id: task.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/1".to_owned(),
    };
    let publisher = RecordingPublisher::default();
    let mut empty_probe = probe_with_queue_fields(false, None, None, None, false, None);
    empty_probe.head_ref_oid = Some("head-1".to_owned());
    update_pr_poll_state(&db, &publisher, &candidate, &empty_probe, None).await;

    let (state, stored_detail) = merge_queue_columns(&db, &task);
    assert_eq!(state.as_deref(), Some("queued"));
    assert_eq!(
        stored_detail["section_order"],
        serde_json::json!(QUEUED_NO_POSITION_SECTION_ORDER)
    );
    assert!(
        db.has_active_github_merge_intent(&task, &candidate.pr_url).unwrap(),
        "an empty observation is not a dequeue"
    );
}

/// A timeline dequeue for the submitted head is the affirmative evidence the
/// empty-probe guard needs before it returns the card to Review.
#[tokio::test]
async fn observed_github_dequeue_retires_intent_and_clears_merging() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "GitHubIntentDequeue", Some("git@github.com:foo/bar.git"));
    let task = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/2");
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(task.clone())
            .pr_url("https://github.com/foo/bar/pull/2")
            .head_sha("head-2")
            .build(),
        "{}",
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: task.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/2".to_owned(),
    };
    let mut events = HashMap::new();
    events.insert(
        candidate.pr_url.clone(),
        vec![MergeQueueDequeueEvent {
            reason: "manual_removal".to_owned(),
            created_at: Some("2100-01-01T00:00:00Z".to_owned()),
            pr_head_oid: Some("head-2".to_owned()),
            before_commit_oid: None,
        }],
    );
    let publisher = RecordingPublisher::default();
    clear_github_merge_intent_on_observed_dequeue(&db, &publisher, &candidate, &events).await;

    let (state, detail) = merge_queue_columns(&db, &task);
    assert!(state.is_none());
    assert_eq!(detail, serde_json::Value::Null);
    assert!(!db.has_active_github_merge_intent(&task, &candidate.pr_url).unwrap());
    assert!(
        publisher
            .events
            .lock()
            .await
            .iter()
            .any(|(_, item_id, reason)| item_id == &task && reason == "github_merge_intent_dequeued"),
        "the authoritative dequeue must notify the push-only app"
    );
}

/// A historical dequeue at the same head must not retire a newly recorded
/// intent when GitHub returns both attempts in its bounded timeline window.
#[tokio::test]
async fn historical_github_dequeue_does_not_retire_newer_intent() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "GitHubIntentStaleDequeue", Some("git@github.com:foo/bar.git"));
    let task = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/3");
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(task.clone())
            .pr_url("https://github.com/foo/bar/pull/3")
            .head_sha("head-3")
            .build(),
        "{}",
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: task.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/3".to_owned(),
    };
    let mut events = HashMap::new();
    events.insert(
        candidate.pr_url.clone(),
        vec![MergeQueueDequeueEvent {
            reason: "manual_removal".to_owned(),
            created_at: Some("1970-01-01T00:00:00Z".to_owned()),
            pr_head_oid: Some("head-3".to_owned()),
            before_commit_oid: None,
        }],
    );
    let publisher = RecordingPublisher::default();
    clear_github_merge_intent_on_observed_dequeue(&db, &publisher, &candidate, &events).await;

    assert!(db.has_active_github_merge_intent(&task, &candidate.pr_url).unwrap());
    assert_eq!(merge_queue_columns(&db, &task).0.as_deref(), Some("queued"));
}

/// A merge request is pinned to its observed head, so a later head must
/// retire the intent and allow an empty lifecycle probe to clear the lane.
#[tokio::test]
async fn update_pr_poll_state_retires_github_intent_when_head_advances() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "GitHubIntentHeadAdvance", Some("git@github.com:foo/bar.git"));
    let task = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/4");
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(task.clone())
            .pr_url("https://github.com/foo/bar/pull/4")
            .head_sha("head-4")
            .build(),
        "{}",
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: task.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/4".to_owned(),
    };
    let mut probe = probe_with_queue_fields(false, None, None, None, false, None);
    probe.head_ref_oid = Some("head-5".to_owned());
    let publisher = RecordingPublisher::default();
    update_pr_poll_state(&db, &publisher, &candidate, &probe, None).await;

    assert!(!db.has_active_github_merge_intent(&task, &candidate.pr_url).unwrap());
    assert!(merge_queue_columns(&db, &task).0.is_none());
}

/// Terminal PR observations for human-driven rows clear a stale Merging lane
/// and notify the board even though they do not complete the row.
#[tokio::test]
async fn mark_merged_clears_human_driven_merging_lane_and_publishes() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "HumanMergedLane", Some("git@github.com:foo/bar.git"));
    let task = chore_in_review_for_product(&db, &product.id, "T", "https://github.com/foo/bar/pull/5");
    db.update_work_item(
        &task,
        WorkItemPatch {
            human_driven: Some(true),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.set_task_merge_queue_state(&task, Some("queued"), Some("{}"))
        .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: task.clone(),
        product_id: product.id.clone(),
        pr_url: "https://github.com/foo/bar/pull/5".to_owned(),
    };
    let publisher = RecordingPublisher::default();
    let probe = probe_with_queue_fields(false, None, None, None, false, None);

    assert!(!mark_merged(&db, &publisher, None, &candidate, &probe).await);
    assert!(merge_queue_columns(&db, &task).0.is_none());
    assert!(
        publisher
            .events
            .lock()
            .await
            .iter()
            .any(|(_, item_id, reason)| item_id == &task && reason == "pr_merged_queue_state_cleared")
    );
}

#[test]
fn is_trunk_queue_product_true_for_trunk_queue_mechanism() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "TrunkMechanism", Some("git@github.com:foo/bar.git"));
    db.set_product_merge_mechanism(&product.id, Some("trunk_queue"))
        .unwrap();

    assert!(is_trunk_queue_product(&db, &product.id));
}

#[test]
fn is_trunk_queue_product_false_for_direct_mechanism() {
    let (_dir, db) = open_db();
    let product = create_test_product_with_repo(&db, "DirectMechanism", Some("git@github.com:foo/bar.git"));

    assert!(!is_trunk_queue_product(&db, &product.id));
}

#[test]
fn is_trunk_queue_product_false_for_unknown_product() {
    let (_dir, db) = open_db();

    assert!(!is_trunk_queue_product(&db, "does-not-exist"));
}
