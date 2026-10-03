use super::*;
use crate::app::{ServerState, handle_frontend_connection};
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope, ProposalKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn call(state: Arc<ServerState>, request: FrontendRequest, pid: Option<libc::pid_t>) -> FrontendEvent {
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, pid));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let envelope = FrontendRequestEnvelope {
        request_id: "burst".into(),
        payload: request,
    };
    write
        .write_all(format!("{}\n", serde_json::to_string(&envelope).unwrap()).as_bytes())
        .await
        .unwrap();
    let response = loop {
        let line = lines.next_line().await.unwrap().unwrap();
        let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
        if event.request_id.as_deref() == Some("burst") {
            break event.payload;
        }
    };
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_and_thirty_two_readers_preserve_worker_write_and_agents_latency() {
    for count in [8, 32] {
        let (state, dir) = crate::app::tests::test_server_state();
        let product = crate::test_support::create_test_product(&state.work_db);
        let chore = crate::test_support::create_test_chore(&state.work_db, product.id, "Burst fixture");
        let execution = crate::test_support::create_ready_chore_execution(&state.work_db, chore.id.clone());
        state
            .work_db
            .create_review_batch(
                crate::work::ReviewBatchCreateInput::builder()
                    .cycle_root_id(chore.id.clone())
                    .base_sha("base")
                    .classification(
                        boss_protocol::ReviewClassification::builder()
                            .changed_files(vec!["src/lib.rs".into()])
                            .complexity_flags(vec![])
                            .has_production_code(true)
                            .metadata_missing(vec![])
                            .production_languages(vec![boss_protocol::ReviewLanguageBucket::Rust])
                            .profile(boss_protocol::ReviewProfile::Light)
                            .subsystem_buckets(vec!["src".into()])
                            .build(),
                    )
                    .phase(boss_protocol::ReviewBatchPhase::PreMerge)
                    .pr_number(42)
                    .pr_url("https://github.com/example/repo/pull/42")
                    .target_sha("head")
                    .build(),
                &[crate::work::ReviewBatchMemberCreateInput::builder()
                    .attempt(1)
                    .provider_effort("medium")
                    .requested_driver("claude")
                    .resolved_model("test-model")
                    .role(boss_protocol::ReviewBatchMemberRole::Supervisor)
                    .status(boss_protocol::ReviewBatchMemberStatus::Pending)
                    .build()],
            )
            .unwrap();
        let pid = std::process::id() as libc::pid_t;
        state.worker_registry.register(pid, execution.id.clone());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let barrier = Arc::new(tokio::sync::Barrier::new(count + 1));
        let mut readers = Vec::new();
        for _ in 0..count {
            let (state, stop, barrier) = (state.clone(), stop.clone(), barrier.clone());
            let (run, task, path) = (execution.id.clone(), chore.id.clone(), dir.path().join("state.db"));
            readers.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut completed = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) || completed == 0 {
                    // Exact direct-DB inspection path used by review batches,
                    // including a fresh connection for each CLI invocation.
                    let (path, task) = (path.clone(), task.clone());
                    tokio::task::spawn_blocking(move || {
                        let db = crate::work::WorkDb::open_read_only(path).unwrap();
                        let root = db.resolve_review_cycle_root(&task).unwrap();
                        for batch in db.review_batches_for_cycle_root(&root).unwrap() {
                            db.review_batch_members(&batch.id).unwrap();
                        }
                    })
                    .await
                    .unwrap();
                    let event = call(
                        state.clone(),
                        FrontendRequest::ListProposals {
                            run_id: run.clone(),
                            kind: None,
                            state: None,
                        },
                        Some(pid),
                    )
                    .await;
                    assert!(matches!(event, FrontendEvent::ProposalsList { .. }), "{event:?}");
                    completed += 1;
                    if completed == 1 {
                        barrier.wait().await;
                    }
                }
                completed
            }));
        }
        barrier.wait().await;
        barrier.wait().await;
        let start = std::time::Instant::now();
        let write = tokio::time::timeout(
            Duration::from_secs(2),
            call(
                state.clone(),
                FrontendRequest::SubmitProposal {
                    run_id: execution.id,
                    kind: ProposalKind::FollowupTask,
                    payload: serde_json::json!({"proposed_name":"N", "proposed_description":"D", "rationale":"R"}),
                    idempotency_key: None,
                },
                Some(pid),
            ),
        )
        .await
        .expect("worker write exceeded two seconds");
        assert!(matches!(write, FrontendEvent::ProposalSubmitted { .. }), "{write:?}");
        let write_ms = start.elapsed().as_millis();
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(2), async {
            assert!(matches!(
                call(state.clone(), FrontendRequest::ListWorkerLiveStates, None).await,
                FrontendEvent::WorkerLiveStatesList { .. }
            ));
            assert!(matches!(
                call(state.clone(), FrontendRequest::ListTmuxWorkerStatuses, None).await,
                FrontendEvent::TmuxWorkerStatusesList { .. }
            ));
        })
        .await
        .expect("agents list exceeded two seconds");
        eprintln!(
            "{count}-way burst: worker write {write_ms}ms, agents list {}ms (bound 2000ms)",
            start.elapsed().as_millis()
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for reader in readers {
            assert!(reader.await.unwrap() > 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_reads_return_busy_over_the_wire_while_status_works() {
    let (state, _dir) = crate::app::tests::test_server_state();
    // Hold every real admission slot, deterministically modelling slow reads.
    let mut held = Vec::new();
    for _ in 0..state.read_admission.active.available_permits() {
        held.push(
            state
                .read_admission
                .enqueue(&state.read_admission.connection())
                .unwrap()
                .acquire()
                .await
                .unwrap(),
        );
    }
    let start = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        call(state.clone(), FrontendRequest::ListProducts, None),
    )
    .await
    .expect("over-limit read hung");
    assert!(matches!(response, FrontendEvent::Error { message } if message == BUSY));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        call(state.clone(), FrontendRequest::ListWorkerLiveStates, None).await,
        FrontendEvent::WorkerLiveStatesList { .. }
    ));
    // A queued bulk request must not head-of-line block live work on the
    // very same connection. Correlation permits the status reply to pass it.
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    for (id, payload) in [
        ("bulk", FrontendRequest::ListProducts),
        ("live", FrontendRequest::ListWorkerLiveStates),
    ] {
        let request = FrontendRequestEnvelope {
            request_id: id.into(),
            payload,
        };
        write
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();
    }
    let mut replies = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while replies.len() < 2 {
            let event: FrontendEventEnvelope =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = event.request_id {
                replies.push((id, event.payload));
            }
        }
    })
    .await
    .expect("pipelined reads hung");
    assert_eq!(replies[0].0, "live");
    assert!(matches!(&replies[1].1, FrontendEvent::Error { message } if message == BUSY));
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_socket_pipelining_sixteen_bulk_reads_never_sees_busy() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let count = 16;
    for index in 0..count {
        let payload = match index % 4 {
            0 => FrontendRequest::ListProducts,
            1 => FrontendRequest::GetSettings,
            2 => FrontendRequest::ListLiveStatusDisabledSlots,
            _ => FrontendRequest::ListHosts,
        };
        let request = FrontendRequestEnvelope {
            request_id: format!("pipelined-{index}"),
            payload,
        };
        write
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();
    }
    let mut replies = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while replies < count {
            let event: FrontendEventEnvelope =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if event.request_id.is_some() {
                assert!(
                    !matches!(&event.payload, FrontendEvent::Error { message } if message == BUSY),
                    "pipelined read rejected as busy: {event:?}"
                );
                replies += 1;
            }
        }
    })
    .await
    .expect("pipelined reads hung");
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}
