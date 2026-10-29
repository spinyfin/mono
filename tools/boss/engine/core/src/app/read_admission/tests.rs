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
    assert_eq!(gate.enqueue(&a).unwrap().acquire().await.unwrap_err(), DEADLINE);
    assert!(start.elapsed() < Duration::from_secs(1));
    let second = gate.enqueue(&b).unwrap().acquire().await.unwrap();
    drop(first);
    drop(second);
    assert!(gate.enqueue(&a).unwrap().acquire().await.is_ok());
    let queued: Vec<_> = (0..4).map(|_| gate.enqueue(&a).unwrap()).collect();
    assert!(matches!(gate.enqueue(&a), Err(QUEUE_FULL)));
    drop(queued);
    assert!(gate.enqueue(&a).is_ok());
}

#[tokio::test]
async fn global_queue_and_concurrency_apply_across_connections() {
    let gate = Arc::new(ReadAdmission::new(1, 2, 2, Duration::from_millis(30)));
    let active = gate.enqueue(&gate.connection()).unwrap().acquire().await.unwrap();
    let first = gate.enqueue(&gate.connection()).unwrap();
    let second = gate.enqueue(&gate.connection()).unwrap();
    assert!(matches!(gate.enqueue(&gate.connection()), Err(QUEUE_FULL)));
    assert_eq!(first.acquire().await.unwrap_err(), DEADLINE);
    assert_eq!(second.acquire().await.unwrap_err(), DEADLINE);
    drop(active);
    assert!(gate.enqueue(&gate.connection()).unwrap().acquire().await.is_ok());
}

/// A read that missed its deadline keeps its queue slot until the caller
/// has queued the busy reply and dropped the rejection. The reader loop's
/// headroom reserve counts outstanding replies through these slots, so
/// freeing one early would let it admit another request whose reply lands
/// on top of the reserve.
#[tokio::test]
async fn rejected_read_holds_its_queue_slot_until_dropped() {
    let gate = Arc::new(ReadAdmission::new(1, 1, 1, Duration::from_millis(30)));
    let connection = gate.connection();
    let active = gate.enqueue(&connection).unwrap().acquire().await.unwrap();
    let rejected = gate.enqueue(&connection).unwrap().acquire().await.unwrap_err();
    assert_eq!(rejected, DEADLINE);
    assert!(
        matches!(gate.enqueue(&connection), Err(QUEUE_FULL)),
        "the slot must stay occupied while the busy reply is still unqueued"
    );
    drop(rejected);
    assert!(
        gate.enqueue(&connection).is_ok(),
        "dropping the rejection frees the slot"
    );
    drop(active);
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
        R::GetEnginePoolConfig,
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
        assert_eq!(lane.wait, Duration::from_secs(60));
    }
    assert_eq!(reply_headroom(&bulk, &live), MAX_REPLY_RESERVE);
    assert_eq!(MAX_REPLY_RESERVE, 128);
    let small = ReadAdmission::new(2, 2, 8, Duration::from_secs(1));
    assert_eq!(reply_headroom(&small, &small), 2 * 5 * 2 + 1);
}

// Acquired permits have no expiry; only waiting for admission has a deadline.
#[tokio::test(start_paused = true)]
async fn active_permits_do_not_expire_after_the_admission_deadline() {
    let gate = Arc::new(ReadAdmission::new(2, 2, 8, Duration::from_millis(20)));
    let connection = gate.connection();
    let slow = gate.enqueue(&connection).unwrap().acquire().await.unwrap();
    tokio::time::sleep(Duration::from_secs(61)).await;
    let later = gate.enqueue(&connection).unwrap().acquire().await.unwrap();
    assert_eq!(gate.active.available_permits(), 0);
    drop(later);
    drop(slow);
    assert_eq!(gate.active.available_permits(), 2);
}

#[test]
fn execution_options_do_not_change_diagnostic_view_identity() {
    use boss_protocol::FrontendRequest as R;
    let tree = |delta, fetch_seq| R::GetWorkTree {
        snapshot_pages: false,
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
