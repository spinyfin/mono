use super::*;

use crate::test_support::*;

#[tokio::test]
async fn spawn_worker_pane_requests_are_serialized() {
    // Two concurrent SpawnWorkerPane calls go through
    // `WorkerSpawner::send_to_app_request`. The mutex inside that
    // path should ensure only one is enqueued on the sink before
    // the first response is delivered. The second request must
    // not appear in the queue until after the first has resolved.
    use crate::spawn_flow::WorkerSpawner;

    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let make_request = |run: &str| {
        EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
            run_id: run.to_owned(),
            workspace_path: "/tmp".into(),
            slot_id: 1,
            initial_input: "claude\n".into(),
            env: vec![],
            summary: None,
            task_title: None,
            pane_monitor: None,
        })
    };

    let server_a = server_state.clone();
    let send_a = tokio::spawn(async move {
        server_a
            .send_to_app_request(make_request("run-a"), Duration::from_secs(5))
            .await
    });
    let server_b = server_state.clone();
    let send_b = tokio::spawn(async move {
        server_b
            .send_to_app_request(make_request("run-b"), Duration::from_secs(5))
            .await
    });

    // The first request must be on the sink; the second must be
    // gated behind the spawn_pane_lock until the first resolves.
    let first = sink.next().await.expect("first EngineRequest enqueued");
    let first_request_id = match &first.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    // Give the runtime time to schedule the second task. With
    // serialization the sink stays empty; without it the second
    // request would already be enqueued and `sink.next()` would
    // resolve before the timeout fires.
    let peek = tokio::time::timeout(Duration::from_millis(100), sink.next()).await;
    assert!(
        peek.is_err(),
        "second SpawnWorkerPane should not be in flight while the first is pending; got {:?}",
        peek.ok().flatten().map(|env| env.payload),
    );

    // Resolve the first response — this releases the mutex and
    // lets the second request go.
    server_state
        .deliver_app_response(
            "session-app",
            &first_request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 1,
                    shell_pid: 0,
                }),
            },
        )
        .await;
    send_a.await.expect("send_a task").expect("ok response");

    let second = sink.next().await.expect("second EngineRequest enqueued");
    let second_request_id = match &second.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &second_request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 2,
                    shell_pid: 0,
                }),
            },
        )
        .await;
    send_b.await.expect("send_b task").expect("ok response");
}

#[tokio::test]
async fn release_worker_pane_drops_live_worker_state() {
    // Regression: chore-done (and other engine-driven release
    // paths) must clear the live-state entry so the UI stops
    // rendering the worker as attached to its work item. Without
    // this, the kanban Doing dot and the pane titlebar pill stayed
    // pinned at the worker's last activity (e.g. WaitingForInput)
    // even after the libghostty pane was torn down.
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-x", 1);
    server_state
        .live_worker_states
        .register_spawn(1, "run-x", "claude-opus-4-7", 0, None);
    assert!(
        server_state.live_worker_states.get(1).is_some(),
        "precondition: live state for slot 1 should be registered",
    );

    // No app session is registered, so the SendToApp call in
    // release_worker_pane returns NotRegistered. The cleanup must
    // run regardless.
    server_state.release_worker_pane("run-x").await;

    assert!(
        server_state.live_worker_states.get(1).is_none(),
        "release_worker_pane must drop the live-state entry alongside the libghostty pane",
    );
    assert_eq!(
        server_state.worker_registry.slot_for_run("run-x"),
        None,
        "release_worker_pane must drop the worker_registry slot mapping",
    );

    // Idempotent: a second call (e.g. completion-detection then
    // chore-done firing for the same run) is a no-op.
    server_state.release_worker_pane("run-x").await;
    assert!(server_state.live_worker_states.get(1).is_none());
}

#[tokio::test]
async fn release_worker_pane_resolves_open_stale_worker_attention() {
    let (server_state, _dir) = test_server_state();
    let product_id = create_product(&server_state.work_db);
    let work_item_id = create_active_chore(&server_state.work_db, &product_id, "test chore");
    let execution_id = create_old_execution(&server_state.work_db, &work_item_id);
    server_state
        .work_db
        .upsert_external_tracker_attention(
            &work_item_id,
            crate::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND,
            "Worker appears stuck; inspection required",
            "prior body",
        )
        .unwrap();
    server_state.worker_registry.register_run_slot(&execution_id, 1);
    server_state.live_worker_states.register_spawn(
        1,
        &execution_id,
        "claude-opus-4-7",
        0,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );

    server_state.release_worker_pane(&execution_id).await;

    let open_items: Vec<_> = server_state
        .work_db
        .list_attention_items_for_work_item(&work_item_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == crate::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND && item.status == "open")
        .collect();
    assert!(
        open_items.is_empty(),
        "release_worker_pane must resolve stale_worker attention so a manual stop cannot strand it: {open_items:?}"
    );
}

#[tokio::test]
async fn release_worker_pane_reaps_the_tmux_session_for_a_slot_mapped_run() {
    // Regression coverage for the slot-mapped call site
    // (`release_worker_pane`'s primary body, as opposed to the
    // no-slot-mapping fallback covered in `worker_process_reaping.rs`):
    // a tmux-hosted worker's session must be torn down and its identity
    // columns cleared alongside the libghostty pane release, even when no
    // app session is registered to answer the pane-release request.
    use super::tmux_stub::{fake_tmux, ok};

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_old_execution(db, &work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, "boss-1-example", "tok-y")
            .unwrap(),
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, "tok-y", 0)
            .unwrap(),
    );

    server_state.worker_registry.register_run_slot(&execution_id, 1);

    let (tmux, runner) = fake_tmux([ok("BOSS_SPAWN_TOKEN=tok-y\n"), ok("BOSS_SPAWN_TOKEN=tok-y\n"), ok("")]);
    server_state.set_tmux_override_for_test(tmux);

    // No app session registered, so the pane-release SendToApp call
    // returns NotRegistered — the tmux reap must still run.
    server_state.release_worker_pane(&execution_id).await;

    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "the slot-mapped release path must also reap the tmux session and clear its identity",
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "kill-session",
                "-t",
                "boss-1-example"
            ],
        ],
        "the slot-mapped reap path must issue show-environment then kill-session, nothing else",
    );
}

#[tokio::test]
async fn release_worker_pane_releases_matching_worker_pool_slot() {
    // Engine-side lifecycle pairing: the WorkerPool slot is held
    // for the lifetime of the libghostty pane (not just for the
    // duration of `run_execution`). Tearing the pane down via
    // `release_worker_pane` must hand the pool slot back so a
    // subsequent `claim_worker` can reuse it — otherwise the
    // engine and the app drift apart and the next
    // SpawnWorkerPane gets rejected as SlotBusy.
    let (server_state, _dir) = test_server_state();
    let pool = server_state.execution_coordinator.worker_pool();

    // Pre-claim slot 1 the way the coordinator would, then wire
    // the worker_registry so `release_worker_pane` can resolve
    // the run id back to that slot.
    let claimed = pool
        .claim_worker("exec-1", None)
        .await
        .expect("worker pool starts with one free slot");
    assert_eq!(claimed, "worker-1");
    assert_eq!(pool.idle_count().await, 0);
    server_state.worker_registry.register_run_slot("run-1", 1);

    // No app session is registered, so the SendToApp call inside
    // release_worker_pane bails on NotRegistered — the pool
    // release must still happen.
    server_state.release_worker_pane("run-1").await;

    assert_eq!(
        pool.idle_count().await,
        1,
        "WorkerPool slot must be freed once the libghostty pane is released",
    );
    // And the next claim lands on the same slot.
    let re_claimed = pool.claim_worker("exec-2", None).await.expect("slot 1 is free");
    assert_eq!(re_claimed, "worker-1");
}

#[tokio::test]
async fn release_worker_pane_pool_release_is_idempotent() {
    // A pane can be released from more than one path (completion
    // handler, force-release, engine shutdown). `take_slot_for_run`
    // is the natural choke point — the second call sees no slot
    // mapping and short-circuits before touching the pool — so a
    // racy double-release must not zero out an unrelated execution
    // that has already re-claimed the slot.
    let (server_state, _dir) = test_server_state();
    let pool = server_state.execution_coordinator.worker_pool();

    let _claimed = pool.claim_worker("exec-1", None).await.unwrap();
    server_state.worker_registry.register_run_slot("run-1", 1);

    server_state.release_worker_pane("run-1").await;
    assert_eq!(pool.idle_count().await, 1);

    // Re-claim the slot for a new execution.
    let claimed_again = pool.claim_worker("exec-2", None).await.unwrap();
    assert_eq!(claimed_again, "worker-1");
    assert_eq!(pool.idle_count().await, 0);

    // A duplicate release for the original run must not steal the
    // slot back from exec-2.
    server_state.release_worker_pane("run-1").await;
    assert_eq!(
        pool.idle_count().await,
        0,
        "duplicate release_worker_pane must not free a slot now held by a different execution",
    );
}

#[tokio::test]
async fn reap_run_releases_worker_pool_claim_and_live_state() {
    // Regression: `bossctl agents reap` (`handle_reap_run`) used to
    // only mark the execution `orphaned` in the DB — unlike every
    // other teardown path (`agents stop`, completion, dead-pid /
    // stale-worker sweeps), it never called `release_worker_pane`,
    // so a reaped run's WorkerPool claim and LiveWorkerStateRegistry
    // entry outlived it forever. Worse, the stale live-state entry
    // defeated `pool_claim_sweep`'s self-heal too: the reconciler
    // treats a claim with a live-state entry still present as "owned
    // by a live pane's teardown path" and skips it, so the one
    // backstop meant to catch leaked claims never fired for a reaped
    // run either.
    use boss_protocol::{CreateProductInput, RequestExecutionInput};

    let (server_state, _dir) = test_server_state();
    let product = server_state
        .work_db
        .create_product(
            CreateProductInput::builder()
                .name("p")
                .repo_remote_url("git@example.com:p.git")
                .build(),
        )
        .unwrap();
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    let pool = server_state.execution_coordinator.worker_pool();
    let claimed = pool
        .claim_worker(&execution.id, None)
        .await
        .expect("pool starts with a free slot");
    assert_eq!(claimed, "worker-1");
    server_state.worker_registry.register_run_slot(&execution.id, 1);
    server_state
        .live_worker_states
        .register_spawn(1, &execution.id, "claude-opus-4-8", 0, None);
    assert_eq!(
        pool.idle_count().await,
        pool.capacity().await - 1,
        "precondition: slot claimed"
    );
    assert!(
        server_state.live_worker_states.get(1).is_some(),
        "precondition: live state registered",
    );

    let sink = make_session_sink();
    let ctx = Dispatch::builder()
        .server_state(server_state.clone())
        .work_db(server_state.work_db.clone())
        .sink(sink.clone())
        .session_id("s1")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    executions::handle_reap_run(
        ctx,
        FrontendRequest::ReapRun {
            run_id: execution.id.clone(),
        },
    )
    .await;

    let response = sink.next().await.expect("reap response enqueued");
    match response.payload {
        FrontendEvent::RunReaped { execution: reaped, .. } => {
            assert_eq!(reaped.status.to_string(), "orphaned");
        }
        other => panic!("expected RunReaped, got {other:?}"),
    }

    // The pane/pool/live-state cleanup happens on a background task
    // (mirrors `handle_stop_run`) so the RPC response doesn't wait on
    // it — poll for it to land.
    for _ in 0..50 {
        if server_state.live_worker_states.get(1).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        server_state.live_worker_states.get(1).is_none(),
        "reap must drop the live-state entry, not just mark the execution terminal",
    );
    assert_eq!(
        pool.idle_count().await,
        pool.capacity().await,
        "reap must release the WorkerPool claim immediately rather than leaving it \
         to outlive the execution until the pool-claim reconciler's grace period",
    );

    let reclaimed = pool
        .claim_worker("exec-fresh", None)
        .await
        .expect("slot must be free after reap");
    assert_eq!(reclaimed, "worker-1");
}
