use super::*;

#[tokio::test]
async fn background_children_recheck_does_not_probe_after_worker_resumes() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db, detector);
    let probe = WatermarkedDescendantProbe::new("before-stop");
    let handler = handler.with_background_activity_probe(probe.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::BackgroundChildrenPending { .. }
    ));
    probe.set_watermark("new-worker-hook");

    assert_eq!(handler.recheck_background_nudge(&execution_id).await, None);
    assert!(
        probes.snapshot().is_empty(),
        "a resumed worker must not receive a recurring probe"
    );
    assert!(handler.pending_background_nudge_execution_ids().is_empty());
}
