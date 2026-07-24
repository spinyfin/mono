//! Periodic PR-lifecycle detection.
//!
//! The on-Stop completion path in [`crate::completion`] handles the
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
use serde_json;
use tokio::sync::Notify;

pub use boss_github::{CiProvider, RequiredCheckFailure};
use boss_github::{fetch_failing_checks_for_commit, parse_provider_job_id, provider_for_url};

use crate::blocking_signal::SignalKind;
use crate::ci_watch;
use crate::completion::{StopOutcome, WorkerCompletionHandler};
use crate::conflict_watch;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::design_detector;
use crate::metrics::Registry;
#[cfg(test)]
use crate::work::TaskStatus;
use crate::work::{GhPrStateChecker, LatePrCandidate, PendingMergeCheck, PrPollStateInput, PrStateChecker, WorkDb};
use boss_engine_gh_invocation::gh_output;
use boss_engine_utils::iso8601::parse_iso8601_lenient;
use boss_github::gh_runner::pr_merge_queue_entry;
use boss_github::pr_url::{parse_pr_url_parts, pr_number_from_url, repo_from_pr_url};
#[cfg(test)]
use boss_protocol::ExecutionKind;
use boss_protocol::{self, CreateAttentionItemInput, TaskKind};

mod classify;
mod merge_queue;
mod metrics;
mod probe;
mod schedule;
mod sweep;

pub use classify::*;
pub use merge_queue::*;
pub use metrics::*;
pub use probe::*;
pub use schedule::*;
pub use sweep::*;

#[cfg(test)]
mod tests;
