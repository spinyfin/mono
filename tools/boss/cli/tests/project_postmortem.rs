use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{CreateProductInput, CreateProjectInput, CreateTaskInput, SetProjectDesignDocInput, WorkItemPatch};
use common::{run_boss, run_boss_expect_failure, run_boss_human};
use harness::TestEngine;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postmortem_command_refuses_open_work_then_starts_once() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    // `boss project postmortem` works with the sweep flag disabled.
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
    let shown = run_boss(engine.socket_str(), &["project", "show", &project.id])?;
    assert_eq!(shown["tasks"][0]["id"], task.id);
    let shown = run_boss_human(engine.socket_str(), &["project", "show", &project.id])?;
    assert!(shown.contains("Open work"), "{shown}");
    let error = run_boss_expect_failure(engine.socket_str(), &args)?;
    assert!(error.contains("1 open task(s) remain"), "{error}");
    assert!(db.last_design_postmortem_for_project(&project.id)?.is_none());
    db.delete_work_item(&task.id)?;
    // A project with no completed implementation work has nothing to review.
    let error = run_boss_expect_failure(engine.socket_str(), &args)?;
    assert!(error.contains("no implementation work completed"), "{error}");
    let done = db.create_task(
        CreateTaskInput::builder()
            .product_id(&product.id)
            .project_id(&project.id)
            .name("Done work")
            .autostart(false)
            .build(),
    )?;
    db.update_work_item(
        &done.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..WorkItemPatch::default()
        },
    )?;
    let created = run_boss(engine.socket_str(), &args)?;
    assert_eq!(created["created"], true);
    assert_eq!(created["task"]["kind"], "design_postmortem");
    let again = run_boss(engine.socket_str(), &args)?;
    assert_eq!(again["created"], false);
    assert_eq!(again["task"]["id"], created["task"]["id"]);
    let message = run_boss_human(engine.socket_str(), &args)?;
    assert!(message.contains("already exists"), "{message}");
    assert!(message.contains("no-op"), "{message}");
    // Deleting the postmortem must not disable the command.
    db.delete_work_item(created["task"]["id"].as_str().unwrap())?;
    let recreated = run_boss(engine.socket_str(), &args)?;
    assert_eq!(recreated["created"], true);
    assert_ne!(recreated["task"]["id"], created["task"]["id"]);
    Ok(())
}
