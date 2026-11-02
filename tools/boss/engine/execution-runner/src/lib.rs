//! Worker execution implementation, isolated from engine orchestration.

mod runner;
pub use pane_spawn::PaneSpawnRunner;
pub use pane_spawn::{install_boss_event_to_stable_bin, resolve_boss_event_binary};
pub use prompt::designated_output_kind;
pub use runner::{pane_spawn, work_item};
use runner::{prompt, worker_spawn};
pub use runner::{spawn_confirmation, spawn_launch_limits};
pub use work_item::{task_bound_pr_url, work_item_name, work_item_task_kind};
pub use worker_spawn::{ComposedWorkerSpawn, REVIEW_GUIDE_DRIVER, WorkerSpawnOpts, compose_worker_spawn};

pub use boss_execution_contracts::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};

pub mod answer_agent;
pub mod automation_triage;
pub mod ci_failure;
pub mod host;
mod operator_question;
pub mod prompt_fragments;
pub mod worker_setup;
use boss_engine_storage_values::{audit_text, breakdown, supersession_scan};
pub mod boss_construct_scan;
pub mod execution_bookmark_recovery;
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
use boss_ssh_transport as ssh_transport;
use boss_work_db as work;
#[cfg(test)]
use boss_work_db::test_support;
use boss_work_db::{deferred_scope, work_dependencies};

#[cfg(test)]
use boss_prose_vocabulary as prose_vocabulary;
