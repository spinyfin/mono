//! Integration test: the `ListEngineAttempts` RPC's opt-in
//! background-work snapshot, driven through a real socket against a real
//! engine — design "Background task visibility: only long-lived, headless
//! engine work enters the toolbar badge"
//! (`tools/boss/docs/designs/background-task-visibility-toolbar-affordance-for-engine-background-work.md`).
//!
//! `common::TestEngine` runs the engine's real request-handling logic via
//! `tokio::spawn` inside this same test process, so
//! `boss_engine::ladder_lease_registry` — a process-wide static — is the
//! exact instance the running engine's handler reads. That lets this test
//! register a rung-1 lease directly and prove the conflict source's
//! live-lease veto end to end over the wire, not just at the `WorkDb`
//! layer (already covered exhaustively by `boss_engine::background_work`'s
//! own unit tests).

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_engine::coordinator::CubeWorkspaceLease;
use boss_engine::ladder_lease_registry;
use boss_engine::work::{ClaimPlannerRunInput, ConflictResolutionInsertInput, WorkDb};
use boss_protocol::{
    BackgroundWorkKind, CreateChoreInput, CreateProductInput, CreateProjectInput, FrontendEvent, FrontendRequest,
    Product, Project, Task, WorkItem,
};

mod common;
use common::TestEngine;

fn list_engine_attempts_request(include_background_work: bool, limit: Option<u32>) -> FrontendRequest {
    FrontendRequest::ListEngineAttempts {
        kinds: vec![],
        product_id: None,
        status: vec![],
        work_item_id: None,
        limit,
        include_background_work,
    }
}

/// The full round trip: seed one project-planner background item and one
/// mechanical conflict-remediation background item, then prove that
/// `include_background_work` gates the snapshot and `limit: Some(0)` is
/// the background-only polling form.
#[tokio::test]
async fn list_engine_attempts_carries_the_opt_in_background_snapshot() -> Result<()> {
    let engine = TestEngine::spawn_with(common::TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = create_product(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/test.git")
            .build(),
    )
    .await?;
    let project = create_project(
        &mut client,
        CreateProjectInput::builder()
            .product_id(product.id.clone())
            .name("Alpha")
            .goal("build it")
            .build(),
    )
    .await?;
    let chore = create_chore(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Fix the merge conflict")
            .autostart(false)
            .build(),
    )
    .await?;

    // Seed both v1 sources directly via `WorkDb`, reopening the on-disk
    // DB the running engine already has open — the same pattern
    // `control_verbs.rs` uses to drive state no RPC exposes.
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db
        .claim_planner_run(ClaimPlannerRunInput {
            project_id: &project.id,
            product_id: &product.id,
            design_task_id: None,
            caller: "merge_trigger",
        })?
        .expect("fresh project must be claimable");
    backdate_planner_run(&engine.db_path, &run.id, 20);

    let attempt = work_db
        .insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product.id.clone())
                .work_item_id(chore.id.clone())
                .pr_url("https://github.com/test/test/pull/1")
                .pr_number(1)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger("abc")
                .head_sha_before("def")
                .build(),
        )?
        .expect("fresh work item must accept a pending conflict attempt");
    work_db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork-rpc-lease", "bgwork-rpc-ws")?;
    // Registers into the SAME process-wide registry the running engine's
    // handler reads, because this test and the engine share one process.
    ladder_lease_registry::register(&CubeWorkspaceLease {
        lease_id: "bgwork-rpc-lease".to_owned(),
        workspace_id: "bgwork-rpc-ws".to_owned(),
        workspace_path: "/tmp/bgwork-rpc-ws".into(),
        dirty_verified: None,
    });

    // Default (`include_background_work: false`) stays backward
    // compatible: the snapshot field is present but empty even though
    // both sources are live.
    let default_response = client.send_request(&list_engine_attempts_request(false, None)).await?;
    let (default_attempts, default_background) = expect_engine_attempts_list(default_response)?;
    assert_eq!(
        default_attempts.len(),
        1,
        "the conflict row is still a normal attempts-list entry"
    );
    assert!(
        default_background.is_empty(),
        "background_work must stay empty unless the caller opts in"
    );

    // Opted in, unlimited: both the normal attempt and the background
    // snapshot come back populated.
    let opted_in_response = client.send_request(&list_engine_attempts_request(true, None)).await?;
    let (opted_in_attempts, background) = expect_engine_attempts_list(opted_in_response)?;
    assert_eq!(opted_in_attempts.len(), 1);
    assert_eq!(background.len(), 2, "the count must equal the returned list");
    let planner_item = background
        .iter()
        .find(|item| item.kind == BackgroundWorkKind::ProjectPlanner)
        .ok_or_else(|| anyhow!("missing project_planner background item"))?;
    assert_eq!(planner_item.source_id, run.id);
    assert_eq!(planner_item.phase, "Planning Alpha");
    assert!(planner_item.started_at.is_some());
    let conflict_item = background
        .iter()
        .find(|item| item.kind == BackgroundWorkKind::ConflictRemediation)
        .ok_or_else(|| anyhow!("missing conflict_remediation background item"))?;
    assert_eq!(conflict_item.source_id, attempt.id);
    assert_eq!(conflict_item.phase, format!("Rebasing {}", chore.id));
    assert!(conflict_item.started_at.is_none());

    // `limit: Some(0)` is the background-only polling form: `attempts`
    // empties out while `background_work` stays fully populated.
    let background_only_response = client
        .send_request(&list_engine_attempts_request(true, Some(0)))
        .await?;
    let (background_only_attempts, background_only) = expect_engine_attempts_list(background_only_response)?;
    assert!(
        background_only_attempts.is_empty(),
        "limit: 0 must empty the attempts list"
    );
    assert_eq!(
        background_only.len(),
        2,
        "limit: 0 must not affect the background snapshot"
    );

    ladder_lease_registry::unregister("bgwork-rpc-lease");
    Ok(())
}

/// Directly rewrites a `planner_runs` row's `created_at` so the 15-second
/// anti-flicker gate can be exercised without sleeping the test. Reopens
/// the on-disk DB with a raw `rusqlite` connection rather than routing
/// through `WorkDb` — this is outside-the-crate integration-test code, so
/// `WorkDb`'s own `connect()` (`pub(crate)`) is not reachable here.
fn backdate_planner_run(db_path: &std::path::Path, run_id: &str, secs_ago: i64) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let new_ts = (now_secs - secs_ago).to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE planner_runs SET created_at = ?2 WHERE id = ?1",
        rusqlite::params![run_id, new_ts],
    )
    .unwrap();
}

async fn create_product(client: &mut BossClient, input: CreateProductInput) -> Result<Product> {
    match client.send_request(&FrontendRequest::CreateProduct { input }).await? {
        FrontendEvent::WorkItemCreated { item } => match item {
            WorkItem::Product(product) => Ok(product),
            other => Err(anyhow!("expected product, got {other:?}")),
        },
        other => Err(anyhow!("unexpected response creating product: {other:?}")),
    }
}

async fn create_project(client: &mut BossClient, input: CreateProjectInput) -> Result<Project> {
    match client.send_request(&FrontendRequest::CreateProject { input }).await? {
        FrontendEvent::WorkItemCreated { item } => match item {
            WorkItem::Project(project) => Ok(project),
            other => Err(anyhow!("expected project, got {other:?}")),
        },
        other => Err(anyhow!("unexpected response creating project: {other:?}")),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task> {
    match client.send_request(&FrontendRequest::CreateChore { input }).await? {
        FrontendEvent::WorkItemCreated { item } => match item {
            WorkItem::Chore(chore) => Ok(chore),
            other => Err(anyhow!("expected chore, got {other:?}")),
        },
        other => Err(anyhow!("unexpected response creating chore: {other:?}")),
    }
}

fn expect_engine_attempts_list(
    event: FrontendEvent,
) -> Result<(
    Vec<boss_protocol::EngineAttemptListEntry>,
    Vec<boss_protocol::BackgroundWorkItem>,
)> {
    match event {
        FrontendEvent::EngineAttemptsList {
            attempts,
            background_work,
        } => Ok((attempts, background_work)),
        other => Err(anyhow!("unexpected response: {other:?}")),
    }
}
