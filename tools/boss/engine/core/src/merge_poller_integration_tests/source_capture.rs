use super::*;

fn enabled_source_capture_handler(
    db: Arc<WorkDb>,
    collector: crate::review_guide_capture::SourcePacketCollector,
) -> WorkerCompletionHandler {
    let flags_dir = tempfile::tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags
        .set(crate::review_guide_capture::REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true)
        .unwrap();
    WorkerCompletionHandler::new(
        db,
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        Arc::new(RecordingPublisher::default()),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
        crate::completion_adapters::completion_collaborators(),
    )
    .with_feature_flags(flags)
    .with_source_packet_collector(collector)
}

#[tokio::test]
async fn open_probe_does_not_recollect_an_unchanged_complete_comparison() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/2";
    let (_pid, chore_id) = make_chore_in_review(&db, "C-source", pr);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler = enabled_source_capture_handler(
        db.clone(),
        counting_source_collector(calls.clone(), source_capture_packet(pr, "base-1", "head-1")),
    );
    let probe = StubProbe::new();
    probe.set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    let publisher = Arc::new(RecordingPublisher::default());

    let _ = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;
    wait_for_source_capture(&db, &chore_id).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let _ = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a second Open probe with unchanged SHAs must not invoke the collector"
    );
    let capture = db
        .get_latest_pr_review_guide_source_capture(&chore_id)
        .unwrap()
        .unwrap();
    assert_eq!(capture.trigger, "poller");
    assert_eq!(capture.packet.head_sha, "head-1");
    let second = source_capture_packet(pr, "base-2", "head-2");
    let second_handler = enabled_source_capture_handler(db.clone(), counting_source_collector(calls.clone(), second));
    probe.set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-2", "head-2");
    run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&second_handler),
        Some(&core_host()),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if db
                .get_latest_pr_review_guide_source_capture(&chore_id)
                .unwrap()
                .unwrap()
                .packet
                .head_sha
                == "head-2"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    probe.set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture(&chore_id)
            .unwrap()
            .unwrap()
            .packet
            .head_sha,
        "head-1"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn open_probe_without_head_ref_oid_persists_nothing() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/3";
    let (_pid, chore_id) = make_chore_in_review(&db, "C-missing-head", pr);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler = enabled_source_capture_handler(
        db.clone(),
        counting_source_collector(calls.clone(), source_capture_packet(pr, "base-1", "head-1")),
    );
    let probe = StubProbe::new();
    probe.set_with_base(pr, PrLifecycleState::Open(OpenPrStatus::clean()), Some("base-1"));
    let publisher = Arc::new(RecordingPublisher::default());
    let _ = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        db.get_latest_pr_review_guide_source_capture(&chore_id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn disabled_source_capture_flag_does_not_allocate_poller_sequences() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/4";
    let (_pid, _chore_id) = make_chore_in_review(&db, "C-flag-off", pr);
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        Arc::new(RecordingPublisher::default()),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
        crate::completion_adapters::completion_collaborators(),
    );
    let probe = StubProbe::new();
    probe.set_with_base_head(pr, PrLifecycleState::Open(OpenPrStatus::clean()), "base-1", "head-1");
    let publisher = Arc::new(RecordingPublisher::default());
    let _ = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;
    assert_eq!(db.allocate_pr_review_guide_source_observation_sequence().unwrap(), 1);
}
