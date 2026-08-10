//! End-to-end RPC-boundary tests for the pause-only forced-dispatch
//! override — `EvaluateDispatchAdmission` and `RequestExecution {
//! bypass_dispatch_pause: true, .. }` driven through a real in-process
//! engine and a real `BossClient` socket, exactly as `bossctl work start
//! --force` and a confirmed macOS drag-to-Doing do. See
//! `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
//!
//! Deterministic dispatch-outcome coverage (does a forced request actually
//! reach `running`, does a refused one leave no residue, is pool routing
//! preserved) lives in `coordinator_tests::pause_bypass` against a fake
//! cube/runner — this file only exercises the RPC shape and refusals that
//! never touch cube (dependency gating, the pause snapshot), which are
//! deterministic even against the real spawn path this harness leaves
//! unstubbed.

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{
    AddDependencyInput, CreateChoreInput, CreateProductInput, DispatchAdmissionEntryPoint, FrontendEvent,
    FrontendRequest, RequestExecutionInput, Task, WorkItem,
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

async fn create_chore_named(client: &mut BossClient, product_id: &str, name: &str) -> Result<Task> {
    create_chore_named_with_autostart(client, product_id, name, true).await
}

async fn create_chore_named_with_autostart(
    client: &mut BossClient,
    product_id: &str,
    name: &str,
    autostart: bool,
) -> Result<Task> {
    match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id(product_id)
                .name(name)
                .autostart(autostart)
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

/// `EvaluateDispatchAdmission` over the real RPC boundary: once dispatch is
/// paused via `SetDispatchPaused`, the response must report the pause as
/// active, operator-originated, and overridable, with its reason intact —
/// the exact snapshot the macOS confirmation dialog renders.
#[tokio::test]
async fn evaluate_dispatch_admission_reports_active_operator_pause() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore_named(&mut client, &product_id, "Plain chore").await?;

    match client
        .send_request(&FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: Some("investigating worker failures".to_owned()),
        })
        .await?
    {
        FrontendEvent::DispatchStateResult { .. } => {}
        other => return Err(anyhow!("unexpected response to SetDispatchPaused: {other:?}")),
    }

    match client
        .send_request(&FrontendRequest::EvaluateDispatchAdmission {
            work_item_id: chore.id.clone(),
        })
        .await?
    {
        FrontendEvent::DispatchAdmissionEvaluated { admission } => {
            assert!(!admission.would_dispatch);
            assert!(admission.pause.active);
            assert!(admission.pause.overridable, "an operator pause must be overridable");
            assert_eq!(admission.pause.origin.as_deref(), Some("operator"));
            assert_eq!(admission.pause.reason.as_deref(), Some("investigating worker failures"));
        }
        other => return Err(anyhow!("unexpected response to EvaluateDispatchAdmission: {other:?}")),
    }
    Ok(())
}

/// `RequestExecution { bypass_dispatch_pause: true }` against a
/// dependency-gated work item must refuse over the real RPC boundary
/// exactly like an ordinary request does — force never lifts unmet
/// dependencies — and must leave no `ready` residue behind.
#[tokio::test]
async fn work_start_force_refuses_dependency_gated_item_no_residue() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let prereq = create_chore_named(&mut client, &product_id, "Prereq").await?;
    let dependent = create_chore_named(&mut client, &product_id, "Dependent").await?;

    match client
        .send_request(&FrontendRequest::AddDependency {
            input: AddDependencyInput {
                dependent: dependent.id.clone(),
                prerequisite: prereq.id.clone(),
                relation: None,
            },
        })
        .await?
    {
        FrontendEvent::DependencyAdded { .. } => {}
        other => return Err(anyhow!("unexpected response to AddDependency: {other:?}")),
    }

    // `AddDependency` reconciles the dependent to a `waiting_dependency`
    // row automatically — capture that baseline so the assertion below is
    // about residue THIS request creates, not pre-existing gating state.
    let before = list_executions_for(&mut client, &dependent.id).await?;

    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(dependent.id.clone())
                .bypass_dispatch_pause(true)
                .entry_point(DispatchAdmissionEntryPoint::Cli)
                .build(),
        })
        .await?;
    match response {
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            assert!(
                message.to_lowercase().contains("depend"),
                "refusal must name the dependency blocker, got: {message}"
            );
        }
        other => return Err(anyhow!("expected a refusal, got: {other:?}")),
    }

    let after = list_executions_for(&mut client, &dependent.id).await?;
    assert!(
        after.iter().all(|e| e.status != boss_protocol::ExecutionStatus::Ready),
        "a dependency-refused forced request must never leave a `ready` row; got {after:?}"
    );
    assert_eq!(
        before.len(),
        after.len(),
        "a dependency-refused forced request must leave the pre-existing row set unchanged"
    );
    Ok(())
}

/// A confirmed drag (`MoveWorkItemOnBoard` with `bypass_dispatch_pause`)
/// that carries a stale pause generation — the pause was re-raised between
/// confirmation and the engine's re-check — must refuse with a work error,
/// leave the card at its previous status (not stranded in `active`), and
/// create no execution row.
#[tokio::test]
async fn board_drag_force_with_stale_pause_generation_refuses_without_activating() -> Result<()> {
    use boss_protocol::{BoardColumn, BoardDropTarget};

    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    // autostart:false so the card sits in Backlog; a default autostart:true
    // chore already renders in Doing, and a drop onto Doing is then a
    // status-preserving reorder that never reaches the force path.
    let chore = create_chore_named_with_autostart(&mut client, &product_id, "Stale gen drag", false).await?;

    match client
        .send_request(&FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: Some("investigating worker failures".to_owned()),
        })
        .await?
    {
        FrontendEvent::DispatchStateResult {
            paused,
            paused_since_epoch_s,
            ..
        } => {
            assert!(paused, "precondition: dispatch must be paused");
            assert!(
                paused_since_epoch_s.is_some(),
                "precondition: a paused state must report a generation"
            );
        }
        other => return Err(anyhow!("unexpected response to SetDispatchPaused: {other:?}")),
    }

    // Echo a deliberately stale generation — as if the pause was re-raised
    // after the confirmation dialog observed an older one.
    let response = client
        .send_request(&FrontendRequest::MoveWorkItemOnBoard {
            id: chore.id.clone(),
            target: BoardDropTarget::new(BoardColumn::Doing, None),
            bypass_dispatch_pause: true,
            observed_pause_since_epoch_s: Some(1),
        })
        .await?;
    match response {
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            assert!(
                message.to_lowercase().contains("changed")
                    || message.to_lowercase().contains("pause")
                    || message.to_lowercase().contains("force")
                    || message.to_lowercase().contains("cannot"),
                "refusal must explain why the force drag was rejected, got: {message}"
            );
        }
        other => return Err(anyhow!("expected a refusal WorkError, got: {other:?}")),
    }

    // Re-read the row: must still be at its previous (non-active) status.
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: chore.id.clone() })
        .await?
    {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t) | WorkItem::Task(t),
        } => {
            assert_ne!(
                t.status,
                boss_protocol::TaskStatus::Active,
                "a refused force drag must not leave the card active with no execution"
            );
        }
        other => return Err(anyhow!("unexpected response re-reading work item: {other:?}")),
    }

    let executions = list_executions_for(&mut client, &chore.id).await?;
    assert!(
        executions.is_empty(),
        "a refused force drag must create no execution row; got {executions:?}"
    );
    Ok(())
}

/// `RequestExecution { bypass_dispatch_pause: true }` without dispatch
/// paused at all must behave exactly like an ordinary `RequestExecution` —
/// the row is accepted and queued `ready`, with no override/refusal claim.
#[tokio::test]
async fn work_start_force_with_no_pause_behaves_ordinarily() -> Result<()> {
    let engine = spawn_engine().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product_id = create_product(&mut client).await?;
    let chore = create_chore_named(&mut client, &product_id, "Never paused").await?;

    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .bypass_dispatch_pause(true)
                .entry_point(DispatchAdmissionEntryPoint::Cli)
                .build(),
        })
        .await?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionResult { execution }
        | FrontendEvent::ExecutionCreated { execution } => {
            assert_eq!(execution.work_item_id, chore.id);
        }
        other => return Err(anyhow!("unexpected response to RequestExecution: {other:?}")),
    }
    Ok(())
}
