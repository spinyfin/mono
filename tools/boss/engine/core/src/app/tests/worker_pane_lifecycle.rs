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
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
            merge_mechanism: None,
        })
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
