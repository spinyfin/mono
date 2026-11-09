//! Worker execution implementation, isolated from engine orchestration.

mod runner;
pub use boss_execution_contracts::REVIEW_GUIDE_DRIVER;
pub use pane_spawn::PaneSpawnRunner;
pub use pane_spawn::{install_boss_event_to_stable_bin, resolve_boss_event_binary};
pub use prompt::designated_output_kind;
pub use runner::{pane_spawn, work_item};
use runner::{prompt, worker_spawn};
pub use runner::{spawn_confirmation, spawn_launch_limits};
pub use work_item::{task_bound_pr_url, work_item_name, work_item_task_kind};
pub use worker_spawn::{ComposedWorkerSpawn, WorkerSpawnOpts, compose_worker_spawn};

/// Thin wrappers over crate-internal helpers so engine-level tests can drive
/// them without widening the runner's production API.
#[cfg(any(test, feature = "test-support"))]
pub mod test_exports {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub fn assemble_worker_command(priority: &str, worker_bin_dir: &Path, plan: &crate::driver::SpawnPlan) -> String {
        crate::pane_spawn::assemble_worker_command(priority, worker_bin_dir, plan)
    }

    pub fn ensure_worker_bin_dir_for_engine(
        settings_dir: &Path,
        workspace_path: &Path,
        engine_path: &Path,
    ) -> Option<PathBuf> {
        crate::pane_spawn::ensure_worker_bin_dir_for_engine(settings_dir, workspace_path, engine_path)
    }

    pub async fn capture_shows_driver_ready(
        tmux: &boss_tmux::Tmux,
        session: &str,
        spec: Option<&boss_protocol::PaneMonitorSpec>,
        snapshot: &std::sync::Mutex<String>,
    ) -> bool {
        crate::pane_spawn::capture_shows_driver_ready(tmux, session, spec, snapshot).await
    }

    pub fn followup_pr_backlink(
        task_kind: &boss_protocol::TaskKind,
        created_via: &str,
        origin_pr_number: Option<i64>,
        repo_remote_url: &str,
    ) -> anyhow::Result<Option<String>> {
        crate::work_item::followup_pr_backlink(task_kind, created_via, origin_pr_number, repo_remote_url)
    }

    pub async fn confirm_spawn_started<Ready, ReadyFut, Started, StartedFut>(
        driver_name: &str,
        run_id: &str,
        composer_timeout: Duration,
        turn_timeout: Duration,
        poll: Duration,
        composer_ready: Ready,
        turn_started: Started,
    ) -> anyhow::Result<()>
    where
        Ready: FnMut() -> ReadyFut,
        ReadyFut: std::future::Future<Output = bool>,
        Started: FnMut() -> StartedFut,
        StartedFut: std::future::Future<Output = bool>,
    {
        crate::spawn_confirmation::confirm_spawn_started(
            driver_name,
            run_id,
            composer_timeout,
            turn_timeout,
            poll,
            composer_ready,
            turn_started,
        )
        .await
    }
}

pub use boss_execution_contracts::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};

pub mod answer_agent;
pub mod automation_triage;
pub mod ci_failure;
pub mod host;
mod operator_question;
pub mod prompt_fragments;
pub mod worker_setup;
use boss_engine_planner_validation::breakdown;
use boss_engine_storage_values::audit_text;
pub mod boss_construct_scan;
pub mod execution_bookmark_recovery;
pub mod supersession_scan;
use boss_ci_log_reader as ci_log_reader;
use boss_conflict_diagnosis as conflict_diagnosis;
use boss_engine_driver as driver;
use boss_engine_effort as effort;
use boss_engine_storage_values::attention_kinds as attention_lifecycle;
use boss_engine_storage_values::ci_contract as queue_ci_contract;
use boss_engine_structured_output as structured_output;
use boss_engine_utility_model as utility_model;
use boss_feature_flags as feature_flags;
use boss_pane_summary as pane_summary;
use boss_pr_review as pr_review;
use boss_pr_template as pr_template;
use boss_work_db as work;
#[cfg(test)]
use boss_work_db::test_support;
use boss_work_db::{deferred_scope, work_dependencies};

#[cfg(test)]
use boss_prose_vocabulary as prose_vocabulary;
