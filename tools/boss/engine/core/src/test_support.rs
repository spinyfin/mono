//! Shared test-only helpers for constructing common DB entities.
//!
//! The engine test suite creates the same "standard product" — name
//! `Boss`, repo remote `git@github.com:spinyfin/mono.git`, every other
//! field `None` — a few hundred times. Centralising that boilerplate
//! here means a new field on [`CreateProductInput`] touches one site
//! instead of ~250, and keeps the setup readable at each call site.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    Automation, AutomationTrigger, CreateAutomationInput, CreateExecutionInput, CreateProductInput, ExecutionKind,
    ExecutionStatus, FinishExecutionRunInput, FrontendEvent, Product, RequestExecutionInput, Task, WorkExecution,
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

/// Like [`create_test_chore`], but creates the chore with `autostart`
/// disabled (`.autostart(false)`).
///
/// This is the manual-start counterpart the completion, runner,
/// coordinator, merge-poller, `work`, and app test modules hand-roll
/// dozens of times as
/// `create_chore(CreateChoreInput::builder().product_id(..).name(..).autostart(false).build())`.
/// Centralising it means a new field on [`CreateChoreInput`] touches one
/// site instead of ~120. As with [`create_test_chore`], args take
/// `impl Into<String>` so existing call sites lift their expressions
/// (`product.id.clone()`, `"Parent chore"`, `format!(...)`) verbatim.
pub fn create_test_chore_manual(db: &WorkDb, product_id: impl Into<String>, name: impl Into<String>) -> Task {
    db.create_chore(
        CreateChoreInput::builder()
            .product_id(product_id)
            .name(name)
            .autostart(false)
            .build(),
    )
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

/// Request an execution for `work_item_id`, then stamp its `started_at`
/// to `secs_ago` seconds in the past. Returns the execution id.
///
/// The sweep test modules use this to age an execution past a
/// grace-period guard so the sweep under test considers it. `secs_ago`
/// is saturated against the epoch, so an implausibly large value simply
/// clamps to `0` rather than underflowing.
pub fn create_execution_started_secs_ago(db: &WorkDb, work_item_id: &str, secs_ago: u64) -> String {
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap();
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(secs_ago) as i64;
    db.force_started_at_for_test(&execution.id, started_at).unwrap();
    execution.id
}

/// Create a `ready` execution for `work_item_id` and stamp its
/// `started_at` to 5 minutes ago so a grace-period guard passes.
///
/// Thin wrapper over [`create_execution_started_secs_ago`] with the
/// 300-second offset the sweep tests share.
pub fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
    create_execution_started_secs_ago(db, work_item_id, 300)
}

/// Finish an execution's active run the way `PaneSpawnRunner` does: record
/// the run as `completed` while parking the execution in `waiting_human`
/// with its workspace lease still held. This is the post-spawn state the
/// coordinator observes, and the `completion` test module hand-rolled the
/// same `finish_execution_run(...)` builder block at a dozen-odd sites.
///
/// Pass the per-site `result_summary` (or `None` where the test omits it).
pub fn finish_run_waiting_human(db: &WorkDb, execution_id: &str, run_id: &str, result_summary: Option<&str>) {
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(execution_id)
            .run_id(run_id)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .maybe_result_summary(result_summary)
            .build(),
    )
    .unwrap();
}

/// Seed the standard "daily automation" fixture under `product_id`:
/// name `daily`, a `0 14 * * *` UTC [`AutomationTrigger::Schedule`],
/// standing instruction `"do the thing"`, every other field defaulted.
///
/// Returns the whole [`Automation`] record; call sites that only need
/// the id take `.id`. The automation-scheduler and sweep test modules
/// (`automation_scheduler`, `cube_lease_heartbeat`, `dead_pane_sweep`,
/// `execution_liveness`, `lost_workspace_sweep`) all hand-rolled this
/// exact block, so centralising it means a new field on
/// [`CreateAutomationInput`] touches one site instead of all of them.
pub fn seed_daily_automation(db: &WorkDb, product_id: &str) -> Automation {
    db.create_automation(
        CreateAutomationInput::builder()
            .product_id(product_id.to_owned())
            .name("daily")
            .trigger(AutomationTrigger::Schedule {
                cron: "0 14 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            })
            .standing_instruction("do the thing")
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

/// Implement [`CubeClient`](crate::coordinator::CubeClient) for a test
/// double, spelling out only the methods a given test actually drives.
/// Every trait method left unlisted is filled in with `unimplemented!()`,
/// which is exactly what a double wants for the paths it never exercises.
///
/// Overrides are written as ordinary `async fn`s with their real bodies
/// and **must appear in the trait's declaration order** (the order the
/// arms below enumerate: `ensure_repo`, `lease_workspace`, `create_change`,
/// `goto_workspace`, `rebase_workspace`, `rebase_workspace_no_push`,
/// `push_resolution`, `release_workspace`, `workspace_status`,
/// `heartbeat_lease`, `force_release_lease`, `list_workspaces`, `list_repos`).
/// `rebase_workspace`, `rebase_workspace_no_push`, and `push_resolution` all
/// have trait defaults (erroring), so leaving any unlisted keeps that
/// default rather than an
/// `unimplemented!()`. The macro emits the `#[async_trait]` impl for you.
///
/// ```ignore
/// stub_cube_client! { RecordingCube {
///     async fn release_workspace(&self, lease_id: &str) -> Result<()> {
///         self.releases.lock().await.push(lease_id.to_owned());
///         Ok(())
///     }
/// } }
/// ```
#[macro_export]
macro_rules! stub_cube_client {
    ($ty:ty { $($body:tt)* }) => {
        $crate::stub_cube_client!(@munch $ty [] @ensure_repo $($body)*);
    };

    // ── ensure_repo ─────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo async fn ensure_repo $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn ensure_repo $a -> $r $b] @lease_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn ensure_repo(&self, _origin: &str) -> ::anyhow::Result<$crate::coordinator::CubeRepoHandle> { ::core::unimplemented!() }
        ] @lease_workspace $($rest)*);
    };

    // ── lease_workspace ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace async fn lease_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn lease_workspace $a -> $r $b] @create_change $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn lease_workspace(&self, _repo_id: &str, _task: &str, _prefer: ::core::option::Option<&str>, _allow_dirty: bool, _exclude: &[&str]) -> ::anyhow::Result<$crate::coordinator::CubeWorkspaceLease> { ::core::unimplemented!() }
        ] @create_change $($rest)*);
    };

    // ── create_change ───────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @create_change async fn create_change $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn create_change $a -> $r $b] @goto_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @create_change $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn create_change(&self, _workspace_path: &::std::path::Path, _title: &str) -> ::anyhow::Result<$crate::coordinator::CubeChangeHandle> { ::core::unimplemented!() }
        ] @goto_workspace $($rest)*);
    };

    // ── goto_workspace ──────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace async fn goto_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn goto_workspace $a -> $r $b] @rebase_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn goto_workspace(&self, _workspace_path: &::std::path::Path, _pr: u64) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @rebase_workspace $($rest)*);
    };

    // ── rebase_workspace ────────────────────────────────────────────────────
    // Has a trait default (erroring), so when a double doesn't override it the
    // macro emits nothing and the default applies — matching how the many
    // pre-ladder stubs behave (they never drive rung 1).
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace async fn rebase_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn rebase_workspace $a -> $r $b] @rebase_workspace_no_push $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @rebase_workspace_no_push $($rest)*);
    };

    // ── rebase_workspace_no_push ────────────────────────────────────────────
    // Also has a trait default (erroring) — same "unlisted keeps the default"
    // behaviour as `rebase_workspace`, used by the speculative-conflict
    // prediction sweep (T10).
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace_no_push async fn rebase_workspace_no_push $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn rebase_workspace_no_push $a -> $r $b] @push_resolution $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace_no_push $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @push_resolution $($rest)*);
    };

    // ── push_resolution ─────────────────────────────────────────────────────
    // Has a trait default (erroring), same convention as rebase_workspace.
    (@munch $ty:ty [$($acc:tt)*] @push_resolution async fn push_resolution $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn push_resolution $a -> $r $b] @verify_deletion_tripwire $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @push_resolution $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @verify_deletion_tripwire $($rest)*);
    };

    // ── verify_deletion_tripwire ────────────────────────────────────────────
    // Has a trait default (fails open, empty findings) — same "unlisted keeps
    // the default" convention as rebase_workspace/push_resolution: a double
    // that doesn't script this never makes a network call.
    (@munch $ty:ty [$($acc:tt)*] @verify_deletion_tripwire async fn verify_deletion_tripwire $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn verify_deletion_tripwire $a -> $r $b] @release_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @verify_deletion_tripwire $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @release_workspace $($rest)*);
    };

    // ── release_workspace ───────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @release_workspace async fn release_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn release_workspace $a -> $r $b] @workspace_status $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn release_workspace(&self, _lease_id: &str) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @workspace_status $($rest)*);
    };

    // ── workspace_status ────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @workspace_status async fn workspace_status $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn workspace_status $a -> $r $b] @heartbeat_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @workspace_status $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn workspace_status(&self, _workspace_path: &::std::path::Path) -> ::anyhow::Result<$crate::coordinator::CubeWorkspaceStatus> { ::core::unimplemented!() }
        ] @heartbeat_lease $($rest)*);
    };

    // ── heartbeat_lease ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease async fn heartbeat_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn heartbeat_lease $a -> $r $b] @force_release_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: ::core::option::Option<u64>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @force_release_lease $($rest)*);
    };

    // ── force_release_lease ─────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease async fn force_release_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn force_release_lease $a -> $r $b] @list_workspaces $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn force_release_lease(&self, _lease_id: &str, _reason: ::core::option::Option<&str>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @list_workspaces $($rest)*);
    };

    // ── list_workspaces ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces async fn list_workspaces $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn list_workspaces $a -> $r $b] @list_repos $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn list_workspaces(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::coordinator::CubeWorkspaceStatus>> { ::core::unimplemented!() }
        ] @list_repos $($rest)*);
    };

    // ── list_repos ──────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_repos async fn list_repos $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn list_repos $a -> $r $b] @done $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_repos $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn list_repos(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::coordinator::CubeRepoSummary>> { ::core::unimplemented!() }
        ] @done $($rest)*);
    };

    // ── emit ────────────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @done) => {
        #[::async_trait::async_trait]
        impl $crate::coordinator::CubeClient for $ty {
            $($acc)*
        }
    };
}

/// Implement [`HostAdapter`](crate::host_adapter::HostAdapter) for a test
/// double, spelling out only the methods a given test drives. Every
/// workspace-lifecycle method and `spawn_worker` left unlisted is filled
/// in with `unimplemented!()`; `command_repr` defaults to `None`; and the
/// three trait-defaulted methods (`read_transcript_tail_bytes`,
/// `reattach_events_forward`, `probe_remote_worker_alive`) fall through to
/// their trait defaults unless overridden.
///
/// `host_id` has no universal default and **must be listed first**;
/// remaining overrides follow in the trait's declaration order.
#[macro_export]
macro_rules! stub_host_adapter {
    ($ty:ty { $($body:tt)* }) => {
        $crate::stub_host_adapter!(@munch $ty [] @host_id $($body)*);
    };

    // ── host_id (required) ──────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @host_id fn host_id $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* fn host_id $a -> $r $b] @ensure_repo $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @host_id $($rest:tt)*) => {
        ::core::compile_error!("stub_host_adapter! requires `fn host_id(&self) -> &str` as the first method");
    };

    // ── ensure_repo ─────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo async fn ensure_repo $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn ensure_repo $a -> $r $b] @lease_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn ensure_repo(&self, _origin: &str) -> ::anyhow::Result<$crate::coordinator::CubeRepoHandle> { ::core::unimplemented!() }
        ] @lease_workspace $($rest)*);
    };

    // ── lease_workspace ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace async fn lease_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn lease_workspace $a -> $r $b] @release_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn lease_workspace(&self, _repo_id: &str, _task: &str, _prefer: ::core::option::Option<&str>, _allow_dirty: bool, _exclude: &[&str]) -> ::anyhow::Result<$crate::coordinator::CubeWorkspaceLease> { ::core::unimplemented!() }
        ] @release_workspace $($rest)*);
    };

    // ── release_workspace ───────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @release_workspace async fn release_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn release_workspace $a -> $r $b] @heartbeat_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn release_workspace(&self, _lease_id: &str) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @heartbeat_lease $($rest)*);
    };

    // ── heartbeat_lease ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease async fn heartbeat_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn heartbeat_lease $a -> $r $b] @force_release_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: ::core::option::Option<u64>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @force_release_lease $($rest)*);
    };

    // ── force_release_lease ─────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease async fn force_release_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn force_release_lease $a -> $r $b] @create_change $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn force_release_lease(&self, _lease_id: &str, _reason: ::core::option::Option<&str>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @create_change $($rest)*);
    };

    // ── create_change ───────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @create_change async fn create_change $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn create_change $a -> $r $b] @goto_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @create_change $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn create_change(&self, _workspace_path: &::std::path::Path, _title: &str) -> ::anyhow::Result<$crate::coordinator::CubeChangeHandle> { ::core::unimplemented!() }
        ] @goto_workspace $($rest)*);
    };

    // ── goto_workspace ──────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace async fn goto_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn goto_workspace $a -> $r $b] @workspace_status $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn goto_workspace(&self, _workspace_path: &::std::path::Path, _pr: u64) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @workspace_status $($rest)*);
    };

    // ── workspace_status ────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @workspace_status async fn workspace_status $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn workspace_status $a -> $r $b] @list_workspaces $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @workspace_status $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn workspace_status(&self, _workspace_path: &::std::path::Path) -> ::anyhow::Result<$crate::coordinator::CubeWorkspaceStatus> { ::core::unimplemented!() }
        ] @list_workspaces $($rest)*);
    };

    // ── list_workspaces ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces async fn list_workspaces $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn list_workspaces $a -> $r $b] @list_repos $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn list_workspaces(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::coordinator::CubeWorkspaceStatus>> { ::core::unimplemented!() }
        ] @list_repos $($rest)*);
    };

    // ── list_repos ──────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_repos async fn list_repos $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn list_repos $a -> $r $b] @command_repr $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_repos $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn list_repos(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::coordinator::CubeRepoSummary>> { ::core::unimplemented!() }
        ] @command_repr $($rest)*);
    };

    // ── command_repr (defaults to None) ─────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @command_repr fn command_repr $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* fn command_repr $a -> $r $b] @spawn_worker $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @command_repr $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            fn command_repr(&self, _args: &[&str]) -> ::core::option::Option<(::std::string::String, ::std::string::String)> { ::core::option::Option::None }
        ] @spawn_worker $($rest)*);
    };

    // ── spawn_worker ────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @spawn_worker async fn spawn_worker $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn spawn_worker $a -> $r $b] @read_transcript_tail_bytes $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @spawn_worker $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn spawn_worker(&self, _worker_id: &str, _execution: &$crate::work::WorkExecution, _work_item: &$crate::work::WorkItem, _workspace_path: &::std::path::Path, _cube_change_id: ::core::option::Option<&str>) -> ::anyhow::Result<$crate::runner::RunOutcome> { ::core::unimplemented!() }
        ] @read_transcript_tail_bytes $($rest)*);
    };

    // ── read_transcript_tail_bytes (trait default unless overridden) ─────────
    (@munch $ty:ty [$($acc:tt)*] @read_transcript_tail_bytes async fn read_transcript_tail_bytes $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn read_transcript_tail_bytes $a -> $r $b] @reattach_events_forward $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @read_transcript_tail_bytes $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @reattach_events_forward $($rest)*);
    };

    // ── reattach_events_forward (trait default unless overridden) ────────────
    (@munch $ty:ty [$($acc:tt)*] @reattach_events_forward async fn reattach_events_forward $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn reattach_events_forward $a -> $r $b] @probe_remote_worker_alive $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @reattach_events_forward $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @probe_remote_worker_alive $($rest)*);
    };

    // ── probe_remote_worker_alive (trait default unless overridden) ──────────
    (@munch $ty:ty [$($acc:tt)*] @probe_remote_worker_alive async fn probe_remote_worker_alive $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn probe_remote_worker_alive $a -> $r $b] @done $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @probe_remote_worker_alive $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @done $($rest)*);
    };

    // ── emit ────────────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @done) => {
        #[::async_trait::async_trait]
        impl $crate::host_adapter::HostAdapter for $ty {
            $($acc)*
        }
    };
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
