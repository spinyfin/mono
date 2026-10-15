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
        gate.enqueue(&connection).unwrap().acquire().await.unwrap();
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
        gate.enqueue(&connection).unwrap().acquire().await.unwrap();
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
