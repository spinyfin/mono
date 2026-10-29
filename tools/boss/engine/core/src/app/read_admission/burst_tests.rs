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

const BOUND: Duration = Duration::from_secs(2);

/// Run `work` against `bound` and report the elapsed milliseconds alongside
/// the (possibly timed-out) result. The task stays owned by the caller, so a
/// missed bound leaves it running and `drain` can still join it.
async fn timed_within<T>(
    work: &mut tokio::task::JoinHandle<T>,
    bound: Duration,
) -> (Option<Result<T, tokio::task::JoinError>>, u128) {
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(bound, &mut *work).await.ok();
    (result, start.elapsed().as_millis())
}

/// Join a task that `timed_within` may have left running, so no server-side
/// work outlives the fixture's temp dir. Returns whether it met its bound.
/// A genuine hang is left to Bazel's test timeout rather than detaching work.
async fn drain<T>(
    work: tokio::task::JoinHandle<T>,
    early: Option<Result<T, tokio::task::JoinError>>,
) -> (bool, Result<T, tokio::task::JoinError>) {
    match early {
        Some(result) => (true, result),
        None => (false, work.await),
    }
}

async fn list_proposals(state: Arc<ServerState>, run_id: String, pid: libc::pid_t) -> FrontendEvent {
    call(
        state,
        FrontendRequest::ListProposals {
            run_id,
            kind: None,
            state: None,
        },
        Some(pid),
    )
    .await
}

/// Eight, then 32, simultaneous CLI readers must not delay a worker's
/// proposal write, the two status reads behind `agents list`, or a board
/// read.
///
/// Each reader models one `bossctl review batches` process. It opens its
/// own read-only connection and runs the review-batch queries inside a
/// read transaction that stays open across the measured window (a CLI
/// process mid-inspection, holding its WAL snapshot), and it sends one
/// socket `ListProposals` at the same instant as the measured requests.
/// The burst is deliberately a bounded set of in-flight reads: an open-ended
/// loop of fresh read-only connections saturates the blocking pool and
/// starves the runtime workers of CPU, so the bounds would measure CPU
/// scheduling instead of serialization. The bounds should trip only on a
/// real serialization: a reader holding the engine's writer connection, or
/// the write waiting out SQLite's busy timeout behind the readers.
///
/// Every reader, held inspection and measured call is joined before anything
/// is asserted (a call that misses its bound keeps running and is drained),
/// so nothing can still touch `state.db` once the fixture (and its temp dir)
/// is dropped, whatever the measured outcome.
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
        // Three rendezvous: start, ready (every inspection holds its
        // snapshot and every reader has completed a warm round), go (the
        // burst and the measured requests are released together).
        let barrier = Arc::new(tokio::sync::Barrier::new(count + 1));
        let mut inspections = Vec::new();
        let mut releases = Vec::new();
        let mut readers = Vec::new();
        for _ in 0..count {
            let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            releases.push(release_tx);
            let (path, task) = (dir.path().join("state.db"), chore.id.clone());
            // Exact direct-DB inspection path used by review batches,
            // including a fresh connection for each CLI invocation. The
            // queries run inside one read transaction that is held open
            // until the measured window has closed.
            inspections.push(tokio::task::spawn_blocking(move || {
                let db = crate::work::WorkDb::open_read_only(path)?;
                db.connect()?.execute_batch("BEGIN")?;
                let root = db.resolve_review_cycle_root(&task)?;
                let mut members = 0;
                for batch in db.review_batches_for_cycle_root(&root)? {
                    members += db.review_batch_members(&batch.id)?.len();
                }
                let _ = held_tx.send(());
                let _ = release_rx.blocking_recv();
                db.connect()?.execute_batch("COMMIT")?;
                anyhow::Ok(members)
            }));
            let (state, barrier, run) = (state.clone(), barrier.clone(), execution.id.clone());
            readers.push(tokio::spawn(async move {
                barrier.wait().await;
                // A failed inspection or a panicking request is captured as a
                // value and reported at the join; the reader still keeps every
                // rendezvous so the main task is never left at a barrier.
                let held = held_rx.await.is_ok();
                let warm = tokio::spawn(list_proposals(state.clone(), run.clone(), pid)).await;
                barrier.wait().await;
                barrier.wait().await;
                let burst = tokio::spawn(list_proposals(state, run, pid)).await;
                (held, warm, burst)
            }));
        }
        barrier.wait().await;
        barrier.wait().await;
        barrier.wait().await;
        // Owned tasks, so a missed bound leaves them to be drained below.
        let mut write_task = tokio::spawn(call(
            state.clone(),
            FrontendRequest::SubmitProposal {
                run_id: execution.id.clone(),
                kind: ProposalKind::FollowupTask,
                payload: serde_json::json!({"proposed_name":"N", "proposed_description":"D", "rationale":"R"}),
                idempotency_key: None,
            },
            Some(pid),
        ));
        let mut reads_task = tokio::spawn({
            let (state, product_id) = (state.clone(), chore.product_id.clone());
            async move {
                let live = call(state.clone(), FrontendRequest::ListWorkerLiveStates, None).await;
                let tmux = call(state.clone(), FrontendRequest::ListTmuxWorkerStatuses, None).await;
                let tree = call(
                    state,
                    FrontendRequest::GetWorkTree {
                        product_id,
                        fetch_seq: None,
                        delta: true,
                    },
                    None,
                )
                .await;
                (live, tmux, tree)
            }
        });
        let (write, reads) = tokio::join!(
            timed_within(&mut write_task, BOUND),
            timed_within(&mut reads_task, BOUND)
        );
        // Release the held snapshots and join everything that can touch
        // the database before judging the window: a failed bound must
        // not let a reader outlive the fixture's temp dir.
        for release in releases {
            let _ = release.send(());
        }
        let mut outcomes = Vec::new();
        for reader in readers {
            outcomes.push(reader.await);
        }
        let mut inspected = Vec::new();
        for inspection in inspections {
            inspected.push(inspection.await);
        }
        let (write, write_ms) = write;
        let (write_in_bound, write) = drain(write_task, write).await;
        let (reads, reads_ms) = reads;
        let (reads_in_bound, reads) = drain(reads_task, reads).await;
        assert!(write_in_bound, "worker write exceeded two seconds");
        let write = write.unwrap();
        assert!(matches!(write, FrontendEvent::ProposalSubmitted { .. }), "{write:?}");
        assert!(reads_in_bound, "app status and board reads exceeded two seconds");
        let (live, tmux, tree) = reads.unwrap();
        assert!(matches!(live, FrontendEvent::WorkerLiveStatesList { .. }), "{live:?}");
        assert!(matches!(tmux, FrontendEvent::TmuxWorkerStatusesList { .. }), "{tmux:?}");
        assert!(matches!(tree, FrontendEvent::WorkTreeUpdate { .. }), "{tree:?}");
        for outcome in outcomes {
            let (held, warm, burst) = outcome.unwrap();
            assert!(held, "inspection never reached its held snapshot");
            let warm = warm.expect("warm proposal read panicked");
            let burst = burst.expect("burst proposal read panicked");
            assert!(matches!(warm, FrontendEvent::ProposalsList { .. }), "{warm:?}");
            assert!(matches!(burst, FrontendEvent::ProposalsList { .. }), "{burst:?}");
        }
        for members in inspected {
            assert_eq!(
                members.unwrap().unwrap(),
                1,
                "inspection must see the fixture's one batch member"
            );
        }
        eprintln!("{count}-way burst: worker write {write_ms}ms, status and board reads {reads_ms}ms (bound 2000ms)");
    }
}

/// A call that misses its bound is reported as missed but stays owned and is
/// run to completion by `drain`, so its work cannot outlive the fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_missing_its_bound_is_drained_to_completion() {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn({
        let finished = finished.clone();
        async move {
            released.await.unwrap();
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            7
        }
    });
    let (early, _) = timed_within(&mut task, Duration::from_millis(20)).await;
    let missed_bound = early.is_none();
    let finished_before_release = finished.load(std::sync::atomic::Ordering::SeqCst);
    let draining = drain(task, early);
    tokio::pin!(draining);
    let drain_pending_before_release = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(draining.as_mut(), cx).is_pending())
    })
    .await;
    let released = release.send(());
    let (in_bound, result) = draining.await;
    assert!(missed_bound, "the bound must trip");
    assert!(!finished_before_release);
    assert!(drain_pending_before_release, "drain must wait for the held call");
    released.unwrap();
    assert!(!in_bound);
    assert_eq!(result.unwrap(), 7);
    assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_reads_return_busy_over_the_wire_while_status_works() {
    // An explicit small budget: the production defaults wait far longer
    // than this test's two-second bound before answering busy.
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        crate::app::ServerStateOverrides::builder()
            .read_admission(Arc::new(ReadAdmission::new(32, 16, 128, Duration::from_millis(500))))
            .build(),
    );
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
    assert!(matches!(response, FrontendEvent::Error { message } if message == DEADLINE));
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
    assert!(matches!(&replies[1].1, FrontendEvent::Error { message } if message == DEADLINE));
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
            2 => FrontendRequest::ListFeatureFlags,
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
                    !matches!(&event.payload, FrontendEvent::Error { .. }),
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

/// No concurrent pushes: every pipelined id gets exactly one BUSY reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_saturation_without_pushes_delivers_every_id_once() {
    socket_saturation_delivers_every_id_once(0).await;
}

/// 300 distinct-topic pushes overflow the bulk lane before 320 replies are
/// queued behind them. Pushes are evicted (behind a resync marker) instead
/// of replies or the session: every id still arrives exactly once and the
/// socket stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socket_saturation_with_distinct_topic_pushes_keeps_replies_and_connection() {
    socket_saturation_delivers_every_id_once(300).await;
}

/// Exercise the socket reader's backpressure with more requests than the
/// outbound queue can hold, after `push_topics` independent topic producers
/// have already overflowed the lane.
///
/// The protocol is ordered so the outcome depends on the engine's
/// accounting alone, never on scheduling:
/// 1. The outbound socket buffer is tiny and the client does not read yet,
///    so the writer blocks within a handful of pushes and every further
///    push lands in the lane. All pushes are published (and awaited) before
///    any request is sent, so no push can arrive once the lane holds only
///    replies.
/// 2. All 320 request frames go down in one write that the inbound buffer
///    must absorb without blocking. The reader is already paused behind the
///    push backlog and nothing drains the outbound side yet, so a write that
///    had to wait for the reader here would deadlock against it.
/// 3. Only then does the client drain. Replies may be delayed but are never
///    evicted, and the connection stays open.
async fn socket_saturation_delivers_every_id_once(push_topics: usize) {
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        crate::app::ServerStateOverrides::builder()
            .read_admission(Arc::new(ReadAdmission::new(1, 32, 128, Duration::from_millis(50))))
            // Small live budget too: reader headroom is derived from both
            // limits, and the production defaults would leave none.
            .live_read_admission(Arc::new(ReadAdmission::new(8, 2, 32, Duration::from_millis(250))))
            .build(),
    );
    let held = state
        .read_admission
        .enqueue(&state.read_admission.connection())
        .unwrap()
        .acquire()
        .await
        .unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    use std::os::fd::AsRawFd;
    // Inbound (requests): room for the whole pipelined batch, about 21 KB,
    // while the reader is paused. Outbound (pushes and replies): small
    // enough that the writer blocks after a few 2 KB pushes on every
    // platform, which is what makes the lane overflow below a certainty.
    let inbound: libc::c_int = 65_536;
    let outbound: libc::c_int = 8_192;
    for (socket, option, capacity) in [
        (&client, libc::SO_SNDBUF, inbound),
        (&server, libc::SO_RCVBUF, inbound),
        (&server, libc::SO_SNDBUF, outbound),
        (&client, libc::SO_RCVBUF, outbound),
    ] {
        // SAFETY: both descriptors and the integer option buffer remain live.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&capacity as *const libc::c_int).cast(),
                std::mem::size_of_val(&capacity) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "socket buffer: {}", std::io::Error::last_os_error());
    }
    let handler = tokio::spawn(handle_frontend_connection(server, state.clone(), None));
    let session = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(id) = state.topic_broker.inner.lock().await.sinks.keys().next().cloned() {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let topics: Vec<_> = (0..push_topics)
        .map(|i| format!("saturation-{i}-{}", "x".repeat(2048)))
        .collect();
    state.topic_broker.subscribe(&session, &topics).await;
    for topic in &topics {
        state
            .topic_broker
            .publish(
                topic,
                FrontendEventEnvelope::push(FrontendEvent::TopicEvent {
                    topic: topic.clone(),
                    revision: 1,
                    origin_session_id: String::new(),
                    origin_request_id: None,
                    event: boss_protocol::TopicEventPayload::ResyncRequired,
                }),
            )
            .await;
    }
    let (read, write) = client.into_split();
    let batch: String = (0..320)
        .map(|index| {
            let request = FrontendRequestEnvelope {
                request_id: format!("saturated-{index}"),
                payload: FrontendRequest::ListProducts,
            };
            format!("{}\n", serde_json::to_string(&request).unwrap())
        })
        .collect();
    write.writable().await.unwrap();
    let written = write.try_write(batch.as_bytes()).unwrap();
    assert_eq!(
        written,
        batch.len(),
        "the inbound socket buffer must absorb the whole pipelined batch without blocking"
    );
    let mut lines = BufReader::new(read).lines();
    let mut replies = std::collections::HashSet::new();
    let mut pushes = 0usize;
    let mut resync_markers = 0usize;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let env: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
                    match env.request_id {
                        Some(id) => {
                            assert!(id.starts_with("saturated-"));
                            assert!(matches!(env.payload, FrontendEvent::Error { message } if message == DEADLINE || message == QUEUE_FULL));
                            assert!(replies.insert(id), "duplicate correlated reply");
                            if replies.len() == 320 {
                                break false;
                            }
                        }
                        None => {
                            if let FrontendEvent::TopicEvent { topic, .. } = &env.payload {
                                if topic == crate::app::session_queue::RESYNC_TOPIC {
                                    resync_markers += 1;
                                } else if topic.starts_with("saturation-") {
                                    pushes += 1;
                                }
                            }
                        }
                    }
                }
                Ok(None) => break true,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break true,
                Err(error) => panic!("unexpected read error: {error}"),
            }
        }
    })
    .await
    .expect("missing replies without an explicit socket close");
    assert!(!closed, "the connection must stay open: replies are never evicted");
    let expected: std::collections::HashSet<_> = (0..320).map(|index| format!("saturated-{index}")).collect();
    assert_eq!(replies, expected);
    if push_topics == 0 {
        assert_eq!(
            resync_markers, 0,
            "nothing overflowed, so nothing should ask for a resync"
        );
    } else {
        assert!(
            pushes < push_topics,
            "all {push_topics} pushes arrived: the lane never overflowed, so eviction was not exercised"
        );
        assert!(
            resync_markers > 0,
            "evicted pushes must be announced by a resync marker ({pushes} of {push_topics} pushes arrived)"
        );
    }
    drop(lines);
    drop(write);
    drop(held);
    // The session ends cleanly however the close reaches the server. Every
    // push was queued ahead of the replies, so nothing should be left unread
    // here; but a peer that does close with unread frames surfaces to a
    // Linux server as a connection reset, which is a clean end of session
    // too (see `app::tests::peer_disconnect`), never an error.
    tokio::time::timeout(Duration::from_secs(5), handler)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Concurrent identical reads retain exactly-once request correlation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_reads_on_one_socket_reply_to_every_request() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let ids: Vec<String> = (0..8).map(|i| format!("order-{i}")).collect();
    for id in &ids {
        let envelope = FrontendRequestEnvelope {
            request_id: id.clone(),
            payload: FrontendRequest::ListProducts,
        };
        write
            .write_all(format!("{}\n", serde_json::to_string(&envelope).unwrap()).as_bytes())
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    while seen.len() < ids.len() {
        let line = lines.next_line().await.unwrap().unwrap();
        let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
        if let Some(id) = event.request_id.filter(|id| id.starts_with("order-")) {
            seen.push(id);
        }
    }
    seen.sort();
    assert_eq!(seen, ids);
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}
