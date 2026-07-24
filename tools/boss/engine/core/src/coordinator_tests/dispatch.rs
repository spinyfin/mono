//! Core dispatch mechanics: remote-transcript reads, host selection and
//! pinning, the interactive concurrency cap, the cold-repo probe, and
//! slot/agent-id stamping plus worker release.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

#[tokio::test]
async fn read_remote_transcript_tail_local_returns_none_and_unknown_host_errors() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    );

    // "local" short-circuits to None so the RPC reads the local fs.
    assert_eq!(
        coordinator
            .read_remote_transcript_tail("local", "/whatever.jsonl", 1024)
            .await
            .unwrap(),
        None,
    );

    // An unknown host is a hard error (the run referenced a host that
    // is no longer registered) rather than a silent empty read.
    let err = coordinator
        .read_remote_transcript_tail("ghost", "/whatever.jsonl", 1024)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err}");
}

#[tokio::test]
async fn schedules_ready_execution_into_running_run() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();
    wait_for_execution_status(
        db.as_ref(),
        &db.list_executions(Some(&chore.id)).unwrap()[0].id,
        ExecutionStatus::Running,
    )
    .await;

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running);
    assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.agent_id, "worker-1");
    assert_eq!(run.status, "active");
    assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    assert_eq!(cube.ensure_calls.lock().await.len(), 1);
    assert_eq!(cube.lease_calls.lock().await.len(), 1);
    assert_eq!(cube.create_calls.lock().await.len(), 1);
    assert_eq!(runner.calls.lock().await.len(), 1);
    assert_eq!(runner.calls.lock().await[0].3.as_deref(), Some("chg-1"));
}

/// Host-adapter provider that records every host the dispatch loop
/// asks it to build an adapter for, then returns a single fixed inner
/// adapter. Lets a routing test assert *which* host was selected
/// without standing up a full SSH-remote adapter double — the inner
/// adapter still drives the FakeCubeClient-backed lease/change/spawn.
struct RecordingHostAdapterProvider {
    inner: Arc<dyn HostAdapter>,
    requested: Mutex<Vec<String>>,
}

#[async_trait]
impl HostAdapterProvider for RecordingHostAdapterProvider {
    async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>> {
        self.requested.lock().await.push(host.id.clone());
        Ok(Arc::clone(&self.inner))
    }
}

/// PR3 routing: an execution pinned to a registered remote host is
/// dispatched through that host's adapter (the dispatch loop asks the
/// provider for `zakalwe`, not `local`) and the run is attributed to
/// the pinned host via `work_runs.host_id`.
#[tokio::test]
async fn pinned_execution_routes_to_remote_host_and_persists_host_id() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    // Register a remote host with spare slots so it survives the
    // free-slots gate.
    db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();

    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Pinned cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    // Pin the ready execution to the remote host.
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coordinator_inner = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    let provider = Arc::new(RecordingHostAdapterProvider {
        inner: coordinator_inner.host_adapter(),
        requested: Mutex::new(Vec::new()),
    });
    coordinator_inner.set_host_adapter_provider(provider.clone());
    let coordinator = Arc::new(coordinator_inner);

    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    // The dispatch loop resolved the adapter for the pinned host —
    // and never for `local`.
    let requested = provider.requested.lock().await.clone();
    assert!(
        requested.iter().any(|h| h == "zakalwe"),
        "expected the provider to be asked for the pinned host, got {requested:?}",
    );
    assert!(
        !requested.iter().any(|h| h == "local"),
        "pinned execution must not route through local, got {requested:?}",
    );

    // The run is attributed to the pinned host.
    let run_ids = db.active_run_ids_for_execution(&execution.id).unwrap();
    assert_eq!(run_ids.len(), 1, "exactly one active run expected");
    assert_eq!(
        db.run_host(&run_ids[0]).unwrap().as_deref(),
        Some("zakalwe"),
        "work_runs.host_id must record the selected host",
    );
}

/// The temporary interactive-pool concurrency cap
/// ([`MAX_CONCURRENT_INTERACTIVE_WORKERS`]): when the interactive pool
/// already carries the capped number of live workers, a drain pass holds
/// every main-pool `ready` row — no run starts, no cube work happens, the
/// row stays `ready`, and an operator-facing wait reason is recorded —
/// even though idle slots exist beyond the cap.
#[tokio::test]
async fn interactive_concurrency_cap_holds_ready_rows() {
    use crate::coordinator::MAX_CONCURRENT_INTERACTIVE_WORKERS;
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Held by cap");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Ready);

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    // Pool has idle slots beyond the cap — the cap, not slot
    // availability, must be what holds the row.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_CONCURRENT_INTERACTIVE_WORKERS + 4),
            cube.clone(),
            runner.clone(),
        )
        .with_pre_start_retry_delays(Vec::new()),
    );
    for i in 0..MAX_CONCURRENT_INTERACTIVE_WORKERS {
        coordinator
            .worker_pool()
            .claim_worker(&format!("exec-busy-{i}"), None)
            .await
            .expect("idle slot below the cap");
    }

    coordinator.drain_ready_queue().await;

    // Never dispatched: no run, no cube activity, still `ready`.
    assert!(db.active_run_ids_for_execution(&execution.id).unwrap().is_empty());
    assert_eq!(cube.ensure_calls.lock().await.len(), 0);
    let held = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready);
    let reason = held.dispatch_wait_reason.unwrap_or_default();
    assert!(
        reason.contains("interactive concurrency cap"),
        "wait reason should name the cap, got: {reason}",
    );

    // One worker frees → the next drain dispatches the held row.
    coordinator.worker_pool().release_worker("worker-1", None).await;
    coordinator.drain_ready_queue().await;
    assert!(
        !db.active_run_ids_for_execution(&execution.id).unwrap().is_empty(),
        "row must dispatch once the pool drops below the cap",
    );
}

/// PR3 routing: an execution pinned to a host that is registered but
/// disabled finds no eligible host. The dispatch records a
/// `no_eligible_host` pre-start failure (leaving the row recoverable)
/// and never starts a run.
#[tokio::test]
async fn pin_to_disabled_host_yields_no_eligible_host() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
    db.set_host_enabled("zakalwe", false).unwrap();

    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Pinned to disabled");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    // No retry delays → a single pre-start failure is terminal, so the
    // assertion doesn't race a backoff timer.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&execution.id, None)
        .await
        .expect("worker available");
    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "no eligible host must fail the dispatch");

    // No worker run was ever started, and no cube work happened.
    assert!(db.active_run_ids_for_execution(&execution.id).unwrap().is_empty());
    assert_eq!(cube.ensure_calls.lock().await.len(), 0);
    // The failure surfaced as a `no_eligible_host` attention item.
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == "no_eligible_host"),
        "expected a no_eligible_host attention item, got {:?}",
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
    );
}

/// The `no_eligible_host` pre-start failure used to emit NO dispatch
/// event, so the per-execution timeline went silent after
/// `worker_claimed` and the stall watchdog mislabelled it a
/// `worker_claimed` stall ~30s later (the exact shape that hid the
/// automation-pool stall). It must now emit a terminal
/// `host_selected:error` carrying the reason, so the blocker is named
/// in dispatch.jsonl and — because the watchdog treats any `error`
/// outcome as terminal — it is never re-flagged as a stall.
#[tokio::test]
async fn no_eligible_host_emits_terminal_host_selected_error_event() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
    db.set_host_enabled("zakalwe", false).unwrap();

    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Pinned to disabled");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    db.set_execution_pinned_host(&execution.id, Some("zakalwe")).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&execution.id, None)
        .await
        .expect("worker available");
    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "no eligible host must fail the dispatch");

    let events = recording.events_for(&execution.id).await;
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .unwrap_or_else(|| panic!("expected a host_selected event; got {events:#?}"));
    assert_eq!(
        host_selected.outcome, "error",
        "no_eligible_host must surface as host_selected:error",
    );
    assert_eq!(
        host_selected.details.get("reason").and_then(|v| v.as_str()),
        Some("no_eligible_host"),
        "host_selected:error must name the blocker reason",
    );
    assert!(
        host_selected.error_message.is_some(),
        "host_selected:error must carry the ineligibility detail",
    );
    // Terminal for the stall watchdog (any error outcome is terminal),
    // so the silent `worker_claimed` stall can never re-present.
    assert!(
        crate::dispatch_reader::is_terminal_event(host_selected),
        "host_selected:error must be a terminal dispatch event",
    );
    // The failure short-circuited before any cube repo work.
    assert_eq!(cube.ensure_calls.lock().await.len(), 0);
}

/// `cube_default_workspace_root_for_test` mirrors the production
/// helper so tests can construct a `workspace_root` value that
/// `workspace_root_is_cube_default` would accept, without
/// mutating process-wide env vars (which would race other tests
/// in the same crate).
fn cube_default_workspace_root_for_test() -> PathBuf {
    if let Some(d) = std::env::var_os("CUBE_DATA_DIR") {
        return PathBuf::from(d).join("workspaces");
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(d).join("cube/workspaces");
    }
    let home = std::env::var_os("HOME").expect(
        "test requires HOME, CUBE_DATA_DIR, or XDG_DATA_HOME to be set so we can \
             construct a cube-default workspace_root that the helper recognises",
    );
    PathBuf::from(home).join(".local/share/cube/workspaces")
}

/// Q6 / Follow-up chore #8: the cold-repo probe raises an
/// advisory `repo_cold_pool` attention item on the first dispatch
/// against a previously-unseen URL whose cube pool config matches
/// auto-provision defaults. Across two dispatches against the
/// same URL only one item is written, and `cube repo list` is
/// only called once — both dispatches still drive the execution
/// to `running`.
#[tokio::test]
async fn cold_repo_probe_raises_advisory_once_across_repeated_dispatches() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let origin = "git@github.com:spinyfin/mono.git";
    let product = create_test_product_with_repo(&db, "Boss", Some(origin));
    // Two chores → two executions against the same product/URL.
    let chore_a = create_test_chore(&db, product.id.clone(), "Cleanup A");
    let chore_b = create_test_chore(&db, product.id.clone(), "Cleanup B");
    db.reconcile_product_executions(&product.id).unwrap();

    // Cube reports a single repo whose pool config exactly
    // matches the auto-provisioned defaults — `cube repo add`
    // / `cube repo configure` were never run.
    let default_repo = CubeRepoSummary {
        repo_id: "mono".to_owned(),
        origin: origin.to_owned(),
        main_branch: "main".to_owned(),
        workspace_root: cube_default_workspace_root_for_test(),
        workspace_prefix: "mono-agent-".to_owned(),
        source: None,
    };
    let cube = Arc::new(FakeCubeClient::default().with_repos(vec![default_repo]));
    // Pool size 2 so both executions can dispatch concurrently.
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    let exec_a = db.list_executions(Some(&chore_a.id)).unwrap().pop().unwrap();
    let exec_b = db.list_executions(Some(&chore_b.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &exec_a.id, ExecutionStatus::Running).await;
    wait_for_execution_status(db.as_ref(), &exec_b.id, ExecutionStatus::Running).await;

    // Two ensure_repo calls (one per execution), but list_repos
    // was deduplicated to exactly one round-trip.
    assert_eq!(cube.ensure_calls.lock().await.len(), 2);
    assert_eq!(*cube.list_repos_calls.lock().await, 1);

    // Exactly one advisory item across both executions. It
    // attaches to the execution that hit the probe first.
    let attn_a = db.list_attention_items(&exec_a.id).unwrap();
    let attn_b = db.list_attention_items(&exec_b.id).unwrap();
    let cold_items: Vec<_> = attn_a
        .iter()
        .chain(attn_b.iter())
        .filter(|item| item.kind == "repo_cold_pool")
        .collect();
    assert_eq!(
        cold_items.len(),
        1,
        "expected exactly one repo_cold_pool item across both executions, \
             got {} (exec_a: {} items, exec_b: {} items)",
        cold_items.len(),
        attn_a.len(),
        attn_b.len(),
    );
    let item = cold_items[0];
    assert_eq!(item.status, "open");
    assert!(
        item.body_markdown
            .contains("cube repo ensure --origin git@github.com:spinyfin/mono.git"),
        "body should name the override command verbatim; got: {}",
        item.body_markdown,
    );
    assert!(
        item.body_markdown.contains(origin),
        "body should echo the repo origin; got: {}",
        item.body_markdown,
    );
}

/// A repo whose cube pool config has been customised (custom
/// `workspace_root` or `workspace_prefix`) is the steady-state we
/// don't want to nag about. Even though it's the first dispatch
/// in this engine's lifetime, no `repo_cold_pool` item should
/// land.
#[tokio::test]
async fn cold_repo_probe_stays_silent_when_pool_is_customised() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let origin = "git@github.com:spinyfin/mono.git";
    let product = create_test_product_with_repo(&db, "Boss", Some(origin));
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let custom_repo = CubeRepoSummary {
        repo_id: "mono".to_owned(),
        origin: origin.to_owned(),
        main_branch: "main".to_owned(),
        workspace_root: PathBuf::from("/Users/operator/Documents/dev/workspaces"),
        workspace_prefix: "mono-agent-".to_owned(),
        source: None,
    };
    let cube = Arc::new(FakeCubeClient::default().with_repos(vec![custom_repo]));
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    assert_eq!(*cube.list_repos_calls.lock().await, 1);
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().all(|i| i.kind != "repo_cold_pool"),
        "no repo_cold_pool item should be raised for a customised pool; got: {:?}",
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
    );
}

#[test]
fn repo_has_default_pool_config_recognises_defaults_only() {
    use super::helpers::{CubeRepoSummary, repo_has_default_pool_config};
    // A repo whose every field matches the auto-provisioned
    // defaults — the case the probe should flag.
    let default_root = cube_default_workspace_root_for_test();
    let base = CubeRepoSummary {
        repo_id: "nimbus".to_owned(),
        origin: "git@github.com:myorg/nimbus.git".to_owned(),
        main_branch: "main".to_owned(),
        workspace_root: default_root.clone(),
        workspace_prefix: "nimbus-agent-".to_owned(),
        source: None,
    };
    assert!(repo_has_default_pool_config(&base));

    // A custom main_branch means the operator has touched the
    // config — stay silent.
    let mut customised = base.clone();
    customised.main_branch = "trunk".to_owned();
    assert!(!repo_has_default_pool_config(&customised));

    // `source` overlay means the user is sharing a local clone;
    // pool is explicitly configured.
    let mut with_source = base.clone();
    with_source.source = Some(PathBuf::from("/Users/dev/Documents/dev/nimbus"));
    assert!(!repo_has_default_pool_config(&with_source));

    // Custom workspace_prefix that doesn't match the auto-derived
    // `{repo_id}-agent-` shape.
    let mut custom_prefix = base.clone();
    custom_prefix.workspace_prefix = "nimbus-pool-".to_owned();
    assert!(!repo_has_default_pool_config(&custom_prefix));

    // Custom workspace_root anywhere outside cube's data dir.
    let mut custom_root = base;
    custom_root.workspace_root = PathBuf::from("/Users/dev/Documents/dev/workspaces");
    assert!(!repo_has_default_pool_config(&custom_root));
}

#[tokio::test]
async fn slot_id_from_outcome_is_stamped_onto_run_agent_id() {
    // When the runner reports a real pane slot back via
    // RunOutcome.slot_id, the coordinator must overwrite the run
    // record's `agent_id` with `worker-{slot}` before recording
    // completion. This is what makes `bossctl agents list` show
    // one entry per active pane instead of collapsing every
    // dispatched run into the worker-pool placeholder.
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    // Pool has only one slot, so the worker-pool placeholder
    // would otherwise be `worker-1`. The runner reports slot 5
    // — the assertion below proves the slot value won, not the
    // pool placeholder.
    let runner = Arc::new(FakeExecutionRunner {
        slot_id: Some(5),
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.status, "completed");
    assert_eq!(run.agent_id, "worker-5");
}

#[tokio::test]
async fn pane_spawn_run_does_not_release_worker_pool_slot() {
    // The libghostty pane outlives the `run_execution` call —
    // PaneSpawnRunner returns Ok(WaitingHuman) the instant the
    // SpawnWorkerPane RPC completes, but the user-visible worker
    // is just getting started. If the coordinator freed the
    // WorkerPool slot at that moment, the next dispatch could
    // re-claim the slot and the app would reject the spawn with
    // SlotBusy. Outcomes that carry slot_id = Some(N) must keep
    // the slot claimed until `release_worker_pane` fires.
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        slot_id: Some(1),
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    // Slot 1 still belongs to the (notionally) live pane. Only
    // `release_worker_pane` (driven by completion / force release
    // / shutdown) is allowed to free it.
    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "WorkerPool slot must stay claimed while the libghostty pane is alive"
    );
}

#[tokio::test]
async fn release_worker_and_kick_frees_pool_slot() {
    // The deferred-release helper called from
    // `ServerState::release_worker_pane` after the pane RPC
    // returns. After it runs, the matching pool slot is idle
    // again and the next claim succeeds.
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(2),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    let claimed = coordinator
        .worker_pool()
        .claim_worker("exec-pre", None)
        .await
        .expect("pool has free slots");
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);

    coordinator.release_worker_and_kick(&claimed, Some("ws-1")).await;

    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        2,
        "release_worker_and_kick must return the slot to the idle pool",
    );
    // Idempotent: a second release on the same already-idle slot
    // is a no-op (the pane-spawn lifecycle can racily re-enter
    // this path from completion + chore-done).
    coordinator.release_worker_and_kick(&claimed, Some("ws-1")).await;
    assert_eq!(coordinator.worker_pool().idle_count().await, 2);
}

#[tokio::test]
async fn missing_slot_id_leaves_worker_pool_placeholder_in_agent_id() {
    // Runners without a pane leave slot_id = None. The coordinator
    // must not touch agent_id in that case — the worker-pool
    // placeholder set at run-create time stays.
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.agent_id, "worker-1");
}

#[tokio::test]
async fn successful_run_moves_execution_to_waiting_human_and_releases_worker() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.status, "completed");
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);
    assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);
}

#[tokio::test]
async fn start_failure_marks_execution_failed_and_releases_worker() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![]),
    );
    coordinator.kick();
    wait_for_execution_status(
        db.as_ref(),
        &db.list_executions(Some(&chore.id)).unwrap()[0].id,
        ExecutionStatus::Failed,
    )
    .await;

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
    assert_eq!(run.status, "failed");
    assert_eq!(run.error_text.as_deref(), Some("cube workspace lease failed"));
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);
}
