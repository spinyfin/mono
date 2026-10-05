use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{CreateProductInput, CreateProjectInput, CreateTaskInput, SetProjectDesignDocInput};
use common::{run_boss, run_boss_expect_failure, run_boss_human};
use harness::TestEngine;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postmortem_command_refuses_open_work_then_starts_once() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    // The operator command remains available with automatic scheduling off.
    // This also keeps the background sweep from racing the explicit start.
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&boss_protocol::FrontendRequest::SetFeatureFlag {
            name: "project_postmortem_sweep".into(),
            enabled: false,
        })
        .await?;
    assert!(
        matches!(response, boss_protocol::FrontendEvent::FeatureFlagSet { .. }),
        "{response:?}"
    );
    let db = engine.db()?;
    let product = db.create_product(
        CreateProductInput::builder()
            .name("Postmortems")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )?;
    let project = db.create_project(
        CreateProjectInput::builder()
            .product_id(&product.id)
            .name("Review")
            .no_design_task(true)
            .build(),
    )?;
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_path: Some("docs/design.md".into()),
        ..Default::default()
    })?;
    let task = db.create_task(
        CreateTaskInput::builder()
            .product_id(&product.id)
            .project_id(&project.id)
            .name("Open work")
            .autostart(false)
            .build(),
    )?;
    let args = ["project", "postmortem", &project.id];
    let error = run_boss_expect_failure(engine.socket_str(), &args)?;
    assert!(error.contains("1 open task(s) remain"), "{error}");
    assert!(db.last_design_postmortem_for_project(&project.id)?.is_none());
    db.delete_work_item(&task.id)?;
    let created = run_boss(engine.socket_str(), &args)?;
    assert_eq!(created["created"], true);
    assert_eq!(created["task"]["kind"], "design_postmortem");
    let again = run_boss(engine.socket_str(), &args)?;
    assert_eq!(again["created"], false);
    assert_eq!(again["task"]["id"], created["task"]["id"]);
    let message = run_boss_human(engine.socket_str(), &args)?;
    assert!(message.contains("already exists"), "{message}");
    assert!(message.contains("no-op"), "{message}");
    Ok(())
}
