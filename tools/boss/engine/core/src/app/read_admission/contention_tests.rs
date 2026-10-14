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
        gate.enqueue(&connection).unwrap().acquire_ordered(turn).await.unwrap();
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
        gate.enqueue(&connection).unwrap().acquire_ordered(turn).await.unwrap();
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

/// Over the wire: a read stuck behind an identical, never-finishing earlier
/// read is answered with the ordering-deadline text, not the generic one.
/// Paused time lets the 60 s ordering deadline pass without waiting.
#[tokio::test(start_paused = true)]
async fn ordering_deadline_rejection_reaches_the_wire_with_ordering_text() {
    use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let gate = Arc::new(ReadAdmission::new(1, 1, 8, Duration::from_secs(3600)));
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        crate::app::ServerStateOverrides::builder()
            .read_admission(gate.clone())
            .build(),
    );
    // With the engine's only permit held, the first ListProducts waits for
    // admission while keeping the ordering turn the identical second read needs.
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
    let message = loop {
        let line = lines.next_line().await.unwrap().unwrap();
        let event: FrontendEventEnvelope = serde_json::from_str(&line).unwrap();
        if event.request_id.as_deref() == Some("second") {
            match event.payload {
                FrontendEvent::Error { message } => break message,
                other => panic!("expected an ordering rejection, got {other:?}"),
            }
        }
    };
    assert_eq!(message, Rejection::OrderingWait.message());
    drop(held);
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[test]
fn rejection_messages_distinguish_queue_ordering_and_permit_deadlines() {
    assert!(QUEUE_FULL.contains("queue full"));
    assert!(Rejection::OrderingWait.message().contains("ordering deadline expired"));
    assert!(
        Rejection::AdmissionWait
            .message()
            .contains("admission deadline expired")
    );
    assert_ne!(Rejection::OrderingWait.message(), Rejection::AdmissionWait.message());
}
