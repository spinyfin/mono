//! Periodic PR-lifecycle detection.
//!
//! The on-Stop completion path in the completion service handles the
//! create-and-merge case during a run, but most merges happen *after*
//! the worker has exited and released its lease — so no Stop event
//! ever arrives to drive the `in_review → done` transition. Without
//! this module, every chore or project_task that lands its PR after
//! the worker finished would sit in the kanban "Review" column
//! forever waiting for a manual `boss chore update --status done`.
//!
//! The poller also handles the second-most-common in_review fate: the
//! PR develops a merge conflict against its base while waiting for
//! review. The merge-conflict design (`tools/boss/docs/designs/
//! merge-conflict-handling-in-review.md`, Q1) extends `gh pr view`'s
//! projection with `mergeable` / `mergeStateStatus` / `baseRefOid` and
//! flips conflicting parents to `blocked: merge_conflict` so a
//! resolution worker can take over. The same sweep clears that flag
//! when the PR is mergeable again.
//!
//! The poller iterates candidate lists per sweep:
//!   - [`WorkDb::list_chores_pending_merge_check`] — `in_review` rows
//!     to watch for a clean merge or a fresh conflict.
//!   - [`WorkDb::list_chores_blocked_on_merge_conflict`] — rows the
//!     engine previously flagged as conflicting, to watch for the
//!     resolution signal.
//!   - [`WorkDb::list_stranded_conflict_resolution_attempts`] — rows
//!     whose `conflict_resolutions` attempt is `pending` but has no live
//!     execution. The sweep re-emits a fresh execution request so a
//!     worker can be dispatched (covers engine-restart and worker-die
//!     gaps without a full PR probe).
//!
//! Errors are logged but never propagate — a temporary network blip
//! must not crash the engine.
//!
//! `gh pr view` accepts a full PR URL and resolves the repo from the
//! URL itself, so the poller works fine inside the engine's process
//! (no workspace context needed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::sync::Notify;

use boss_event_bus::Event;
pub use boss_github::{CiProvider, RequiredCheckFailure};
use boss_github::{fetch_failing_checks_for_commit, parse_provider_job_id, provider_for_url};
pub(crate) use boss_observation_contracts::{
    CompletionRequests, MergeQueueTimeline, PrPollStateWriter, ReviewBatchEnqueuer, StopOutcome,
    file_admission_deferred_attention,
};

use crate::blocking_signal::SignalKind;
use crate::metrics::Registry;
#[cfg(test)]
use crate::work::TaskStatus;
use crate::work::{
    CiRemediation, GhPrStateChecker, LatePrCandidate, PendingMergeCheck, PrPollStateInput, PrStateChecker, WorkDb,
    WorkItem,
};
use boss_engine_gh_invocation::gh_output;
use boss_engine_utils::iso8601::parse_iso8601_lenient;
use boss_execution_contracts::{CubeClient, ExecutionPublisher};
// GitHub-call attribution. Every path in this module that reaches GitHub
// runs inside a `gh_scope`, because "the merge poller" is not a fine
// enough answer: the batched full sweep and the un-batched per-PR
// adaptive reconcile spend very differently, and the instrumentation has
// to tell them apart or it will confirm the wrong culprit.
use boss_gh_telemetry::{callers, scope as gh_scope};
use boss_github::gh_runner::pr_merge_queue_entry;
use boss_github::pr_url::{parse_pr_url_parts, repo_from_pr_url};
use boss_protocol::{self, CreateAttentionItemInput, TaskKind};

// Lower-crate vocabulary under the short module aliases the poller's sources use.
pub(crate) use boss_ci_log_reader as ci_log_reader;
pub(crate) use boss_engine_storage_values::merge_mechanism;
pub(crate) use boss_metrics as metrics;
pub(crate) use boss_metrics::{register_counter, register_gauge};
pub(crate) use boss_observation_contracts::conflict_stop_gate;
pub(crate) use boss_work_db as work;

// The stub macros live with the contract traits they implement; re-exported so
// `crate::stub_cube_client!` resolves in this crate's tests and fixtures.
#[cfg(any(test, feature = "test-support"))]
pub(crate) use boss_execution_contracts::stub_cube_client;

pub use host::{
    ConflictCleared, ConflictHandling, ConflictResponse, LoopPasses, PopulationRequest, PopulationRequests, SweepHost,
    SweepPasses,
};

mod classify;
mod deferred_review_admission;
mod merge_queue;
mod parked;
mod poll_metrics;
mod post_merge_review;
mod probe;
mod schedule;
mod sweep;

pub mod blocking_signal;
pub mod ci_watch;
pub mod host;
pub mod merge_queue_renumber;
pub mod queue_ci_contract;
pub mod remediation_push;
pub mod trunk_merge;
pub mod trunk_queue_adopt;
pub mod trunk_queue_poller;

pub use boss_observation_contracts::MergeQueueDequeueEvent;
pub use classify::*;
pub use merge_queue::QUEUED_NO_POSITION_SECTION_ORDER;
pub(crate) use merge_queue::*;
pub use parked::candidate_is_parked;
pub use poll_metrics::*;
#[cfg(test)]
pub(crate) use post_merge_review::maybe_trigger_post_merge_review;
pub use probe::*;
pub use schedule::*;
pub use sweep::*;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
// The shared fixtures are public under `test-support` so the engine's
// integration suite can drive the same harness; the test cases themselves are
// compiled only under `cfg(test)`.
#[cfg(any(test, feature = "test-support"))]
pub mod tests;

/// GitHub timeline transport shared with completion at composition time.
pub struct MergePollerQueueTimeline;

#[async_trait]
impl MergeQueueTimeline for MergePollerQueueTimeline {
    async fn fetch_dequeues(&self, pr_url: &str) -> Result<Vec<MergeQueueDequeueEvent>> {
        let mut events = fetch_merge_queue_dequeue_events_batch(&[pr_url.to_owned()]).await;
        events.remove(pr_url).context("queue timeline unavailable")
    }
}
