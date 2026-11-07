pub use boss_transcript_markdown as transcript_markdown;
pub use coordinator_tmux::inbox::{
    COORDINATOR_INBOUND_SETTINGS, DELIVERY_TIMEOUT as COORDINATOR_DELIVERY_TIMEOUT,
    deliver as deliver_coordinator_handoff_to_inbox, deliver_with_turn as deliver_coordinator_handoff_with_turn,
    relay_launch_command, resolve_claude_pid,
};

pub mod abandoned_branch_pr_sweep;
mod abandoned_execution_bookmarks;
pub mod agent_jsonl_discovery;
pub mod agent_jsonl_progress;
pub use boss_execution_runner::answer_agent;
pub mod answer_agent_completion_sweep;
pub mod attention_lifecycle;
pub mod attention_reconcile_sweep;
pub mod attentions_detector;
pub mod audit;
pub mod audit_effort;
mod execution_bookmark_prune;
pub(crate) mod execution_publisher;
pub mod pr_binding_repair_sweep;
pub use boss_engine_automation_schedule as automation_schedule;
pub mod automation_scheduler;
pub mod automation_triage;
pub mod awaiting_input_status;
pub mod background_children;
pub mod background_work;
pub mod board_invalidation_metrics;
#[cfg(test)]
mod board_invalidation_replay;
pub use boss_execution_runner::boss_construct_scan;
pub mod build_info;
pub use boss_ci_log_reader as ci_log_reader;
pub use boss_claude_client as claude_client;
pub mod cli;
pub use boss_comment_anchor as comments_anchor;
pub use boss_engine_comment_classifier as comment_classifier;
pub mod completion_adapters;
#[cfg(test)]
mod completion_integration_tests;
pub mod merge_poller_adapters;
#[cfg(test)]
mod merge_poller_integration_tests;
pub use boss_engine_completion as completion;
// The merge-polling service and the CI / merge-queue collaborators only its loop
// drives live in their own crate; each is re-exported under its historical path.
pub use boss_engine_completion::{
    answer_agent_observability, build_wait, build_wait_tracker, codex_unobserved_command, conflict_stop_gate,
    driver_teardown, driver_transcript, hold_registry, merge_parent_deletion, nudge_breaker, postmortem_followups,
    pr_url_capture, proposal_channel_error, reconcile_audit, review_guide_capture, run_done_backstop,
    teardown_registry, wait_registry, worker_escalation,
};
pub use boss_engine_merge_poller as merge_poller;
pub(crate) use boss_engine_merge_poller::remediation_push;
pub use boss_engine_merge_poller::{
    blocking_signal, ci_watch, merge_queue_renumber, trunk_merge, trunk_queue_adopt, trunk_queue_poller,
};
pub mod config;
#[cfg(test)]
mod conformance;
pub use boss_conflict_diagnosis as conflict_diagnosis;
pub mod conflict_ladder;
pub mod conflict_remediation;
pub mod conflict_watch;
pub mod coordinator_guidance;
pub mod coordinator_handoff;
pub mod coordinator_tmux;
pub mod cost_pricing;
pub mod cost_report;
pub use boss_host_execution::cube as cube_commands;
pub mod cube_client;
pub mod cube_lease_heartbeat;
pub use boss_host_execution::cube_op_budget;
pub mod database_backup;
pub mod dead_pane_sweep;
pub mod dead_pid_sweep;
pub use work::deferred_scope;
pub mod dep_unblock_sweep;
pub mod design_detector;
mod offsite_runtime;
pub use boss_deterministic_resolvers as deterministic_resolvers;
pub use boss_dispatch_events as dispatch_events;
// The dispatch coordinator lives in its own crate; alias it under the module
// paths the engine has always used it by.
pub use boss_engine_dispatch::{coordinator, dispatch_inflight, dispatch_metrics};
pub mod attachment_retention_sweep;
pub mod attachment_server;
pub mod codex_guard_trace;
pub mod codex_home_retention_sweep;
#[cfg(test)]
mod coordinator_tests;
pub mod dispatch_failure_recovery_sweep;
pub mod durable_liveness;
pub mod grok_home_retention_sweep;
pub mod grok_quota_refresh;
pub use boss_dispatch_reader as dispatch_reader;
pub use boss_engine_attachments as attachments;
pub use boss_engine_codex_rollout_retention as codex_rollout_retention;
pub use boss_engine_grok_home_retention as grok_home_retention;
pub mod dispatch_stall_escalation;
pub use boss_editorial as editorial;
pub use boss_engine_driver as driver;
pub mod editorial_hook;
pub use boss_engine_effort as effort;
pub mod engine_control;
pub mod envelope_watch;
pub use work::event_publish;
pub mod events_socket;
pub mod execution_liveness;
pub mod execution_retention_sweep;
pub mod external_tracker;
pub use boss_feature_flags as feature_flags;
pub mod execution_bookmark_recovery;
pub mod github_api_usage;
pub mod host_adapter;
pub mod host_capability_probe;
pub mod host_provisioning;
pub mod host_reconcile;
pub mod host_registry;
pub mod host_scheduling;
pub mod husk_pane_sweep;
pub mod interrupt_recovery;
pub mod ipc_log;
pub mod ladder_lease_heartbeat;
pub mod ladder_lease_reap;
pub mod ladder_lease_registry;
#[cfg(target_os = "macos")]
pub(crate) mod libproc;
pub mod live_status;
pub mod live_status_loop;
pub mod live_worker_metadata;
pub use boss_engine_live_worker_state as live_worker_state;
pub use boss_engine_live_worker_state::semantic_progress;
pub mod lost_workspace_sweep;
pub mod materializer;
pub mod merge_mechanism;
pub mod merge_when_ready;
pub mod run_cost;
// The metrics framework lives in its own crate; alias it under the
// module path the engine has always used it by. The engine-specific startup
// registration lives in `boss_engine_app::metrics_init`.
pub use boss_metrics as metrics;
// Re-exported so the declaration macros keep resolving as
// `crate::register_counter!` at every engine call site; the macros
// are `#[macro_export]`ed from `boss_engine_metrics_registry` and
// re-exported through `boss_metrics`.
pub use boss_metrics::{register_counter, register_gauge};
pub mod orphan_sweep;
pub mod planner;
pub mod pool_claim_sweep;
pub use boss_metrics::population_progress;
pub use boss_metrics::population_timing;
pub mod populator;
pub use boss_pr_review as pr_review;
pub use boss_pr_template as pr_template;
pub mod board_notification_bridge;
pub mod hung_worker;
pub mod pr_review_recovery;
pub mod pre_start_streak;
pub mod project_postmortem_sweep;
pub use boss_execution_runner::prompt_fragments;
pub mod proposal_expiry_sweep;
#[cfg(test)]
pub(crate) use boss_prose_vocabulary as prose_vocabulary;
pub mod protocol;
pub mod remote_input_lifecycle;
pub mod remote_lease_reconcile;
pub mod remote_reattach;
pub mod remote_transcript;
pub mod remote_wrapper;
pub mod repo_slug;
pub mod resolution_signal_capture;
pub mod review_guide_validation;
mod review_guide_workspace;
pub mod review_verdict_apply_sweep;
pub mod rotating_file;
pub mod run_done_finalize_sweep;
pub mod run_reconcile;
pub mod runner;
pub mod runtime_shutdown;
pub mod settings;
pub mod sleep_assertion;
pub mod spawn_ack_sweep;
pub mod spawn_flow;
pub mod spawn_health;
pub mod speculative_conflict;
pub mod ssh_spawn;
pub use boss_ssh_transport as ssh_transport;
pub mod local_worker_quarantine;
pub mod stacked_pr_structuring;
pub mod stale_worker_sweep;
pub mod startup_pane_reconcile;
pub mod startup_timing;
pub mod stdout_progress;
pub mod stranded_answering_sweep;
pub use boss_engine_structured_output as structured_output;
#[cfg(test)]
mod storage_orchestration_tests;
pub use boss_execution_runner::supersession_scan;
pub mod stuck_metrics;
pub(crate) mod sweep_loop;
pub mod sweep_metrics;
pub mod syspolicyd_monitor;
pub mod terminal_work_sweep;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
// The stub macros live with the contract traits they implement; re-exported so
// `crate::stub_cube_client!` keeps resolving in engine tests.
#[cfg(any(test, feature = "test-support"))]
pub(crate) use boss_execution_contracts::stub_cube_client;
#[cfg(test)]
pub(crate) use boss_execution_contracts::stub_host_adapter;
pub mod tmux_adoption;
pub(crate) mod tmux_adoption_live_state;
pub(crate) mod tmux_session_options;
pub use boss_transcript_tail as transcript_tail;
pub mod trace_rotation;
pub use boss_engine_transient_error as transient_error;
pub mod tmux_health;
pub mod tmux_preflight;
pub mod unpark_reconcile;
pub mod transcript_liveness;
pub mod transient_recovery;
pub use boss_engine_utility_model as utility_model;
pub use boss_work_db as work;
pub use work::work_dependencies;
pub mod worker_readoption;
pub mod worker_refresh_liveness;
pub mod worker_registry;
pub mod worker_sandbox_audit;
pub mod worker_setup;
pub mod worker_throttle;
pub mod workspace_lease_release;
pub mod wrapper_distribution;

pub mod lifecycle_adapter;
pub use lifecycle_adapter::CoreCoordinatorConstructors;
mod runner_adapter;
