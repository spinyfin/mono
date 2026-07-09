//! Integration tests for friendly-id selector semantics (chore 3 of 5 —
//! "Friendly numeric IDs for work items"). Drives the `boss` binary against
//! an in-process engine to verify every selector form resolves correctly and
//! that wrong-kind errors name the right corrective verb.
//!
//! Selector forms under test:
//!   `42`        — plain integer → short_id (task show, chore show, project show)
//!   `#42`       — hash-prefixed → short_id
//!   `boss/42`   — cross-product (slug/N) for task/chore show
//!   `boss/#42`  — cross-product with hash for task/chore show
//!   `task_…`    — primary id still works
//!   wrong-kind: `boss chore show 42` when #42 is a project_task → names `boss task show`
//!   wrong-kind: `boss chore show boss/42` when #42 is a project → names `boss project show`

use anyhow::{Result, anyhow};
use boss_client::BossClient;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_product, create_project, create_task};

// ── task show — all selector forms ──────────────────────────────────────────
// `boss task show` accepts any kind (chore_only: false), so we use chores
// as the fixture item since they don't require a project to be pre-created.

/// `boss task show 42` — plain integer short_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_plain_integer_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let value = run_boss(
        engine.socket_str(),
        &["task", "show", "--product", &product.id, &short_id.to_string()],
    )?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    assert_eq!(value["chore"]["short_id"].as_i64(), Some(short_id));
    Ok(())
}

/// `boss task show #42` — hash-prefixed short_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_hash_prefixed_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let selector = format!("#{short_id}");
    let value = run_boss(
        engine.socket_str(),
        &["task", "show", "--product", &product.id, &selector],
    )?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

/// `boss task show boss/42` — cross-product slug/N form (no --product needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_cross_product_slug_slash_n() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let selector = format!("{}/{short_id}", product.slug);
    let value = run_boss(engine.socket_str(), &["task", "show", &selector])?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

/// `boss task show boss/#42` — cross-product with hash prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_cross_product_slug_slash_hash_n() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let selector = format!("{}/#{short_id}", product.slug);
    let value = run_boss(engine.socket_str(), &["task", "show", &selector])?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

/// `boss task show task_xxx` / primary id still resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_primary_id_still_works() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;

    let value = run_boss(engine.socket_str(), &["task", "show", &chore.id])?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

// ── chore show ───────────────────────────────────────────────────────────────

/// `boss chore show 42` — plain integer short_id resolves a chore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_plain_integer_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Fix the thing").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let value = run_boss(
        engine.socket_str(),
        &["chore", "show", "--product", &product.id, &short_id.to_string()],
    )?;
    assert_eq!(value["chore"]["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

// ── project show ─────────────────────────────────────────────────────────────

/// `boss project show 42` — plain integer short_id resolves a project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_plain_integer_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Phase 1").await?;
    let short_id = project.short_id.ok_or_else(|| anyhow!("project has no short_id"))?;

    let value = run_boss(
        engine.socket_str(),
        &["project", "show", "--product", &product.id, &short_id.to_string()],
    )?;
    assert_eq!(value["project"]["id"].as_str(), Some(project.id.as_str()));
    Ok(())
}

/// `boss project show #42` — hash-prefixed short_id resolves a project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_hash_prefixed_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Phase 1").await?;
    let short_id = project.short_id.ok_or_else(|| anyhow!("project has no short_id"))?;

    let selector = format!("#{short_id}");
    let value = run_boss(
        engine.socket_str(),
        &["project", "show", "--product", &product.id, &selector],
    )?;
    assert_eq!(value["project"]["id"].as_str(), Some(project.id.as_str()));
    Ok(())
}

// ── wrong-kind errors ────────────────────────────────────────────────────────

/// `boss chore show 42` when T42 is a project_task → error naming `boss task show`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_wrong_kind_task_names_correct_verb() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Phase 1").await?;
    let task = create_task(&mut client, &product.id, &project.id, "Do a task").await?;
    let short_id = task.short_id.ok_or_else(|| anyhow!("task has no short_id"))?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["chore", "show", "--product", &product.id, &short_id.to_string()],
    )?;
    assert!(
        stderr.contains("boss task show"),
        "expected error to suggest `boss task show`, got: {stderr}"
    );
    assert!(
        stderr.contains(&format!("T{short_id}")),
        "expected error to mention T{short_id}, got: {stderr}"
    );
    Ok(())
}

/// `boss chore show boss/42` when #42 is a project → error naming `boss project show`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_wrong_kind_project_names_correct_verb() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Phase 1").await?;
    let short_id = project.short_id.ok_or_else(|| anyhow!("project has no short_id"))?;

    let selector = format!("{}/{short_id}", product.slug);
    let stderr = run_boss_expect_failure(engine.socket_str(), &["chore", "show", &selector])?;
    assert!(
        stderr.contains("boss project show"),
        "expected error to suggest `boss project show`, got: {stderr}"
    );
    Ok(())
}

// ── short_id in JSON output ──────────────────────────────────────────────────

/// `boss chore show task_xxx` includes `short_id` in JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_json_includes_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Do something").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;

    let value = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    assert_eq!(
        value["chore"]["short_id"].as_i64(),
        Some(short_id),
        "short_id missing from JSON: {value}"
    );
    Ok(())
}

/// `boss project show proj_xxx` includes `short_id` in JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_show_json_includes_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Phase 1").await?;
    let short_id = project.short_id.ok_or_else(|| anyhow!("project has no short_id"))?;

    let value = run_boss(engine.socket_str(), &["project", "show", &project.id])?;
    assert_eq!(
        value["project"]["short_id"].as_i64(),
        Some(short_id),
        "short_id missing from JSON: {value}"
    );
    Ok(())
}

/// `boss chore show <id> --json` always emits `current_execution_id`
/// and `current_run_id` inside the chore object — `null` when the
/// chore has never been dispatched. The coordinator parses these
/// keys directly off `.chore`, so the engine must keep them present
/// (not skipped) even when the underlying engine state is empty.
/// Backs the agent-visibility chore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_json_exposes_runtime_keys_when_empty() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Just created").await?;

    let value = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    let chore_value = value
        .get("chore")
        .ok_or_else(|| anyhow!("expected `chore` key in JSON: {value}"))?;
    assert!(
        chore_value
            .as_object()
            .map(|m| m.contains_key("current_execution_id"))
            .unwrap_or(false),
        "current_execution_id key must always be present: {value}",
    );
    assert!(
        chore_value
            .as_object()
            .map(|m| m.contains_key("current_run_id"))
            .unwrap_or(false),
        "current_run_id key must always be present: {value}",
    );
    assert!(
        chore_value["current_execution_id"].is_null(),
        "pre-dispatch chore must have null current_execution_id: {value}",
    );
    assert!(
        chore_value["current_run_id"].is_null(),
        "pre-dispatch chore must have null current_run_id: {value}",
    );
    Ok(())
}
