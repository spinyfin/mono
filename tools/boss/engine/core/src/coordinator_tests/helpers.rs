//! Shared fixtures for the `coordinator` test suite, grouped by the concern
//! each sibling module exercises. The `pub(super) use` re-exports below let
//! every test module pull the whole vocabulary in with `use super::helpers::*;`.

#![allow(unused_imports)]

pub(super) use super::super::*;

pub(super) use crate::spawn_flow::StartWorkerError;
pub(super) use crate::test_support::*;
pub(super) use std::future::pending;
pub(super) use std::path::PathBuf;
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::Ordering;
pub(super) use std::time::Duration;

pub(super) use anyhow::{Result, anyhow};
pub(super) use async_trait::async_trait;
pub(super) use tempfile::tempdir;
pub(super) use tokio::sync::Mutex;
pub(super) use tokio::time::sleep;

pub(super) use boss_protocol::{EngineToAppError, ExecutionStatus};

pub(super) use crate::runner::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};
pub(super) use crate::work::{
    AddDependencyInput, CreateChoreInput, CreateExecutionInput, CreateProductInput, CreateProjectInput,
    CreateTaskInput, FinishExecutionRunInput, RequestExecutionInput, TaskStatus, WorkDb, WorkExecution, WorkItem,
    WorkItemPatch,
};

/// Recorded args for each `lease_workspace` call:
/// `(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)`.
pub(super) type LeaseCall = (String, String, Option<String>, bool, Vec<String>);

// `#[cfg(test)]` is redundant under this file's `#[cfg(test)] mod tests` parent,
// but checkleft's `rust/giant-structs` check parses each file in isolation and
// cannot see the ancestor `cfg(test)`; the marker tells it this is the test-only
// fixture it is meant to skip (the builder-pattern rule targets production types).
#[cfg(test)]
#[derive(Default)]
pub(super) struct FakeCubeClient {
    pub(super) ensure_calls: Mutex<Vec<String>>,
    pub(super) lease_calls: Mutex<Vec<LeaseCall>>,
    pub(super) goto_calls: Mutex<Vec<(String, u64)>>,
    pub(super) create_calls: Mutex<Vec<(String, String)>>,
    pub(super) release_calls: Mutex<Vec<String>>,
    pub(super) status_calls: Mutex<Vec<PathBuf>>,
    pub(super) heartbeat_calls: Mutex<Vec<(String, Option<u64>)>>,
    pub(super) force_release_calls: Mutex<Vec<(String, Option<String>)>>,
    /// Counts how many times `list_repos` has been invoked. Tests
    /// for the cold-pool probe assert this equals 1 across two
    /// dispatches against the same URL (probe is engine-lifetime
    /// deduped).
    pub(super) list_repos_calls: Mutex<u32>,
    /// Snapshot returned by `list_repos`. Default is the empty
    /// slice — most tests don't exercise the cold-pool probe and
    /// the empty list short-circuits before any attention item is
    /// written.
    pub(super) repos: Mutex<Vec<CubeRepoSummary>>,
    pub(super) fail_ensure: bool,
    pub(super) fail_lease: bool,
    /// Model the anaplian failure-mode A: cube exits non-zero on a
    /// post-lease setup step, surfaced as a typed [`CubeCliError`]
    /// carrying the exit code + stderr (as the remote SSH adapter
    /// does). Lets a test assert those signals reach the dispatch
    /// event `details`.
    pub(super) fail_lease_with_cube_cli_error: bool,
    /// Simulate cube refusing a `--prefer` request because the
    /// preferred workspace is held: `lease_workspace` errors when
    /// `prefer_workspace_id` is `Some(_)`. Models the "prefer set,
    /// no fallback" path — the engine should fail fast rather than
    /// silently landing on a different workspace.
    pub(super) fail_lease_when_prefer_set: bool,
    /// Fail the first N lease calls (0-indexed), then succeed. Used
    /// to model a single bad workspace being skipped via `any_free`
    /// retry when `preferred_workspace_id=null`.
    pub(super) fail_first_n_leases: usize,
    pub(super) fail_create: bool,
    pub(super) fail_goto: bool,
    pub(super) next_workspace_id: Mutex<Option<String>>,
    /// Ordered queue of workspace IDs to return from successive
    /// `lease_workspace` calls. When non-empty, dequeues from the
    /// front; when empty, falls through to `next_workspace_id` or the
    /// default "mono-agent-001". Used by livelock tests that need the
    /// first lease call to return an occupied workspace and subsequent
    /// calls to return a free one.
    pub(super) workspace_id_queue: Mutex<std::collections::VecDeque<String>>,
    /// Canned response for `list_workspaces` — lets a test model cube
    /// reporting a workspace still leased to a dead worker so the
    /// stale-lease reclaim path (issue #962) can be exercised.
    pub(super) list_workspaces_response: Mutex<Vec<CubeWorkspaceStatus>>,
}

impl FakeCubeClient {
    pub(super) fn with_next_workspace_id(self, id: impl Into<String>) -> Self {
        *self.next_workspace_id.try_lock().expect("uncontended") = Some(id.into());
        self
    }

    pub(super) fn with_workspace_id_queue(self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        *self.workspace_id_queue.try_lock().expect("uncontended") = ids.into_iter().map(|s| s.into()).collect();
        self
    }

    pub(super) fn with_repos(self, repos: Vec<CubeRepoSummary>) -> Self {
        *self.repos.try_lock().expect("uncontended") = repos;
        self
    }

    pub(super) fn with_list_workspaces(self, rows: Vec<CubeWorkspaceStatus>) -> Self {
        *self.list_workspaces_response.try_lock().expect("uncontended") = rows;
        self
    }
}

crate::stub_cube_client! { FakeCubeClient {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        self.ensure_calls.lock().await.push(origin.to_owned());
        if self.fail_ensure {
            return Err(anyhow!("cube repo ensure failed"));
        }
        Ok(CubeRepoHandle {
            repo_id: "mono".to_owned(),
        })
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        let mut calls = self.lease_calls.lock().await;
        let call_index = calls.len();
        calls.push((
            repo_id.to_owned(),
            task.to_owned(),
            prefer_workspace_id.map(str::to_owned),
            allow_dirty,
            exclude_workspace_ids.iter().map(|s| s.to_string()).collect(),
        ));
        drop(calls);
        if self.fail_lease_with_cube_cli_error {
            return Err(crate::cube_commands::CubeCliError {
                host: "anaplian".to_owned(),
                exit_code: Some(1),
                stderr: "setup step `copy-config-secrets` failed: cp: backend/config-secrets.toml: \
                         No such file or directory"
                    .to_owned(),
                stdout: String::new(),
            }
            .into());
        }
        if self.fail_lease {
            return Err(anyhow!("cube workspace lease failed"));
        }
        if self.fail_lease_when_prefer_set && prefer_workspace_id.is_some() {
            return Err(anyhow!(
                "cube workspace lease failed: preferred workspace held by another worker"
            ));
        }
        if call_index < self.fail_first_n_leases {
            return Err(anyhow!("cube workspace lease failed: workspace has uncommitted work"));
        }
        // Queue takes priority; falls through to next_workspace_id, then prefer,
        // then the default. The queue lets tests model a sequence of
        // workspace responses (e.g. occupied-then-free for livelock tests).
        let workspace_id = self
            .workspace_id_queue
            .lock()
            .await
            .pop_front()
            .or_else(|| self.next_workspace_id.try_lock().ok().and_then(|g| g.clone()))
            .or_else(|| prefer_workspace_id.map(str::to_owned))
            .unwrap_or_else(|| "mono-agent-001".to_owned());
        Ok(CubeWorkspaceLease {
            lease_id: "lease-1".to_owned(),
            workspace_id: workspace_id.clone(),
            workspace_path: PathBuf::from(format!("/tmp/{workspace_id}")),
            dirty_verified: None,
        })
    }

    async fn create_change(&self, workspace_path: &std::path::Path, title: &str) -> Result<CubeChangeHandle> {
        self.create_calls
            .lock()
            .await
            .push((workspace_path.display().to_string(), title.to_owned()));
        if self.fail_create {
            return Err(anyhow!("cube change create failed"));
        }
        Ok(CubeChangeHandle {
            change_id: "chg-1".to_owned(),
        })
    }

    async fn goto_workspace(&self, workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.goto_calls
            .lock()
            .await
            .push((workspace_path.display().to_string(), pr));
        if self.fail_goto {
            return Err(anyhow!("cube workspace goto failed"));
        }
        Ok(())
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.release_calls.lock().await.push(lease_id.to_owned());
        Ok(())
    }

    async fn workspace_status(&self, workspace_path: &std::path::Path) -> Result<CubeWorkspaceStatus> {
        self.status_calls.lock().await.push(workspace_path.to_path_buf());
        Ok(CubeWorkspaceStatus::builder()
            .workspace_id("mono-agent-001")
            .workspace_path(workspace_path.to_path_buf())
            .state("leased")
            .lease_id("lease-1")
            .holder("boss/0")
            .task("test task")
            .leased_at_epoch_s(1_700_000_000)
            .lease_expires_at_epoch_s(1_700_001_800)
            .build())
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.heartbeat_calls
            .lock()
            .await
            .push((lease_id.to_owned(), ttl_seconds));
        Ok(())
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.force_release_calls
            .lock()
            .await
            .push((lease_id.to_owned(), reason.map(str::to_owned)));
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(self.list_workspaces_response.lock().await.clone())
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        *self.list_repos_calls.lock().await += 1;
        Ok(self.repos.lock().await.clone())
    }
} }

/// Recorded args for each `run` call:
/// `(worker_id, execution_id, workspace_path, cube_change_id)`.
pub(super) type RunnerCall = (String, String, String, Option<String>);

// See the note on `FakeCubeClient` above: the redundant `#[cfg(test)]` is what
// lets checkleft's per-file `rust/giant-structs` parser recognize this fixture
// as test-only and skip the builder-pattern requirement.
#[cfg(test)]
pub(super) struct FakeExecutionRunner {
    pub(super) calls: Mutex<Vec<RunnerCall>>,
    pub(super) fail: bool,
    /// When `true`, `run_execution` fails with a `SlotBusy` app
    /// rejection (wrapped the same way `spawn_flow` wraps it) instead
    /// of the generic `fail` error, so tests can exercise the
    /// hold-slot / requeue-instead-of-fail path distinctly from a
    /// genuine spawn failure. Takes priority over `fail`.
    pub(super) slot_busy: bool,
    pub(super) pending: bool,
    /// If `Some`, the runner reports this slot id back to the
    /// coordinator in the `RunOutcome`, simulating a successful
    /// `SpawnWorkerPane` round-trip. Used to verify that the
    /// coordinator stamps the slot-based agent_id onto the run
    /// record.
    pub(super) slot_id: Option<u8>,
    /// Resolved spawn knobs the fake runner reports back. `None`
    /// matches the default fake-runner contract (no effort/model
    /// resolution happened). Production `PaneSpawnRunner` always
    /// fills this in — tests that want to assert on the
    /// dispatcher's effort/model surfacing set it explicitly.
    pub(super) spawn_config: Option<crate::effort::SpawnConfig>,
    /// When `true`, simulate the T981 mid-spawn cancel: cancel the
    /// execution row (via `work_db`, mirroring the real cancel path
    /// that ran while the spawn was in flight) and report
    /// `RunWaitState::CancelledDuringSpawn`. The coordinator must
    /// then release the deferred lease and skip completion recording.
    pub(super) cancelled_during_spawn: bool,
    /// When `true`, simulate the T267 provisional spawn: the real
    /// `PaneSpawnRunner` converts a `SpawnWorkerPane` ack timeout into
    /// a provisional (unverified) spawn — the pane may be live, so the
    /// run is tracked in `waiting_human` with its slot retained and the
    /// lease is NOT released. The fake returns that same outcome so the
    /// coordinator-side contract (no release, no duplicate dispatch,
    /// still tracked) can be asserted. The Timeout→provisional
    /// conversion itself is unit-tested in `spawn_flow`.
    pub(super) ack_timed_out: bool,
    /// Handle used by the `cancelled_during_spawn` path to cancel the
    /// row before returning. `None` for the default fake.
    pub(super) work_db: Option<Arc<WorkDb>>,
}

impl Default for FakeExecutionRunner {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: false,
            slot_busy: false,
            pending: false,
            slot_id: None,
            spawn_config: None,
            cancelled_during_spawn: false,
            ack_timed_out: false,
            work_db: None,
        }
    }
}

#[async_trait]
impl ExecutionRunner for FakeExecutionRunner {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &std::path::Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        self.calls.lock().await.push((
            worker_id.to_owned(),
            execution.id.clone(),
            workspace_path.display().to_string(),
            cube_change_id.map(str::to_owned),
        ));
        if self.pending {
            pending::<()>().await;
        }
        if self.slot_busy {
            let root = StartWorkerError::AppError(EngineToAppError::SlotBusy {
                occupying_run_id: Some("exec_other_occupant".to_owned()),
            });
            return Err(anyhow::Error::new(root).context("failed to spawn worker pane"));
        }
        if self.fail {
            return Err(anyhow!("worker prompt failed"));
        }

        if self.cancelled_during_spawn {
            // Mirror the real race: the cancel landed while the
            // spawn round-trip was in flight, marking the row
            // cancelled. The runner (having reaped the pane) reports
            // CancelledDuringSpawn so the coordinator releases the
            // lease the cancel path left held.
            if let Some(db) = &self.work_db {
                db.cancel_running_execution(&execution.id)
                    .expect("cancel row in fake mid-spawn cancel");
            }
            return Ok(RunOutcome {
                wait_state: RunWaitState::CancelledDuringSpawn,
                result_summary: Some("cancelled during spawn".to_owned()),
                attention: None,
                slot_id: None,
                spawn_config: None,
            });
        }

        if self.ack_timed_out {
            // Mirror the real PaneSpawnRunner after a SpawnWorkerPane
            // ack timeout: a PROVISIONAL spawn. The pane may be live,
            // so the run is tracked in `waiting_human` with its slot
            // retained (slot_id = Some ⇒ the coordinator defers the
            // pool-slot release and does NOT release the workspace
            // lease). No attention item — this is not a failure.
            return Ok(RunOutcome {
                wait_state: RunWaitState::WaitingHuman,
                result_summary: Some("provisional spawn: SpawnWorkerPane ack timed out".to_owned()),
                attention: None,
                slot_id: Some(1),
                spawn_config: None,
            });
        }

        Ok(RunOutcome {
            wait_state: RunWaitState::WaitingHuman,
            result_summary: Some(format!("finished {}", execution.kind)),
            attention: Some(RunAttention {
                kind: "review_required".to_owned(),
                title: format!("Review {}", execution.kind),
                body_markdown: format!("Review {}", test_work_item_name(work_item)),
            }),
            slot_id: self.slot_id,
            spawn_config: self.spawn_config.clone(),
        })
    }
}

pub(super) async fn wait_for_execution_status(db: &WorkDb, execution_id: &str, expected: ExecutionStatus) {
    for _ in 0..100 {
        let execution = db.get_execution(execution_id).unwrap();
        if execution.status == expected {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("execution {execution_id} never reached status `{expected}`");
}

pub(super) fn test_work_item_name(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.name,
        WorkItem::Project(project) => &project.name,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
    }
}

/// Helper: create a product + chore pair and return their ids. When
/// `pr_url` is `Some`, the chore's `pr_url` field is also set so that
/// `schedule_execution` picks it up for the reviewer positioning path.
pub(super) fn make_pr_review_fixture(db: &WorkDb, pr_url: Option<&str>) -> (String, String) {
    let product = create_test_product_named(db, "TestProduct");
    let chore = create_test_chore_manual(db, product.id.clone(), "Test chore");
    if let Some(url) = pr_url {
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                pr_url: Some(url.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    }
    (product.id, chore.id)
}
