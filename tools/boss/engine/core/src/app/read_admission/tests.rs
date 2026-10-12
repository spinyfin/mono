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
fn live_lane_is_exactly_the_audited_local_only_set() {
    use boss_protocol::FrontendRequest as R;
    // Handlers that read local process state, local SQL, or the local cube
    // socket only. Changing live membership requires this audit.
    let live = [
        R::GetEngineHealth,
        R::GetEngineVersion,
        R::ListHostedPaneStatuses,
        R::ListTmuxWorkerStatuses,
        R::ListWorkerLiveStates,
        R::MetricsListLive,
        R::MetricsShowLive { name: "n".into() },
        R::ProbeStatus { probe_id: "p".into() },
        R::WorkerPoolSummary,
        R::WorkspacePoolSummary,
    ];
    let audited: std::collections::BTreeSet<_> = live
        .iter()
        .map(|request| format!("{request:?}").split([' ', '{']).next().unwrap().to_owned())
        .collect();
    let classified: std::collections::BTreeSet<_> = lane::LIVE_VARIANTS.iter().map(|name| name.to_string()).collect();
    assert_eq!(classified, audited);
    for request in &live {
        assert_eq!(read_lane(request), ReadLane::Live, "{request:?}");
    }
    // Network-bound reads must stay out of the live lane.
    for request in [
        R::TrunkStatus,
        R::GitHubAuthStatus,
        R::ListCoordinatorGuidance { product_id: None },
    ] {
        assert_ne!(read_lane(&request), ReadLane::Live, "{request:?}");
    }
}

#[test]
fn connection_settings_accept_upstream_limits_without_clamping() {
    let name = "BOSS_TEST_READ_ADMISSION_LIMIT";
    // SAFETY: this variable name is unique to this test.
    for value in [1024, 4096, 8192] {
        unsafe { std::env::set_var(name, value.to_string()) };
        assert_eq!(setting(name, 2), value);
    }
    unsafe { std::env::remove_var(name) };
}

#[test]
fn default_limits_and_waits_match_upstream_policy() {
    let bulk = ReadAdmission::default();
    let live = ReadAdmission::live();
    assert_eq!(bulk.active.available_permits(), 4096);
    assert_eq!(bulk.per_connection, 4096);
    assert_eq!(live.active.available_permits(), 1024);
    assert_eq!(live.per_connection, 1024);
    for lane in [&bulk, &live] {
        assert_eq!(lane.pending.available_permits(), 100_000);
        assert_eq!(lane.wait, Duration::from_secs(60));
    }
    assert_eq!(ORDER_WAIT, Duration::from_secs(60));
    assert_eq!(reply_headroom(&bulk, &live), (4096 + 1024) * 5 + 1);
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

#[tokio::test(start_paused = true)]
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

/// A slow read must not turn later reads BUSY: a different key proceeds
/// immediately, and an identical read waits past the admission deadline for
/// its turn rather than being rejected.
#[tokio::test(start_paused = true)]
async fn slow_read_does_not_reject_other_keys_or_patient_same_key_reads() {
    use boss_protocol::FrontendRequest as R;
    let gate = Arc::new(ReadAdmission::new(4, 4, 8, Duration::from_millis(20)));
    let connection = gate.connection();
    let mut ordering = ReadOrdering::default();
    let doc = |path: &str| R::GetProductDesignDoc {
        repo_remote_url: "r".into(),
        path: path.into(),
        git_ref: "main".into(),
    };
    let slow = ordering.reserve(&doc("a")).await.await;
    let same = ordering.reserve(&doc("a")).await;
    let other = ordering.reserve(&doc("b")).await;
    assert!(gate.enqueue(&connection).unwrap().acquire_ordered(other).await.is_ok());
    let waiting = gate.enqueue(&connection).unwrap();
    let same_read = tokio::spawn(async move { waiting.acquire_ordered(same).await.is_ok() });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!same_read.is_finished());
    drop(slow);
    assert!(same_read.await.unwrap());
}

#[tokio::test]
async fn completed_keys_are_not_retained() {
    use boss_protocol::FrontendRequest as R;
    let mut ordering = ReadOrdering::default();
    for i in 0..50 {
        let turn = ordering.reserve(&R::GetWorkItem { id: format!("w{i}") }).await.await;
        drop(turn);
    }
    let _last = ordering.reserve(&R::ListProducts).await;
    assert!(ordering.len() <= 2);
}

#[tokio::test]
async fn refresh_options_share_a_view_but_products_remain_concurrent() {
    use boss_protocol::FrontendRequest as R;
    let mut ordering = ReadOrdering::default();
    let listing = |product: &str, refresh| R::ListProductDesignDocs {
        product_id: product.into(),
        refresh,
    };
    let first = ordering.reserve(&listing("a", false)).await.await;
    let mut refresh = ordering.reserve(&listing("a", true)).await;
    let other = ordering.reserve(&listing("b", true)).await;
    let _other = tokio::time::timeout(Duration::from_secs(1), other).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut refresh)
            .await
            .is_err()
    );
    drop(first);
    assert!(tokio::time::timeout(Duration::from_secs(1), refresh).await.is_ok());

    let first = ordering.reserve(&R::GetDriverQuotaUsage { refresh: false }).await.await;
    let mut refresh = ordering.reserve(&R::GetDriverQuotaUsage { refresh: true }).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut refresh)
            .await
            .is_err()
    );
    drop(first);
    assert!(tokio::time::timeout(Duration::from_secs(1), refresh).await.is_ok());
}

#[tokio::test]
async fn work_tree_reads_share_a_turn_regardless_of_fetch_seq() {
    use boss_protocol::FrontendRequest as R;
    let mut ordering = ReadOrdering::default();
    let tree = |product: &str, fetch_seq| R::GetWorkTree {
        product_id: product.into(),
        fetch_seq,
    };
    let first = ordering.reserve(&tree("a", Some(1))).await.await;
    let mut later = ordering.reserve(&tree("a", Some(2))).await;
    let other = ordering.reserve(&tree("b", Some(1))).await;
    let _other = tokio::time::timeout(Duration::from_secs(1), other).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut later)
            .await
            .is_err()
    );
    drop(first);
    assert!(tokio::time::timeout(Duration::from_secs(1), later).await.is_ok());
}
