//! Integration tests for durable state recovered during engine startup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::{ClaimPlannerRunInput, PLANNER_RUN_ENGINE_RESTART_SUMMARY, WorkDb};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, ExecutionKind, ExecutionStatus,
    PLANNER_OUTCOME_PLANNER_FAILED, RequestExecutionInput,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn serve_quarantines_historical_local_workers_before_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");
    let db = WorkDb::open(db_path.clone())?;
    let product = db.create_product(
        CreateProductInput::builder()
            .name("Historical local workers")
            .repo_remote_url("https://example.invalid/historical.git")
            .build(),
    )?;
    let mut executions = Vec::new();
    for (name, pid) in [
        ("live", Some(i64::from(std::process::id()))),
        ("unknown", None),
        ("dead", Some(i64::from(i32::MAX))),
    ] {
        let chore = db.create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(name)
                .build(),
        )?;
        let execution = db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id).build())?;
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "historical-repo",
            "expired-lease",
            "historical-workspace",
            temp.path().to_str().unwrap(),
        )?;
        if let Some(pid) = pid {
            db.set_run_shell_pid_for_execution(&execution.id, pid)?;
        }
        executions.push(execution);
    }
    let work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(db_path)
        .build();
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));
    let bound_socket = socket_path.clone();
    let join = tokio::spawn(async move { serve(cfg, bound_socket, None, None, None, None).await });
    let bound = wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await;
    // The socket binds before post-bind orphan recovery. Wait for the dead
    // worker's durable outcome rather than aborting startup in that gap.
    let recovered = async {
        if !bound {
            return Err(anyhow!("engine never bound its isolated frontend socket"));
        }
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while db.get_execution(&executions[2].id)?.status != ExecutionStatus::Orphaned {
            if Instant::now() >= deadline {
                return Err(anyhow!("startup did not orphan the proven-dead historical worker"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
    .await;
    // Stop the isolated engine before assertions, including on failure.
    join.abort();
    let _ = join.await;
    recovered?;

    for execution in &executions[..2] {
        assert_eq!(db.get_execution(&execution.id)?.status, ExecutionStatus::Running);
        assert!(db.mark_execution_orphaned(&execution.id, "expired lease").is_err());
        assert!(
            db.request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(execution.work_item_id.clone())
                    .build()
            )
            .is_err()
        );
        assert!(
            db.list_attention_items_for_work_item(&execution.work_item_id)?
                .iter()
                .any(|item| item.kind == "local_worker_startup_quarantine" && item.status == "open")
        );
    }
    assert_eq!(db.get_execution(&executions[2].id)?.status, ExecutionStatus::Orphaned);
    Ok(())
}

/// Engine startup moves local capability discovery out of schema init so the
/// frontend can bind first, but it must still replace the cleared auto rows.
#[tokio::test]
async fn serve_discovers_local_capabilities_after_binding_the_socket() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");
    WorkDb::open(db_path.clone())?;

    let work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(db_path.clone())
        .build();
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));
    let join = tokio::spawn(async move { serve(cfg, socket_path, None, None, None, None).await });

    let socket_for_wait = temp.path().join("engine.sock");
    if !wait_for_socket(socket_for_wait.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound socket {}", socket_for_wait.display()));
    }

    let recovered_db = WorkDb::open(db_path)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let capabilities = recovered_db.list_host_capabilities("local")?;
        if capabilities
            .iter()
            .any(|capability| capability.source == "auto" && capability.capability == "drivers-probed=true")
        {
            break;
        }
        if Instant::now() >= deadline {
            join.abort();
            return Err(anyhow!(
                "startup did not restore the local drivers-probed=true auto capability within {STARTUP_TIMEOUT:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    join.abort();
    Ok(())
}

/// Once the frontend socket is usable, recovery has completed: the inherited
/// run is terminal, its project can be claimed again, and its follow-up is
/// visible through the attention-list query path.
#[tokio::test]
async fn serve_recovers_running_planner_runs_by_the_time_frontend_is_usable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");
    let db = WorkDb::open(db_path.clone())?;
    let product = db.create_product(
        CreateProductInput::builder()
            .name("Test")
            .repo_remote_url("git@github.com:test/test.git")
            .build(),
    )?;
    let project = db.create_project(
        CreateProjectInput::builder()
            .product_id(product.id.clone())
            .name("Alpha")
            .goal("build it")
            .build(),
    )?;
    let design_task = db.create_task(
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(project.id.clone())
            .name("Design")
            .build(),
    )?;
    let stranded = db
        .claim_planner_run(ClaimPlannerRunInput {
            project_id: &project.id,
            product_id: &product.id,
            design_task_id: Some(&design_task.id),
            caller: "merge_trigger",
        })?
        .expect("fixture claim must create a running planner row");
    drop(db);

    let work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(db_path.clone())
        .build();
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));
    let join = tokio::spawn(async move { serve(cfg, socket_path.clone(), None, None, None, None).await });

    let socket_for_wait = temp.path().join("engine.sock");
    if !wait_for_socket(socket_for_wait.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound socket {}", socket_for_wait.display()));
    }

    let recovered_db = WorkDb::open(db_path)?;
    let recovered = recovered_db
        .get_planner_run(&stranded.id)?
        .expect("startup recovery must retain the audit row");
    assert_eq!(recovered.outcome, PLANNER_OUTCOME_PLANNER_FAILED);
    assert_eq!(
        recovered.result_summary.as_deref(),
        Some(PLANNER_RUN_ENGINE_RESTART_SUMMARY)
    );
    assert!(
        recovered_db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project.id,
                product_id: &product.id,
                design_task_id: None,
                caller: "operator",
            })?
            .is_some(),
        "startup recovery must release the project before frontend serving"
    );
    let groups =
        recovered_db.list_attention_groups(&product.id, None, Some(&design_task.id), Some("followup"), None)?;
    assert_eq!(
        groups.len(),
        1,
        "recovered Planner runs must be visible as follow-up attention"
    );

    join.abort();
    Ok(())
}

// ── Review-guide attempt reconciliation (design:
// automatic-pr-review-guides.md, "On restart, resume ready requests from the
// database and reconcile running attempts against their specific execution
// binding"). `WorkDb::create_pr_review_guide_attempt` and
// `dispatch_pr_review_guide_attempt` are crate-internal, so these fixtures
// seed the durable rows with raw SQL exactly like
// `tests/guide_comments_crud.rs` does for the same tables — the schema is
// the one `work::review_guide_jobs`/`work::review_guide_sources` own.

/// Seed a root chore with a repository, plus a captured, complete source
/// comparison for `pr_url`. Returns `(root_task_id, series_id, comparison_id)`.
///
/// `WorkDb::connect` is crate-internal, so the raw-table inserts below open
/// their own `rusqlite::Connection` against the same on-disk file — exactly
/// how `tests/guide_comments_crud.rs` seeds these same tables.
fn seed_review_guide_root_and_comparison(
    db: &WorkDb,
    db_path: &std::path::Path,
    pr_url: &str,
) -> Result<(String, String, String)> {
    // The product's own repo is deliberately DIFFERENT from the task-level
    // override below: `migrate_null_redundant_task_repo_remote_urls`
    // re-runs on every `WorkDb::open` and NULLs any task-level
    // `repo_remote_url` that merely mirrors its product's own repo (a
    // cleanup for a historical creation-time bug). A product needs some
    // repo for chore creation to succeed at all, so a same-valued override
    // would look exactly like that historical bug and get cleaned up right
    // back out from under this fixture on the engine's next startup.
    let product = db.create_product(
        CreateProductInput::builder()
            .name("Review guide restart test")
            .repo_remote_url("https://github.com/acme/widget-product-default.git")
            .build(),
    )?;
    let chore = db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Review guide restart test root")
            .build(),
    )?;
    let conn = rusqlite::Connection::open(db_path)?;
    let updated = conn.execute(
        "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
        rusqlite::params!["https://github.com/acme/widget.git", chore.id],
    )?;
    anyhow::ensure!(
        updated == 1,
        "expected the repo_remote_url UPDATE to match exactly the seeded chore row {}, matched {updated}",
        chore.id
    );
    let series_id = "prgs_restart_test".to_owned();
    let comparison_id = "prgc_restart_test".to_owned();
    conn.execute(
        "INSERT INTO pr_review_guide_source_series
         (id, root_task_id, canonical_pr_url, latest_observation_sequence, selected_comparison_id,
          request_epoch, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, ?4, 1, '1', '1')",
        rusqlite::params![series_id, chore.id, pr_url, comparison_id],
    )?;
    conn.execute(
        "INSERT INTO pr_review_guide_source_comparisons
         (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
          trigger, packet_hash, complete, captured_at)
         VALUES (?1, ?2, 1, 'base', 'base', 'head', 'creation', 'packet-hash', 1, '1')",
        rusqlite::params![comparison_id, series_id],
    )?;
    Ok((chore.id, series_id, comparison_id))
}

fn attempt_status(db_path: &std::path::Path, attempt_id: &str) -> Result<String> {
    rusqlite::Connection::open(db_path)?
        .query_row(
            "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn attempt_execution_id(db_path: &std::path::Path, attempt_id: &str) -> Result<Option<String>> {
    rusqlite::Connection::open(db_path)?
        .query_row(
            "SELECT execution_id FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

/// A queued attempt with no bound execution — the exact shape a crash
/// between `create_pr_review_guide_attempt` and `dispatch_pr_review_guide_attempt`
/// leaves behind — must be dispatched (given a bound execution) once the
/// engine's post-bind `reconcile_pr_review_guide_attempts` pass runs at
/// startup, without waiting for the app to reconnect or the human to open a
/// viewer.
#[tokio::test]
async fn serve_dispatches_a_stranded_queued_review_guide_attempt_on_restart() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");
    let db = WorkDb::open(db_path.clone())?;
    let (_root, series_id, comparison_id) =
        seed_review_guide_root_and_comparison(&db, &db_path, "https://github.com/acme/widget/pull/9")?;
    let attempt_id = "prga_restart_test".to_owned();
    rusqlite::Connection::open(&db_path)?.execute(
        "INSERT INTO pr_review_guide_attempts
         (id, series_id, comparison_id, request_epoch, ordinal, execution_id, status, prompt_version, created_at)
         VALUES (?1, ?2, ?3, 1, 1, NULL, 'queued', 'review-guide-v1', '1')",
        rusqlite::params![attempt_id, series_id, comparison_id],
    )?;
    drop(db);

    let work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(db_path.clone())
        .build();
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));
    let bound_socket = socket_path.clone();
    let join = tokio::spawn(async move { serve(cfg, bound_socket, None, None, None, None).await });

    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound its isolated frontend socket"));
    }

    let recovered_db = WorkDb::open(db_path.clone())?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let dispatched = loop {
        if let Some(execution_id) = attempt_execution_id(&db_path, &attempt_id)? {
            break execution_id;
        }
        if Instant::now() >= deadline {
            join.abort();
            let (status, error, retries): (String, Option<String>, i64) = rusqlite::Connection::open(&db_path)?
                .query_row(
                    "SELECT status, error, retries FROM pr_review_guide_attempts WHERE id = ?1",
                    [&attempt_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            return Err(anyhow!(
                "startup did not dispatch the stranded queued review-guide attempt: status={status} error={error:?} retries={retries}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    join.abort();

    let execution = recovered_db.get_execution(&dispatched)?;
    assert_eq!(execution.kind, ExecutionKind::PrReviewGuide);
    assert_eq!(execution.work_item_id, comparison_id);
    Ok(())
}

/// An attempt whose bound execution has already gone terminal out-of-band —
/// e.g. the general orphan/lease-loss sweep marked it `cancelled` — but
/// whose own `pr_review_guide_attempts.status` is still `running` because
/// the engine crashed before `reconcile_pr_review_guide_attempts` observed
/// it, must be finished as `cancelled` on the very next startup, so the
/// series is not stuck reporting an in-flight generation that is never
/// coming back.
#[tokio::test]
async fn serve_finishes_a_review_guide_attempt_whose_execution_vanished_before_restart() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("engine.sock");
    let db_path = temp.path().join("state.db");
    let db = WorkDb::open(db_path.clone())?;
    let (_root, series_id, comparison_id) =
        seed_review_guide_root_and_comparison(&db, &db_path, "https://github.com/acme/widget/pull/9")?;
    drop(db);
    // `WorkDb::create_execution`'s generic resolver assumes `work_item_id`
    // is a task, but a `pr_review_guide` execution's `work_item_id` is a
    // comparison id (see `review_guide_jobs::insert_review_guide_execution`,
    // crate-internal) — seed the row with the same raw shape that function
    // writes, already terminal (`cancelled`) as if the general orphan/
    // lease-loss sweep reaped it before a crash prevented the
    // review-guide-specific reconcile from ever observing the transition.
    let execution_id = "exec_restart_test_vanished".to_owned();
    let conn = rusqlite::Connection::open(&db_path)?;
    conn.execute(
        "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
                allow_dirty, branch_naming
             ) VALUES (?1, ?2, 'pr_review_guide', 'cancelled', ?3, NULL, NULL, NULL, NULL, 0, NULL, '1', NULL, NULL, 0, NULL, NULL, 0, '{}')",
        rusqlite::params![execution_id, comparison_id, "https://github.com/acme/widget.git"],
    )?;
    let attempt_id = "prga_restart_test_vanished".to_owned();
    conn.execute(
        "INSERT INTO pr_review_guide_attempts
         (id, series_id, comparison_id, request_epoch, ordinal, execution_id, status, prompt_version, created_at)
         VALUES (?1, ?2, ?3, 1, 1, ?4, 'running', 'review-guide-v1', '1')",
        rusqlite::params![attempt_id, series_id, comparison_id, execution_id],
    )?;
    drop(conn);

    let work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(db_path.clone())
        .build();
    let cfg = Arc::new(RuntimeConfig::from_parts(work, None));
    let bound_socket = socket_path.clone();
    let join = tokio::spawn(async move { serve(cfg, bound_socket, None, None, None, None).await });

    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound its isolated frontend socket"));
    }

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let status = attempt_status(&db_path, &attempt_id)?;
        if status != "running" {
            assert_eq!(
                status, "cancelled",
                "a cancelled execution must fail the attempt as cancelled, not failed"
            );
            break;
        }
        if Instant::now() >= deadline {
            join.abort();
            return Err(anyhow!(
                "startup did not reconcile the review-guide attempt whose execution vanished"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    join.abort();
    Ok(())
}
