use super::*;
use crate::test_support::*;
use crate::work::{
    ConflictInventoryLimits, ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState, WorkItemPatch,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct NoAutomaticRebase;
crate::stub_cube_client! { NoAutomaticRebase {
    async fn ensure_repo(&self, _: &str) -> anyhow::Result<boss_host_execution::types::CubeRepoHandle> {
        panic!("the lifetime guard must run before the mechanical ladder")
    }
} }

#[derive(Default)]
struct Actions {
    closes: AtomicUsize,
    merges: AtomicUsize,
    fail: AtomicBool,
}

#[async_trait]
impl DecisionActions for Actions {
    async fn close(&self, _: &str) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "GitHub unavailable");
        Ok(())
    }
}

#[async_trait]
impl crate::merge_when_ready::DirectMergeExecutor for Actions {
    async fn execute(&self, _: &str) -> Result<crate::merge_when_ready::DirectMergeSuccess> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "GH013: repository rule violations");
        Ok(crate::merge_when_ready::DirectMergeSuccess {
            head_sha: "fixed".into(),
        })
    }
}

fn fixture(choice: &str) -> (WorkDb, String, String) {
    let db = WorkDb::open(":memory:".into()).unwrap();
    let product = create_test_product_with_repo(&db, "inventory", Some("git@github.com:foo/bar.git"));
    let task = create_test_chore_manual(&db, product.id.clone(), "inventory");
    let url = "https://github.com/foo/bar/pull/1";
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let input = ConflictResolutionInsertInput {
        product_id: product.id,
        work_item_id: task.id.clone(),
        pr_url: url.into(),
        pr_number: 1,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: None,
        head_sha_before: None,
    };
    let attempt = db.insert_conflict_resolution(input.clone()).unwrap().unwrap();
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("old-fix"))
        .unwrap();
    let (question, group, _) = db
        .stop_long_conflicting_pr(
            &input,
            ConflictInventoryLimits {
                resolutions: 1,
                days: 7,
            },
        )
        .unwrap()
        .unwrap();
    db.answer_attention(&question.id, Some(choice.into()), false, false)
        .unwrap();
    db.action_attention_group_as_actor(
        &group.id,
        false,
        &Default::default(),
        &FakePrStateChecker::always(PrOpenState::Open),
        "human",
    )
    .unwrap();
    (db, task.id, question.source_anchor.unwrap())
}

#[tokio::test]
async fn close_is_explicit_retries_failure_and_never_marks_the_stop_clean() {
    let (db, _, stopped) = fixture("Close");
    let actions = std::sync::Arc::new(Actions::default());
    actions.fail.store(true, Ordering::SeqCst);
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert_eq!(db.pending_conflict_inventory_actions().unwrap().len(), 1);
    assert!(
        db.get_conflict_resolution(&stopped)
            .unwrap()
            .unwrap()
            .conflict_diagnosis
            .unwrap()
            .contains("GitHub unavailable")
    );
    actions.fail.store(false, Ordering::SeqCst);
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert_eq!(actions.closes.load(Ordering::SeqCst), 2);
    assert_eq!(actions.merges.load(Ordering::SeqCst), 0);
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
    assert_eq!(
        db.get_conflict_resolution(&stopped).unwrap().unwrap().status,
        "abandoned"
    );
}

#[tokio::test]
async fn merge_waits_for_resolution_and_revisions_then_uses_the_checked_merge() {
    let (db, root, _) = fixture("Merge now");
    let actions = std::sync::Arc::new(Actions::default());
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert_eq!(actions.merges.load(Ordering::SeqCst), 0);
    let approved = db.active_conflict_resolution_for_work_item(&root).unwrap().unwrap();
    db.mark_conflict_resolution_succeeded(&approved.id, Some("fixed"))
        .unwrap();
    db.clear_chore_blocked_merge_conflict(&root, &approved.pr_url).unwrap();
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert_eq!(
        actions.merges.load(Ordering::SeqCst),
        0,
        "unfinished revision blocks merging"
    );
    let revision = approved.revision_task_id.unwrap();
    db.update_work_item(
        &revision,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::PrReconcileRequested,
    ));
    actions.fail.store(true, Ordering::SeqCst);
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert!(
        db.list_attention_items_for_work_item(&root)
            .unwrap()
            .iter()
            .any(|item| item.kind == crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND)
    );
    actions.fail.store(false, Ordering::SeqCst);
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    assert!(matches!(
        events.recv().await,
        Some(boss_event_bus::Event::PrReconcileRequested { .. })
    ));
    assert_eq!(actions.merges.load(Ordering::SeqCst), 2);
    assert_eq!(actions.closes.load(Ordering::SeqCst), 0);
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
}

#[tokio::test]
async fn trunk_merge_uses_the_existing_durable_queue_submission_path() {
    let (db, root, _) = fixture("Merge now");
    let approved = db.active_conflict_resolution_for_work_item(&root).unwrap().unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE products SET merge_mechanism = 'trunk_queue' WHERE id = ?1",
            [&approved.product_id],
        )
        .unwrap();
    db.mark_conflict_resolution_succeeded(&approved.id, Some("fixed"))
        .unwrap();
    db.update_work_item(
        approved.revision_task_id.as_deref().unwrap(),
        WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let actions = std::sync::Arc::new(Actions::default());
    run(&db, &RecordingPublisher::default(), actions.as_ref(), actions.clone()).await;
    let intent = db.get_active_trunk_merge_intent(&root).unwrap().unwrap();
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::work::TRUNK_INTENT_AWAITING_RESUBMIT)
    );
    assert_eq!(intent.submit_count, 0);
    assert_eq!(intent.pr_url, approved.pr_url);
    assert_eq!(
        actions.merges.load(Ordering::SeqCst),
        0,
        "Trunk products never use direct merge"
    );
    assert_eq!(actions.closes.load(Ordering::SeqCst), 0);
    assert!(db.pending_conflict_inventory_actions().unwrap().is_empty());
}

#[test]
fn limits_are_configurable_and_invalid_values_are_refused() {
    let defaults = crate::config::conflict_inventory_limits(|_| None).unwrap();
    assert_eq!((defaults.resolutions, defaults.days), (3, 7));
    let custom =
        crate::config::conflict_inventory_limits(|key| Some(if key.ends_with("DAYS") { "14" } else { "5" }.into()))
            .unwrap();
    assert_eq!((custom.resolutions, custom.days), (5, 14));
    for value in ["0", "invalid", "-1"] {
        assert!(crate::config::conflict_inventory_limits(|_| Some(value.into())).is_err());
    }
}

async fn detect_stopped_conflict(block: Option<&str>) {
    let db = WorkDb::open(":memory:".into()).unwrap();
    let product = create_test_product_with_repo(&db, "threshold", Some("git@github.com:foo/bar.git"));
    let task = create_test_chore_manual(&db, product.id.clone(), "threshold");
    let url = "https://github.com/foo/bar/pull/2";
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    for n in 0..3 {
        let attempt = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product.id.clone(),
                work_item_id: task.id.clone(),
                pr_url: url.into(),
                pr_number: 2,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base".into()),
                head_sha_before: Some(format!("head-{n}")),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_succeeded(&attempt.id, Some("fixed"))
            .unwrap();
    }
    if let Some(reason) = block {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = ?2 WHERE id = ?1",
                rusqlite::params![task.id, reason],
            )
            .unwrap();
    }
    let candidate = crate::work::PendingMergeCheck {
        product_id: product.id.clone(),
        work_item_id: task.id.clone(),
        pr_url: url.into(),
    };
    let probe = crate::merge_poller::PrLifecycleProbe::builder()
        .url(url)
        .state(crate::merge_poller::PrLifecycleState::Open(
            crate::merge_poller::OpenPrStatus {
                mergeability: crate::merge_poller::OpenPrMergeability::Conflict,
                ci: crate::merge_poller::OpenPrCiStatus::InFlight,
            },
        ))
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .head_ref_name("feature")
        .base_ref_name("main")
        .head_ref_oid("latest")
        .build();
    let publisher = RecordingPublisher::default();
    if block.is_none() {
        assert!(
            crate::conflict_watch::on_conflict_detected_with_limits(
                &db,
                &publisher,
                crate::conflict_watch::ConflictRemediationMode::Inline(&NoAutomaticRebase),
                &FakePrStateChecker::always(PrOpenState::Open),
                &candidate,
                &probe,
                Err(anyhow::anyhow!("invalid configured conflict limit"))
            )
            .await
        );
        let WorkItem::Chore(blocked) = db.get_work_item(&task.id).unwrap() else {
            panic!("expected chore")
        };
        assert_eq!(blocked.blocked_reason.as_deref(), Some("merge_conflict"));
        assert!(
            db.active_blocked_signals(&task.id)
                .unwrap()
                .iter()
                .any(|s| s.reason == "merge_conflict")
        );
        db.clear_chore_blocked_merge_conflict(&task.id, url).unwrap();
    }

    assert!(
        crate::conflict_watch::on_conflict_detected(
            &db,
            &publisher,
            Some(&NoAutomaticRebase),
            &FakePrStateChecker::always(PrOpenState::Open),
            &candidate,
            &probe
        )
        .await
    );
    assert!(db.active_conflict_resolution_for_work_item(&task.id).unwrap().is_none());
    assert!(db.open_merge_revisions(&task.id).unwrap().is_empty());
    let groups = db
        .list_attention_groups(&product.id, None, None, Some("question"), None)
        .unwrap();
    assert_eq!(groups.len(), 1);
    let attempt = db.latest_conflict_resolution_for_work_item(&task.id).unwrap().unwrap();
    assert_eq!(attempt.status, "abandoned");
    assert_eq!(
        attempt.failure_reason.as_deref(),
        Some(crate::work::CONFLICT_INVENTORY_STOP_REASON)
    );
    if let Some(reason) = block {
        let task = db.get_work_item(&task.id).unwrap();
        let WorkItem::Chore(task) = task else {
            panic!("expected chore")
        };
        assert_eq!(task.blocked_reason.as_deref(), Some(reason));
    } else {
        assert!(crate::conflict_watch::on_resolved(&db, &publisher, None, &candidate, &[], "MERGEABLE", "CLEAN").await);
        assert!(
            crate::conflict_watch::on_conflict_detected(
                &db,
                &publisher,
                Some(&NoAutomaticRebase),
                &FakePrStateChecker::always(PrOpenState::Open),
                &candidate,
                &probe
            )
            .await
        );
        let WorkItem::Chore(task) = db.get_work_item(&task.id).unwrap() else {
            panic!("expected chore")
        };
        assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
        assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));
        assert_eq!(
            db.list_attention_groups(&product.id, None, None, Some("question"), None)
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn conflict_detection_stops_before_spawning_in_review_and_under_other_blocks() {
    for reason in [None, Some("ci_failure"), Some("review_feedback"), Some("dependency")] {
        detect_stopped_conflict(reason).await;
    }
}
