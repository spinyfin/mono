//! End-to-end RPC-boundary tests for the per-dispatch host pin —
//! `RequestExecution { pinned_host_id: Some(..), .. }`, which is what
//! `bossctl work start --host` and `bossctl agents launch --host` send —
//! driven through a real in-process engine and a real `BossClient` socket.
//!
//! The contract under test is the refusal side: an operator naming a host
//! is testing *that* host, so an unusable one must fail the whole request
//! with a reason and leave nothing queued, rather than falling back to
//! `local`. The accept side (a pin actually routing an execution to its
//! host) is host-selection behaviour and is covered against a fake
//! cube/runner in `coordinator_tests::dispatch`, which can assert the
//! placement deterministically; this harness leaves the real spawn path
//! unstubbed, so it only exercises the outcomes that never reach cube.

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_engine::work::WorkDb;
use boss_protocol::{
    CreateChoreInput, CreateProductInput, ExecutionStatus, FrontendEvent, FrontendRequest, RequestExecutionInput, Task,
    WorkItem,
};

mod common;
use common::{TestEngine, TestEngineOptions};

async fn spawn_engine() -> Result<TestEngine> {
    TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        ..Default::default()
    })
    .await
}

/// Second handle onto the engine's on-disk `state.db`, for planting host
/// rows. `AddHost` over the RPC would provision the host over real SSH,
/// which no hermetic test can do.
fn work_db(engine: &TestEngine) -> Result<WorkDb> {
    WorkDb::open(engine.db_path.clone())
}

async fn create_product(client: &mut BossClient) -> Result<String> {
    match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@example.com:boss.git")
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => Ok(p.id),
        other => Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    }
}

/// Chores are created with `autostart: false` so the auto-dispatcher never
/// races these assertions by minting an execution of its own.
async fn create_chore(client: &mut BossClient, product_id: &str, name: &str) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product_id)
                .name(name)
                .autostart(false)
                .build(),
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t) | WorkItem::Task(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    }
}

async fn list_executions_for(client: &mut BossClient, work_item_id: &str) -> Result<Vec<boss_protocol::WorkExecution>> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
            include_revision_chain: false,
        })
        .await?
    {
        FrontendEvent::ExecutionsList { executions, .. } => Ok(executions),
        other => Err(anyhow!("unexpected response to ListExecutions: {other:?}")),
    }
}

async fn start_pinned(client: &mut BossClient, work_item_id: &str, host: &str) -> Result<FrontendEvent> {
    client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .pinned_host_id(host)
                .build(),
        })
        .await
}

fn refusal_message(event: FrontendEvent) -> Result<String> {
    match event {
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => Ok(message),
        other => Err(anyhow!("expected a refusal, got: {other:?}")),
    }
}

/// An unknown host id is an error, not a warning — and the message lists
/// the ids that *are* registered so the operator can fix a typo without a
/// second command.
#[tokio::test]
async fn pin_to_unknown_host_refuses_and_lists_known_hosts() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore(&mut client, &product_id, "Smoke test").await?;

    let message = refusal_message(start_pinned(&mut client, &chore.id, "no-such-host").await?)?;
    assert!(
        message.contains("unknown host") && message.contains("no-such-host"),
        "refusal must name the unknown host, got: {message}"
    );
    assert!(
        message.contains("local"),
        "refusal must list the known host ids, got: {message}"
    );

    assert!(
        list_executions_for(&mut client, &chore.id).await?.is_empty(),
        "an unknown-host refusal must leave no queued residue behind"
    );
    Ok(())
}

/// A disabled host refuses by name and dispatches nothing — the case the
/// flag exists for. Falling back to `local` here would let a smoke test
/// "pass" while proving nothing about the host under test.
#[tokio::test]
async fn pin_to_disabled_host_refuses_with_no_residue() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore(&mut client, &product_id, "Smoke test").await?;

    let db = work_db(&engine)?;
    db.add_host("zakalwe", "zakalwe.local", 1, &[])?;
    db.set_host_enabled("zakalwe", false)?;

    let message = refusal_message(start_pinned(&mut client, &chore.id, "zakalwe").await?)?;
    assert!(
        message.contains("zakalwe") && message.contains("disabled"),
        "refusal must name the host and say it is disabled, got: {message}"
    );
    assert!(
        message.contains("never falls back"),
        "refusal must state that nothing was dispatched elsewhere, got: {message}"
    );

    let executions = list_executions_for(&mut client, &chore.id).await?;
    assert!(
        executions.is_empty(),
        "a disabled-host refusal must create no execution row at all; got {executions:?}"
    );
    Ok(())
}

/// A host whose every slot is busy refuses too: the pin does not queue
/// behind the busy host, because a queued row is indistinguishable from
/// ordinary scheduling latency and hides the fact that nothing ran.
#[tokio::test]
async fn pin_to_host_with_no_free_slot_refuses() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore(&mut client, &product_id, "Smoke test").await?;

    let db = work_db(&engine)?;
    db.add_host("zakalwe", "zakalwe.local", 1, &[])?;
    // Occupy the host's single slot with an active run, the shape
    // `active_runs_per_host` counts. Planted through a raw connection:
    // no public `WorkDb` method mints a run attributed to a host without
    // going through a real spawn.
    let conn = rusqlite::Connection::open(&engine.db_path)?;
    conn.execute(
        "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
         VALUES ('exec-occupant', 'wi-other', 'chore_implementation', 'running',
                 'git@example.com:boss.git', '100')",
        [],
    )?;
    conn.execute(
        "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, host_id)
         VALUES ('run-occupant', 'exec-occupant', 'agent-1', 'active', '100', 'zakalwe')",
        [],
    )?;
    drop(conn);

    let message = refusal_message(start_pinned(&mut client, &chore.id, "zakalwe").await?)?;
    assert!(
        message.contains("no free slot"),
        "refusal must name the slot exhaustion, got: {message}"
    );

    assert!(
        list_executions_for(&mut client, &chore.id).await?.is_empty(),
        "a no-slot refusal must leave no queued residue behind"
    );
    Ok(())
}

/// The accepted path stamps the pin on the row it creates, which is what
/// host selection reads. Pinning to `local` keeps this deterministic: the
/// local host is never slot-gated, so the only variable under test is the
/// pin itself.
#[tokio::test]
async fn accepted_pin_is_stamped_on_the_created_execution() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore(&mut client, &product_id, "Smoke test").await?;

    let execution = match start_pinned(&mut client, &chore.id, "local").await? {
        FrontendEvent::ExecutionRequested { execution } => execution,
        other => return Err(anyhow!("expected ExecutionRequested, got: {other:?}")),
    };
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(
        work_db(&engine)?.execution_pinned_host(&execution.id)?.as_deref(),
        Some("local"),
        "an accepted pin must reach `work_executions.pinned_host_id`, where host selection reads it",
    );
    Ok(())
}

/// Omitting the pin must leave selection exactly as it was: no pin
/// written, so `select_host` ranks every eligible host as before.
#[tokio::test]
async fn unpinned_request_writes_no_pin() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore(&mut client, &product_id, "Smoke test").await?;

    let execution = match client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder().work_item_id(chore.id.clone()).build(),
        })
        .await?
    {
        FrontendEvent::ExecutionRequested { execution } => execution,
        other => return Err(anyhow!("expected ExecutionRequested, got: {other:?}")),
    };
    assert!(
        work_db(&engine)?.execution_pinned_host(&execution.id)?.is_none(),
        "a request without --host must not pin the execution to anything",
    );
    Ok(())
}
