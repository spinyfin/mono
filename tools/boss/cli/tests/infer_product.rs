//! End-to-end coverage for `boss` CLI product inference. Spawns an
//! in-process engine on a temp socket, sets up a product / project /
//! task via `boss-client`, then drives the `boss` binary with typed
//! ids (no `--product`) and checks the response.
//!
//! The fix this exercises: `boss project show proj_…` and
//! `boss task list --project proj_…` previously errored with
//! "product is required" — globally-unique typed ids are enough to
//! locate the product, and the CLI now infers it.

use std::process::Command;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{CreateProductInput, CreateProjectInput, CreateTaskInput};

use common::{boss_binary, run_boss};
use harness::{TestEngine, create_product_with, create_project_with, create_task_with};

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_infers_product_from_typed_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product_with(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project_with(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;

    // The bug: `project show proj_…` errored with
    // "product is required" even though the id is globally unique.
    let value = run_boss(engine.socket_str(), &["project", "show", &project.id])?;
    assert_eq!(value["project"]["id"].as_str(), Some(project.id.as_str()),);
    assert_eq!(value["project"]["product_id"].as_str(), Some(product.id.as_str()),);
    Ok(())
}

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_infers_product_from_project_typed_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product_with(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project_with(
        &mut client,
        CreateProjectInput {
            product_id: product.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;
    let task = create_task_with(
        &mut client,
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(project.id.clone())
            .name("wire it up")
            .autostart(false)
            .build(),
    )
    .await?;

    let value = run_boss(engine.socket_str(), &["task", "list", "--project", &project.id])?;
    let tasks = value["tasks"]
        .as_array()
        .ok_or_else(|| anyhow!("expected `tasks` array in CLI output: {value}"))?;
    // Two rows: the auto-created design task plus the explicit one
    // we just inserted. Both belong to the inferred product.
    assert!(tasks.iter().any(|t| t["id"].as_str() == Some(&task.id)));
    assert!(tasks.iter().all(|t| t["product_id"].as_str() == Some(&product.id)));
    Ok(())
}

/// When the user supplies both `--product` and a typed id whose
/// product disagrees, the CLI must refuse instead of silently
/// favouring one side. The error names both sides so the redundant
/// `--product` is easy to drop.
// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_rejects_disagreeing_explicit_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let primary = create_product_with(
        &mut client,
        CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let other = create_product_with(
        &mut client,
        CreateProductInput {
            name: "Mono".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;
    let project = create_project_with(
        &mut client,
        CreateProjectInput {
            product_id: primary.id.clone(),
            name: "Phase 1".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await?;

    let output = Command::new(boss_binary())
        .args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
            "project",
            "show",
            "--product",
            &other.slug,
            &project.id,
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "mismatch must exit non-zero, stdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&project.id) && stderr.contains(&primary.id),
        "error must name both products: {stderr}"
    );
    Ok(())
}
