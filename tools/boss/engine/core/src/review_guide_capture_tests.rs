use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn divergent_probe(existing: bool) {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "divergent probe");
    let url = format!("https://github.com/acme/widget/pull/{}", if existing { 81 } else { 82 });
    let mut packet = crate::test_support::source_capture_packet(&url, "rest-base", "head");
    if existing {
        db.persist_pr_review_guide_source_capture(&root, 0, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap();
    }
    packet.probe_base_sha = Some("probe-base".to_owned());
    let calls = Arc::new(AtomicUsize::new(0));
    let metadata_calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let output = packet.clone();
    let collect: PacketCollectFn = Arc::new(move |_, observed, _, metadata| {
        count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(observed.unwrap().base_sha, "probe-base");
        assert_eq!(metadata.unwrap().base_sha, "rest-base");
        let packet = output.clone();
        Box::pin(async move { Ok(packet) })
    });
    let mut collector = SourcePacketCollector::fixture_with_metadata_base(collect, packet, "rest-base".to_owned());
    let metadata = collector.metadata.clone();
    let count = metadata_calls.clone();
    collector.metadata = Arc::new(move |url, branch| {
        count.fetch_add(1, Ordering::SeqCst);
        metadata(url, branch)
    });
    let flags = Arc::new(FeatureFlagsStore::new(_dir.path().join("flags.toml")));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    let request = || {
        SourceCaptureRequest::builder()
            .root_task_id(root.clone())
            .pr_url(url.clone())
            .trigger(PrSourceCaptureTrigger::Poller)
            .observed(PinnedComparison {
                base_sha: "probe-base".to_owned(),
                head_sha: "head".to_owned(),
            })
            .build()
    };
    reconcile_review_guide_source_with_collector(db.clone(), flags.clone(), request(), collector.clone())
        .expect("new probe must resolve REST identity")
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(!existing));
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
    let stored = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert_eq!(stored.packet.observed_base_sha, "rest-base");
    if !existing {
        assert_eq!(stored.packet.probe_base_sha.as_deref(), Some("probe-base"));
    }
    assert_eq!(
        db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_review_guide_source_comparisons", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(reconcile_review_guide_source_with_collector(db, flags, request(), collector).is_none());
    assert_eq!(
        metadata_calls.load(Ordering::SeqCst),
        1,
        "repeat probe must not spawn metadata fetch"
    );
}

#[tokio::test]
async fn divergent_probe_reuses_complete_rest_comparison() {
    divergent_probe(true).await;
}

#[tokio::test]
async fn divergent_probe_collects_and_persists_rest_identity() {
    divergent_probe(false).await;
}
use crate::feature_flags::FeatureFlagsStore;
use crate::test_support::{create_active_chore, create_product, open_db};
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

#[test]
fn unpolled_future_releases_its_claim_and_coalesces_sequences() {
    let key = ("cancel-test".to_owned(), "base".to_owned(), "head".to_owned());
    let guard = InFlightGuard::acquire(key.clone(), 1).unwrap();
    assert!(InFlightGuard::acquire(key.clone(), 4).is_none());
    assert_eq!(*guard.sequence.lock().unwrap(), 4);
    let future = async move {
        let _guard = guard;
    };
    drop(future);
    assert!(InFlightGuard::acquire(key, 5).is_some());
}

#[test]
fn automatic_capture_defaults_off_and_respects_the_rollout_flag() {
    let directory = tempfile::tempdir().unwrap();
    let flags = FeatureFlagsStore::new(directory.path().join("feature-flags.toml"));
    flags.load().unwrap();
    assert!(!source_capture_enabled(&flags));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    assert!(source_capture_enabled(&flags));
}

#[test]
fn disabled_reconciler_does_not_allocate_or_spawn_collection() {
    let (_directory, work_db) = open_db();
    let work_db = Arc::new(work_db);
    let flag_directory = tempfile::tempdir().unwrap();
    let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
    reconcile_review_guide_source_with_collector(
        work_db.clone(),
        flags,
        SourceCaptureRequest::builder()
            .root_task_id("unreachable-root")
            .pr_url("https://github.com/acme/widget/pull/25")
            .trigger(PrSourceCaptureTrigger::Creation)
            .observed(PinnedComparison {
                base_sha: "base".to_owned(),
                head_sha: "head".to_owned(),
            })
            .build(),
        github_source_packet_collector(),
    );
    assert_eq!(
        work_db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
        1
    );
}

#[tokio::test]
async fn execution_reconciler_persists_a_revision_capture_on_its_canonical_root() {
    let (_directory, work_db) = open_db();
    let work_db = Arc::new(work_db);
    let product = create_product(&work_db);
    let root = create_active_chore(&work_db, &product, "root source capture");
    let pr_url = "https://github.com/acme/widget/pull/25";
    work_db
        .update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
    let revision = work_db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root.clone())
                .description("refresh the implementation")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let execution = work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let flag_directory = tempfile::tempdir().unwrap();
    let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    let packet = SourcePacket {
        schema_version: 3,
        canonical_pr_url: pr_url.to_owned(),
        pr_number: 25,
        title: "Captured revision".to_owned(),
        body: None,
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        observed_base_sha: "base".to_owned(),
        probe_base_sha: None,
        merge_base_sha: "merge-base".to_owned(),
        head_sha: "head".to_owned(),
        files: Vec::new(),
        omissions: Vec::new(),
    };
    let fixture_packet = packet.clone();
    let collect: PacketCollectFn = Arc::new(move |url, observed, expected_head_branch, _metadata| {
        let packet = packet.clone();
        Box::pin(async move {
            let Some(observed) = observed else {
                anyhow::bail!("execution reconciler supplied unexpected comparison identity");
            };
            if url != packet.canonical_pr_url || observed.base_sha != "base" || observed.head_sha != "head" {
                anyhow::bail!("execution reconciler supplied unexpected comparison identity");
            }
            if expected_head_branch.is_some() {
                anyhow::bail!("revision capture must not require an implementation branch name");
            }
            Ok(packet)
        })
    });
    let collector = SourcePacketCollector::fixture(collect, fixture_packet);
    let handle = reconcile_review_guide_source_for_execution_with_collector(
        work_db.clone(),
        flags,
        &execution.id,
        pr_url,
        PrSourceCaptureTrigger::Completion,
        Some(PinnedComparison {
            base_sha: "base".to_owned(),
            head_sha: "head".to_owned(),
        }),
        collector,
    )
    .expect("enabled execution reconciliation must start collection");
    handle.await.unwrap();

    let capture = work_db
        .get_latest_pr_review_guide_source_capture(&root)
        .unwrap()
        .expect("capture persisted on canonical root");
    assert_eq!(capture.trigger, "completion");
    assert_eq!(capture.packet.head_sha, "head");
}
