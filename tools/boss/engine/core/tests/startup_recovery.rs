//! Integration tests for durable state recovered during engine startup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::{ClaimPlannerRunInput, PLANNER_RUN_ENGINE_RESTART_SUMMARY, WorkDb};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, ExecutionStatus,
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
