use super::*;

/// Routine background SQL calls share the engine's connection. A slow holder
/// must not turn every runtime worker into a synchronous mutex waiter and
/// prevent the app's admission task from running despite available permits.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn background_database_contention_keeps_read_admission_running() {
    let db = Arc::new(crate::work::WorkDb::open(":memory:".into()).unwrap());
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn({
        let db = db.clone();
        move || {
            let _connection = db.connect().unwrap();
            locked_tx.send(()).unwrap();
            // A wall-clock escape keeps even the broken implementation bounded.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        }
    });
    locked_rx.await.unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let background = tokio::spawn(async move {
        entered_tx.send(()).unwrap();
        db.list_products().unwrap();
    });
    entered_rx.await.unwrap();
    let read = tokio::spawn(async {
        let gate = Arc::new(ReadAdmission::new(2, 2, 8, Duration::from_millis(20)));
        let connection = gate.connection();
        let mut ordering = ReadOrdering::default();
        let turn = ordering.reserve(&boss_protocol::FrontendRequest::ListProducts).await;
        drop(gate.enqueue(&connection).await.acquire_ordered(turn).await);
    });
    let result = tokio::time::timeout(Duration::from_secs(1), read).await;
    let released_before_escape = release_tx.send(()).is_ok();
    holder.join().unwrap();
    background.await.unwrap();
    assert!(
        released_before_escape,
        "runtime stalled until the database holders wall-clock escape"
    );
    result
        .expect("background DB lock wait starved app read admission")
        .unwrap();
}

/// More contended background callers than runtime workers must not pin every
/// worker: each hands its core back, so an admitted read still completes
/// while a slow holder keeps the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_background_waiters_than_workers_keep_read_admission_running() {
    let db = Arc::new(crate::work::WorkDb::open(":memory:".into()).unwrap());
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn({
        let db = db.clone();
        move || {
            let _connection = db.connect().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        }
    });
    locked_rx.await.unwrap();
    let waiters = 8;
    let entered = Arc::new(tokio::sync::Barrier::new(waiters + 1));
    let background: Vec<_> = (0..waiters)
        .map(|_| {
            let (db, entered) = (db.clone(), entered.clone());
            tokio::spawn(async move {
                entered.wait().await;
                db.list_products().unwrap();
            })
        })
        .collect();
    entered.wait().await;
    let read = tokio::spawn(async {
        let gate = Arc::new(ReadAdmission::new(2, 2, 8, Duration::from_millis(20)));
        let connection = gate.connection();
        let mut ordering = ReadOrdering::default();
        let turn = ordering.reserve(&boss_protocol::FrontendRequest::ListProducts).await;
        drop(gate.enqueue(&connection).await.acquire_ordered(turn).await);
    });
    let result = tokio::time::timeout(Duration::from_secs(1), read).await;
    let released_before_escape = release_tx.send(()).is_ok();
    holder.join().unwrap();
    for task in background {
        task.await.unwrap();
    }
    assert!(
        released_before_escape,
        "runtime stalled until the holders wall-clock escape"
    );
    result
        .expect("background DB lock waiters starved app read admission")
        .unwrap();
}

/// Over the wire: a read behind an identical earlier read that cannot be
/// admitted for longer than the old 60 s ordering deadline is still
/// answered with data once the permit frees — and shares the earlier
/// read's handler run. Paused time lets the minutes pass without waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_behind_a_long_held_identical_read_is_answered_not_rejected() {
    use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let gate = Arc::new(ReadAdmission::new(1, 1, 8, Duration::from_millis(50)));
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        crate::app::ServerStateOverrides::builder()
            .read_admission(gate.clone())
            .build(),
    );
    // With the engine's only permit held, the first ListProducts waits for
    // admission while the identical second read joins it.
    let held = gate.active.clone().acquire_owned().await.unwrap();
    let (client, server) = tokio::net::UnixStream::pair().unwrap();
    let handler = tokio::spawn(crate::app::handle_frontend_connection(server, state.clone(), None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    for id in ["first", "second"] {
        let envelope = FrontendRequestEnvelope {
            request_id: id.into(),
            payload: FrontendRequest::ListProducts,
        };
        write
            .write_all(format!("{}\n", serde_json::to_string(&envelope).unwrap()).as_bytes())
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.read_coalescing.followers() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second read must join the first");
    // Well past the slow-wait threshold: logged, not rejected.
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(held);
    let mut answered = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while answered.len() < 2 {
            let line = lines.next_line().await.unwrap().unwrap();
            let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
            if let Some(id) = event.request_id {
                assert!(
                    matches!(event.payload, FrontendEvent::ProductsList { .. }),
                    "{id}: {:?}",
                    event.payload
                );
                answered.push(id);
            }
        }
    })
    .await
    .expect("both reads must be answered");
    assert_eq!(answered, ["first", "second"]);
    assert_eq!(state.read_coalescing.leaders(), 1);
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[test]
fn slow_wait_reasons_say_the_read_is_waiting_and_name_the_cause() {
    for reason in [SLOW_QUEUE, SLOW_ORDERING, SLOW_PERMIT] {
        assert!(reason.starts_with("read waiting:"), "{reason}");
        assert!(!reason.contains("busy") && !reason.contains("retry"), "{reason}");
    }
    assert!(SLOW_QUEUE.contains("queue full"));
    assert!(SLOW_ORDERING.contains("identical read"));
    assert!(SLOW_PERMIT.contains("permits"));
}
