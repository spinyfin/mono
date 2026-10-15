use super::*;

async fn fetch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, product: &str, delta: bool) -> FrontendEvent {
    fetch_with_db(state, sink, product, delta, state.work_db.clone()).await
}

async fn fetch_with_db(
    state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    product: &str,
    delta: bool,
    db: Arc<crate::work::WorkDb>,
) -> FrontendEvent {
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(db)
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
fn slow_board_read_does_not_block_a_later_read_on_the_same_session() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let (state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&state.work_db, "concurrent");
    for i in 0..64 {
        crate::test_support::create_test_chore(&state.work_db, &product.id, format!("fixture-{i}"));
    }
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET name = 'before'", [])
        .unwrap();
    let sink = make_session_sink();
    let slow_db = Arc::new(state.work_db.query_connection().unwrap());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut first = true;
    slow_db
        .connect()
        .unwrap()
        .authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            if first
                && matches!(
                    ctx.action,
                    rusqlite::hooks::AuthAction::Read {
                        table_name: "projects",
                        ..
                    }
                )
            {
                first = false;
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            rusqlite::hooks::Authorization::Allow
        }));
    let reader_state = state.clone();
    let reader_sink = sink.clone();
    let product_id = product.id.clone();
    let reader = std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(fetch_with_db(
            &reader_state,
            &reader_sink,
            &product_id,
            true,
            slow_db,
        ))
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET name = 'after'", [])
        .unwrap();
    let fast_db = Arc::new(state.work_db.reader().unwrap());
    let fast = update(runtime.block_on(fetch_with_db(&state, &sink, &product.id, true, fast_db)));
    assert!(fast.base_sequence.is_none());
    assert_eq!(fast.changes["chores"][0]["name"], "after");
    release_tx.send(()).unwrap();
    let slow = update(reader.join().unwrap());
    assert_eq!(slow.base_sequence, Some(fast.sequence));
    assert_eq!(slow.changes, serde_json::json!({}));
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
