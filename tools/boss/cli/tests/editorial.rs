//! End-to-end coverage for `boss product set-editorial-rules`,
//! the editorial-rules section of `boss product show`, and
//! `boss editorial test`.
//!
//! Acceptance criteria:
//! - set → show round-trip persists instructions and renders them.
//! - unset → show no longer shows editorial rules block.
//! - `boss editorial test` returns correct decision for a body with
//!   a Boss identifier and for a clean body.
//! - `boss editorial show` returns an empty list initially.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::CreateProductInput;

use common::run_boss;
use harness::{TestEngine, create_product_with};

/// set-editorial-rules → product show → unset → product show round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_then_show_then_unset_editorial_rules() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput {
            name: "EditorialProduct".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:example/repo.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    // Write a minimal editorial rules JSON file.
    let rules_dir = tempfile::tempdir()?;
    let rules_path = rules_dir.path().join("rules.json");
    std::fs::write(
        &rules_path,
        r#"{"instructions": "Do not mention Boss internals in PR bodies."}"#,
    )?;

    // Set the rules.
    let set_result = run_boss(
        engine.socket_str(),
        &[
            "product",
            "set-editorial-rules",
            &product.id,
            "--from-file",
            rules_path.to_str().unwrap(),
        ],
    )?;
    assert_eq!(
        set_result["product"]["editorial_rules"]["instructions"].as_str(),
        Some("Do not mention Boss internals in PR bodies."),
        "set should persist instructions: {set_result}"
    );

    // product show should include the editorial_rules block.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert_eq!(
        shown["product"]["editorial_rules"]["instructions"].as_str(),
        Some("Do not mention Boss internals in PR bodies."),
        "product show should include editorial_rules: {shown}"
    );

    // Unset the rules.
    let unset_result = run_boss(
        engine.socket_str(),
        &["product", "set-editorial-rules", &product.id, "--unset"],
    )?;
    assert!(
        unset_result["product"]["editorial_rules"].is_null(),
        "unset should clear editorial_rules: {unset_result}"
    );

    // product show after unset should not include editorial_rules.
    let after_unset = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert!(
        after_unset["product"]["editorial_rules"].is_null(),
        "after unset product show should have no editorial_rules: {after_unset}"
    );

    Ok(())
}

/// `boss editorial test` returns `allow` for a clean body and `deny`/`rewrite`
/// for a body containing a Boss identifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editorial_test_produces_correct_decision() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput {
            name: "TestDecisionProduct".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    let body_dir = tempfile::tempdir()?;

    // Clean body — baked-in defaults should produce Allow.
    let clean_path = body_dir.path().join("clean.md");
    std::fs::write(&clean_path, "## Summary\n\nThis fixes a bug in the auth flow.\n")?;
    let clean_result = run_boss(
        engine.socket_str(),
        &[
            "editorial",
            "test",
            &product.id,
            "--body-file",
            clean_path.to_str().unwrap(),
        ],
    )?;
    assert_eq!(
        clean_result["decision"].as_str(),
        Some("allow"),
        "clean body should yield allow: {clean_result}"
    );

    // Body with a Boss execution id — baked-in rewrite rule should fire.
    let dirty_path = body_dir.path().join("dirty.md");
    std::fs::write(
        &dirty_path,
        "## Summary\n\nThis run (exec_18b07a506d2518d0_1b) fixes a bug.\n",
    )?;
    let dirty_result = run_boss(
        engine.socket_str(),
        &[
            "editorial",
            "test",
            &product.id,
            "--body-file",
            dirty_path.to_str().unwrap(),
        ],
    )?;
    assert_ne!(
        dirty_result["decision"].as_str(),
        Some("allow"),
        "body with Boss exec id should not be allowed unchanged: {dirty_result}"
    );

    Ok(())
}

/// `boss editorial show` returns an empty list when no actions have been recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editorial_show_returns_empty_initially() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput {
            name: "ShowTestProduct".to_owned(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        },
    )
    .await?;

    let result = run_boss(engine.socket_str(), &["editorial", "show", &product.id])?;
    assert_eq!(
        result["actions"].as_array().map(Vec::len),
        Some(0),
        "editorial show should return empty list initially: {result}"
    );

    Ok(())
}
