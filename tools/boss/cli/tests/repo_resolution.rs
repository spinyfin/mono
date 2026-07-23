//! End-to-end coverage for the creation-time repo resolver
//! (`cli/src/repo_resolution.rs`, design §Q4 + follow-up chore #6).
//!
//! Spawns an in-process engine on a temp socket, primes the product
//! with a known-repo set, and drives the `boss` binary through chore
//! create with three signals:
//!
//!   - prompt names a known repo  → resolver picks it (parser step)
//!   - prompt names no known repo → resolver falls through to the
//!     product default
//!   - `--no-input`, product has no default, parser whiffs, no recent
//!     row → resolver errors clearly
//!
//! Together they pin the inference order the way the rest of the
//! system (engine dispatch, app rendering) expects to see it.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{CreateChoreInput, CreateProductInput};

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore_with, create_product_with};

/// Multi-repo product: prompt names a known repo → resolver picks it.
/// This is the "parser" arm of the Q4 inference chain for products
/// that have no default repo of their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_with_prompt_naming_known_repo_auto_resolves() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    // Product has NO repo (multi-repo product). The known-repo set is
    // bootstrapped from the seed chore's row-level override.
    let product = create_product_with(&mut client, CreateProductInput::builder().name("Work").build()).await?;
    create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("seed nimbus")
            .autostart(false)
            .repo_remote_url("git@github.com:foo/nimbus.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "In the nimbus repo, fix the deploy script",
            "--description",
            "",
        ],
    )?;
    assert_eq!(
        created["chore"]["repo_remote_url"].as_str(),
        Some("git@github.com:foo/nimbus.git"),
        "prompt named nimbus → resolver should pick nimbus from the known-repo set"
    );
    Ok(())
}

/// Single-repo product: chore created without --repo stores NULL in
/// `repo_remote_url`. The engine resolves the repo from the product at
/// dispatch time — the row does not need to carry a redundant copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_on_single_repo_product_stores_null_repo_remote_url() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Work")
            .repo_remote_url("git@github.com:foo/console.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Rewrite the welcome docs",
            "--description",
            "",
        ],
    )?;
    assert!(
        created["chore"]["repo_remote_url"].is_null(),
        "single-repo product: task row must store NULL, not the product's URL; got: {}",
        created["chore"]["repo_remote_url"]
    );
    Ok(())
}

/// Acceptance for the "no-input + no resolution" error path. Product
/// has no default, no recent override, prompt mentions no known repo,
/// `--no-input` is on → refuse with an actionable message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_no_input_with_no_resolution_errors_clearly() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(&mut client, CreateProductInput::builder().name("Greenfield").build()).await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "do a thing",
            "--description",
            "",
        ],
    )?;
    assert!(
        stderr.contains("could not resolve repo"),
        "stderr should explain the resolver whiffed: {stderr}"
    );
    assert!(
        stderr.contains("--repo"),
        "stderr should point at the --repo flag: {stderr}"
    );
    assert!(
        stderr.contains(&product.slug) || stderr.contains("product update"),
        "stderr should mention the product or the `product update` remedy: {stderr}"
    );
    Ok(())
}

/// `--repo` on a single-repo product is rejected with a clear error.
/// Products that have their own `repo_remote_url` do not allow per-task
/// overrides; the error message names the product and its repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_explicit_repo_rejected_on_single_repo_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Work")
            .repo_remote_url("git@github.com:foo/console.git")
            .build(),
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "do a thing",
            "--description",
            "",
            "--repo",
            "git@github.com:foo/other.git",
        ],
    )?;
    assert!(
        stderr.contains("cannot set per-task repo override"),
        "stderr should explain the override is not allowed: {stderr}"
    );
    assert!(
        stderr.contains(&product.slug),
        "stderr should name the product: {stderr}"
    );
    assert!(
        stderr.contains("console.git"),
        "stderr should name the product's repo: {stderr}"
    );
    Ok(())
}

/// `--repo` on a multi-repo product (no product default) is accepted and
/// stored in the task row. This is the legitimate per-task override path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_explicit_repo_accepted_on_multi_repo_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(&mut client, CreateProductInput::builder().name("Greenfield").build()).await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "bootstrap the new service",
            "--description",
            "",
            "--repo",
            "git@github.com:foo/new-service.git",
        ],
    )?;
    assert_eq!(
        created["chore"]["repo_remote_url"].as_str(),
        Some("git@github.com:foo/new-service.git"),
        "multi-repo product: explicit --repo should be stored in the row"
    );
    Ok(())
}
