//! Dependency cycle errors survive RPC transport and fail the CLI command.
use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{AddDependencyInput, CreateProductInput, FrontendEvent, FrontendRequest};
use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_product_with};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_cycle_has_structured_path_and_cli_exits_nonzero() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Cycle checks")
            .repo_remote_url("git@example.com:cycles.git")
            .build(),
    )
    .await?;
    let schema = create_chore(&mut client, &product.id, "Schema").await?;
    let handler = create_chore(&mut client, &product.id, "Handler").await?;
    run_boss(engine.socket_str(), &["task", "depend", "add", &handler.id, &schema.id])?;
    let reply = client
        .send_request(&FrontendRequest::AddDependency {
            input: AddDependencyInput {
                dependent: schema.id.clone(),
                prerequisite: handler.id.clone(),
                relation: None,
            },
        })
        .await?;
    let FrontendEvent::DependencyCycleRejected { error } = reply else {
        return Err(anyhow!("expected structured cycle error, got {reply:?}"));
    };
    assert_eq!(
        error.path.iter().map(|step| step.id.as_str()).collect::<Vec<_>>(),
        [&schema.id, &handler.id]
    );
    assert_eq!(
        error.path.iter().map(|step| step.name.as_str()).collect::<Vec<_>>(),
        ["Schema", "Handler"]
    );
    assert!(error.path.iter().all(|step| step.relation == "blocks"));
    let stderr = run_boss_expect_failure(engine.socket_str(), &["task", "depend", "add", &schema.id, &handler.id])?;
    assert!(stderr.contains(&error.to_string()), "{stderr}");
    let stderr = run_boss_expect_failure(engine.socket_str(), &["task", "depend", "add", &schema.id, &schema.id])?;
    assert!(stderr.contains("dependency cycle"), "{stderr}");
    assert!(
        stderr.contains(&schema.id) && stderr.contains("Schema") && stderr.contains("blocks"),
        "{stderr}"
    );
    Ok(())
}
