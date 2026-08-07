//! Integration tests for `--ids` on the list verbs (`boss task list`,
//! `boss chore list`). Drives the `boss` binary against an in-process
//! engine to verify: friendly-id resolution (mixing short and primary
//! id forms in one call), the all-or-nothing unknown-id error, and
//! composition with an existing filter (`--priority`).

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::CreateChoreInput;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_chore_with, create_product};

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

fn chore_ids(value: &serde_json::Value) -> Vec<String> {
    sorted(
        value["chores"]
            .as_array()
            .expect("chores array")
            .iter()
            .map(|c| c["id"].as_str().expect("chore id").to_owned())
            .collect(),
    )
}

/// A single `--ids` call mixing a friendly short id (`T<n>`) and a
/// primary id (`task_…`) resolves both the same way `boss task show`
/// would, and returns exactly the requested rows — nothing else from
/// the product's other chores leaks in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_accepts_mixed_short_and_primary_ids() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_chore(&mut client, &product.id, "A").await?;
    let b = create_chore(&mut client, &product.id, "B").await?;
    let _c = create_chore(&mut client, &product.id, "C").await?;
    let a_short = a.short_id.expect("chore has short_id");

    let selector = format!("T{a_short},{}", b.id);
    let value = run_boss(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--ids", &selector, "--json"],
    )?;

    assert_eq!(chore_ids(&value), sorted(vec![a.id, b.id]));
    Ok(())
}

/// An id that doesn't name any row in the product is a hard error —
/// asking for N ids and silently getting fewer back is exactly the
/// footgun `--ids` exists to close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_unknown_id_errors() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_chore(&mut client, &product.id, "A").await?;

    let bogus_short_id = 999_999;
    let selector = format!("{},T{bogus_short_id}", a.id);
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--ids", &selector, "--json"],
    )?;
    assert!(
        stderr.contains(&bogus_short_id.to_string()) || stderr.to_lowercase().contains("not found"),
        "unexpected stderr: {stderr}"
    );
    Ok(())
}

/// `--ids` composes with other filters (here `--priority`) as an AND:
/// a requested id that exists but doesn't match the other filter is
/// dropped from the result, not treated as an unknown-id error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_composes_with_priority_filter() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let high = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("High")
            .priority("high")
            .created_via("test")
            .build(),
    )
    .await?;
    let medium = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Medium")
            .priority("medium")
            .created_via("test")
            .build(),
    )
    .await?;

    let selector = format!("{},{}", high.id, medium.id);
    let value = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "list",
            "--product",
            &product.id,
            "--ids",
            &selector,
            "--priority",
            "high",
            "--json",
        ],
    )?;

    // Both requested ids exist — the priority filter narrows the
    // result without raising an unknown-id error.
    assert_eq!(chore_ids(&value), vec![high.id]);
    Ok(())
}
