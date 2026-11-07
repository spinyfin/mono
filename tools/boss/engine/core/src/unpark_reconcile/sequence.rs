//! The reconciliation steps themselves: the environment they run against, the
//! record each leaves, and [`reconcile_unparked_item`], which runs steps 3 to 7
//! for one item. The module documentation in [`super`] explains the sequence.

use std::sync::Arc;

use boss_engine_merge_poller::{MergeProbe, SweepHost, SweepOutcome, reconcile_one};
use boss_execution_contracts::{CubeClient, ExecutionPublisher};
use boss_observation_contracts::CompletionRequests;
use boss_protocol::{TaskKind, TaskStatus};
use boss_work_db::parking_reconcile::{HaltStamp, UnparkReport, UnparkedRow};

use crate::completion::reconcile_audit::append_parking_audit;
use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::metrics::Registry;
use crate::orphan_sweep::run_one_pass_for_item_with_min_age;
use crate::work::{PrOpenState, StaticPrStateChecker, Task, WorkDb, WorkItem};
use crate::worker_readoption::LiveWorkerConvergence;

/// `updated_at` age the orphan sweep requires before it will treat an `active`
/// item with no live execution as an orphan. The race that guard exists for — a
/// fresh `todo → active` flip whose worker is still spinning up — cannot apply
/// to an item the owner un-parked, which was already `active`; and the
/// reconciliation's own audit lines stamp `updated_at` in the current second,
/// which a zero age would still read as too fresh. `-1` makes the cutoff one
/// second in the future, so the item is a candidate immediately.
const UNPARK_ORPHAN_MIN_AGE_SECS: i64 = -1;

/// One step of the sequence, by the stable name that appears in the audit
/// trail and in the `details.step` of an `unpark_reconcile` dispatch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    ClearPark,
    ClearHaltStamps,
    RefreshPr,
    ConflictProbe,
    CiProbe,
    AiReview,
    ReconcileExecution,
    Revisions,
    ProgressionGrace,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::ClearPark => "clear_park",
            Step::ClearHaltStamps => "clear_halt_stamps",
            Step::RefreshPr => "refresh_pr",
            Step::ConflictProbe => "conflict_probe",
            Step::CiProbe => "ci_probe",
            Step::AiReview => "ai_review",
            Step::ReconcileExecution => "reconcile_execution",
            Step::Revisions => "revisions",
            Step::ProgressionGrace => "progression_grace",
        }
    }
}

/// How a step ended. Maps onto the dispatch event's `ok` / `skipped` / `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// The step acted, or looked and found nothing to do.
    Ok,
    /// The step's precondition did not hold, or it was deferred.
    Skipped,
    /// The step failed. The sequence continued.
    Error,
}

/// What one step did to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    pub step: Step,
    pub kind: StepKind,
    /// A stable machine-readable outcome word (`merged`, `conflict_routed`, …).
    pub code: &'static str,
    /// The human-readable sentence the audit line carries.
    pub detail: String,
}

impl StepRecord {
    fn new(step: Step, kind: StepKind, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            step,
            kind,
            code,
            detail: detail.into(),
        }
    }

    fn ok(step: Step, code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(step, StepKind::Ok, code, detail)
    }

    fn skipped(step: Step, code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(step, StepKind::Skipped, code, detail)
    }

    fn error(step: Step, code: &'static str, err: impl std::fmt::Display) -> Self {
        Self::new(step, StepKind::Error, code, format!("{err:#}"))
    }
}

/// The steps one item went through, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemOutcome {
    pub work_item_id: String,
    pub records: Vec<StepRecord>,
}

impl ItemOutcome {
    /// The record for `step`, if the step ran (or was skipped) for this item.
    pub fn get(&self, step: Step) -> Option<&StepRecord> {
        self.records.iter().find(|record| record.step == step)
    }

    pub fn has_error(&self) -> bool {
        self.records.iter().any(|record| record.kind == StepKind::Error)
    }
}

/// Whether the GitHub-touching steps may run for an item now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubAccess {
    Allowed,
    /// The job is out of GitHub quota headroom: steps 3 and 4 are recorded as
    /// skipped and left to the merge poller's ordinary sweep.
    Deferred { remaining: i64, reserve: i64 },
}

/// The GitHub quota the engine last saw. A trait so the job's rate limiting can
/// be driven from a test without a registry.
pub trait GhBudget: Send + Sync {
    /// GraphQL points left in the current hourly window, or `None` when the
    /// engine has no recent reading. `None` means unknown, never exhausted.
    fn graphql_remaining(&self) -> Option<i64>;
}

/// [`GhBudget`] over the engine's metrics registry — the gauge
/// `github_api_usage` keeps from every GitHub response.
pub struct RegistryGhBudget(pub Arc<Registry>);

impl GhBudget for RegistryGhBudget {
    fn graphql_remaining(&self) -> Option<i64> {
        crate::github_api_usage::graphql_points_remaining(&self.0)
    }
}

/// Everything the background steps run against: the same services the engine's
/// own sweeps hold, borrowed from the engine rather than rebuilt.
#[derive(Clone, bon::Builder)]
pub struct UnparkEnv {
    pub work_db: Arc<WorkDb>,
    pub probe: Arc<dyn MergeProbe>,
    pub publisher: Arc<dyn ExecutionPublisher>,
    pub cube_client: Arc<dyn CubeClient>,
    pub completion: Arc<dyn CompletionRequests>,
    /// The merge poller's host, shared with the poller itself so conflict
    /// remediation started here goes through the same bounded queue and
    /// per-PR cooldown as one the poller started.
    pub host: Arc<dyn SweepHost>,
    pub coordinator: Arc<ExecutionCoordinator>,
    pub dispatch_events: Arc<dyn DispatchEventSink>,
    pub convergence: Arc<dyn LiveWorkerConvergence>,
    pub gh_budget: Arc<dyn GhBudget>,
    /// The engine's `max_review_cycles`: an item whose PR has already used its
    /// automated review cycles is not given another by an un-park.
    pub max_review_cycles: usize,
}

/// Emit one `unpark_reconcile` dispatch event, keyed by the work item (or, for
/// a job-level event, the project or item the job was started for).
pub(super) async fn emit(env: &UnparkEnv, key: &str, kind: StepKind, details: serde_json::Value) {
    let outcome = match kind {
        StepKind::Ok => Outcome::Ok,
        StepKind::Skipped => Outcome::Skipped,
        StepKind::Error => Outcome::Error,
    };
    env.dispatch_events
        .emit(
            DispatchEvent::new(Stage::UnparkReconcile, outcome, key)
                .with_work_item(key)
                .with_details(details),
        )
        .await;
}

/// Leave `record` on `work_item_id`: the `[parking]` audit line and the
/// `unpark_reconcile` event. Neither can fail the sequence.
async fn leave_trace(env: &UnparkEnv, work_item_id: &str, record: &StepRecord) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    if let Err(err) = append_parking_audit(&env.work_db, work_item_id, now, record.step.name(), &record.detail) {
        tracing::warn!(work_item_id, step = record.step.name(), ?err, "unpark: audit line not written");
    }
    emit(
        env,
        work_item_id,
        record.kind,
        serde_json::json!({
            "step": record.step.name(),
            "outcome": record.code,
            "detail": record.detail,
        }),
    )
    .await;
}

/// Steps 1 and 2 are database writes made inside the un-park transaction. This
/// leaves their trace: one `[parking]` line and one event per step per released
/// row. Runs in the un-park RPC, before the background job is scheduled.
pub async fn record_released_rows(env: &UnparkEnv, actor: &str, report: &UnparkReport) {
    for row in &report.rows {
        let via = match (&report.project_id, row.is_revision()) {
            (Some(project), _) => format!(" with project {project}"),
            (None, true) => " with its parent".to_owned(),
            (None, false) => String::new(),
        };
        let clear_park = StepRecord::ok(
            Step::ClearPark,
            "unparked",
            format!("un-parked{via} by {actor}; upkeep and dispatch resume for this item"),
        );
        leave_trace(env, &row.id, &clear_park).await;
        leave_trace(env, &row.id, &halt_stamp_record(row)).await;
    }
}

fn halt_stamp_record(row: &UnparkedRow) -> StepRecord {
    let hold = row
        .kept_hold
        .as_deref()
        .map(|hold| format!("; kept the `{hold}` hold, which a human or a progression agent resolves"))
        .unwrap_or_default();
    match &row.halt_stamp {
        HaltStamp::Cleared { reason } => StepRecord::ok(
            Step::ClearHaltStamps,
            "cleared",
            format!("cleared the `{reason}` dispatch-failure stamp left from before the park{hold}"),
        ),
        HaltStamp::Kept { reason, why } => StepRecord::ok(
            Step::ClearHaltStamps,
            "kept",
            format!("kept the `{reason}` stamp ({why}){hold}"),
        ),
        HaltStamp::Absent => StepRecord::ok(
            Step::ClearHaltStamps,
            "nothing_to_clear",
            format!("no dispatch-failure stamp to clear{hold}"),
        ),
    }
}

/// Run steps 3 to 7 for one un-parked item: refresh its PR and route what it
/// finds, schedule the AI review it missed, re-derive its execution, re-enter
/// its revisions into chain serialization, and flag it for progression. Every
/// step is recorded on the item; a failing step is recorded and the next one
/// still runs. Idempotent.
pub async fn reconcile_unparked_item(env: &UnparkEnv, work_item_id: &str, github: GithubAccess) -> ItemOutcome {
    let mut outcome = ItemOutcome {
        work_item_id: work_item_id.to_owned(),
        records: Vec::new(),
    };
    let task = match env.work_db.get_work_item(work_item_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => task,
        Ok(_) | Err(_) => {
            let record = StepRecord::error(Step::RefreshPr, "item_unreadable", "work item could not be read");
            leave_trace_best_effort(env, work_item_id, &record).await;
            outcome.records.push(record);
            return outcome;
        }
    };
    if task.deleted_at.is_some() || task.status.is_terminal() {
        let record = StepRecord::skipped(
            Step::RefreshPr,
            "finished",
            format!("item is {}; there is nothing left to reconcile", task.status),
        );
        leave_trace(env, work_item_id, &record).await;
        outcome.records.push(record);
        return outcome;
    }

    pr_steps(env, &task, github, &mut outcome).await;

    // Re-read: the PR steps can move the item (merged, closed, blocked).
    let task = env
        .work_db
        .get_work_item(work_item_id)
        .ok()
        .and_then(|item| match item {
            WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
            _ => None,
        })
        .unwrap_or(task);
    let record = reconcile_execution_step(env, &task).await;
    push(env, &mut outcome, record).await;
    if task.kind != TaskKind::Revision {
        let record = revisions_step(env, &task).await;
        push(env, &mut outcome, record).await;
    }
    let record = progression_grace_step(env, &task);
    push(env, &mut outcome, record).await;
    outcome
}

async fn push(env: &UnparkEnv, outcome: &mut ItemOutcome, record: StepRecord) {
    leave_trace(env, &outcome.work_item_id, &record).await;
    outcome.records.push(record);
}

async fn leave_trace_best_effort(env: &UnparkEnv, work_item_id: &str, record: &StepRecord) {
    // The audit append fails for an unreadable item; the event still carries it.
    leave_trace(env, work_item_id, record).await;
}

/// Steps 3 and 4. Only a non-revision item with a bound PR in `in_review` or
/// `blocked` has an open-PR lifecycle to refresh; a revision follows its
/// parent's PR and the parent's own pass covers it.
async fn pr_steps(env: &UnparkEnv, task: &Task, github: GithubAccess, outcome: &mut ItemOutcome) {
    let pr_url = task.pr_url.as_deref().filter(|url| !url.is_empty());
    let skip = |code: &'static str, detail: String| {
        [Step::RefreshPr, Step::ConflictProbe, Step::CiProbe, Step::AiReview]
            .map(|step| StepRecord::skipped(step, code, detail.clone()))
    };
    let skipped = match (task.kind == TaskKind::Revision, pr_url, task.status.clone(), github) {
        (true, ..) => Some(skip("follows_parent", "a revision follows its parent's PR; the parent's pass covers it".into())),
        (_, None, ..) => Some(skip("no_pr", "no PR is bound to this item; nothing to refresh".into())),
        (_, Some(_), status, _) if !matches!(status, TaskStatus::InReview | TaskStatus::Blocked) => Some(skip(
            "no_open_pr_lifecycle",
            format!("item is {status}; it has no review-phase PR to refresh"),
        )),
        (_, Some(_), _, GithubAccess::Deferred { remaining, reserve }) => Some(skip(
            "rate_limited",
            format!(
                "GitHub quota is low ({remaining} points left, below the {reserve} this job leaves for the \
                 engine's other callers); deferred to the merge poller's ordinary sweep"
            ),
        )),
        _ => None,
    };
    if let Some(records) = skipped {
        for record in records {
            push(env, outcome, record).await;
        }
        return;
    }
    let pr_url = pr_url.expect("checked above");

    // Part of step 4, run first so the CI detection below sees a fresh budget:
    // attempts spent before the park are stale evidence about CI the owner is
    // now asking the engine to look at again.
    let budget = prepare_ci_budget(env, task, pr_url);

    let (sweep, tier) = reconcile_one(
        &env.work_db,
        env.probe.as_ref(),
        env.publisher.as_ref(),
        Some(env.cube_client.as_ref()),
        Some(env.completion.as_ref()),
        Some(env.host.as_ref()),
        pr_url,
    )
    .await;
    let after = env.work_db.get_work_item(&task.id).ok().and_then(|item| match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
        _ => None,
    });

    let refresh = refresh_record(&sweep, tier.is_some(), after.as_ref(), pr_url);
    let open = refresh.code == "open";
    push(env, outcome, refresh).await;
    if !open {
        for step in [Step::ConflictProbe, Step::CiProbe, Step::AiReview] {
            let record = StepRecord::skipped(step, "pr_not_open", "the PR is not open, so there is nothing to probe");
            push(env, outcome, record).await;
        }
        return;
    }
    let conflict = if sweep.conflict_flagged > 0 {
        StepRecord::ok(
            Step::ConflictProbe,
            "conflict_routed",
            "the PR conflicts with its base; it entered the conflict-resolution path",
        )
    } else {
        StepRecord::ok(Step::ConflictProbe, "clear", "the PR has no merge conflict")
    };
    let ci = if sweep.ci_flagged > 0 {
        StepRecord::ok(
            Step::CiProbe,
            "ci_remediation_routed",
            format!("required checks are red; they entered CI remediation with a fresh attempt budget ({budget})"),
        )
    } else {
        StepRecord::ok(
            Step::CiProbe,
            "clear",
            format!("required checks are not red ({budget})"),
        )
    };
    push(env, outcome, conflict).await;
    push(env, outcome, ci).await;
    let routed = sweep.conflict_flagged > 0 || sweep.ci_flagged > 0;
    let in_review = after.as_ref().is_some_and(|task| task.status == TaskStatus::InReview);
    let record = if routed || !in_review {
        StepRecord::skipped(
            Step::AiReview,
            "fix_in_flight",
            "a conflict or CI fix is being routed; the review runs on the fix's push",
        )
    } else {
        ai_review_step(env, task, pr_url).await
    };
    push(env, outcome, record).await;
}

/// Reset the CI attempt budget and, if the item was parked `blocked:
/// ci_failure_exhausted`, move it back to `in_review` so the budget can be
/// spent. Returns a clause for the audit line describing what was done.
fn prepare_ci_budget(env: &UnparkEnv, task: &Task, pr_url: &str) -> String {
    let used = env.work_db.get_ci_attempts_used(&task.id).unwrap_or(0);
    let mut parts = Vec::new();
    if task.status == TaskStatus::Blocked && task.blocked_reason.as_deref() == Some("ci_failure_exhausted") {
        match env.work_db.clear_chore_blocked_ci_failure(&task.id, pr_url) {
            Ok(Some(_)) => parts.push("re-armed from ci_failure_exhausted".to_owned()),
            Ok(None) => {}
            Err(err) => tracing::warn!(work_item_id = %task.id, ?err, "unpark: could not re-arm exhausted CI block"),
        }
    }
    match env.work_db.reset_ci_attempts_used(&task.id) {
        Ok(()) => parts.push(format!("attempts used reset from {used}")),
        Err(err) => {
            tracing::warn!(work_item_id = %task.id, ?err, "unpark: could not reset the CI attempt budget");
            parts.push("attempt budget reset failed".to_owned());
        }
    }
    parts.join(", ")
}

/// Step 3's record, read from what the poller's targeted reconcile did.
///
/// The reconcile reports counts and the poll tier of the PR it probed, not a
/// per-PR verdict. A merge or close shows in the counts (and the item leaves
/// review). An open PR is the only probe result that yields a poll tier, so a
/// non-terminal item with no tier is one the probe could not observe — a failed
/// `gh` call, or an item the poller's candidate lists no longer include.
fn refresh_record(sweep: &SweepOutcome, observed_open: bool, after: Option<&Task>, pr_url: &str) -> StepRecord {
    if sweep.merged > 0 {
        return StepRecord::ok(
            Step::RefreshPr,
            "merged",
            format!("the PR merged while the item was parked; it took the ordinary merged path to done ({pr_url})"),
        );
    }
    if sweep.closed_unmerged > 0 {
        return StepRecord::ok(
            Step::RefreshPr,
            "closed_unmerged",
            format!(
                "the PR was closed without merging while the item was parked; it took the closed-unmerged path ({pr_url})"
            ),
        );
    }
    match after {
        Some(task) if task.status.is_terminal() => {
            StepRecord::ok(Step::RefreshPr, "finished", format!("the item is now {}", task.status))
        }
        _ if observed_open => StepRecord::ok(Step::RefreshPr, "open", format!("the PR is open ({pr_url})")),
        _ => StepRecord::error(
            Step::RefreshPr,
            "unobserved",
            "the live PR check did not observe an open PR; the merge poller will retry on its next sweep",
        ),
    }
}

/// Step 4's AI review: schedule one if no review has covered the PR's current
/// head, through the admission `bossctl review start` uses.
async fn ai_review_step(env: &UnparkEnv, task: &Task, pr_url: &str) -> StepRecord {
    let root = env.work_db.review_cycle_root_id(&task.id);
    let (cycle, last_reviewed) = match env.work_db.get_task_review_cycle_state(&root) {
        Ok(state) => state,
        Err(err) => return StepRecord::error(Step::AiReview, "state_unreadable", err),
    };
    let head = env
        .work_db
        .get_pr_status_snapshot(&task.id)
        .ok()
        .flatten()
        .and_then(|snapshot| snapshot.head_sha);
    if cycle >= 1 && last_reviewed.is_some() && last_reviewed == head {
        return StepRecord::ok(
            Step::AiReview,
            "current",
            "an automated review already covers the PR's current head",
        );
    }
    if usize::try_from(cycle).unwrap_or(0) >= env.max_review_cycles {
        return StepRecord::skipped(
            Step::AiReview,
            "cycle_bound",
            format!(
                "the PR has used its {} automated review cycle(s); a human reviews it from here",
                env.max_review_cycles
            ),
        );
    }
    if env.completion.review_batch_fanout_enabled() {
        return batch_review(env, task, pr_url).await;
    }
    match env
        .work_db
        .request_pr_review(&task.id, &StaticPrStateChecker(PrOpenState::Open))
    {
        Ok(execution) => {
            env.coordinator.kick();
            StepRecord::ok(
                Step::AiReview,
                "scheduled",
                format!("scheduled an automated review of the current head (execution {})", execution.id),
            )
        }
        Err(err) => StepRecord::error(Step::AiReview, "admission_refused", err),
    }
}

/// The review-batch flavour of [`ai_review_step`], used when `review_batch_fanout`
/// is on: the same admission `bossctl review start` performs.
async fn batch_review(env: &UnparkEnv, task: &Task, pr_url: &str) -> StepRecord {
    let metadata = match boss_github::pr_files::fetch_pr_view_json(
        pr_url,
        "state,baseRefOid,headRefOid,files,additions,deletions",
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(err) => return StepRecord::error(Step::AiReview, "metadata_unavailable", err),
    };
    if metadata.get("state").and_then(serde_json::Value::as_str) != Some("OPEN") {
        return StepRecord::skipped(Step::AiReview, "pr_not_open", "the PR is no longer open");
    }
    let input = match crate::completion::review_batch_input_from_metadata(&env.work_db, &task.id, pr_url, metadata) {
        Ok(input) => input,
        Err(err) => return StepRecord::error(Step::AiReview, "metadata_invalid", err),
    };
    match env
        .work_db
        .request_pre_merge_review_batch_for_pool(input, env.completion.review_pool_size())
    {
        Ok(crate::work::ReviewBatchDispatch::Created { batch, .. }) => {
            env.coordinator.kick();
            StepRecord::ok(
                Step::AiReview,
                "scheduled",
                format!("admitted an automated review batch for the current head ({})", batch.id),
            )
        }
        Ok(crate::work::ReviewBatchDispatch::ExistingBatch { .. } | crate::work::ReviewBatchDispatch::LegacyExecution(_)) => {
            StepRecord::ok(Step::AiReview, "in_flight", "an automated review of this head is already in flight")
        }
        Ok(crate::work::ReviewBatchDispatch::AlreadyReviewed) => StepRecord::ok(
            Step::AiReview,
            "current",
            "an automated review already covers the PR's current head",
        ),
        Ok(other) => StepRecord::skipped(
            Step::AiReview,
            "deferred",
            format!("review admission was deferred ({other:?}); the deferred-admission sweep retries it"),
        ),
        Err(err) => StepRecord::error(Step::AiReview, "admission_refused", err),
    }
}

/// Step 5: re-derive the item's execution.
///
/// `todo` and `blocked` rows get the board's ordinary execution reconcile, so a
/// `ready` or `waiting_dependency` row is minted exactly as for a fresh row. An
/// `active` row with no live execution goes through the orphan sweep's
/// re-dispatch, whose liveness guards and churn guard (counting from the
/// un-park) apply. A `worker_failed` or `awaiting_operator_answer` hold is
/// never redispatched. `in_review` rows have no execution of their own to
/// derive.
async fn reconcile_execution_step(env: &UnparkEnv, task: &Task) -> StepRecord {
    let step = Step::ReconcileExecution;
    if let Some(hold @ ("worker_failed" | "awaiting_operator_answer")) = task.blocked_reason.as_deref() {
        return StepRecord::skipped(
            step,
            "held",
            format!("the item is held as `{hold}`; it is not redispatched, and the progression sweep takes it from here"),
        );
    }
    match task.status {
        TaskStatus::InReview | TaskStatus::Done | TaskStatus::Archived => StepRecord::skipped(
            step,
            "no_execution_state",
            format!("item is {}; it has no implementation execution to derive", task.status),
        ),
        TaskStatus::Active => orphan_redispatch(env, task).await,
        TaskStatus::Todo | TaskStatus::Blocked => match env.work_db.reconcile_item_execution(&task.id) {
            Ok(result) => {
                let mine: Vec<_> = result
                    .created
                    .iter()
                    .chain(result.updated.iter())
                    .filter(|execution| execution.work_item_id == task.id)
                    .collect();
                match mine.first() {
                    Some(execution) => {
                        env.coordinator.kick();
                        StepRecord::ok(
                            step,
                            "minted",
                            format!("{} execution {} is {}", execution.kind, execution.id, execution.status),
                        )
                    }
                    None => StepRecord::ok(
                        step,
                        "nothing_to_mint",
                        "no execution needed: one is already pending, or the item is not dispatchable \
                         automatically (autostart off, waiting on dependencies, deferred or human-driven)",
                    ),
                }
            }
            Err(err) => StepRecord::error(step, "reconcile_failed", err),
        },
    }
}

async fn orphan_redispatch(env: &UnparkEnv, task: &Task) -> StepRecord {
    let step = Step::ReconcileExecution;
    let pass = run_one_pass_for_item_with_min_age(
        &env.work_db,
        env.coordinator.clone(),
        env.dispatch_events.as_ref(),
        env.convergence.as_ref(),
        &task.id,
        UNPARK_ORPHAN_MIN_AGE_SECS,
    )
    .await;
    if pass.redispatched > 0 {
        StepRecord::ok(step, "redispatched", "no worker was running; a fresh execution was queued")
    } else if pass.churn_skipped > 0 {
        StepRecord::skipped(
            step,
            "churn_guard",
            "the churn guard tripped on executions since the un-park, so the item was returned to Backlog",
        )
    } else if pass.dispatch_paused_skipped > 0 {
        StepRecord::skipped(step, "dispatch_paused", "dispatch is paused; the orphan sweep redispatches it on resume")
    } else if pass.live_execution_skipped + pass.running_reviewer_skipped + pass.live_process_skipped > 0 {
        StepRecord::ok(step, "live_worker", "a worker is already running for this item")
    } else if pass.deliberate_park_skipped > 0 {
        StepRecord::skipped(step, "held", "the item's last run ended in a deliberate park; it is not redispatched")
    } else if pass.admission_unknown_skipped > 0 {
        StepRecord::error(step, "admission_unknown", "the item's dispatch admission could not be read")
    } else {
        StepRecord::ok(step, "not_orphaned", "the item is not an orphan: it already has an execution")
    }
}

/// Step 6: the item's open revisions re-enter per-PR chain serialization. They
/// were un-parked with their parent; the reconcile re-derives each one's
/// execution under the chain's own rules.
async fn revisions_step(env: &UnparkEnv, task: &Task) -> StepRecord {
    let step = Step::Revisions;
    let revision_ids = match env.work_db.open_revision_ids(&task.id) {
        Ok(ids) => ids,
        Err(err) => return StepRecord::error(step, "revisions_unreadable", err),
    };
    if revision_ids.is_empty() {
        return StepRecord::skipped(step, "no_revisions", "the item has no open revisions");
    }
    let mut minted = 0;
    let mut failures = Vec::new();
    for revision_id in &revision_ids {
        match env.work_db.reconcile_item_execution(revision_id) {
            Ok(result) => {
                minted += result
                    .created
                    .iter()
                    .chain(result.updated.iter())
                    .filter(|execution| &execution.work_item_id == revision_id)
                    .count();
            }
            Err(err) => failures.push(format!("{revision_id}: {err:#}")),
        }
    }
    if minted > 0 {
        env.coordinator.kick();
    }
    if failures.is_empty() {
        StepRecord::ok(
            step,
            "re_entered",
            format!(
                "{} open revision(s) re-entered per-PR chain serialization ({minted} execution(s) minted or promoted)",
                revision_ids.len()
            ),
        )
    } else {
        StepRecord::error(step, "reconcile_failed", failures.join("; "))
    }
}

/// Step 7: tell the progression sweep to skip its stuck-age grace once for this
/// item, so one that is still stuck after steps 2 to 6 is diagnosed now.
fn progression_grace_step(env: &UnparkEnv, task: &Task) -> StepRecord {
    let step = Step::ProgressionGrace;
    match env.work_db.stamp_progression_grace_waived(&task.id) {
        Ok(true) => StepRecord::ok(
            step,
            "stamped",
            "the stuck-age grace period is waived once, so anything still stuck is diagnosed now",
        ),
        Ok(false) => StepRecord::skipped(step, "finished", "the item is finished; nothing to flag"),
        Err(err) => StepRecord::error(step, "stamp_failed", err),
    }
}
