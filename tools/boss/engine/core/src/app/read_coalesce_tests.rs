use super::*;
use crate::app::read_admission::ReadAdmission;
use crate::app::{ServerState, ServerStateOverrides, handle_frontend_connection};
use crate::work::contention::HEALTH_THRESHOLD;
use boss_protocol::{CreateChoreInput, FrontendRequestEnvelope};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// One frontend socket session against `state`.
struct Session {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    handler: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Session {
    fn open(state: Arc<ServerState>) -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_frontend_connection(server, state, None));
        let (read, write) = client.into_split();
        Self {
            lines: BufReader::new(read).lines(),
            write,
            handler,
        }
    }

    async fn send(&mut self, request_id: &str, payload: FrontendRequest) {
        let envelope = FrontendRequestEnvelope {
            request_id: request_id.into(),
            payload,
        };
        self.write
            .write_all(format!("{}\n", serde_json::to_string(&envelope).unwrap()).as_bytes())
            .await
            .unwrap();
    }

    /// The next `count` correlated replies, in arrival order; pushes are skipped.
    async fn replies(&mut self, count: usize) -> Vec<(String, FrontendEvent)> {
        let mut replies = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            while replies.len() < count {
                let line = self.lines.next_line().await.unwrap().unwrap();
                let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
                if let Some(id) = event.request_id {
                    replies.push((id, event.payload));
                }
            }
        })
        .await
        .expect("replies never arrived");
        replies
    }

    async fn close(self) {
        drop(self.lines);
        drop(self.write);
        self.handler.await.unwrap().unwrap();
    }
}

fn small_gate_state() -> (Arc<ServerState>, Arc<ReadAdmission>, tempfile::TempDir) {
    // One engine permit (held by the test), but queue room on the
    // connection for the whole burst: per-connection pending is four per
    // active permit, so four active permits admit sixteen queued reads.
    let gate = Arc::new(ReadAdmission::new(1, 4, 64, Duration::from_secs(3600)));
    let (state, dir) = crate::app::tests::test_server_state_with_overrides(
        ServerStateOverrides::builder().read_admission(gate.clone()).build(),
    );
    (state, gate, dir)
}

/// Take the write lock on an independent connection and hold it for `hold`.
fn hold_write_lock(db: &crate::work::WorkDb, hold: Duration) -> std::thread::JoinHandle<()> {
    let holder = db.connect_new().unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        holder
            .execute_batch("BEGIN IMMEDIATE; UPDATE metadata SET value = value WHERE key = 'schema_version'")
            .unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(hold);
        holder.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();
    handle
}

/// Eight identical reads pipelined while admission is held cost one
/// handler run and produce eight replies, in receive order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_pipelined_reads_run_one_handler_and_answer_every_request() {
    let (state, gate, _dir) = small_gate_state();
    let held = gate.enqueue(&gate.connection()).await.acquire().await;
    let mut session = Session::open(state.clone());
    let ids: Vec<String> = (0..8).map(|i| format!("same-{i}")).collect();
    for id in &ids {
        session.send(id, FrontendRequest::ListProducts).await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.read_coalescing.followers() < 7 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("identical reads were not coalesced behind the held leader");
    assert_eq!(state.read_coalescing.leaders(), 1);
    drop(held);
    let replies = session.replies(8).await;
    assert_eq!(replies.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), ids);
    for (id, payload) in &replies {
        assert!(
            matches!(payload, FrontendEvent::ProductsList { .. }),
            "{id}: {payload:?}"
        );
    }
    assert_eq!(state.read_coalescing.leaders(), 1);
    assert_eq!(state.read_coalescing.followers(), 7);
    session.close().await;
}

/// A read that arrives once the leader has started (here: finished) runs
/// its own handler rather than being answered from the earlier snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_after_the_leader_started_runs_its_own_handler() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let mut session = Session::open(state.clone());
    session.send("first", FrontendRequest::ListProducts).await;
    let first = session.replies(1).await;
    assert!(matches!(first[0].1, FrontendEvent::ProductsList { .. }));
    session.send("second", FrontendRequest::ListProducts).await;
    let second = session.replies(1).await;
    assert_eq!(second[0].0, "second");
    assert!(matches!(second[0].1, FrontendEvent::ProductsList { .. }));
    assert_eq!(state.read_coalescing.leaders(), 2);
    assert_eq!(state.read_coalescing.followers(), 0);
    session.close().await;
}

#[tokio::test]
async fn followers_join_only_a_leader_that_has_not_started() {
    let gate = Arc::new(ReadAdmission::new(4, 4, 16, Duration::from_secs(3600)));
    let connection = gate.connection();
    let stats = Stats::default();
    let mut coalescer = ReadCoalescer::default();
    let Role::Lead { batch, waiting } = coalescer.join(
        &FrontendRequest::ListProducts,
        "a",
        gate.enqueue(&connection).await,
        &stats,
    ) else {
        panic!("first read must lead")
    };
    assert!(matches!(
        coalescer.join(
            &FrontendRequest::ListProducts,
            "b",
            gate.enqueue(&connection).await,
            &stats
        ),
        Role::Follow
    ));
    assert!(matches!(
        coalescer.join(
            &FrontendRequest::ListHosts,
            "c",
            gate.enqueue(&connection).await,
            &stats
        ),
        Role::Lead { .. }
    ));
    assert_eq!(batch.start(), 1, "one follower shares the leader's run");
    assert!(matches!(
        coalescer.join(
            &FrontendRequest::ListProducts,
            "d",
            gate.enqueue(&connection).await,
            &stats
        ),
        Role::Lead { .. }
    ));
    drop(waiting);
    assert_eq!((stats.leaders(), stats.followers()), (3, 1));
}

#[test]
fn work_tree_reads_keep_their_own_handler_run() {
    assert!(!coalescable(&FrontendRequest::GetWorkTree {
        product_id: "p".into(),
        fetch_seq: None,
        delta: true,
    }));
    assert!(coalescable(&FrontendRequest::ListProducts));
    assert!(coalescable(&FrontendRequest::GetEngineHealth));
}

fn sink() -> SessionSink {
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    SessionSink::new(shutdown_tx)
}

fn queued(sink: &SessionSink) -> Vec<FrontendEventEnvelope> {
    sink.queue
        .lock()
        .unwrap()
        .items
        .iter()
        .map(|(_, env)| env.clone())
        .collect()
}

/// Followers are answered with the leader's responses under their own ids
/// (revision included); a leader that answered nothing, or failed, fails
/// its followers rather than leaving them hanging.
#[tokio::test]
async fn followers_receive_the_leaders_responses_or_its_failure() {
    let gate = Arc::new(ReadAdmission::new(4, 4, 16, Duration::from_secs(3600)));
    let connection = gate.connection();
    let stats = Stats::default();
    let mut coalescer = ReadCoalescer::default();
    async fn lead(
        coalescer: &mut ReadCoalescer,
        gate: &Arc<ReadAdmission>,
        connection: &Arc<crate::app::read_admission::ConnectionReads>,
        stats: &Stats,
        request: &FrontendRequest,
        id: &str,
    ) -> Arc<Batch> {
        match coalescer.join(request, id, gate.enqueue(connection).await, stats) {
            Role::Lead { batch, waiting } => {
                drop(waiting);
                batch
            }
            Role::Follow => panic!("{id} must lead"),
        }
    }
    let batch = lead(
        &mut coalescer,
        &gate,
        &connection,
        &stats,
        &FrontendRequest::ListProducts,
        "lead",
    )
    .await;
    let followed = coalescer.join(
        &FrontendRequest::ListProducts,
        "follow",
        gate.enqueue(&connection).await,
        &stats,
    );
    assert!(matches!(followed, Role::Follow));
    let sink = sink();
    sink.tap_response("lead", batch.clone());
    sink.enqueue(FrontendEventEnvelope::response_with_revision(
        "lead",
        7,
        FrontendEvent::ProductsList { products: vec![] },
    ));
    sink.untap_response("lead");
    batch.finish(&sink);
    let items = queued(&sink);
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].request_id.as_deref(), Some("follow"));
    assert_eq!(items[1].revision, Some(7));
    assert!(matches!(items[1].payload, FrontendEvent::ProductsList { .. }));

    let silent = lead(
        &mut coalescer,
        &gate,
        &connection,
        &stats,
        &FrontendRequest::ListHosts,
        "silent",
    )
    .await;
    assert!(matches!(
        coalescer.join(
            &FrontendRequest::ListHosts,
            "orphan",
            gate.enqueue(&connection).await,
            &stats
        ),
        Role::Follow
    ));
    silent.finish(&sink);
    let items = queued(&sink);
    assert_eq!(items[2].request_id.as_deref(), Some("orphan"));
    assert!(matches!(&items[2].payload, FrontendEvent::Error { message } if message.contains("no response")));

    let failed = lead(
        &mut coalescer,
        &gate,
        &connection,
        &stats,
        &FrontendRequest::ListFeatureFlags,
        "failed",
    )
    .await;
    assert!(matches!(
        coalescer.join(
            &FrontendRequest::ListFeatureFlags,
            "victim",
            gate.enqueue(&connection).await,
            &stats
        ),
        Role::Follow
    ));
    failed.fail(&sink, "read RPC handler failed");
    let items = queued(&sink);
    assert_eq!(items[3].request_id.as_deref(), Some("victim"));
    assert!(matches!(&items[3].payload, FrontendEvent::Error { message } if message == "read RPC handler failed"));
}

/// Over the wire: a read whose handler opens a transaction while another
/// process holds the write lock waits and is answered with data — never
/// with a `WorkError`, never with an `Error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_under_an_external_write_lock_waits_and_is_answered_with_data() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let product = crate::test_support::create_test_product(&state.work_db);
    let mut session = Session::open(state.clone());
    let holder = hold_write_lock(&state.work_db, Duration::from_millis(400));
    let started = std::time::Instant::now();
    session
        .send(
            "tree",
            FrontendRequest::GetWorkTree {
                product_id: product.id.clone(),
                fetch_seq: None,
                delta: false,
            },
        )
        .await;
    let replies = session.replies(1).await;
    holder.join().unwrap();
    assert!(started.elapsed() < HEALTH_THRESHOLD);
    assert!(
        matches!(replies[0].1, FrontendEvent::WorkTree { .. }),
        "expected the work tree, got {:?}",
        replies[0].1
    );
    session.close().await;
}

/// Over the wire: a create issued while another process holds the write
/// lock commits exactly once and is answered with the created item, not a
/// `WorkError`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_under_an_external_write_lock_commits_once_and_is_answered_with_the_item() {
    let (state, _dir) = crate::app::tests::test_server_state();
    let product = crate::test_support::create_test_product(&state.work_db);
    crate::test_support::create_test_chore_manual(&state.work_db, product.id.clone(), "Seed");
    let mut session = Session::open(state.clone());
    let holder = hold_write_lock(&state.work_db, Duration::from_millis(400));
    let started = std::time::Instant::now();
    session
        .send(
            "create",
            FrontendRequest::CreateChore {
                input: CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Contended chore")
                    .autostart(false)
                    .build(),
            },
        )
        .await;
    let replies = session.replies(1).await;
    holder.join().unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "the create did not wait for the lock"
    );
    match &replies[0].1 {
        FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Chore(chore),
        } => assert_eq!(chore.name, "Contended chore"),
        other => panic!("expected the created chore, got {other:?}"),
    }
    let count: i64 = state
        .work_db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tasks WHERE name = 'Contended chore'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
    session.close().await;
}
