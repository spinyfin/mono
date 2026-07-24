//! Execution runner.
//!
//! The pane-spawn [`ExecutionRunner`] and its supporting machinery, split out
//! of a single oversized `runner.rs` into functionally grouped sibling
//! modules:
//!
//! - [`pane_spawn`] — the [`PaneSpawnRunner`] `ExecutionRunner` impl plus the
//!   boss-event shim install/resolve helpers.
//! - [`worker_spawn`] — worker-spawn composition ([`ComposedWorkerSpawn`],
//!   [`compose_worker_spawn`], PR review/diff fetch).
//! - [`prompt`] — worker prompt composition (the directive/fragment family).
//! - [`work_item`] — [`WorkItem`] accessor helpers and PR-URL extraction.
//!
//! This module owns the shared run-outcome vocabulary and the
//! [`ExecutionRunner`] trait, and re-exports the items other engine modules
//! reach as `crate::runner::*`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::effort::SpawnConfig;
use crate::work::{WorkExecution, WorkItem};
use boss_protocol::ExecutionStatus;

mod pane_spawn;
mod prompt;
mod work_item;
mod worker_spawn;

pub use pane_spawn::PaneSpawnRunner;
pub(crate) use pane_spawn::{install_boss_event_to_stable_bin, resolve_boss_event_binary};
pub(crate) use prompt::bazel_prepush_gate_text;
pub(crate) use work_item::{task_bound_pr_url, work_item_name, work_item_task_kind};
pub(crate) use worker_spawn::{ComposedWorkerSpawn, compose_worker_spawn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAttention {
    pub kind: String,
    pub title: String,
    pub body_markdown: String,
}

/// What a worker is waiting for after a run ends. Drives the lease
/// retain/release decision in the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunWaitState {
    /// Run finished cleanly with no further work expected (`completed` or
    /// equivalent terminal status). Workspace is released.
    Terminal,
    /// Worker is blocked on an upstream dependency. Workspace is released
    /// and re-leased when the work becomes ready again.
    WaitingDependency,
    /// Worker is awaiting human input/redirect. Workspace is retained so
    /// the next run can continue in-place.
    WaitingHuman,
    /// Worker is awaiting human review of an open PR. Workspace retained.
    WaitingReview,
    /// Worker is awaiting merge of an approved PR. Workspace retained.
    WaitingMerge,
    /// The execution was cancelled (kanban drag to Backlog, force-stop)
    /// *during* its spawn window — after the worker pane came up but
    /// before the run could be recorded. The runner has already reaped
    /// the just-spawned pane; the coordinator must release the cube
    /// lease the cancel path deliberately left held and skip the normal
    /// completion recording (the row is already terminal). See
    /// [`PaneSpawnRunner::run_execution`] and the mid-spawn-cancel
    /// collision this closes.
    CancelledDuringSpawn,
    /// A `pr_review` reviewer pane was successfully spawned. The pane is
    /// alive and the reviewer agent is actively working. The execution
    /// stays in `running` (not `waiting_human`) until the Stop hook fires
    /// and `finalize_pr_review_pass` transitions it to `completed` via
    /// `record_worker_pr_completion`. Workspace is retained so the reviewer
    /// pane can continue.
    ///
    /// Using `running` (rather than `waiting_human`) is what keeps the
    /// "AI reviewing" badge visible on kanban cards for the duration of
    /// the review — the badge queries `pr_review` executions in `running`
    /// status. `waiting_human` is semantically wrong here: nobody is waiting
    /// for a human while the reviewer agent is working.
    ReviewerPaneAlive,
}

impl RunWaitState {
    pub fn execution_status(self) -> ExecutionStatus {
        match self {
            RunWaitState::Terminal => ExecutionStatus::Completed,
            RunWaitState::WaitingDependency => ExecutionStatus::WaitingDependency,
            RunWaitState::WaitingHuman => ExecutionStatus::WaitingHuman,
            RunWaitState::WaitingReview => ExecutionStatus::WaitingReview,
            RunWaitState::WaitingMerge => ExecutionStatus::WaitingMerge,
            // The row is already `cancelled`; the coordinator never
            // drives a status transition for this variant. Report the
            // terminal status for completeness.
            RunWaitState::CancelledDuringSpawn => ExecutionStatus::Cancelled,
            // Reviewer pane is alive; execution stays `running`.
            RunWaitState::ReviewerPaneAlive => ExecutionStatus::Running,
        }
    }

    pub fn release_workspace(self) -> bool {
        matches!(
            self,
            RunWaitState::Terminal | RunWaitState::WaitingDependency | RunWaitState::CancelledDuringSpawn
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub wait_state: RunWaitState,
    pub result_summary: Option<String>,
    pub attention: Option<RunAttention>,
    /// Pane slot the worker was actually allocated into, if this run
    /// hosts a libghostty pane. The coordinator stamps this onto the
    /// run record's `agent_id` (as `worker-{slot_id}`) so `bossctl
    /// agents list` shows one entry per active pane instead of
    /// collapsing every run into the worker-pool placeholder. `None`
    /// means the runner doesn't have a pane (e.g., a test fake);
    /// the coordinator leaves agent_id alone.
    pub slot_id: Option<u8>,
    /// Resolved per-execution effort + model knobs the runner used
    /// to construct the worker's `claude` invocation. The coordinator
    /// surfaces this on the `pane_spawned` dispatch event so
    /// `bossctl dispatch diagnose <exec-id>` shows what model and
    /// effort value the worker actually launched with — design §Q2:
    /// "surfaces the chosen model, effort value, and level on the
    /// dispatch instrumentation stream." `None` for fake runners that
    /// don't go through the spawn-config resolver.
    pub spawn_config: Option<SpawnConfig>,
}

#[async_trait]
pub trait ExecutionRunner: Send + Sync {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome>;
}

/// Absolute path of the events socket this engine bound — where worker hook
/// shims connect, and the target of each remote run's reverse `ssh -R`
/// forward. Read from the config `serve` stamped the bound path onto, so the
/// local `PaneSpawnRunner`, the remote `SshHostAdapterProvider`, and the
/// engine-restart reattacher all agree on the one socket that is actually
/// listening.
///
/// Callers must NOT re-derive this from `$BOSS_EVENTS_SOCKET`. That was the
/// second half of the 2026-07-23 outage: a test-fixture engine that correctly
/// isolated its own socket would still have handed its workers the production
/// path, because this resolver read the environment rather than the binding.
///
/// The fallback covers the one shape where nothing was bound —
/// `serve(..., events_socket_path: None, ...)`, used by in-process tests that
/// never spawn a real worker. It re-reads the environment when it fires,
/// which is exactly the read the rest of this module exists to avoid, so it
/// logs a warning: a live caller hitting this path is a regression in the
/// "nothing but config load re-reads `$BOSS_EVENTS_SOCKET`" invariant, and
/// should be visible in the log rather than only in a worker's `settings.json`.
pub fn bound_events_socket_path(cfg: &RuntimeConfig) -> PathBuf {
    match cfg.work.events_socket_path.clone() {
        Some(path) => path,
        None => {
            tracing::warn!(
                "bound_events_socket_path: no events socket bound on the config; \
                 falling back to re-reading $BOSS_EVENTS_SOCKET / the production default. \
                 Every real engine start should bind one via `serve`; seeing this outside \
                 an in-process test that never spawns a worker is a regression.",
            );
            engine_events_socket_path()
        }
    }
}

/// Resolve the events socket from the environment: `BOSS_EVENTS_SOCKET` if
/// set, otherwise the production `~/Library/Application Support/Boss`
/// location.
///
/// This is the *seed* for [`crate::config::WorkConfig::events_socket_path`]
/// and the last-resort fallback in [`bound_events_socket_path`]. Everything
/// that needs to know where the engine is listening goes through the config;
/// see that field's doc comment.
pub fn engine_events_socket_path() -> PathBuf {
    if let Ok(override_path) = std::env::var(crate::config::EVENTS_SOCKET_ENV) {
        return override_path.into();
    }
    boss_log_files::default_events_socket_path().unwrap_or_default()
}
