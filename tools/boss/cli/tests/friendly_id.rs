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
use boss_engine::work::{PrOpenState, StaticPrStateChecker};
use boss_protocol::{
    AddDependencyInput, CreateExecutionInput, CreateRevisionInput, CreateRunInput, ExecutionKind, ExecutionStatus,
    WorkItemPatch,
};

use common::{run_boss, run_boss_expect_failure, run_boss_human};
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
    assert_eq!(value["short_id"].as_i64(), Some(short_id));
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
    Ok(())
}

/// An archived-and-tombstoned revision remains addressable by its canonical
/// id so an investigator can read the persisted archival provenance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_primary_id_surfaces_archived_revision_provenance() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let parent = create_chore(&mut client, &product.id, "Merge parent").await?;
    let dependent = create_chore(&mut client, &product.id, "Blocked dependent").await?;
    run_boss(
        engine.socket_str(),
        &["task", "bind-pr", &parent.id, "https://github.com/acme/repo/pull/1"],
    )?;

    let db = engine.db()?;
    let revision = db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(parent.id.clone())
            .description("Archived because its parent merged")
            .build(),
        &StaticPrStateChecker(PrOpenState::Open),
    )?;
    db.update_work_item(
        &revision.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: revision.id.clone(),
        relation: None,
    })?;
    db.mark_chore_pr_merged(&parent.id, "https://github.com/acme/repo/pull/1")?;
    // Reproduce the diagnostic state of a stale blocked row without changing
    // dependency-gate behavior: the show surface must retain the archived
    // prerequisite's provenance even if a separate reconciler is responsible
    // for resolving the stale block.
    db.update_work_item(
        &dependent.id,
        WorkItemPatch {
            status: Some("blocked".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;

    let value = run_boss(engine.socket_str(), &["task", "show", &revision.id])?;
    assert_eq!(value["id"].as_str(), Some(revision.id.as_str()));
    assert_eq!(value["status"].as_str(), Some("archived"));
    assert_eq!(value["archived_by"].as_str(), Some("revision_parent_close_sweep"));
    assert!(
        value["archived_at"].as_str().is_some_and(|at| !at.is_empty()),
        "archived_at missing from task show output: {value}"
    );
    assert!(
        value["archived_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("superseded by chore task_")),
        "archived_reason must identify the superseding chore: {value}"
    );
    let short_id = revision.short_id.ok_or_else(|| anyhow!("revision has no short_id"))?;
    let by_short = run_boss(
        engine.socket_str(),
        &["task", "show", "--product", &product.id, &format!("T{short_id}")],
    )?;
    assert_eq!(
        by_short["id"].as_str(),
        Some(revision.id.as_str()),
        "friendly short id must resolve the same archived revision: {by_short}"
    );
    assert_eq!(by_short["archived_by"].as_str(), Some("revision_parent_close_sweep"));
    let human = run_boss_human(engine.socket_str(), &["task", "show", &revision.id])?;
    assert!(
        human.contains("Archived by: revision_parent_close_sweep"),
        "output: {human}"
    );
    assert!(human.contains("Archived actor: engine"), "output: {human}");
    let archived_at_line = human
        .lines()
        .find(|line| line.starts_with("Archived at:"))
        .unwrap_or("");
    let rendered_at = archived_at_line.trim_start_matches("Archived at:").trim();
    assert!(
        !rendered_at.is_empty() && rendered_at.parse::<i64>().is_err(),
        "Archived at must be a formatted timestamp, not a raw epoch: {human}"
    );
    assert!(human.contains("Archived reason:"), "output: {human}");
    assert!(
        human.contains("Deleted:"),
        "tombstoned archived revision must print Deleted: {human}"
    );

    let blocked = run_boss(engine.socket_str(), &["task", "show", &dependent.id])?;
    assert_eq!(blocked["status"].as_str(), Some("blocked"));
    let prerequisite = &blocked["dependencies"]["prerequisites"][0];
    assert_eq!(prerequisite["id"].as_str(), Some(revision.id.as_str()));
    assert_eq!(prerequisite["status"].as_str(), Some("archived"));
    assert_eq!(
        prerequisite["archived_by"].as_str(),
        Some("revision_parent_close_sweep")
    );
    assert!(
        prerequisite["archived_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "archived prerequisite reason missing from dependency JSON: {blocked}"
    );
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
    assert_eq!(value["id"].as_str(), Some(chore.id.as_str()));
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
        value["short_id"].as_i64(),
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
/// and `current_run_id` at the top level of the row — `null` when the
/// chore has never been dispatched. The coordinator parses these
/// keys directly off the row (not nested under a `chore` wrapper), so
/// the engine must keep them present (not skipped) even when the
/// underlying engine state is empty. Backs the agent-visibility chore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_json_exposes_runtime_keys_when_empty() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Just created").await?;

    let value = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    let chore_value = value
        .as_object()
        .ok_or_else(|| anyhow!("expected a JSON object row: {value}"))?;
    assert!(
        chore_value.contains_key("current_execution_id"),
        "current_execution_id key must always be present: {value}",
    );
    assert!(
        chore_value.contains_key("current_run_id"),
        "current_run_id key must always be present: {value}",
    );
    assert!(
        value["current_execution_id"].is_null(),
        "pre-dispatch chore must have null current_execution_id: {value}",
    );
    assert!(
        value["current_run_id"].is_null(),
        "pre-dispatch chore must have null current_run_id: {value}",
    );
    Ok(())
}

/// A completed execution without a PR is self-explaining in ordinary task
/// inspection: its run history retains the engine-authored completion reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_show_json_includes_run_result_summary() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let chore = create_chore(&mut client, &product.id, "Already complete").await?;
    let db = engine.db()?;
    let execution = db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )?;
    let reason = "Worker verified the assigned work was already done (empty diff — no changes needed); closed as a no-op without a PR.";
    db.create_run(
        CreateRunInput::builder()
            .execution_id(execution.id.clone())
            .agent_id("test-agent")
            .finished_at("1000")
            .result_summary(reason)
            .started_at("999")
            .status("completed")
            .build(),
    )?;

    let value = run_boss(engine.socket_str(), &["chore", "show", &chore.id])?;
    assert_eq!(value["executions"][0]["id"].as_str(), Some(execution.id.as_str()));
    assert_eq!(
        value["executions"][0]["runs"][0]["result_summary"].as_str(),
        Some(reason)
    );
    Ok(())
}
