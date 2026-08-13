//! Integration tests for durable state recovered during engine startup.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::{ClaimPlannerRunInput, PLANNER_RUN_ENGINE_RESTART_SUMMARY, WorkDb};
use boss_protocol::{CreateProductInput, CreateProjectInput, CreateTaskInput, PLANNER_OUTCOME_PLANNER_FAILED};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

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
