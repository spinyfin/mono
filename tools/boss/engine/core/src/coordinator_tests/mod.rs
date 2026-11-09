//! Integration tests for the dispatch coordinator (`boss_engine_dispatch`)
//! driven against the engine's real lifecycle services, grouped by the concern
//! each module exercises. Shared fixtures live in [`helpers`].

mod answer_agent_positioning;
mod automation;
mod blocked_workspace;
mod claimed_dispatch;
mod dispatch;
mod engine_rebase_hold;
mod execution_bookmarks;
mod execution_contracts;
mod focus_mode;
mod helpers;
mod lease_pending;
mod lease_retry;
mod live_writer_deferral;
mod local_worker_quarantine;
mod pause_admission;
mod pause_all_spawns;
mod pause_bypass;
mod pool;
mod pool_contract;
mod post_merge_review_dispatch;
mod pre_start_streak;
mod preserved_work;
mod progression_pool;
mod recovery;
mod recovery_fresh_start;
mod recovery_restage;
mod review_batch_dispatch;
mod review_pause;
mod revision_gating;
mod spawn_failures;
mod stacked_restore_retry;
