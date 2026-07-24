//! End-to-end coverage for the `blocked_reason` grapheme-length limit and
//! the sibling `blocked_detail` field (Kanban card blocked-status detail
//! surface chore). Spawns an in-process engine on a temp socket and drives
//! the `boss` binary through `chore update`, exercising the exact
//! verification checklist from the work item:
//!
//! - an over-long `--blocked-reason` is rejected with a message naming
//!   `--blocked-detail` as the alternative, and nothing is written;
//! - a short custom tag with an emoji (`🚧 Future`) succeeds — grapheme
//!   counting, not byte/char counting, so the emoji doesn't inflate the
//!   count;
//! - `blocked_detail` round-trips verbatim through both `--json` output
//!   and a re-fetch via `show`;
//! - `--blocked-detail` without an accompanying `--blocked-reason` is
//!   rejected.
//!
//! Driving the compiled `boss` binary as a subprocess against a real
//! engine over its Unix-socket RPC (rather than calling `WorkDb` methods
//! in-process, as the engine crate's own unit tests do) demonstrates the
//! limit is enforced at the engine RPC boundary itself, not bolted onto
//! the CLI's argument parsing.
//!
//! One checklist item is deliberately NOT covered here: "a row with a
//! pre-existing over-long `blocked_reason` can still have an unrelated
//! field updated". Producing that row requires writing directly to the
//! DB, bypassing the validated write path entirely — `WorkDb::connect`
//! is `pub(crate)`, unreachable from this black-box test even as a dev-
//! dependency. That's the correct outcome: there is no longer any way to
//! *create* such a row through the system, grandfathered rows are the
//! only ones that can exist. See
//! `updating_unrelated_field_succeeds_despite_preexisting_over_long_blocked_reason`
//! in `engine/core/src/work/tests/t06.rs`, which has legitimate in-crate
//! access to seed one directly and is the authoritative coverage for
//! this scenario.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::CreateProductInput;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore_with, create_product_with};

/// The exact reported bad input from the work item: 84 graphemes,
/// visibly truncates at ~44 characters on the kanban pill.
const OVER_LONG_REASON: &str = "FUTURE — deferred scope per design doc; requires explicit operator approval to start";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_long_blocked_reason_is_rejected_and_names_blocked_detail() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;
    let chore = create_chore_with(
        &mut client,
        boss_protocol::CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Some chore")
            .autostart(false)
            .build(),
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore.id,
            "--status",
            "blocked",
            "--blocked-reason",
            OVER_LONG_REASON,
        ],
    )?;
    assert!(stderr.contains("too long"), "stderr should explain why: {stderr}");
    assert!(
        stderr.contains("blocked-detail") || stderr.contains("blocked_detail"),
        "stderr should name the alternative field: {stderr}"
    );

    // Nothing was written.
    let shown = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    assert_eq!(shown["chore"]["status"].as_str(), Some("todo"));
    assert!(shown["chore"]["blocked_reason"].is_null());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_custom_tag_with_emoji_succeeds() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;
    let chore = create_chore_with(
        &mut client,
        boss_protocol::CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Some chore")
            .autostart(false)
            .build(),
    )
    .await?;

    let updated = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore.id,
            "--status",
            "blocked",
            "--blocked-reason",
            "🚧 Future",
        ],
    )?;
    assert_eq!(updated["chore"]["blocked_reason"].as_str(), Some("🚧 Future"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_detail_round_trips_through_json_and_show() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;
    let chore = create_chore_with(
        &mut client,
        boss_protocol::CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Some chore")
            .autostart(false)
            .build(),
    )
    .await?;
    let detail = "deferred scope per design doc; requires explicit operator approval (pr_created)";

    let updated = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore.id,
            "--status",
            "blocked",
            "--blocked-reason",
            "🚧 Future",
            "--blocked-detail",
            detail,
        ],
    )?;
    assert_eq!(updated["chore"]["blocked_reason"].as_str(), Some("🚧 Future"));
    assert_eq!(updated["chore"]["blocked_detail"].as_str(), Some(detail));

    let shown = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    assert_eq!(shown["chore"]["blocked_detail"].as_str(), Some(detail));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_detail_without_blocked_reason_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;
    let chore = create_chore_with(
        &mut client,
        boss_protocol::CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Some chore")
            .autostart(false)
            .build(),
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "update",
            &chore.id,
            "--status",
            "blocked",
            "--blocked-detail",
            "deferred scope per design doc",
        ],
    )?;
    assert!(
        stderr.contains("blocked_detail"),
        "stderr should mention blocked_detail: {stderr}"
    );

    Ok(())
}
