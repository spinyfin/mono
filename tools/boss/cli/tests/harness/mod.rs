//! Shared in-process engine harness for the `boss` CLI integration tests
//! that need a live engine.
//!
//! `TestEngine::spawn` starts the engine's `serve` loop on a temp Unix socket
//! backed by a temp SQLite db; `socket_str()` exposes the wire path to pass to
//! the `boss` binary and `db()` opens the same db for tests that seed rows
//! directly. This is its own `rust_library` (testonly), depended on by every
//! engine-backed integration test and used via `use harness::...`. Because
//! it's a library crate, `pub` items that only some dependents use (e.g.
//! `db()`) are not flagged as dead code the way they would be if this were
//! compiled directly into each test binary.
//!
//! The subprocess-driving helpers (`boss_binary`, `run_boss`, …) live in the
//! sibling `common` library.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::WorkDb;
use boss_protocol::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, FrontendEvent, FrontendRequest, Product,
    Project, Task, WorkItem,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct TestEngine {
    socket_path: PathBuf,
    db_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    pub async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let work_config = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path.clone())
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }
        Ok(Self {
            socket_path,
            db_path,
            _temp: temp,
            join,
        })
    }

    pub fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }

    pub fn db(&self) -> Result<WorkDb> {
        WorkDb::open(self.db_path.clone())
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

// ---------------------------------------------------------------------------
// BossClient-based work-item creation helpers.
//
// Every engine-backed CLI test needs to seed a product / project / task /
// chore over the wire before driving the `boss` binary. Each `create_*`
// helper sends the matching `FrontendRequest` and unwraps the created item
// from `FrontendEvent::WorkItemCreated`. The `*_with` variant takes a fully
// built input; the plain variant is a convenience that fills in stock test
// values. These previously lived as copy-pasted private fns in ~8 test
// files.
// ---------------------------------------------------------------------------

/// Send a `CreateProduct` request and unwrap the created [`Product`].
pub async fn create_product_with(client: &mut BossClient, input: CreateProductInput) -> Result<Product> {
    match client.send_request(&FrontendRequest::CreateProduct { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => Ok(p),
        other => Err(anyhow!("unexpected engine event for product create: {other:?}")),
    }
}

/// Create a product from a name, with a stock test repo remote.
pub async fn create_product(client: &mut BossClient, name: &str) -> Result<Product> {
    create_product_with(
        client,
        CreateProductInput::builder()
            .name(name)
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await
}

/// Send a `CreateProject` request and unwrap the created [`Project`].
pub async fn create_project_with(client: &mut BossClient, input: CreateProjectInput) -> Result<Project> {
    match client.send_request(&FrontendRequest::CreateProject { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Project(p),
        } => Ok(p),
        other => Err(anyhow!("unexpected engine event for project create: {other:?}")),
    }
}

/// Create a non-autostarting project under `product_id` from a name.
pub async fn create_project(client: &mut BossClient, product_id: &str, name: &str) -> Result<Project> {
    create_project_with(
        client,
        CreateProjectInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        },
    )
    .await
}

/// Send a `CreateTask` request and unwrap the created [`Task`].
pub async fn create_task_with(client: &mut BossClient, input: CreateTaskInput) -> Result<Task> {
    match client.send_request(&FrontendRequest::CreateTask { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected engine event for task create: {other:?}")),
    }
}

/// Create a non-autostarting task under `product_id` / `project_id`.
pub async fn create_task(client: &mut BossClient, product_id: &str, project_id: &str, name: &str) -> Result<Task> {
    create_task_with(
        client,
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .await
}

/// Send a `CreateChore` request and unwrap the created [`Task`]. Accepts
/// either the `Chore` or `Task` `WorkItem` shape the engine may return.
pub async fn create_chore_with(client: &mut BossClient, input: CreateChoreInput) -> Result<Task> {
    match client.send_request(&FrontendRequest::CreateChore { input }).await? {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t) | WorkItem::Task(t),
        } => Ok(t),
        other => Err(anyhow!("unexpected engine event for chore create: {other:?}")),
    }
}

/// Create a non-autostarting chore under `product_id` from a name.
pub async fn create_chore(client: &mut BossClient, product_id: &str, name: &str) -> Result<Task> {
    create_chore_with(
        client,
        CreateChoreInput::builder()
            .product_id(product_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .await
}
