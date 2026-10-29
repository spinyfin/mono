//! Lease hand-back across pre-start retries: a lease kept or failed by one
//! attempt must neither be released without recovery proof nor wedge the
//! next attempt. Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use crate::workspace_lease_release::{LEASE_RELEASE_FAILED_ATTENTION_KIND, LEASE_UNRELEASED_ATTENTION_KIND};

fn open_kinds(db: &WorkDb, execution_id: &str) -> Vec<String> {
    db.list_attention_items(execution_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.status == "open")
        .map(|item| item.kind)
        .collect()
}

async fn wait_for_runner_calls(runner: &FakeExecutionRunner, expected: usize) {
    for _ in 0..1000 {
        if runner.calls.lock().await.len() >= expected {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("runner never reached {expected} call(s)");
}

fn pr_review_execution(db: &WorkDb) -> WorkExecution {
    let (_, chore_id) = make_pr_review_fixture(db, Some("https://github.com/spinyfin/mono/pull/7"));
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id)
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

async fn schedule(coordinator: &Arc<ExecutionCoordinator>, execution: &WorkExecution) -> Result<()> {
    let worker_id = coordinator
        .pool_for_execution(execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");
    coordinator
        .schedule_execution(execution, &worker_id, DispatchAdmission::Queued)
        .await
        .map(|_| ())
}

/// Positioning fails once, the retry policy has a real backoff, and the next
/// dispatch of the same execution reaches the spawn.
#[tokio::test]
async fn positioning_failure_then_success_reaches_spawn() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution = pr_review_execution(&db);
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_goto: 1,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(2));
    let coordinator = Arc::new(coord.with_pre_start_retry_delays(vec![Duration::from_millis(5)]));

    assert!(schedule(&coordinator, &execution).await.is_err());
    assert_eq!(
        cube.release_calls.lock().await.len(),
        1,
        "the failed attempt hands its workspace back"
    );
    assert_eq!(db.get_execution(&execution.id).unwrap().cube_lease_id, None);
    assert!(runner.calls.lock().await.is_empty());

    let retry = db.get_execution(&execution.id).unwrap();
    schedule(&coordinator, &retry).await.expect("the retry spawns");
    wait_for_runner_calls(&runner, 1).await;
    assert_eq!(cube.goto_calls.lock().await.len(), 2);
}

/// A sibling that goes live after the lease was taken refuses the spawn,
/// hands the workspace back, and leaves the row dispatchable once the
/// sibling is gone.
#[tokio::test]
async fn post_lease_chain_sibling_refusal_releases_lease_and_redispatch_succeeds() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/1849";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&root_id])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             SELECT 'task_rev_late', product_id, 'revision', 'Late sibling', '', 'todo', '1', '1', ?1
             FROM tasks WHERE id = ?1",
            [&root_id],
        )
        .unwrap();
    }
    let revision = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_late")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();
    // The chain root's writer goes live while the lease call is in flight.
    let sibling = Arc::new(std::sync::Mutex::new(None::<String>));
    let hook_db = db.clone();
    let hook_sibling = sibling.clone();
    let cube = Arc::new(FakeCubeClient {
        on_lease: Some(Arc::new(move || {
            let mut slot = hook_sibling.lock().unwrap();
            if slot.is_none() {
                let live = hook_db
                    .create_execution(
                        CreateExecutionInput::builder()
                            .work_item_id(root_id.clone())
                            .kind(ExecutionKind::ChoreImplementation)
                            .status(ExecutionStatus::Running)
                            .build(),
                    )
                    .unwrap();
                *slot = Some(live.id);
            }
        })),
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(2), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    // Bypass the pre-lease guard the auto-dispatcher runs, as a forced dispatch does.
    let result = schedule(&coordinator, &revision).await;
    let err = format!("{:#}", result.expect_err("the late sibling refuses the spawn"));
    assert!(err.contains("went live"), "{err}");
    assert_eq!(
        cube.release_calls.lock().await.len(),
        1,
        "the refused lease is handed back"
    );
    assert_eq!(db.get_execution(&revision.id).unwrap().cube_lease_id, None);
    assert!(runner.calls.lock().await.is_empty());

    // The sibling finishes; the same execution now dispatches.
    let sibling_id = sibling.lock().unwrap().clone().expect("sibling went live");
    db.cancel_running_execution(&sibling_id).unwrap();
    let after = db.get_execution(&revision.id).unwrap();
    schedule(&coordinator, &after).await.expect("re-dispatch succeeds");
    wait_for_runner_calls(&runner, 1).await;
}

/// A bookmark write fails before any row is recorded. The retry must not hand
/// the workspace back on the strength of the missing row, and once a later
/// attempt writes its own bookmark the stale failure no longer blocks it.
#[tokio::test]
async fn bookmark_failure_retry_does_not_release_without_recovery_proof() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark retry");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_bookmark_creates: 1,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(vec![Duration::from_millis(5)]),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    for _ in 0..1000 {
        if !runner.calls.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(runner.calls.lock().await.len(), 1, "the retry spawns");
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "the lease retained after the bookmark failure is never released without recovery proof"
    );
    let open = open_kinds(&db, &id);
    assert!(
        !open
            .iter()
            .any(|kind| kind == crate::execution_bookmark_recovery::CREATE_FAILED
                || kind == crate::execution_bookmark_recovery::RECOVERY_FAILED),
        "the successful attempt supersedes the earlier failure: {open:?}"
    );
    assert!(
        open.iter().any(|kind| kind == LEASE_UNRELEASED_ATTENTION_KIND),
        "{open:?}"
    );
}

/// Cube fails the pre-start release once and the execution then fails for
/// good. The lease stays recorded and the leftover-lease sweep returns it.
#[tokio::test]
async fn pre_start_cube_release_failure_is_retried_by_the_sweep() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution = pr_review_execution(&db);
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_goto: 1,
        fail_first_n_releases: 1,
        ..FakeCubeClient::default()
    });
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord.with_pre_start_retry_delays(Vec::new()));

    assert!(schedule(&coordinator, &execution).await.is_err());
    let failed = db.get_execution(&execution.id).unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(
        failed.cube_lease_id.as_deref(),
        Some("lease-1"),
        "the lease stays recorded"
    );
    assert!(open_kinds(&db, &execution.id).contains(&LEASE_RELEASE_FAILED_ATTENTION_KIND.to_owned()));

    let live = crate::live_worker_state::LiveWorkerStateRegistry::default();
    let outcome = crate::workspace_lease_release::run_one_pass(&db, cube.as_ref(), &live).await;
    assert_eq!(outcome.released, 1, "{outcome:?}");
    assert_eq!(db.get_execution(&execution.id).unwrap().cube_lease_id, None);
    assert!(
        !open_kinds(&db, &execution.id).contains(&LEASE_RELEASE_FAILED_ATTENTION_KIND.to_owned()),
        "a successful release lowers the attention"
    );
}

/// When only cube's own call failed, the next dispatch keeps the lease
/// recorded instead of detaching it and leasing a second workspace.
#[tokio::test]
async fn retry_after_cube_release_failure_keeps_the_lease_tracked() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let execution = pr_review_execution(&db);
    db.restore_execution_workspace(
        &execution.id,
        "stuck-lease",
        Some("workspace-stuck"),
        Some("/tmp/gone-stuck"),
    )
    .unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_releases: 1,
        ..FakeCubeClient::default()
    });
    *cube.list_workspaces_response.lock().await = vec![
        CubeWorkspaceStatus::builder()
            .workspace_id("workspace-stuck")
            .workspace_path(PathBuf::from("/tmp/gone-stuck"))
            .state("leased")
            .lease_id("stuck-lease")
            .build(),
    ];
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    let repo = CubeRepoHandle { repo_id: "mono".into() };
    let held = db.get_execution(&execution.id).unwrap();
    let err = coordinator
        .lease_workspace_with_fallback(&held, "worker", &repo, "task", &coordinator.host_adapter)
        .await
        .expect_err("a lease cube could not release stays tracked");
    assert!(format!("{err:#}").contains("could not be released"), "{err:#}");
    assert_eq!(
        db.get_execution(&execution.id).unwrap().cube_lease_id.as_deref(),
        Some("stuck-lease")
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no second workspace is leased"
    );
    assert!(open_kinds(&db, &execution.id).contains(&LEASE_RELEASE_FAILED_ATTENTION_KIND.to_owned()));
}
