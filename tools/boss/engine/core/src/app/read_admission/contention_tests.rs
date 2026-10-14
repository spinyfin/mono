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
