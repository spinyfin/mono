use super::*;

fn is_bulk_read(request: &boss_protocol::FrontendRequest) -> bool {
    read_lane(request) == ReadLane::Bulk
}

/// A pending future that must not have completed within `wait`.
async fn still_pending<T>(task: &mut tokio::task::JoinHandle<T>, wait: Duration) {
    tokio::time::sleep(wait).await;
    assert!(!task.is_finished(), "must still be waiting");
}

/// Limits bound concurrency and queue depth, but a read that hits either
/// waits for its turn — past the slow-wait threshold, which only logs —
/// and is never rejected.
#[tokio::test(start_paused = true)]
async fn limits_and_full_queues_make_reads_wait_instead_of_rejecting() {
    let gate = Arc::new(ReadAdmission::new(2, 1, 8, Duration::from_millis(30)));
    let a = gate.connection();
    let b = gate.connection();
    let first = gate.enqueue(&a).await.acquire().await;
    // One connection cannot consume the second global slot, even while
    // that slot is free: its second read waits. A different connection can
    // still use the slot.
    let mut second_on_a = tokio::spawn({
        let (gate, a) = (gate.clone(), a.clone());
        async move { gate.enqueue(&a).await.acquire().await }
    });
    still_pending(&mut second_on_a, Duration::from_millis(200)).await;
    let second = gate.enqueue(&b).await.acquire().await;
    still_pending(&mut second_on_a, Duration::from_millis(50)).await;
    drop(first);
    let second_on_a = (&mut second_on_a).await.unwrap();
    drop(second);
    drop(second_on_a);
    drop(gate.enqueue(&a).await.acquire().await);
    // The connection queue holds four pending reads; a fifth waits for a
    // slot rather than being rejected, and gets one as soon as any frees.
    let mut queued = Vec::new();
    for _ in 0..4 {
        queued.push(gate.enqueue(&a).await);
    }
    let mut fifth = tokio::spawn({
        let (gate, a) = (gate.clone(), a.clone());
        async move { gate.enqueue(&a).await }
    });
    still_pending(&mut fifth, Duration::from_millis(200)).await;
    drop(queued.pop());
    let fifth = (&mut fifth).await.unwrap();
    drop(fifth);
    drop(queued);
}

#[tokio::test(start_paused = true)]
async fn global_queue_and_concurrency_apply_across_connections() {
    let gate = Arc::new(ReadAdmission::new(1, 2, 2, Duration::from_millis(30)));
    let active = gate.enqueue(&gate.connection()).await.acquire().await;
    let first = gate.enqueue(&gate.connection()).await;
    let second = gate.enqueue(&gate.connection()).await;
    let mut third = tokio::spawn({
        let gate = gate.clone();
        async move { gate.enqueue(&gate.connection()).await }
    });
    still_pending(&mut third, Duration::from_millis(200)).await;
    let mut first = tokio::spawn(first.acquire());
    let mut second = tokio::spawn(second.acquire());
    still_pending(&mut first, Duration::from_millis(200)).await;
    assert!(!second.is_finished());
    drop(active);
    let first = (&mut first).await.unwrap();
    assert!(!second.is_finished());
    drop(first);
    let second = (&mut second).await.unwrap();
    // Admitted reads release their queue slot, so the third's wait ends.
    let third = (&mut third).await.unwrap();
    drop(second);
    drop(third.acquire().await);
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
    let slow = gate.enqueue(&connection).await.acquire().await;
    let mut held = Vec::new();
    for _ in 0..15 {
        let waiting = tokio::time::timeout(Duration::from_secs(1), gate.enqueue(&connection))
            .await
            .expect("fan-out must be queueable");
        held.push(
            tokio::time::timeout(Duration::from_secs(1), waiting.acquire())
                .await
                .expect("fan-out read held up behind the slow read"),
        );
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
fn settings_accept_large_per_connection_limits() {
    let name = "BOSS_TEST_READ_ADMISSION_LIMIT";
    // SAFETY: this variable name is unique to this test.
    for value in [1024, 4096, 8192] {
        unsafe { std::env::set_var(name, value.to_string()) };
        assert_eq!(setting(name, 2), value);
    }
    unsafe { std::env::remove_var(name) };
}

#[test]
fn default_limits_waits_and_reply_headroom() {
    let bulk = ReadAdmission::default();
    let live = ReadAdmission::live();
    assert_eq!(bulk.active.available_permits(), 4096);
    assert_eq!(bulk.per_connection, 4096);
    assert_eq!(live.active.available_permits(), 1024);
    assert_eq!(live.per_connection, 1024);
    for lane in [&bulk, &live] {
        assert_eq!(lane.pending.available_permits(), 100_000);
        assert_eq!(lane.slow_wait, Duration::from_secs(60));
    }
    assert_eq!(reply_headroom(&bulk, &live), MAX_REPLY_RESERVE);
    assert_eq!(MAX_REPLY_RESERVE, 128);
    let small = ReadAdmission::new(2, 2, 8, Duration::from_secs(1));
    assert_eq!(reply_headroom(&small, &small), 2 * 5 * 2 + 1);
}

// Acquired permits have no expiry, and admission has no deadline either.
#[tokio::test(start_paused = true)]
async fn active_permits_do_not_expire() {
    let gate = Arc::new(ReadAdmission::new(2, 2, 8, Duration::from_millis(20)));
    let connection = gate.connection();
    let slow = gate.enqueue(&connection).await.acquire().await;
    tokio::time::sleep(Duration::from_secs(61)).await;
    let later = gate.enqueue(&connection).await.acquire().await;
    assert_eq!(gate.active.available_permits(), 0);
    drop(later);
    drop(slow);
    assert_eq!(gate.active.available_permits(), 2);
}

#[test]
fn execution_options_do_not_change_diagnostic_view_identity() {
    use boss_protocol::FrontendRequest as R;
    let tree = |delta, fetch_seq| R::GetWorkTree {
        product_id: "a".into(),
        delta,
        fetch_seq,
    };
    assert_eq!(key_id(&tree(false, None)), key_id(&tree(true, Some(2))));
    assert_eq!(
        key_id(&R::GetDriverQuotaUsage { refresh: false }),
        key_id(&R::GetDriverQuotaUsage { refresh: true })
    );
}
