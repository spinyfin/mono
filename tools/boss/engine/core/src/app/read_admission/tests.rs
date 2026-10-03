use super::*;

#[tokio::test]
async fn limits_queue_deadline_and_release_are_bounded() {
    let gate = Arc::new(ReadAdmission::new(2, 1, 8, Duration::from_millis(30)));
    let a = gate.connection();
    let b = gate.connection();
    let first = gate.enqueue(&a).unwrap().acquire().await.unwrap();
    // One connection cannot consume the second global slot, even while
    // that slot is free. A different connection can still use it.
    let start = std::time::Instant::now();
    assert_eq!(gate.enqueue(&a).unwrap().acquire().await.unwrap_err(), BUSY);
    assert!(start.elapsed() < Duration::from_secs(1));
    let second = gate.enqueue(&b).unwrap().acquire().await.unwrap();
    drop(first);
    drop(second);
    assert!(gate.enqueue(&a).unwrap().acquire().await.is_ok());
    let queued: Vec<_> = (0..4).map(|_| gate.enqueue(&a).unwrap()).collect();
    assert!(matches!(gate.enqueue(&a), Err(BUSY)));
    drop(queued);
    assert!(gate.enqueue(&a).is_ok());
}

#[tokio::test]
async fn global_queue_and_concurrency_apply_across_connections() {
    let gate = Arc::new(ReadAdmission::new(1, 2, 2, Duration::from_millis(30)));
    let active = gate.enqueue(&gate.connection()).unwrap().acquire().await.unwrap();
    let first = gate.enqueue(&gate.connection()).unwrap();
    let second = gate.enqueue(&gate.connection()).unwrap();
    assert!(matches!(gate.enqueue(&gate.connection()), Err(BUSY)));
    assert_eq!(first.acquire().await.unwrap_err(), BUSY);
    assert_eq!(second.acquire().await.unwrap_err(), BUSY);
    drop(active);
    assert!(gate.enqueue(&gate.connection()).unwrap().acquire().await.is_ok());
}

#[test]
fn live_status_and_worker_writes_do_not_use_bulk_lane() {
    use boss_protocol::FrontendRequest as R;
    assert!(!is_bulk_read(&R::ListWorkerLiveStates));
    assert!(!is_bulk_read(&R::ListTmuxWorkerStatuses));
    assert!(!is_bulk_read(&R::SubmitProposal {
        run_id: "run".into(),
        kind: boss_protocol::ProposalKind::FollowupTask,
        payload: serde_json::json!({}),
        idempotency_key: None,
    }));
    assert!(is_bulk_read(&R::ListProposals {
        run_id: "run".into(),
        kind: None,
        state: None,
    }));
}
