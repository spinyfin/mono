//! End-to-end coverage for `boss product set-external-tracker` and the
//! external-tracker block in `boss product show`.
//!
//! Acceptance criteria:
//! - bind → `product show` renders the tracker block correctly.
//! - unbind → `product show` no longer renders the tracker block.
//! - missing required flags (e.g. no `--org` for `kind=github`) is rejected.
//! - `--json` output round-trips the external_tracker_kind / config fields.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::CreateProductInput;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_product_with};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_then_show_renders_external_tracker_block() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .await?;

    // Bind the external tracker.
    let bound = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--org",
            "spinyfin",
            "--repo",
            "mono",
            "--project",
            "1",
        ],
    )?;
    assert_eq!(
        bound["product"]["external_tracker_kind"].as_str(),
        Some("github"),
        "bound product should show external_tracker_kind=github: {bound}"
    );
    let config = &bound["product"]["external_tracker_config"];
    assert_eq!(config["org"].as_str(), Some("spinyfin"));
    assert_eq!(config["repo"].as_str(), Some("mono"));
    assert_eq!(config["project_number"].as_u64(), Some(1));
    assert_eq!(config["reverse_close"].as_bool(), Some(false));

    // product show --json should include the same fields.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert_eq!(
        shown["product"]["external_tracker_kind"].as_str(),
        Some("github"),
        "product show should include external_tracker_kind: {shown}"
    );

    // Unbind.
    let unbound = run_boss(
        engine.socket_str(),
        &["product", "set-external-tracker", &product.id, "--unset"],
    )?;
    assert!(
        unbound["product"]["external_tracker_kind"].is_null(),
        "unset should clear external_tracker_kind: {unbound}"
    );

    // product show after unbind should not show the tracker block.
    let after_unset = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert!(
        after_unset["product"]["external_tracker_kind"].is_null(),
        "after unset product show should have no external_tracker_kind: {after_unset}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_with_reverse_close_flag_persists() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("ReverseCloseProd")
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .await?;

    let bound = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--org",
            "spinyfin",
            "--repo",
            "mono",
            "--project",
            "2",
            "--reverse-close",
        ],
    )?;
    let config = &bound["product"]["external_tracker_config"];
    assert_eq!(
        config["reverse_close"].as_bool(),
        Some(true),
        "reverse_close flag should be stored: {config}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_org_for_github_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("NoBind")
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .await?;

    // Missing --org should fail at CLI validation level.
    let err = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "product",
            "set-external-tracker",
            &product.id,
            "--kind",
            "github",
            "--repo",
            "mono",
            "--project",
            "1",
        ],
    )?;
    assert!(
        err.contains("--org") || err.contains("org"),
        "error should mention --org: {err}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_kind_without_unset_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("NoKind")
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .await?;

    let err = run_boss_expect_failure(
        engine.socket_str(),
        &["product", "set-external-tracker", &product.id, "--org", "spinyfin"],
    )?;
    assert!(
        err.contains("--kind") || err.contains("kind") || err.contains("unset"),
        "error should mention --kind or --unset: {err}"
    );

    Ok(())
}
