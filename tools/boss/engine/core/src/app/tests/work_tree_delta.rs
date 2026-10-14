use super::*;

async fn fetch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, product: &str, delta: bool) -> FrontendEvent {
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("board")
        .request_id("fetch")
        .recv_instant(Instant::now())
        .decode_ms(0.0)
        .build();
    work_items::handle_get_work_tree(
        ctx,
        FrontendRequest::GetWorkTree {
            product_id: product.into(),
            fetch_seq: None,
            delta,
        },
    )
    .await;
    sink.next().await.unwrap().payload
}

fn update(event: FrontendEvent) -> boss_protocol::work_tree_delta::WorkTreeUpdate {
    let FrontendEvent::WorkTreeUpdate { update } = event else {
        panic!("expected update")
    };
    update
}

#[test]
fn work_tree_snapshot_lock_precedes_database_read_for_concurrent_admission() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let (state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&state.work_db, "ordered");
    let sink = make_session_sink();
    // Stall the reader at the DB boundary. It must already own the session's
    // snapshot lock, otherwise a second admitted read can overtake its result.
    let connection = state.work_db.connect().unwrap();
    let reader_state = state.clone();
    let reader_sink = sink.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        started_tx.send(()).unwrap();
        runtime.block_on(fetch(&reader_state, &reader_sink, &product.id, true))
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut locked = false;
    while Instant::now() < deadline {
        if sink.work_tree_cache.try_lock().is_err() {
            locked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(connection);
    assert!(update(reader.join().unwrap()).base_sequence.is_none());
    assert!(locked, "snapshot lock must cover the database read, not just enqueue");
}

#[tokio::test]
async fn work_tree_delta_handler_moves_deletes_and_recovers_full_snapshots() {
    let (state, _dir) = test_server_state();
    let first = crate::test_support::create_test_product_named(&state.work_db, "first");
    let second = crate::test_support::create_test_product_named(&state.work_db, "second");
    let task = crate::test_support::create_test_chore(&state.work_db, &first.id, "moving");
    let sink = make_session_sink();
    let snapshot = update(fetch(&state, &sink, &first.id, true).await);
    assert!(snapshot.base_sequence.is_none());
    assert_eq!(
        snapshot.changes,
        serde_json::to_value(state.work_db.get_work_tree(&first.id).unwrap()).unwrap()
    );
    // Unrelated topic revisions do not create gaps in the board base.
    state.bump_work_revision();
    state.bump_work_revision();
    let unchanged = update(fetch(&state, &sink, &first.id, true).await);
    assert_eq!(unchanged.base_sequence, Some(snapshot.sequence));
    assert_eq!(unchanged.changes, serde_json::json!({}));
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET product_id = ?1 WHERE id = ?2", [&second.id, &task.id])
        .unwrap();
    let removed = update(fetch(&state, &sink, &first.id, true).await);
    assert_eq!(removed.changes["chores"]["order"], serde_json::json!([]));
    assert_eq!(removed.changes["task_runtimes"]["order"], serde_json::json!([]));
    let moved = update(fetch(&state, &sink, &second.id, true).await);
    assert!(moved.base_sequence.is_none());
    assert_eq!(moved.changes["chores"][0]["id"], task.id);
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", [&task.id])
        .unwrap();
    let deleted = update(fetch(&state, &sink, &second.id, true).await);
    assert_eq!(deleted.changes["chores"]["order"], serde_json::json!([]));
    // A client with a missing base asks for a legacy full response. The next
    // opted-in reply must establish a new base rather than extend the lost one.
    assert!(matches!(
        fetch(&state, &sink, &second.id, false).await,
        FrontendEvent::WorkTree { .. }
    ));
    assert!(
        update(fetch(&state, &sink, &second.id, true).await)
            .base_sequence
            .is_none()
    );
    let reconnected = make_session_sink();
    assert!(
        update(fetch(&state, &reconnected, &second.id, true).await)
            .base_sequence
            .is_none()
    );
}
