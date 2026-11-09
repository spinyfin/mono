//! The background job that runs the un-park reconciliation's GitHub-touching
//! steps, one item at a time, with pacing against the GitHub quota and progress
//! events. Module documentation in [`super`] has the sequence and what happens
//! when a limit fires.

use std::sync::Arc;
use std::time::Duration;

use boss_work_db::parking_reconcile::UnparkReport;
use tokio::time::Instant;

use super::sequence::{
    GithubAccess, ItemOutcome, StepKind, UnparkEnv, emit, polled_for_pr_refresh, reconcile_unparked_item,
    record_released_rows,
};
use crate::work::WorkItem;

/// How long consecutive GitHub-touching items are spaced apart.
const DEFAULT_CHILD_SPACING: Duration = Duration::from_secs(1);
/// GraphQL points the job leaves in the hourly window for the engine's other
/// callers.
const DEFAULT_QUOTA_RESERVE: i64 = 2_000;
/// How often the job re-reads the quota while waiting for it to recover.
const DEFAULT_QUOTA_POLL: Duration = Duration::from_secs(30);
/// The longest the job waits for the quota to recover, once per job.
const DEFAULT_QUOTA_MAX_WAIT: Duration = Duration::from_secs(5 * 60);
/// A `progress` event is emitted after every this many items.
const DEFAULT_PROGRESS_EVERY: usize = 10;

/// The job's pacing limits.
///
/// **How these were chosen, and what they were not.** They come from arithmetic
/// on known per-call costs, not from measured traffic. A child that touches GitHub costs at
/// most three GraphQL points: the targeted reconcile's one batched probe and its
/// one merge-queue dequeue query are each billed `max(1, nodes / 100)`, which is
/// 1 for a single PR, and the optional review-metadata fetch is one more. Even
/// a 500-child project is therefore about 1,500 points of the 5,000 an hourly
/// window allows. The merge poller starts stretching its own cadence when fewer
/// than 1,500 remain (`RATE_LIMIT_LOW_WATER`); the reserve here sits 500 above
/// that, so this job backs off before the poller must. The spacing keeps a
/// project of that size under a thousand probes an hour regardless of quota.
/// Replace these estimates with measured values: the child count of the largest
/// real project and the GraphQL points one un-park actually spends (attributed to
/// the `unpark_reconcile` GitHub caller).
///
/// **When a limit fires.** Spacing only delays. The quota reserve makes the job
/// wait up to `quota_max_wait` — once per job — for the window to recover, then
/// defers the remaining items' GitHub steps (3 and 4) rather than spend into the
/// reserve; they are recorded as skipped, with the reason, and the merge poller's
/// ordinary sweep refreshes those PRs on its own cadence. A later item is let
/// through again as soon as the quota recovers.
#[derive(Debug, Clone, bon::Builder)]
pub struct UnparkLimits {
    #[builder(default = DEFAULT_CHILD_SPACING)]
    pub child_spacing: Duration,
    #[builder(default = DEFAULT_QUOTA_RESERVE)]
    pub quota_reserve: i64,
    #[builder(default = DEFAULT_QUOTA_POLL)]
    pub quota_poll: Duration,
    #[builder(default = DEFAULT_QUOTA_MAX_WAIT)]
    pub quota_max_wait: Duration,
    #[builder(default = DEFAULT_PROGRESS_EVERY)]
    pub progress_every: usize,
}

impl Default for UnparkLimits {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// One un-park's worth of background work: the items to reconcile, in order,
/// and the project or item the job was started for (the key of its job-level
/// events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnparkJob {
    pub key: String,
    pub project_id: Option<String>,
    pub items: Vec<String>,
}

impl UnparkJob {
    /// The job for an un-park of `target_id` that released `report`.
    ///
    /// Each released non-revision row is an item. A released revision is its own
    /// item only when its chain root was not also released (its parent is
    /// reconciled with it otherwise, in step 6). An un-park that released nothing
    /// is a re-run on an item that was not parked: the item itself, steps 3 to 7.
    pub fn for_report(target_id: &str, report: &UnparkReport) -> Self {
        let released: std::collections::HashSet<&str> = report.rows.iter().map(|row| row.id.as_str()).collect();
        let mut items: Vec<String> = report
            .rows
            .iter()
            .filter(|row| match &row.parent_task_id {
                None => true,
                Some(parent) => !released.contains(parent.as_str()),
            })
            .map(|row| row.id.clone())
            .collect();
        if items.is_empty() && report.project_id.is_none() && target_id.starts_with("task_") {
            items.push(target_id.to_owned());
        }
        Self {
            key: target_id.to_owned(),
            project_id: report.project_id.clone(),
            items,
        }
    }
}

/// What a job did, for its final progress event and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobSummary {
    pub total: usize,
    pub completed: usize,
    /// Items whose GitHub steps were deferred because the quota was low.
    pub deferred_for_quota: usize,
    /// Items on which at least one step failed.
    pub with_errors: usize,
    /// Per-item outcomes, in order.
    pub outcomes: Vec<ItemOutcome>,
}

/// Runs un-park jobs. One instance is shared by the whole engine so that two
/// un-parks never run their GitHub steps at the same time.
pub struct UnparkReconciler {
    limits: UnparkLimits,
    one_job_at_a_time: tokio::sync::Mutex<()>,
}

impl UnparkReconciler {
    pub fn new(limits: UnparkLimits) -> Self {
        Self {
            limits,
            one_job_at_a_time: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(super) async fn hold_queue(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.one_job_at_a_time.lock().await
    }

    pub fn limits(&self) -> &UnparkLimits {
        &self.limits
    }

    /// The un-park RPC's hook: leave the trace of the steps already done in the
    /// database (1 and 2) and hand the rest to a background job. Returns once the
    /// trace is written, with the job's handle; the RPC does not wait for it.
    pub async fn begin(
        self: &Arc<Self>,
        env: &UnparkEnv,
        actor: &str,
        target_id: &str,
        report: UnparkReport,
    ) -> tokio::task::JoinHandle<JobSummary> {
        record_released_rows(env, actor, &report).await;
        let job = UnparkJob::for_report(target_id, &report);
        let reconciler = Arc::clone(self);
        let env = env.clone();
        boss_task_scope::spawn(async move { reconciler.run(&env, job).await })
    }

    /// Run `job` to completion (after any job already running), one item at a
    /// time. Never fails: every problem is recorded on the item it concerns.
    pub async fn run(&self, env: &UnparkEnv, job: UnparkJob) -> JobSummary {
        // The spawned job does not inherit its spawner's task-locals, so the
        // attribution is set where the job's own GitHub calls begin.
        boss_gh_telemetry::scope(boss_gh_telemetry::callers::UNPARK_RECONCILE, self.run_job(env, job)).await
    }

    async fn run_job(&self, env: &UnparkEnv, job: UnparkJob) -> JobSummary {
        let _turn = self.one_job_at_a_time.lock().await;
        let total = job.items.len();
        let mut summary = JobSummary {
            total,
            ..JobSummary::default()
        };
        self.progress(env, &job, &summary, "started").await;
        let mut pacer = Pacer::default();
        for (index, item_id) in job.items.iter().enumerate() {
            let github = if touches_github(env, item_id) {
                pacer.admit(env, &self.limits, &job.key, item_id).await
            } else {
                GithubAccess::Allowed
            };
            if matches!(github, GithubAccess::Deferred { .. }) {
                summary.deferred_for_quota += 1;
            }
            let outcome = reconcile_unparked_item(env, item_id, github).await;
            if outcome.has_error() {
                summary.with_errors += 1;
            }
            summary.completed = index + 1;
            summary.outcomes.push(outcome);
            let every = self.limits.progress_every.max(1);
            if summary.completed.is_multiple_of(every) && summary.completed < total {
                self.progress(env, &job, &summary, "running").await;
            }
        }
        self.progress(env, &job, &summary, "finished").await;
        summary
    }

    async fn progress(&self, env: &UnparkEnv, job: &UnparkJob, summary: &JobSummary, phase: &str) {
        let kind = if summary.with_errors > 0 && phase == "finished" {
            StepKind::Error
        } else {
            StepKind::Ok
        };
        emit(
            env,
            &job.key,
            kind,
            serde_json::json!({
                "step": "progress",
                "outcome": phase,
                "project_id": job.project_id,
                "total": summary.total,
                "completed": summary.completed,
                "deferred_for_quota": summary.deferred_for_quota,
                "with_errors": summary.with_errors,
            }),
        )
        .await;
    }
}

/// Whether reconciling `item_id` will reach for GitHub: a non-revision item
/// with a bound PR that the merge poller lists for a refresh.
fn touches_github(env: &UnparkEnv, item_id: &str) -> bool {
    match env.work_db.get_work_item(item_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => polled_for_pr_refresh(env, &task),
        _ => false,
    }
}

/// Spacing and quota gating between GitHub-touching items.
#[derive(Default)]
struct Pacer {
    last_github: Option<Instant>,
    /// Set once a wait for the quota to recover has started in this job, so
    /// later items are deferred without waiting again.
    quota_wait_spent: bool,
}

impl Pacer {
    async fn admit(&mut self, env: &UnparkEnv, limits: &UnparkLimits, key: &str, item_id: &str) -> GithubAccess {
        if let Some(last) = self.last_github {
            let since = last.elapsed();
            if since < limits.child_spacing {
                tokio::time::sleep(limits.child_spacing - since).await;
            }
        }
        let access = self.check_quota(env, limits, key, item_id).await;
        if access == GithubAccess::Allowed {
            self.last_github = Some(Instant::now());
        }
        access
    }

    async fn check_quota(&mut self, env: &UnparkEnv, limits: &UnparkLimits, key: &str, item_id: &str) -> GithubAccess {
        let reserve = limits.quota_reserve;
        let low = |remaining: Option<i64>| remaining.filter(|remaining| *remaining < reserve);
        let Some(remaining) = low(env.gh_budget.graphql_remaining()) else {
            return GithubAccess::Allowed;
        };
        if !self.quota_wait_spent {
            self.quota_wait_spent = true;
            emit(
                env,
                key,
                StepKind::Skipped,
                serde_json::json!({
                    "step": "rate_limit",
                    "outcome": "waiting",
                    "work_item_id": item_id,
                    "remaining": remaining,
                    "reserve": reserve,
                    "max_wait_secs": limits.quota_max_wait.as_secs(),
                }),
            )
            .await;
            let deadline = Instant::now() + limits.quota_max_wait;
            while low(env.gh_budget.graphql_remaining()).is_some() && Instant::now() < deadline {
                tokio::time::sleep(
                    limits
                        .quota_poll
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
        }
        match low(env.gh_budget.graphql_remaining()) {
            None => GithubAccess::Allowed,
            Some(remaining) => {
                self.quota_wait_spent = true;
                emit(
                    env,
                    key,
                    StepKind::Skipped,
                    serde_json::json!({
                        "step": "rate_limit",
                        "outcome": "deferred",
                        "work_item_id": item_id,
                        "remaining": remaining,
                        "reserve": reserve,
                    }),
                )
                .await;
                GithubAccess::Deferred { remaining, reserve }
            }
        }
    }
}
