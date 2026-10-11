use super::*;

fn is_bulk_read(request: &boss_protocol::FrontendRequest) -> bool {
    read_lane(request) == ReadLane::Bulk
}

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

#[test]
fn pr_status_refresh_probe_does_not_hold_a_bulk_slot() {
    use boss_protocol::FrontendRequest as R;
    let status = |refresh| R::GetPrStatus {
        run_id: "run".into(),
        refresh,
    };
    assert!(is_bulk_read(&status(false)));
    assert!(!is_bulk_read(&status(true)));
}

/// The macOS app pipelines its connect and invalidation fan-out (about a
/// dozen bulk reads, plus per-viewer comment reads) over one socket. With the
/// default budget none of it may be rejected, even while one slow read
/// (a large `GetWorkTree`) holds its slot for longer than the wait deadline.
#[tokio::test]
async fn default_budget_admits_app_session_fan_out_behind_a_slow_read() {
    let gate = Arc::new(ReadAdmission::default());
    let connection = gate.connection();
    let slow = gate.enqueue(&connection).unwrap().acquire().await.unwrap();
    let mut held = Vec::new();
    for _ in 0..15 {
        let waiting = gate.enqueue(&connection).expect("fan-out must be queueable");
        held.push(waiting.acquire().await.expect("fan-out read rejected as busy"));
    }
    drop(held);
    drop(slow);
}

#[test]
fn omitted_and_network_reads_use_bulk() {
    use boss_protocol::FrontendRequest as R;
    for request in [
        R::ListOperatorQuestions { id: "task".into() },
        R::ListCoordinatorGuidance { product_id: None },
        R::TrunkStatus,
        R::GitHubAuthStatus,
    ] {
        assert_eq!(read_lane(&request), ReadLane::Bulk);
    }
}

#[test]
fn every_live_variant_has_an_audited_local_only_handler() {
    // Inventory of handlers that read local process state, local SQL, or the
    // local cube socket only. Changing live membership requires this audit.
    let source = include_str!("lane.rs");
    let live = source
        .split("=> ReadLane::Bulk,")
        .nth(1)
        .unwrap()
        .split("=> ReadLane::Live,")
        .next()
        .unwrap();
    let actual: std::collections::BTreeSet<_> = live
        .split("R::")
        .skip(1)
        .map(|s| s.split(|c: char| !c.is_alphanumeric()).next().unwrap())
        .collect();
    let audited = [
        "GetEngineHealth",
        "GetEngineVersion",
        "ListHostedPaneStatuses",
        "ListTmuxWorkerStatuses",
        "ListWorkerLiveStates",
        "MetricsListLive",
        "MetricsShowLive",
        "ProbeStatus",
        "WorkerPoolSummary",
        "WorkspacePoolSummary",
    ]
    .into_iter()
    .collect();
    assert_eq!(actual, audited);
}

#[test]
fn maximum_connection_limits_preserve_reader_headroom() {
    let lane = ReadAdmission::new(32, MAX_PER_CONNECTION, 128, Duration::from_millis(500));
    assert_eq!(
        super::super::session_queue::MAX_SESSION_QUEUE - 2 * lane.outstanding_per_connection() - 1,
        55
    );
}

#[tokio::test]
async fn same_kind_reads_keep_receive_order_even_when_tasks_poll_in_reverse() {
    use boss_protocol::FrontendRequest as R;
    let mut ordering = ReadOrdering::default();
    let first = ordering.reserve(&R::ListProducts).await;
    let mut second = ordering.reserve(&R::ListProducts).await;
    let third = ordering.reserve(&R::ListProducts).await;
    let unrelated = ordering.reserve(&R::GetEngineHealth).await;
    let unrelated_guard = tokio::time::timeout(Duration::from_secs(1), unrelated).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut second)
            .await
            .is_err()
    );
    let first_guard = first.await;
    drop(first_guard);
    let second_guard = second.await;
    let mut third = third;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut third)
            .await
            .is_err()
    );
    drop(second_guard);
    assert!(tokio::time::timeout(Duration::from_secs(1), third).await.is_ok());
    drop(unrelated_guard);
}

#[tokio::test]
async fn expired_ordering_wait_does_not_release_predecessor_or_leak_capacity() {
    use boss_protocol::FrontendRequest as R;
    let gate = Arc::new(ReadAdmission::new(2, 2, 8, Duration::from_millis(20)));
    let connection = gate.connection();
    let mut ordering = ReadOrdering::default();
    let first = ordering.reserve(&R::ListProducts).await.await;
    let second = ordering.reserve(&R::ListProducts).await;
    assert_eq!(
        gate.enqueue(&connection)
            .unwrap()
            .acquire_ordered(second)
            .await
            .unwrap_err(),
        BUSY
    );
    let mut third = ordering.reserve(&R::ListProducts).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut third)
            .await
            .is_err()
    );
    drop(first);
    assert!(gate.enqueue(&connection).unwrap().acquire_ordered(third).await.is_ok());
}
