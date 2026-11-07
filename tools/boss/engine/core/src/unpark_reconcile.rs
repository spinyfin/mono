//! Un-park reconciliation: what happens to an item when the owner un-parks it.
//!
//! A parked row atrophies on purpose — no CI fixing, conflict resolution, AI
//! review, re-dispatch or any other upkeep runs for it — so by the time the
//! owner brings it back its PR may have merged, closed, conflicted or gone red,
//! its dispatch stamps may describe a world that no longer exists, and its
//! execution rows were cancelled when it was parked. Resuming normal work
//! without catching up first would act on stale state. Section 1.5 of
//! `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! specifies a fixed sequence that catches the item up; this module is that
//! sequence. Each step reuses the entry point the engine already has for the
//! same job, so a reconciled item behaves exactly as one that had never been
//! parked and had just been seen by the engine's sweeps.
//!
//! # The steps
//!
//! Steps 1 and 2 are database writes and run inside the un-park RPC
//! ([`boss_work_db::parking`]); [`record_released_rows`] writes their audit
//! trail and events. Steps 3 to 7 run as a background job
//! ([`UnparkReconciler`]) because they call GitHub and the dispatcher, one
//! item at a time through [`reconcile_unparked_item`]:
//!
//! | # | Step (`details.step`) | Reuses |
//! |---|---|---|
//! | 1 | `clear_park` | the un-park write and its ledger row |
//! | 2 | `clear_halt_stamps` | stale `dispatch_failed_*` cleared; operator-question and worker-failed holds kept |
//! | 3 | `refresh_pr` | the merge poller's targeted reconcile (`boss_engine_merge_poller::reconcile_one`): one live probe, and merged / closed PRs take their ordinary path to `done` |
//! | 4 | `conflict_probe`, `ci_probe`, `ai_review` | the same reconcile's conflict and CI paths, with the CI attempt budget reset first; the AI review admission that `bossctl review start` uses |
//! | 5 | `reconcile_execution` | [`WorkDb::reconcile_item_execution`] for `todo` / `blocked`; the orphan sweep's re-dispatch for `active`, with the churn guard counting only from the un-park |
//! | 6 | `revisions` | the same reconcile for the item's open revisions, so they re-enter per-PR chain serialization |
//! | 7 | `progression_grace` | stamps `tasks.progression_grace_waived_at` |
//!
//! Steps 3 and 4 share one GitHub probe, because the poller's targeted
//! reconcile does the refresh and both detections off a single batched query;
//! running them as separate probes would double the GitHub cost for nothing.
//! The CI budget reset (part of step 4) therefore runs just before that call,
//! so the CI detection sees a fresh budget. A `blocked: ci_failure_exhausted`
//! item is moved back to `in_review` first, otherwise a fresh budget could not
//! be spent: the exhausted state is exactly the stale evidence being cleared.
//!
//! # Guarantees
//!
//! - **Every step leaves a trace.** A `[parking]` line is appended to the
//!   item's description (`reconcile_audit::append_parking_audit`) and an `unpark_reconcile`
//!   dispatch event is emitted, with the step name and outcome
//!   (`ok` / `skipped` / `error`).
//! - **A failing step does not stop the sequence.** The error is recorded, the
//!   remaining steps run, and the item stays un-parked. Step 7 is what catches
//!   whatever the failed step would have fixed.
//! - **It is idempotent.** Re-running it on an item that is already un-parked
//!   performs steps 3 to 7 only (steps 1 and 2 have nothing to clear), and each
//!   probe is a no-op when its signal is clear.
//! - **Holds are not overridden.** `blocked: awaiting_operator_answer` and
//!   `blocked: worker_failed` stay as the evidence a human or a progression
//!   agent resolves; step 5 never redispatches them.
//! - **It never fights live work.** Step 5 for an `active` item goes through the
//!   orphan sweep, whose liveness guards refuse to put a second worker on a row
//!   whose previous worker still runs.
//!
//! # Bandwidth and what happens when a limit fires
//!
//! A project un-park runs one child at a time, and only one un-park job runs at
//! a time across the engine. Each child that touches GitHub waits for
//! [`UnparkLimits::child_spacing`] since the previous one, and the job stops
//! spending when the GraphQL quota the engine last saw drops below
//! [`UnparkLimits::quota_reserve`]: it waits up to
//! [`UnparkLimits::quota_max_wait`] for the quota to recover, once per job. If it
//! has not, the remaining children's GitHub steps (3 and 4) are **deferred** —
//! recorded as skipped with the reason, in the audit trail and as `rate_limit`
//! events — while steps 5 to 7 still run. Nothing is dropped: a deferred
//! item is an ordinary un-parked `in_review` row, which the merge poller's own
//! sweep refreshes on its normal cadence, and step 7 hands the whole item to
//! progression. See [`UnparkLimits`] for how the values were chosen and what
//! they were not.
//!
//! The job is not durable: an engine restart mid-job loses the children not yet
//! reached. Everything a step does is also done, later, by the engine's
//! ordinary sweeps, so the cost of a lost job is latency, not correctness.

mod job;
mod sequence;
#[cfg(test)]
mod tests;

pub use job::{JobSummary, UnparkJob, UnparkLimits, UnparkReconciler};
pub use sequence::{
    GhBudget, GithubAccess, ItemOutcome, RegistryGhBudget, Step, StepKind, StepRecord, UnparkEnv,
    reconcile_unparked_item, record_released_rows,
};
