//! Worker execution implementation, isolated from engine orchestration.
//! Execution runner and prompt composition.

#[path = "runner/answer_agent_prompt.rs"]
mod answer_agent_prompt;
#[path = "runner/pane_spawn.rs"]
pub mod pane_spawn;
#[path = "runner/prompt.rs"]
mod prompt;
#[path = "runner/review_brief.rs"]
mod review_brief;
#[path = "runner/review_guide_prompt.rs"]
mod review_guide_prompt;
#[path = "runner/spawn_confirmation.rs"]
pub mod spawn_confirmation;
#[path = "runner/spawn_launch_limits.rs"]
pub mod spawn_launch_limits;
#[path = "runner/work_item.rs"]
pub mod work_item;
#[path = "runner/worker_spawn.rs"]
mod worker_spawn;

pub use pane_spawn::PaneSpawnRunner;
pub use pane_spawn::{install_boss_event_to_stable_bin, resolve_boss_event_binary};
pub use prompt::designated_output_kind;
pub use work_item::{task_bound_pr_url, work_item_name, work_item_task_kind};
pub use worker_spawn::{ComposedWorkerSpawn, REVIEW_GUIDE_DRIVER, WorkerSpawnOpts, compose_worker_spawn};

pub use boss_execution_contracts::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};

pub mod automation_triage;
pub mod host;
pub mod worker_setup;
pub use answer_agent_prompt::rules as answer_agent;
pub mod prompt_fragments;
pub use prompt_fragments::audit_text;
pub use prompt_fragments::breakdown;
pub use prompt_fragments::execution_bookmark_recovery;
pub mod boss_construct_scan;
pub use boss_construct_scan::supersession_scan;
pub mod pane_summary;
use boss_ci_log_reader as ci_log_reader;
use boss_claude_client as claude_client;
use boss_conflict_diagnosis as conflict_diagnosis;
use boss_engine_driver as driver;
use boss_engine_effort as effort;
use boss_engine_storage_values::attention_kinds as attention_lifecycle;
use boss_engine_storage_values::ci_contract as queue_ci_contract;
use boss_engine_structured_output as structured_output;
use boss_engine_utility_model as utility_model;
use boss_feature_flags as feature_flags;
use boss_pr_review as pr_review;
use boss_pr_template as pr_template;
use boss_ssh_transport as ssh_transport;
use boss_work_db as work;
#[cfg(test)]
use boss_work_db::test_support;
use boss_work_db::{deferred_scope, work_dependencies};

#[cfg(test)]
use boss_prose_vocabulary as prose_vocabulary;
