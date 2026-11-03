//! Shared test-only helpers for constructing common DB entities.
//!
//! The engine test suite creates the same "standard product" — name
//! `Boss`, repo remote `git@github.com:spinyfin/mono.git`, every other
//! field `None` — a few hundred times. Centralising that boilerplate
//! here means a new field on [`CreateProductInput`] touches one site
//! instead of ~250, and keeps the setup readable at each call site.

pub use crate::lifecycle_adapter::CoreCoordinatorConstructors;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;

use crate::coordinator::ExecutionCoordinator;
use crate::work::WorkDb;
use boss_execution_contracts::WorkerPool;
use boss_execution_contracts::{ExecutionRunner, RunOutcome, RunWaitState};
use boss_host_execution::types::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
use boss_protocol::WorkExecution;

pub use crate::work::test_support::{
    TEST_REPO_REMOTE_URL, create_active_chore, create_execution_started_now, create_execution_started_secs_ago,
    create_old_execution, create_product, create_ready_chore_execution, create_spawned_execution, create_test_chore,
    create_test_chore_manual, create_test_product, create_test_product_named, create_test_product_with_repo,
    finish_run_worker_pane_alive, insert_host_capability, log_capture, review_guide_source_packet,
    seed_daily_automation, seed_published_guide, seed_review_guide_series, seed_review_guide_series_for_pr,
    start_tmux_run,
};

/// Open a fresh file-backed [`WorkDb`] under a throwaway [`TempDir`], stamped
/// with the engine's build identity.
///
/// The returned `TempDir` must be kept alive for the lifetime of the
/// `WorkDb` — dropping it deletes the backing `state.db`. Storage-only tests
/// use the unstamped `crate::work::test_support::open_db`.
pub fn open_db() -> (TempDir, WorkDb) {
    let (dir, db) = crate::work::test_support::open_db();
    (dir, db.with_engine_build_identity(crate::build_info::build_identity()))
}

/// Like [`open_db`], but returns the [`WorkDb`] wrapped in an [`Arc`] for
/// the modules that share the DB across coordinator/sweep tasks.
pub fn open_db_arc() -> (TempDir, Arc<WorkDb>) {
    let (dir, db) = open_db();
    (dir, Arc::new(db))
}

pub use boss_engine_completion::test_support::{
    confirm_remote_test_recovery, counting_source_collector, declare_no_changes_needed, preserve_test_workspace,
    record_remote_test_recovery, seed_empty_execution_bookmark, seed_published_guide_comment, source_capture_packet,
    spawn_group_leader_sleeper, wait_for_source_capture,
};
pub use boss_execution_contracts::test_doubles::{NoopCube, RecordingPublisher};

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

/// A [`CubeClient`] test double that always succeeds with fixed canned
/// values. Pairs with [`AlwaysSucceedsRunner`] so `ExecutionCoordinator::
/// force_dispatch` / `schedule_execution` can complete end-to-end in a
/// test — unlike [`NoopCube`], which panics on the first real call and is
/// only safe for tests that never actually dispatch anything.
///
/// Added for the spawn-capability breaker's half-open recovery probe
/// (`spawn_health::maybe_admit_recovery_probe`): a probe genuinely
/// force-dispatches a real ready execution, so any sweep test whose setup
/// happens to leave one behind (e.g. the steady-state active-work rescan
/// that fires after a reap — see `ExecutionCoordinator::
/// rescan_active_dispatch_after_release`) now needs a coordinator that can
/// actually carry a dispatch through, not just claim a pool slot.
pub struct AlwaysSucceedsCube;

crate::stub_cube_client! { AlwaysSucceedsCube {
    async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "test-repo".to_owned() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        prefer_workspace_id: Option<&str>,
        _allow_dirty: bool,
        _exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        let workspace_id = prefer_workspace_id
            .map(str::to_owned)
            .unwrap_or_else(|| "test-workspace-001".to_owned());
        Ok(CubeWorkspaceLease {
            lease_id: "test-lease-1".to_owned(),
            workspace_path: PathBuf::from(format!("/tmp/{workspace_id}")),
            workspace_id,
            dirty_verified: None,
        })
    }
    async fn create_change(&self, _workspace_path: &Path, _title: &str) -> Result<CubeChangeHandle> {
        Ok(CubeChangeHandle { change_id: "test-change-1".to_owned() })
    }
    async fn release_workspace(&self, _lease_id: &str) -> Result<()> {
        Ok(())
    }
    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        Ok(CubeWorkspaceStatus::builder()
            .workspace_id("test-workspace-001")
            .workspace_path(workspace_path.to_path_buf())
            .state("leased")
            .lease_id("test-lease-1")
            .holder("boss/0")
            .task("test task")
            .leased_at_epoch_s(1_700_000_000)
            .lease_expires_at_epoch_s(1_700_001_800)
            .build())
    }
    async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: Option<u64>) -> Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _lease_id: &str, _reason: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
} }

/// An [`ExecutionRunner`] test double that always reports a benign
/// `WorkerPaneAlive` outcome instead of panicking. Pairs with
/// [`AlwaysSucceedsCube`] — see its doc comment for why this pair exists.
pub struct AlwaysSucceedsRunner;

#[async_trait]
impl ExecutionRunner for AlwaysSucceedsRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        execution: &WorkExecution,
        _work_item: &crate::work::WorkItem,
        _workspace_path: &Path,
        _cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        Ok(RunOutcome {
            wait_state: RunWaitState::WorkerPaneAlive,
            result_summary: Some(format!("test run finished for {}", execution.id)),
            attention: None,
            slot_id: None,
            spawn_config: None,
        })
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

/// Like [`make_coordinator`], but backed by [`AlwaysSucceedsCube`] /
/// [`AlwaysSucceedsRunner`] instead of the panic-on-any-call `Noop*`
/// doubles, so `force_dispatch` / `schedule_execution` can actually carry a
/// dispatch through end-to-end. Use this whenever a test's coordinator
/// might realistically get a real dispatch attempt — e.g. anything that
/// exercises the spawn-capability breaker's half-open recovery probe.
pub fn make_dispatchable_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(pool_size),
        Arc::new(AlwaysSucceedsCube),
        Arc::new(AlwaysSucceedsRunner),
    ))
}

/// Running guide in the crash window before the execution launch stamp.
pub fn unstamped_codex_guide(db: &WorkDb) -> crate::work::WorkExecution {
    let root = create_active_chore(db, &create_product(db), "guide restart");
    let (series, comparison) = seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
        .unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.record_pr_review_guide_attempt_spawn_config(&attempt.id, "codex", "gpt-6-astra", "high")
        .unwrap();
    db.start_execution_run(&execution.id, "review-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    db.set_run_shell_pid_for_execution(&execution.id, i64::from(std::process::id()))
        .unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    assert!(execution.model.is_none());
    assert!(db.launched_driver_slug(&execution.id).unwrap().is_none());
    execution
}

pub fn apply_review_verdict(
    db: &WorkDb,
    id: &str,
    checker: &dyn crate::work::PrStateChecker,
) -> anyhow::Result<Option<String>> {
    crate::review_verdict_apply_sweep::apply_review_verdict_proposal(db, id, checker, &|input| {
        checker.merged_parent_deletions(&input.repo_slug, &input.head_before, &input.base_sha, &input.head_after)
    })
}

pub fn apply_pending_review_verdicts(
    db: &WorkDb,
    checker: &dyn crate::work::PrStateChecker,
) -> anyhow::Result<crate::work::ReviewVerdictApplyStats> {
    crate::review_verdict_apply_sweep::apply_pending_review_verdicts(db, checker, &|input| {
        checker.merged_parent_deletions(&input.repo_slug, &input.head_before, &input.base_sha, &input.head_after)
    })
}
