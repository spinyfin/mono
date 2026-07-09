//! Shared test-only helpers for constructing common DB entities.
//!
//! The engine test suite creates the same "standard product" — name
//! `Boss`, repo remote `git@github.com:spinyfin/mono.git`, every other
//! field `None` — a few hundred times. Centralising that boilerplate
//! here means a new field on [`CreateProductInput`] touches one site
//! instead of ~250, and keeps the setup readable at each call site.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;

use tokio::sync::Mutex;

use crate::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    ExecutionCoordinator, ExecutionPublisher, WorkerPool,
};
use crate::runner::{ExecutionRunner, RunOutcome};
use crate::work::{CreateChoreInput, WorkDb, WorkItemPatch};
use boss_protocol::{
    CreateExecutionInput, CreateProductInput, ExecutionKind, ExecutionStatus, FrontendEvent, Product, Task,
    WorkExecution,
};

/// The mono repo remote used by the overwhelming majority of tests.
pub const TEST_REPO_REMOTE_URL: &str = "git@github.com:spinyfin/mono.git";

/// Open a fresh file-backed [`WorkDb`] under a throwaway [`TempDir`].
///
/// The returned `TempDir` must be kept alive for the lifetime of the
/// `WorkDb` — dropping it deletes the backing `state.db`. The sweep and
/// scheduler test modules all open a DB this exact way, so this replaces
/// the byte-identical local `open_db()` each used to hand-roll.
pub fn open_db() -> (TempDir, WorkDb) {
    let dir = TempDir::new().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    (dir, db)
}

/// Create the standard test product: name `Boss`, the mono repo
/// remote, all other fields defaulted to `None`.
pub fn create_test_product(db: &WorkDb) -> Product {
    create_test_product_named(db, "Boss")
}

/// Like [`create_test_product`], but with a caller-chosen product name.
/// The repo remote is still the standard mono URL.
pub fn create_test_product_named(db: &WorkDb, name: &str) -> Product {
    create_test_product_with_repo(db, name, Some(TEST_REPO_REMOTE_URL))
}

/// Like [`create_test_product`], but with a caller-chosen name and repo
/// remote (`None` for a repo-less product). All other fields default to
/// `None`.
pub fn create_test_product_with_repo(db: &WorkDb, name: &str, repo_remote_url: Option<&str>) -> Product {
    db.create_product(CreateProductInput {
        name: name.to_owned(),
        description: None,
        repo_remote_url: repo_remote_url.map(str::to_owned),
        design_repo: None,
        docs_repo: None,
        worker_branch_prefix: None,
    })
    .unwrap()
}

/// Create the standard sweep-test product — name `test-product`, repo
/// remote `https://github.com/test/repo` — and return its id.
///
/// The sweep and scheduler test modules all create this exact product
/// and keep only the id, so this replaces the byte-identical local
/// `create_product()` each used to hand-roll.
pub fn create_product(db: &WorkDb) -> String {
    create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id
}

/// Create a chore named `name` under `product_id`, mark it `active`, and
/// return its id. This is the general form of the per-module
/// `create_active_chore` helpers; call sites that hardcoded the name
/// pass `"test chore"`.
pub fn create_active_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
        .unwrap();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    chore.id
}

/// Create a plain chore named `name` under `product_id` and return it.
///
/// Unlike [`create_active_chore`], the chore keeps its default status —
/// this is the bare
/// `create_chore(CreateChoreInput::builder().product_id(..).name(..).build())`
/// that the coordinator, completion, runner, and `work` test modules
/// hand-roll dozens of times. Centralising it means a new field on
/// [`CreateChoreInput`] touches one site instead of ~50. Args take
/// `impl Into<String>` so existing call sites lift their expressions
/// (`product.id.clone()`, `"Cleanup"`, `format!(...)`) verbatim.
pub fn create_test_chore(db: &WorkDb, product_id: impl Into<String>, name: impl Into<String>) -> Task {
    db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
        .unwrap()
}

/// Create a `ready` [`ExecutionKind::ChoreImplementation`] execution for
/// `work_item_id` and return it.
///
/// Replaces the byte-identical
/// `create_execution(CreateExecutionInput::builder().work_item_id(..)
/// .kind(ChoreImplementation).status(Ready).build())` that the
/// completion, runner, and coordinator test modules hand-roll ~33
/// times, so a new field on [`CreateExecutionInput`] touches one site.
pub fn create_ready_chore_execution(db: &WorkDb, work_item_id: impl Into<String>) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

/// A [`CubeClient`] test double that never touches cube. Every method
/// panics with `unimplemented!()` except `list_workspaces`/`list_repos`,
/// which return an empty vector.
///
/// The sweep test modules (`dead_pid_sweep`, `orphan_sweep`,
/// `pool_claim_sweep`, `stale_worker_sweep`) drive coordinators whose
/// cube interactions are never exercised, so this single stub replaces
/// the byte-identical copy each used to hand-roll.
pub struct NoopCube;

#[async_trait]
impl CubeClient for NoopCube {
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        unimplemented!()
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        unimplemented!()
    }
    async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
        unimplemented!()
    }
    async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
        unimplemented!()
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
        unimplemented!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        unimplemented!()
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        unimplemented!()
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
}

/// An [`ExecutionRunner`] test double whose `run_execution` panics with
/// `unimplemented!()`. The sweep and recovery test modules
/// (`dead_pid_sweep`, `orphan_sweep`, `pool_claim_sweep`,
/// `stale_worker_sweep`, `transient_recovery`) construct coordinators
/// whose runner is never actually driven, so this single stub replaces
/// the byte-identical copy each used to hand-roll.
pub struct NoopRunner;

#[async_trait]
impl ExecutionRunner for NoopRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        _execution: &WorkExecution,
        _work_item: &crate::work::WorkItem,
        _workspace_path: &Path,
        _cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        unimplemented!()
    }
}

/// Construct an [`ExecutionCoordinator`] wired to a `pool_size`-worker
/// [`WorkerPool`] and the [`NoopCube`]/[`NoopRunner`] test doubles.
///
/// The sweep and recovery test modules (`dead_pid_sweep`,
/// `orphan_sweep`, `pool_claim_sweep`, `spawn_ack_sweep`,
/// `stale_worker_sweep`, `transient_recovery`,
/// `dispatch_failure_recovery_sweep`, `pr_review_recovery`) all built
/// this exact coordinator, so this single helper replaces the
/// byte-identical copy each used to hand-roll.
pub fn make_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(pool_size),
        Arc::new(NoopCube),
        Arc::new(NoopRunner),
    ))
}

/// Test-only [`ExecutionPublisher`] that records every call it receives.
///
/// All three [`ExecutionPublisher`] call kinds are captured behind public
/// fields, so a single recorder serves every module that needs to assert on
/// publisher activity:
///
/// - `publish_calls` — `(execution_id, work_item_id, status, reason)` 4-tuples
///   from [`publish`](ExecutionPublisher::publish).
/// - `events` — `(product_id, work_item_id, reason)` triples from
///   [`publish_work_item_changed`](ExecutionPublisher::publish_work_item_changed).
/// - `typed_events` — `(product_id, event)` pairs from
///   [`publish_frontend_event_on_product`](ExecutionPublisher::publish_frontend_event_on_product).
///
/// The `conflict_watch`, `ci_watch`, `completion`, `coordinator`,
/// `merge_poller`, and `populator` test modules all previously hand-rolled a
/// near-duplicate recorder over some subset of these fields; this canonical
/// copy is a superset of every one, so it replaces all of them. Module-specific
/// query helpers live in the `impl` block below.
#[derive(Default)]
pub struct RecordingPublisher {
    pub publish_calls: Mutex<Vec<(String, String, String, String)>>,
    pub events: Mutex<Vec<(String, String, String)>>,
    pub typed_events: Mutex<Vec<(String, FrontendEvent)>>,
}

impl RecordingPublisher {
    /// `publish_work_item_changed` reasons with poll-state housekeeping
    /// (`pr_poll_state_updated`) filtered out, so lifecycle-focused assertions
    /// don't have to account for the background sweep's bookkeeping writes.
    pub async fn lifecycle_reasons(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|(_, _, reason)| reason != "pr_poll_state_updated")
            .map(|(_, _, reason)| reason.clone())
            .collect()
    }

    /// Count of `CiFailureCleared` frontend events broadcast for `pr_url`.
    pub async fn ci_failure_cleared_count(&self, pr_url: &str) -> usize {
        self.typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    FrontendEvent::CiFailureCleared { pr_url: p, .. } if p == pr_url
                )
            })
            .count()
    }

    /// Count of `AttentionItemCreated` frontend events published.
    pub async fn attention_items_created(&self) -> usize {
        self.typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, e)| matches!(e, FrontendEvent::AttentionItemCreated { .. }))
            .count()
    }

    /// `Some(n)` — a `WorkItemsCreated` event was published carrying `n`
    /// items. `None` if no such event was published.
    pub async fn work_items_created_len(&self) -> Option<usize> {
        self.typed_events.lock().await.iter().find_map(|(_, e)| match e {
            FrontendEvent::WorkItemsCreated { items } => Some(items.len()),
            _ => None,
        })
    }
}

#[async_trait]
impl ExecutionPublisher for RecordingPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        self.publish_calls.lock().await.push((
            execution_id.to_owned(),
            work_item_id.to_owned(),
            status.to_owned(),
            reason.to_owned(),
        ));
    }
    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.events
            .lock()
            .await
            .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
    }
    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        self.typed_events.lock().await.push((product_id.to_owned(), event));
    }
}
