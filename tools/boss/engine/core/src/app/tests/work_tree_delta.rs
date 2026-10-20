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
    let id = send_fetch(state, sink, product, delta, db).await;
    let reply = sink.next().await.unwrap();
    assert_eq!(reply.request_id.as_deref(), Some(id.as_str()));
    reply.payload
}

async fn send_fetch(
    state: &Arc<ServerState>,
    sink: &Arc<SessionSink>,
    product: &str,
    delta: bool,
    db: Arc<crate::work::WorkDb>,
) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!("fetch-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(db)
        .sink(sink.clone())
        .session_id("board")
        .request_id(id.clone())
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
    id
}

fn update(event: FrontendEvent) -> boss_protocol::work_tree_delta::WorkTreeUpdate {
    let FrontendEvent::WorkTreeUpdate { update } = event else {
        panic!("expected update")
    };
    update
}

#[tokio::test]
async fn standalone_attention_reads_do_not_acquire_the_writer_connection() {
    let (state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&state.work_db, "reader");
    let task = crate::test_support::create_test_chore(&state.work_db, &product.id, "read filter");
    let reader = Arc::new(state.work_db.reader().unwrap());
    let worker_state = state.clone();
    let selector = boss_protocol::short_id_wire_form(task.short_id.unwrap());
    let writer = state.work_db.connect().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let sink = make_session_sink();
            for request in [
                FrontendRequest::ListAttentionItemsForWorkItem {
                    work_item_id: selector.clone(),
                },
                FrontendRequest::ListAttentionGroups {
                    product_id: product.id,
                    project_id: None,
                    task_id: Some(selector),
                    kind: None,
                    state: None,
                },
            ] {
                let ctx = Dispatch::builder()
                    .server_state(worker_state.clone())
                    .work_db(reader.clone())
                    .sink(sink.clone())
                    .session_id("attention reader")
                    .request_id("read")
                    .recv_instant(Instant::now())
                    .decode_ms(0.0)
                    .build();
                match request {
                    req @ FrontendRequest::ListAttentionItemsForWorkItem { .. } => {
                        attentions::handle_list_attention_items_for_work_item(ctx, req).await
                    }
                    req => attentions::handle_list_attention_groups(ctx, req).await,
                }
                let response = sink.next().await.unwrap().payload;
                assert!(matches!(
                    response,
                    FrontendEvent::AttentionItemsForWorkItemList { .. } | FrontendEvent::AttentionGroupsList { .. }
                ));
            }
            tx.send(()).unwrap();
        });
    });
    let result = rx.recv_timeout(Duration::from_secs(5));
    // Release even on failure so a regression cannot leave a deadlocked thread.
    drop(writer);
    worker.join().unwrap();
    result.expect("attention reads must finish while the writer is held");
}

#[tokio::test]
async fn work_tree_attention_snapshots_and_deltas_follow_create_and_dismiss() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let product = crate::test_support::create_test_product_named(db, "attention tree");
    let other = crate::test_support::create_test_product_named(db, "other");
    let task = crate::test_support::create_test_chore(db, &product.id, "attention task");
    let sink = make_session_sink();
    let first = update(fetch(&state, &sink, &product.id, true).await);
    for field in ["attention_items", "attention_groups", "attention_members"] {
        assert_eq!(first.changes[field], serde_json::json!([]));
    }
    db.upsert_external_tracker_attention(&product.id, "external_tracker_auth_failed", "Auth", "Reconnect")
        .unwrap();
    db.upsert_external_tracker_attention(&other.id, "external_tracker_auth_failed", "Other", "Other")
        .unwrap();
    let (member, group) = db
        .create_attention(
            boss_protocol::CreateAttentionInput::builder()
                .kind("followup")
                .association_task_id(&task.id)
                .source_kind("task_transcript")
                .source_task_id(&task.id)
                .proposed_name("Follow up")
                .proposed_description("Do the followup")
                .build(),
        )
        .unwrap();
    let created = update(fetch(&state, &sink, &product.id, true).await);
    assert_eq!(created.base_sequence, Some(first.sequence));
    assert_eq!(
        created.changes["attention_items"]["upserts"].as_array().unwrap().len(),
        1
    );
    assert_eq!(created.changes["attention_groups"]["upserts"][0]["id"], group.id);
    assert_eq!(created.changes["attention_members"]["upserts"][0]["id"], member.id);

    let FrontendEvent::WorkTree {
        attention_items,
        attention_groups,
        attention_members,
        ..
    } = fetch(&state, &sink, &product.id, false).await
    else {
        panic!("full tree")
    };
    assert_eq!(
        serde_json::to_value(&attention_items).unwrap(),
        serde_json::to_value(db.list_attention_items_for_work_item(&product.id).unwrap()).unwrap()
    );
    assert_eq!(attention_groups[0].id, group.id);
    assert_eq!(attention_members[0].id, member.id);
    update(fetch(&state, &sink, &product.id, true).await);
    db.dismiss_attention(&group.id, None).unwrap();
    db.resolve_external_tracker_attention(&product.id, "external_tracker_auth_failed")
        .unwrap();
    let dismissed = update(fetch(&state, &sink, &product.id, true).await);
    assert_eq!(dismissed.changes["attention_groups"]["order"], serde_json::json!([]));
    assert_eq!(dismissed.changes["attention_members"]["order"], serde_json::json!([]));
    assert_eq!(dismissed.changes["attention_items"]["upserts"][0]["status"], "resolved");
    assert_eq!(
        update(fetch(&state, &sink, &product.id, true).await).changes,
        serde_json::json!({})
    );
}

#[test]
fn same_product_reads_preserve_freshness_and_delta_sequence() {
    overlapping_reads(false);
}

#[test]
fn slow_board_read_does_not_block_a_different_product_on_the_same_session() {
    overlapping_reads(true);
}

fn overlapping_reads(other_product: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
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
    let other = crate::test_support::create_test_product_named(&state.work_db, "other");
    let sink = make_session_sink();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = attempts.clone();
    let slow_db = Arc::new(state.work_db.reader().unwrap());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut first = true;
    slow_db
        .connect()
        .unwrap()
        .authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            if matches!(
                ctx.action,
                rusqlite::hooks::AuthAction::Transaction {
                    operation: rusqlite::hooks::TransactionOperation::Begin
                }
            ) {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
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
        tokio::runtime::Runtime::new().unwrap().block_on(send_fetch(
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
    if other_product {
        // Preserve the overtaking guarantee for independent keys, including
        // repeated delta updates while the first product remains blocked.
        let fast = update(runtime.block_on(fetch_with_db(&state, &sink, &other.id, true, fast_db)));
        assert!(fast.base_sequence.is_none());
        let mut last = fast.sequence;
        for _ in 0..8 {
            let next = update(runtime.block_on(fetch(&state, &sink, &other.id, true)));
            assert_eq!(next.base_sequence, Some(last));
            assert_eq!(next.changes, serde_json::json!({}));
            last = next.sequence;
        }
        release_tx.send(()).unwrap();
        let id = reader.join().unwrap();
        let reply = runtime.block_on(sink.next()).unwrap();
        assert_eq!(reply.request_id.as_deref(), Some(id.as_str()));
        let slow = update(reply.payload);
        assert!(slow.base_sequence.is_none());
        assert_eq!(slow.changes["chores"][0]["name"], "before");
    } else {
        let (fast_state, fast_sink, product_id) = (state.clone(), sink.clone(), product.id.clone());
        let fast = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(send_fetch(
                &fast_state,
                &fast_sink,
                &product_id,
                true,
                fast_db,
            ))
        });
        release_tx.send(()).unwrap();
        let old_id = reader.join().unwrap();
        let new_id = fast.join().unwrap();
        let first = runtime.block_on(sink.next()).unwrap();
        let second = runtime.block_on(sink.next()).unwrap();
        assert_ne!(first.request_id, second.request_id);
        assert!([old_id.as_str(), new_id.as_str()].contains(&first.request_id.as_deref().unwrap()));
        assert!([old_id.as_str(), new_id.as_str()].contains(&second.request_id.as_deref().unwrap()));
        let first_update = update(first.payload);
        let second_update = update(second.payload);
        assert!(first_update.base_sequence.is_none());
        assert_eq!(second_update.base_sequence, Some(first_update.sequence));
        assert!(second_update.sequence > first_update.sequence);
        if first.request_id.as_deref() == Some(new_id.as_str()) {
            // Newer scan enqueued first: the delayed older reply must reuse
            // that snapshot, never roll the board back.
            assert_eq!(first_update.changes["chores"][0]["name"], "after");
            assert_eq!(second_update.changes, serde_json::json!({}));
        } else {
            assert_eq!(first_update.changes["chores"][0]["name"], "before");
            let changed = second_update.changes["chores"]["upserts"].as_array().unwrap();
            assert_eq!(changed.len(), 64);
            assert!(changed.iter().all(|row| row["name"] == "after"));
        }
        assert_eq!(
            update(runtime.block_on(fetch(&state, &sink, &product.id, true))).changes,
            serde_json::json!({}),
            "the session must retain the fresh board"
        );
    }
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 1);
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
