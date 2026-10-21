//! Why a Review-lane card is blocked or stalled, as one human-readable
//! sentence plus the one safe thing the engine may do about it.
//!
//! The engine's blocked-PR sweep (`blocked_pr_sweep` in `engine/core`)
//! gathers [`BlockedFacts`] for every open Boss-tracked PR from the work
//! database and calls [`diagnose`]. This crate owns the rules: which
//! category the facts fall into, how that category reads on the card, and
//! whether the engine re-kicks an existing path ([`Action::RedispatchRevision`])
//! or hands the card to a human ([`Action::NeedsHuman`]). It is pure so every
//! category is unit-testable without a database.
//!
//! [`is_review_blocked`] mirrors the engine's `classify_review_section`
//! Blocked rule exactly; the engine has a parity test for that. The sweep
//! also diagnoses cards that are *not* in the Blocked section but have
//! silently stopped progressing (a revision whose worker died, an automated
//! review that was reaped at its generation cap), so "blocked" here means
//! "will not reach merge without something happening".

/// How many times the sweep may re-dispatch dead revisions on one PR head
/// before it stops and asks a human. The budget is per `(work item, PR head)`
/// across all revisions, so a fresh revision on the same head does not get a
/// fresh allowance, and a new head starts over. Each re-dispatch is a fresh
/// worker run; the engine's own orphan sweep already caps consecutive dead
/// runs at three, and two sweep re-dispatches on top of the original
/// dispatch is the same "three runs died" budget seen from the PR's side.
/// A revision is additionally capped at the same number over its lifetime.
pub const DEAD_REVISION_REDISPATCH_BUDGET: u32 = 2;

/// A dead `active` revision belongs to the orphan sweep (90s minimum age,
/// 60s cadence, its own churn and attention guards). Only once it has sat
/// dead this long without that sweep re-dispatching it does the card say a
/// human must look: by then the orphan sweep has declined it on purpose.
pub const ORPHAN_RECOVERY_GRACE_SECS: i64 = 10 * 60;

/// A revision execution that has not started running after this long is
/// reported as a dispatch stall. Matches the engine's own persistent-stall
/// threshold (`dispatch_stall_escalation::PERSISTENT_STALL_THRESHOLD`, five
/// minutes) with margin for a slow cube workspace lease: the measured
/// dispatch stalls that motivated this sweep ran 12–168 minutes, so ten
/// minutes reports every one of them without flagging a healthy lease.
pub const DISPATCH_STALL_SECS: i64 = 10 * 60;

/// A transient-CI retry older than this is no longer "retrying": a retried
/// `checks` / `bazel-build-test` job passes within 409s / 503s (p99), so a
/// retry the poller has not seen clear after 30 minutes has either failed
/// again (a newer ledger row says so) or is a Buildkite problem the card
/// should name as such.
pub const TRANSIENT_RETRY_FRESH_SECS: i64 = 30 * 60;

/// Which engine path minted a revision, from `tasks.created_via`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevisionOrigin {
    CiFix,
    MergeConflict,
    Review,
    #[default]
    Other,
}

impl RevisionOrigin {
    pub fn from_created_via(created_via: &str) -> Self {
        if created_via.starts_with("ci-fix:") {
            Self::CiFix
        } else if created_via.starts_with("merge-conflict:") {
            Self::MergeConflict
        } else if created_via.starts_with("pr_review:") {
            Self::Review
        } else {
            Self::Other
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CiFix => "CI fix revision",
            Self::MergeConflict => "conflict fix revision",
            Self::Review => "review revision",
            Self::Other => "revision",
        }
    }
}

/// The newest `work_executions` row of a revision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionFacts {
    pub id: String,
    /// `work_executions.status` wire value.
    pub status: String,
    /// Seconds since the execution was created.
    pub age_secs: i64,
    pub dispatch_wait_reason: Option<String>,
    pub last_error: Option<String>,
}

impl ExecutionFacts {
    fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
        )
    }

    fn is_running(&self) -> bool {
        matches!(self.status.as_str(), "running" | "waiting_human")
    }

    /// Not started and not finished, and older than [`DISPATCH_STALL_SECS`]:
    /// stuck claiming a workspace or waiting on dispatch.
    fn is_dispatch_stalled(&self) -> bool {
        !self.is_terminal() && !self.is_running() && self.age_secs >= DISPATCH_STALL_SECS
    }

    /// The run ended because the worker died or failed, not because anyone
    /// stopped it: the only terminal states the sweep may re-dispatch from.
    fn died(&self) -> bool {
        matches!(self.status.as_str(), "failed" | "orphaned")
    }
}

/// One open or recently open revision in the PR's chain.
#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct RevisionFacts {
    pub id: String,
    /// Card title (the revision's description, first line).
    pub name: String,
    /// `tasks.status` wire value.
    pub status: String,
    pub origin: RevisionOrigin,
    pub blocked_reason: Option<String>,
    pub blocked_detail: Option<String>,
    pub latest_execution: Option<ExecutionFacts>,
    /// Number of `work_executions` rows that are not terminal.
    pub live_execution_count: usize,
    /// Sweep re-dispatches already recorded against this revision over its
    /// lifetime (the secondary cap; the per-head count is
    /// [`BlockedFacts::head_redispatches_used`]).
    pub redispatches_used: u32,
}

impl RevisionFacts {
    fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active" | "blocked")
    }

    /// Open, nothing non-terminal, and the newest run ended by failing or
    /// being orphaned: the worker died. A run someone cancelled or abandoned
    /// is [`Self::is_halted`], never re-dispatched automatically.
    fn is_dead(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active")
            && self.live_execution_count == 0
            && self.latest_execution.as_ref().is_some_and(ExecutionFacts::died)
    }

    /// Open, nothing non-terminal, and the newest run was stopped on purpose
    /// (cancelled or abandoned) or completed without moving the revision to
    /// Review. Re-dispatching would override that decision.
    fn is_halted(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active")
            && self.live_execution_count == 0
            && self
                .latest_execution
                .as_ref()
                .is_some_and(|e| e.is_terminal() && !e.died())
    }

    fn is_dispatch_stalled(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active")
            && self
                .latest_execution
                .as_ref()
                .is_some_and(ExecutionFacts::is_dispatch_stalled)
    }

    fn has_running_worker(&self) -> bool {
        self.latest_execution.as_ref().is_some_and(ExecutionFacts::is_running)
    }
}

/// The newest transient-CI retry the engine recorded for the PR head.
#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct TransientRetryFacts {
    pub check_name: String,
    /// Short label of the signal ("HTTP 504", "agent stopped").
    pub summary: String,
    /// `retried` (job re-run requested), `retry_unconfirmed` (the request
    /// timed out and may have been accepted), `retry_failed` (Buildkite
    /// refused the retry), or `budget_exhausted`.
    pub outcome: String,
    pub age_secs: i64,
    pub attempt: u32,
    pub budget: u32,
}

/// A live `ci_remediations` / `conflict_resolutions` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttemptFacts {
    pub status: String,
    pub revision_task_id: Option<String>,
    /// Conflict ladder rung running in-process, when one is.
    pub mechanical_rung_in_flight: Option<i64>,
    pub age_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct CiFacts {
    /// Names of the failing required checks, from `ci_required_detail`.
    pub failing_checks: Vec<String>,
    pub attempts_used: i64,
    pub budget: i64,
    pub last_transient_retry: Option<TransientRetryFacts>,
    pub active_fix_attempt: Option<AttemptFacts>,
    pub suppressed_for_head: bool,
    pub auto_maintenance_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictFacts {
    pub active_attempt: Option<AttemptFacts>,
    pub auto_maintenance_enabled: bool,
}

/// The newest pre-merge review batch **for the observed PR head**.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewBatchFacts {
    pub status: String,
    pub generation: i64,
}

/// The newest review verdict recorded for the observed PR head.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadVerdictFacts {
    /// `pr_review_verdicts.gate_outcome` wire value.
    pub gate_outcome: String,
    /// A findings revision exists for this verdict and is either live
    /// (queued, running or blocked) or already delivered. A deleted or
    /// archived revision that never delivered does not count: nothing is
    /// addressing the findings.
    pub has_successor_revision: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewFacts {
    /// The PR head is known (the poller has observed one).
    pub head_known: bool,
    /// Newest batch whose target is the observed head; `None` means no batch
    /// has been admitted for it.
    pub latest_pre_merge_batch: Option<ReviewBatchFacts>,
    /// Newest non-stale verdict on the observed head, batch or legacy.
    pub head_verdict: Option<HeadVerdictFacts>,
    pub generation_cap: i64,
    /// `tasks.review_required_state` (`required`, `approved`, …).
    pub review_required_state: Option<String>,
    /// Automated (batch) review is turned on for this engine, so a failed
    /// batch is replaced by admission.
    pub fanout_enabled: bool,
    /// The admission path owes this card a pre-merge review right now: it is
    /// a candidate of `list_tasks_awaiting_pre_merge_review_admission` (held
    /// in PendingReview, an admission deferral on file, or a reaped batch).
    /// Independent of `review_required_state`, which is GitHub's human
    /// approval gate.
    pub admission_owed: bool,
    /// Non-terminal `pr_review` executions belonging to the head's live batch.
    pub reviewers: Vec<ExecutionFacts>,
}

/// Everything the sweep knows about one Review-lane card.
#[derive(Debug, Clone, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct BlockedFacts {
    /// `tasks.status` wire value.
    pub status: String,
    pub blocked_reason: Option<String>,
    pub blocked_detail: Option<String>,
    pub ci_required_state: Option<String>,
    pub pr_mergeable_state: Option<String>,
    pub revisions: Vec<RevisionFacts>,
    /// Dead-revision re-dispatches already recorded for this PR head, across
    /// every revision of the card.
    pub head_redispatches_used: u32,
    pub ci: CiFacts,
    pub conflict: ConflictFacts,
    pub review: ReviewFacts,
}

/// The cause bucket. `key` is stored and shipped to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    ReviewFeedback,
    MergeConflictResolving,
    MergeConflictRevisionDead,
    MergeConflictNoRemediation,
    CiTransientRetrying,
    CiTransientExhausted,
    CiFixExhausted,
    CiFixRevisionLive,
    CiFixRevisionDead,
    CiFixRevisionBlocked,
    CiFailingNoRemediation,
    RevisionBlocked,
    RevisionDead,
    DispatchStalled,
    RevisionHalted,
    ReviewUnreviewedAfterReap,
    ReviewReadmissionPending,
    ReviewAwaitingApproval,
    ReviewFindingsUnaddressed,
    ReviewGaveUp,
    ReviewNotAdmitted,
    ReviewerStalled,
    BlockedOther,
}

impl Category {
    pub fn key(self) -> &'static str {
        match self {
            Self::ReviewFeedback => "review_feedback",
            Self::MergeConflictResolving => "merge_conflict_resolving",
            Self::MergeConflictRevisionDead => "merge_conflict_revision_dead",
            Self::MergeConflictNoRemediation => "merge_conflict_no_remediation",
            Self::CiTransientRetrying => "ci_transient_retrying",
            Self::CiTransientExhausted => "ci_transient_exhausted",
            Self::CiFixExhausted => "ci_fix_exhausted",
            Self::CiFixRevisionLive => "ci_fix_revision_live",
            Self::CiFixRevisionDead => "ci_fix_revision_dead",
            Self::CiFixRevisionBlocked => "ci_fix_revision_blocked",
            Self::CiFailingNoRemediation => "ci_failing_no_remediation",
            Self::RevisionBlocked => "revision_blocked",
            Self::RevisionDead => "revision_dead",
            Self::DispatchStalled => "dispatch_stalled",
            Self::RevisionHalted => "revision_halted",
            Self::ReviewAwaitingApproval => "review_awaiting_approval",
            Self::ReviewFindingsUnaddressed => "review_findings_unaddressed",
            Self::ReviewGaveUp => "review_gave_up",
            Self::ReviewNotAdmitted => "review_not_admitted",
            Self::ReviewerStalled => "review_reviewer_stalled",
            Self::ReviewUnreviewedAfterReap => "review_unreviewed_after_reap",
            Self::ReviewReadmissionPending => "review_readmission_pending",
            Self::BlockedOther => "blocked_other",
        }
    }
}

/// What the sweep does with a diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// An engine path is already working on it; only render the reason.
    None,
    /// Re-request an execution for a `todo` revision whose last run died
    /// with no successor (the `bossctl work start` path), within
    /// [`DEAD_REVISION_REDISPATCH_BUDGET`]. An `active` one is left to the
    /// orphan sweep, which owns that state.
    RedispatchRevision { revision_id: String },
    /// Nothing safe is left for the engine; the reason names what a human
    /// must do.
    NeedsHuman,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub category: Category,
    pub reason: String,
    pub action: Action,
}

impl Diagnosis {
    fn new(category: Category, reason: impl Into<String>, action: Action) -> Self {
        Self {
            category,
            reason: reason.into(),
            action,
        }
    }

    fn human(category: Category, reason: impl Into<String>) -> Self {
        Self::new(category, format!("Needs you: {}", reason.into()), Action::NeedsHuman)
    }

    /// Prefix the reason with what the card is blocked on, keeping the
    /// "Needs you:" marker first.
    fn in_context(mut self, context: &str) -> Self {
        self.reason = match self.reason.strip_prefix("Needs you: ") {
            Some(rest) => format!("Needs you: {context}: {rest}"),
            None => format!("{context}: {}", self.reason),
        };
        self
    }

    fn waiting(category: Category, reason: impl Into<String>) -> Self {
        Self::new(category, reason, Action::None)
    }
}

/// The engine's Review-column Blocked rule (`classify_review_section`).
pub fn is_review_blocked(f: &BlockedFacts) -> bool {
    let open: Vec<&RevisionFacts> = f.revisions.iter().filter(|r| r.is_open()).collect();
    let in_revision_blocked_only = !open.is_empty() && open.iter().all(|r| r.status == "blocked");
    f.status == "blocked"
        || f.blocked_reason.is_some()
        || f.ci_required_state.as_deref() == Some("fail")
        || f.pr_mergeable_state.as_deref() == Some("conflicting")
        || in_revision_blocked_only
}

/// Diagnose the card. `None` means nothing is blocking or stalling it.
pub fn diagnose(f: &BlockedFacts) -> Option<Diagnosis> {
    if f.status == "blocked" && f.blocked_reason.as_deref() == Some("review_feedback") {
        return Some(Diagnosis::human(
            Category::ReviewFeedback,
            "a reviewer requested changes; address the review and push",
        ));
    }
    let conflicting =
        f.pr_mergeable_state.as_deref() == Some("conflicting") || f.blocked_reason.as_deref() == Some("merge_conflict");
    if conflicting {
        return Some(diagnose_conflict(f));
    }
    let ci_failing = f.ci_required_state.as_deref() == Some("fail")
        || matches!(f.blocked_reason.as_deref(), Some("ci_failure" | "ci_failure_exhausted"));
    if ci_failing {
        return Some(diagnose_ci(f));
    }
    let open: Vec<&RevisionFacts> = f.revisions.iter().filter(|r| r.is_open()).collect();
    if !open.is_empty() && open.iter().all(|r| r.status == "blocked") {
        let r = open[0];
        return Some(Diagnosis::human(
            Category::RevisionBlocked,
            format!("{} \"{}\" is blocked ({})", r.origin.label(), r.name, blocked_label(r)),
        ));
    }
    if f.status == "blocked" {
        let reason = f.blocked_reason.as_deref().unwrap_or("no reason recorded");
        let detail = f
            .blocked_detail
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        return Some(Diagnosis::human(
            Category::BlockedOther,
            format!("blocked: {reason}{detail}"),
        ));
    }
    // Not in the Blocked section, but stalled: a revision with no live
    // worker, a revision stuck in dispatch, or an unreviewed head.
    if let Some(d) = open
        .iter()
        .find_map(|r| revision_health(r, f.head_redispatches_used, Category::RevisionDead))
    {
        return Some(d);
    }
    diagnose_review(f)
}

fn diagnose_conflict(f: &BlockedFacts) -> Diagnosis {
    if let Some(attempt) = &f.conflict.active_attempt {
        if let Some(rung) = attempt.mechanical_rung_in_flight {
            return Diagnosis::waiting(
                Category::MergeConflictResolving,
                format!(
                    "merge conflict with main: automatic resolution running (rung {rung}, {})",
                    minutes(attempt.age_secs)
                ),
            );
        }
        if let Some(rev) = attempt
            .revision_task_id
            .as_deref()
            .and_then(|id| f.revisions.iter().find(|r| r.id == id))
        {
            if rev.status == "blocked" {
                return Diagnosis::human(
                    Category::MergeConflictRevisionDead,
                    format!(
                        "conflict fix revision \"{}\" is blocked ({})",
                        rev.name,
                        blocked_label(rev)
                    ),
                );
            }
            if let Some(d) = revision_health(rev, f.head_redispatches_used, Category::MergeConflictRevisionDead) {
                return d.in_context("merge conflict with main");
            }
            if rev.is_open() {
                return Diagnosis::waiting(
                    Category::MergeConflictResolving,
                    format!(
                        "merge conflict with main: fix revision \"{}\" {}",
                        rev.name,
                        if rev.has_running_worker() { "running" } else { "queued" }
                    ),
                );
            }
        }
        return Diagnosis::waiting(
            Category::MergeConflictResolving,
            format!(
                "merge conflict with main: resolution attempt {} ({})",
                attempt.status,
                minutes(attempt.age_secs)
            ),
        );
    }
    if !f.conflict.auto_maintenance_enabled {
        return Diagnosis::human(
            Category::MergeConflictNoRemediation,
            "merge conflict with main; automatic resolution is disabled for this product or PR",
        );
    }
    Diagnosis::waiting(
        Category::MergeConflictNoRemediation,
        "merge conflict with main; no resolution attempt is running yet",
    )
}

fn diagnose_ci(f: &BlockedFacts) -> Diagnosis {
    let checks = if f.ci.failing_checks.is_empty() {
        "required checks".to_owned()
    } else {
        f.ci.failing_checks.join(", ")
    };
    // The linked fix revision comes first: it is the newest cause of the
    // card's state, and it must be health-checked before either the retry
    // ledger or the creation budget can speak for it.
    let fix =
        f.ci.active_fix_attempt
            .as_ref()
            .and_then(|a| a.revision_task_id.as_deref())
            .and_then(|id| f.revisions.iter().find(|r| r.id == id))
            .or_else(|| {
                f.revisions
                    .iter()
                    .find(|r| r.origin == RevisionOrigin::CiFix && r.is_open())
            });
    if let Some(rev) = fix {
        if rev.status == "blocked" {
            return Diagnosis::human(
                Category::CiFixRevisionBlocked,
                format!(
                    "CI failing on {checks}; fix revision \"{}\" is blocked ({})",
                    rev.name,
                    blocked_label(rev)
                ),
            );
        }
        if let Some(d) = revision_health(rev, f.head_redispatches_used, Category::CiFixRevisionDead) {
            return d.in_context(&format!("CI failing on {checks}"));
        }
        return Diagnosis::waiting(
            Category::CiFixRevisionLive,
            format!(
                "CI failing on {checks}: fix revision \"{}\" {} (attempt {}/{})",
                rev.name,
                if rev.has_running_worker() { "running" } else { "queued" },
                f.ci.attempts_used,
                f.ci.budget
            ),
        );
    }
    // With no live fix revision, the newest transient-retry row speaks only
    // while it still describes the current failure.
    if let Some(t) =
        f.ci.last_transient_retry
            .as_ref()
            .filter(|t| f.ci.failing_checks.is_empty() || f.ci.failing_checks.contains(&t.check_name))
    {
        match t.outcome.as_str() {
            "retry_unconfirmed" if t.age_secs < TRANSIENT_RETRY_FRESH_SECS => {
                return Diagnosis::waiting(
                    Category::CiTransientRetrying,
                    format!(
                        "CI retry of {} ({}) requested; awaiting confirmation from Buildkite ({}/{})",
                        t.check_name, t.summary, t.attempt, t.budget
                    ),
                );
            }
            "retried" | "pending" if t.age_secs < TRANSIENT_RETRY_FRESH_SECS => {
                return Diagnosis::waiting(
                    Category::CiTransientRetrying,
                    format!(
                        "CI retrying: {} in {} ({}/{})",
                        t.summary, t.check_name, t.attempt, t.budget
                    ),
                );
            }
            "retried" | "pending" | "retry_unconfirmed" => {
                return Diagnosis::human(
                    Category::CiTransientExhausted,
                    format!(
                        "CI retry of {} ({}) requested {} ago has not cleared; check the build",
                        t.check_name,
                        t.summary,
                        minutes(t.age_secs)
                    ),
                );
            }
            "budget_exhausted" => {
                return Diagnosis::human(
                    Category::CiTransientExhausted,
                    format!(
                        "CI retry budget exhausted ({}/{}): {} in {}; retry the job by hand",
                        t.attempt, t.budget, t.summary, t.check_name
                    ),
                );
            }
            _ => {
                return Diagnosis::human(
                    Category::CiTransientExhausted,
                    format!(
                        "CI retry of {} ({}) could not be requested; retry the job by hand",
                        t.check_name, t.summary
                    ),
                );
            }
        }
    }
    if f.blocked_reason.as_deref() == Some("ci_failure_exhausted") || f.ci.attempts_used >= f.ci.budget {
        return Diagnosis::human(
            Category::CiFixExhausted,
            format!(
                "CI auto-fix budget exhausted ({}/{}) on {checks}; fix CI by hand or raise the budget",
                f.ci.attempts_used, f.ci.budget
            ),
        );
    }
    if !f.ci.auto_maintenance_enabled {
        return Diagnosis::human(
            Category::CiFailingNoRemediation,
            format!("CI failing on {checks}; automatic fixes are disabled for this product or PR"),
        );
    }
    if f.ci.suppressed_for_head {
        return Diagnosis::human(
            Category::CiFailingNoRemediation,
            format!("CI failing on {checks}; automatic fixes are suppressed for this head"),
        );
    }
    if f.conflict.active_attempt.is_some() {
        return Diagnosis::waiting(
            Category::CiFailingNoRemediation,
            format!("CI failing on {checks}; waiting for the conflict resolution to land first"),
        );
    }
    Diagnosis::waiting(
        Category::CiFailingNoRemediation,
        format!(
            "CI failing on {checks}; no fix revision is running (attempts {}/{})",
            f.ci.attempts_used, f.ci.budget
        ),
    )
}

/// Reasons for a card whose only open gate is the automated review. Only
/// reached when CI, conflicts and revisions are all healthy, so every
/// outcome here names why the review gate is not progressing.
fn diagnose_review(f: &BlockedFacts) -> Option<Diagnosis> {
    let r = &f.review;
    if !r.head_known {
        return None;
    }
    let green =
        f.ci_required_state.as_deref() == Some("success") && f.pr_mergeable_state.as_deref() == Some("mergeable");
    if let Some(v) = &r.head_verdict {
        return match v.gate_outcome.as_str() {
            "completed_clean" if green && r.review_required_state.as_deref() == Some("required") => {
                Some(Diagnosis::human(
                    Category::ReviewAwaitingApproval,
                    "the automated review is clean and every check passed; a required human approval is still missing",
                ))
            }
            "completed_with_findings" if !v.has_successor_revision => Some(Diagnosis::human(
                Category::ReviewFindingsUnaddressed,
                "the automated review found issues on this head but no revision is addressing them; create one or fix the findings by hand",
            )),
            "revision_creation_failed" => Some(Diagnosis::human(
                Category::ReviewFindingsUnaddressed,
                "the automated review found issues on this head but creating the revision for them failed; create one by hand",
            )),
            "gave_up" => Some(Diagnosis::human(
                Category::ReviewGaveUp,
                "the automated review gave up on this head; start a review by hand",
            )),
            _ => None,
        };
    }
    if let Some(e) = r.reviewers.iter().find(|e| e.is_dispatch_stalled()) {
        let wait = e
            .dispatch_wait_reason
            .as_deref()
            .map(|w| format!(", waiting on {w}"))
            .unwrap_or_default();
        return Some(Diagnosis::waiting(
            Category::ReviewerStalled,
            format!(
                "the automated reviewer has waited {} for dispatch ({}{wait}); the review cannot finish until it starts",
                minutes(e.age_secs),
                e.status
            ),
        ));
    }
    let Some(batch) = r.latest_pre_merge_batch.as_ref() else {
        // No batch and no verdict for this head. Only call it a stall once
        // every check passes. Whether an automated review is owed comes from
        // the admission path, never from the human approval gate; that gate
        // only explains a missing approval.
        if !green {
            return None;
        }
        if r.admission_owed {
            return Some(if r.fanout_enabled {
                Diagnosis::waiting(
                    Category::ReviewNotAdmitted,
                    "checks pass but no automated review has been admitted for this head yet",
                )
            } else {
                Diagnosis::human(
                    Category::ReviewNotAdmitted,
                    "checks pass but no automated review has run on this head and batch review is turned off; start a review by hand",
                )
            });
        }
        return (r.review_required_state.as_deref() == Some("required")).then(|| {
            Diagnosis::human(
                Category::ReviewAwaitingApproval,
                "checks pass and no automated review is pending; a required human approval is still missing",
            )
        });
    };
    if batch.status != "failed" {
        return None;
    }
    if batch.generation >= r.generation_cap {
        return Some(Diagnosis::human(
            Category::ReviewUnreviewedAfterReap,
            format!(
                "not reviewed: the automated review failed {} times on this head; start one by hand",
                batch.generation
            ),
        ));
    }
    if !r.fanout_enabled {
        return Some(Diagnosis::human(
            Category::ReviewUnreviewedAfterReap,
            format!(
                "not reviewed: the automated review failed (generation {}/{}) and batch review is turned off, so no replacement will start; start one by hand",
                batch.generation, r.generation_cap
            ),
        ));
    }
    Some(Diagnosis::waiting(
        Category::ReviewReadmissionPending,
        format!(
            "automated review failed (generation {}/{}); a replacement is being re-admitted",
            batch.generation, r.generation_cap
        ),
    ))
}

/// Health of one open revision, shared by the generic, CI and conflict
/// paths so a stalled or dead fix revision can never read as "queued":
/// dead (worker died), halted (someone stopped it), or stuck in dispatch.
/// `None` means the revision is healthy (queued or running).
fn revision_health(r: &RevisionFacts, head_used: u32, dead_category: Category) -> Option<Diagnosis> {
    if r.is_dead() {
        return Some(dead_revision(r, head_used, dead_category));
    }
    if r.is_halted() {
        let last = r.latest_execution.as_ref().map_or("unknown", |e| e.status.as_str());
        return Some(Diagnosis::human(
            Category::RevisionHalted,
            format!(
                "{} \"{}\" has no running worker because its last run was {last}; start it by hand or close it",
                r.origin.label(),
                r.name
            ),
        ));
    }
    if r.is_dispatch_stalled() {
        let e = r.latest_execution.as_ref()?;
        let wait = e
            .dispatch_wait_reason
            .as_deref()
            .map(|w| format!(", waiting on {w}"))
            .unwrap_or_default();
        return Some(Diagnosis::waiting(
            Category::DispatchStalled,
            format!(
                "{} \"{}\" has waited {} for dispatch ({}{wait})",
                r.origin.label(),
                r.name,
                minutes(e.age_secs),
                e.status
            ),
        ));
    }
    None
}

fn dead_revision(r: &RevisionFacts, head_used: u32, category: Category) -> Diagnosis {
    let (last, age) = r
        .latest_execution
        .as_ref()
        .map(|e| (e.status.clone(), e.age_secs))
        .unwrap_or_else(|| ("unknown".to_owned(), 0));
    if r.status == "active" {
        // The orphan sweep owns dead `active` work items.
        return if age < ORPHAN_RECOVERY_GRACE_SECS {
            Diagnosis::waiting(
                category,
                format!(
                    "{} \"{}\" has no running worker (last run {last}); waiting for orphan recovery",
                    r.origin.label(),
                    r.name
                ),
            )
        } else {
            Diagnosis::human(
                category,
                format!(
                    "{} \"{}\" has had no running worker for {} (last run {last}) and orphan recovery did not re-dispatch it; check its attention items",
                    r.origin.label(),
                    r.name,
                    minutes(age)
                ),
            )
        };
    }
    // The head budget is the contract; the per-revision count only tightens it.
    let used = head_used.max(r.redispatches_used);
    if used < DEAD_REVISION_REDISPATCH_BUDGET {
        Diagnosis::new(
            category,
            format!(
                "{} \"{}\" has no running worker (last run {last}); re-dispatching ({}/{} on this PR head)",
                r.origin.label(),
                r.name,
                used + 1,
                DEAD_REVISION_REDISPATCH_BUDGET
            ),
            Action::RedispatchRevision {
                revision_id: r.id.clone(),
            },
        )
    } else {
        Diagnosis::human(
            category,
            format!(
                "{} \"{}\" died {} times on this PR head (last run {last}); start it by hand or close it",
                r.origin.label(),
                r.name,
                DEAD_REVISION_REDISPATCH_BUDGET + 1
            ),
        )
    }
}

fn blocked_label(r: &RevisionFacts) -> String {
    match (&r.blocked_reason, &r.blocked_detail) {
        (Some(reason), Some(detail)) => format!("{reason}: {detail}"),
        (Some(reason), None) => reason.clone(),
        (None, Some(detail)) => detail.clone(),
        (None, None) => "no reason recorded".to_owned(),
    }
}

fn minutes(secs: i64) -> String {
    let m = secs.max(0) / 60;
    if m >= 120 {
        format!("{}h", m / 60)
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_review() -> BlockedFacts {
        BlockedFacts {
            status: "in_review".into(),
            ci_required_state: Some("success".into()),
            pr_mergeable_state: Some("mergeable".into()),
            ci: CiFacts {
                budget: 3,
                auto_maintenance_enabled: true,
                ..Default::default()
            },
            conflict: ConflictFacts {
                auto_maintenance_enabled: true,
                ..Default::default()
            },
            review: ReviewFacts {
                generation_cap: 2,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn exec(status: &str, age_secs: i64) -> ExecutionFacts {
        ExecutionFacts {
            id: "exec_1".into(),
            status: status.into(),
            age_secs,
            ..Default::default()
        }
    }

    fn revision(status: &str, origin: RevisionOrigin, latest: Option<ExecutionFacts>) -> RevisionFacts {
        let live = latest.as_ref().is_some_and(|e| !e.is_terminal()) as usize;
        RevisionFacts {
            id: "rev_1".into(),
            name: "Fix failing CI: checks".into(),
            status: status.into(),
            origin,
            latest_execution: latest,
            live_execution_count: live,
            ..Default::default()
        }
    }

    #[test]
    fn healthy_card_has_no_diagnosis() {
        assert!(!is_review_blocked(&in_review()));
        assert_eq!(diagnose(&in_review()), None);
    }

    #[test]
    fn blocked_rule_matches_each_review_section_input() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        assert!(is_review_blocked(&f));
        let mut f = in_review();
        f.pr_mergeable_state = Some("conflicting".into());
        assert!(is_review_blocked(&f));
        let mut f = in_review();
        f.status = "blocked".into();
        assert!(is_review_blocked(&f));
        let mut f = in_review();
        f.blocked_reason = Some("ci_failure".into());
        assert!(is_review_blocked(&f));
        let mut f = in_review();
        f.revisions = vec![revision("blocked", RevisionOrigin::Review, None)];
        assert!(is_review_blocked(&f));
        f.revisions
            .push(revision("active", RevisionOrigin::Review, Some(exec("running", 10))));
        assert!(
            !is_review_blocked(&f),
            "one running revision is In Revision, not Blocked"
        );
    }

    #[test]
    fn transient_retry_in_flight_renders_attempt_over_budget() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.failing_checks = vec!["buildkite/mono/checks".into()];
        f.ci.last_transient_retry = Some(TransientRetryFacts {
            check_name: "buildkite/mono/checks".into(),
            summary: "HTTP 504".into(),
            outcome: "retried".into(),
            age_secs: 90,
            attempt: 1,
            budget: 2,
        });
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiTransientRetrying);
        assert_eq!(d.reason, "CI retrying: HTTP 504 in buildkite/mono/checks (1/2)");
        assert_eq!(d.action, Action::None);
    }

    #[test]
    fn transient_budget_exhausted_asks_a_human() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.last_transient_retry = Some(TransientRetryFacts {
            check_name: "buildkite/mono/checks".into(),
            summary: "HTTP 504".into(),
            outcome: "budget_exhausted".into(),
            age_secs: 5,
            attempt: 2,
            budget: 2,
        });
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiTransientExhausted);
        assert!(
            d.reason
                .starts_with("Needs you: CI retry budget exhausted (2/2): HTTP 504"),
            "{}",
            d.reason
        );
        assert_eq!(d.action, Action::NeedsHuman);
    }

    #[test]
    fn stale_transient_retry_is_no_longer_retrying() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.last_transient_retry = Some(TransientRetryFacts {
            check_name: "checks".into(),
            summary: "HTTP 504".into(),
            outcome: "retried".into(),
            age_secs: TRANSIENT_RETRY_FRESH_SECS + 1,
            attempt: 1,
            budget: 2,
        });
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiTransientExhausted);
        assert!(d.reason.contains("has not cleared"), "{}", d.reason);
    }

    #[test]
    fn deterministic_failure_with_live_fix_revision_is_waiting() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.failing_checks = vec!["buildkite/mono/bazel-build-test".into()];
        f.ci.attempts_used = 1;
        f.revisions = vec![revision("active", RevisionOrigin::CiFix, Some(exec("running", 60)))];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiFixRevisionLive);
        assert_eq!(
            d.reason,
            "CI failing on buildkite/mono/bazel-build-test: fix revision \"Fix failing CI: checks\" running (attempt 1/3)"
        );
        assert_eq!(d.action, Action::None);
    }

    #[test]
    fn dead_fix_revision_is_redispatched_within_budget_then_handed_over() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.revisions = vec![revision("todo", RevisionOrigin::CiFix, Some(exec("orphaned", 600)))];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiFixRevisionDead);
        assert_eq!(
            d.action,
            Action::RedispatchRevision {
                revision_id: "rev_1".into()
            }
        );
        assert!(
            d.reason.contains("re-dispatching (1/2 on this PR head)"),
            "{}",
            d.reason
        );

        f.revisions[0].redispatches_used = DEAD_REVISION_REDISPATCH_BUDGET;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.action, Action::NeedsHuman);
        assert!(d.reason.contains("died 3 times"), "{}", d.reason);
    }

    #[test]
    fn dead_active_revision_is_left_to_orphan_recovery_then_escalated() {
        let mut f = in_review();
        f.revisions = vec![revision("active", RevisionOrigin::Review, Some(exec("orphaned", 120)))];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::RevisionDead);
        assert_eq!(d.action, Action::None, "the orphan sweep owns a young dead active item");
        assert!(d.reason.contains("waiting for orphan recovery"), "{}", d.reason);
        f.revisions[0].latest_execution.as_mut().unwrap().age_secs = ORPHAN_RECOVERY_GRACE_SECS + 60;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.action, Action::NeedsHuman);
        assert!(d.reason.contains("orphan recovery did not re-dispatch"), "{}", d.reason);
    }

    #[test]
    fn ci_fix_budget_exhausted_names_checks_and_counts() {
        let mut f = in_review();
        f.status = "blocked".into();
        f.blocked_reason = Some("ci_failure_exhausted".into());
        f.ci.failing_checks = vec!["checks".into(), "mac-app-build".into()];
        f.ci.attempts_used = 3;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiFixExhausted);
        assert_eq!(
            d.reason,
            "Needs you: CI auto-fix budget exhausted (3/3) on checks, mac-app-build; fix CI by hand or raise the budget"
        );
    }

    #[test]
    fn ci_failing_with_nothing_running_says_so() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.failing_checks = vec!["checks".into()];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiFailingNoRemediation);
        assert_eq!(
            d.reason,
            "CI failing on checks; no fix revision is running (attempts 0/3)"
        );
        f.ci.auto_maintenance_enabled = false;
        assert_eq!(diagnose(&f).unwrap().action, Action::NeedsHuman);
    }

    #[test]
    fn conflict_outranks_ci_and_names_the_rung_or_revision() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.pr_mergeable_state = Some("conflicting".into());
        f.conflict.active_attempt = Some(AttemptFacts {
            status: "running".into(),
            mechanical_rung_in_flight: Some(1),
            age_secs: 120,
            ..Default::default()
        });
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::MergeConflictResolving);
        assert_eq!(
            d.reason,
            "merge conflict with main: automatic resolution running (rung 1, 2m)"
        );

        f.conflict.active_attempt = Some(AttemptFacts {
            status: "running".into(),
            revision_task_id: Some("rev_1".into()),
            ..Default::default()
        });
        f.revisions = vec![revision(
            "todo",
            RevisionOrigin::MergeConflict,
            Some(exec("failed", 900)),
        )];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::MergeConflictRevisionDead);
        assert!(matches!(d.action, Action::RedispatchRevision { .. }));

        f.conflict.active_attempt = None;
        f.conflict.auto_maintenance_enabled = false;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::MergeConflictNoRemediation);
        assert_eq!(d.action, Action::NeedsHuman);
    }

    #[test]
    fn review_feedback_and_other_blocked_reasons_need_a_human() {
        let mut f = in_review();
        f.status = "blocked".into();
        f.blocked_reason = Some("review_feedback".into());
        assert_eq!(diagnose(&f).unwrap().category, Category::ReviewFeedback);
        f.blocked_reason = Some("deletion_signoff".into());
        f.blocked_detail = Some("deletes 40 files".into());
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::BlockedOther);
        assert_eq!(d.reason, "Needs you: blocked: deletion_signoff — deletes 40 files");
    }

    #[test]
    fn only_blocked_revisions_name_the_revision_and_its_reason() {
        let mut f = in_review();
        let mut r = revision("blocked", RevisionOrigin::Review, None);
        r.blocked_reason = Some("dependency".into());
        f.revisions = vec![r];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::RevisionBlocked);
        assert_eq!(
            d.reason,
            "Needs you: review revision \"Fix failing CI: checks\" is blocked (dependency)"
        );
    }

    #[test]
    fn dispatch_stall_is_reported_after_the_threshold_only() {
        let mut f = in_review();
        f.revisions = vec![revision(
            "todo",
            RevisionOrigin::Review,
            Some(ExecutionFacts {
                dispatch_wait_reason: Some("cube_workspace_lease".into()),
                ..exec("claimed", DISPATCH_STALL_SECS - 1)
            }),
        )];
        assert_eq!(diagnose(&f), None, "a young claim is not a stall");
        f.revisions[0].latest_execution.as_mut().unwrap().age_secs = 38 * 60;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::DispatchStalled);
        assert_eq!(
            d.reason,
            "review revision \"Fix failing CI: checks\" has waited 38m for dispatch (claimed, waiting on cube_workspace_lease)"
        );
    }

    fn review_ready() -> BlockedFacts {
        let mut f = in_review();
        f.review.head_known = true;
        f.review.fanout_enabled = true;
        f
    }

    fn failed_batch(generation: i64) -> Option<ReviewBatchFacts> {
        Some(ReviewBatchFacts {
            status: "failed".into(),
            generation,
        })
    }

    fn verdict(outcome: &str, has_successor_revision: bool) -> Option<HeadVerdictFacts> {
        Some(HeadVerdictFacts {
            gate_outcome: outcome.into(),
            has_successor_revision,
        })
    }

    #[test]
    fn reaped_review_batch_is_readmitting_below_cap_and_human_at_cap() {
        let mut f = review_ready();
        f.review.latest_pre_merge_batch = failed_batch(1);
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewReadmissionPending);
        f.review.latest_pre_merge_batch = failed_batch(2);
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewUnreviewedAfterReap);
        assert_eq!(d.action, Action::NeedsHuman);
    }

    #[test]
    fn failed_review_batch_with_fanout_off_is_not_reported_as_readmitting() {
        let mut f = review_ready();
        f.review.fanout_enabled = false;
        f.review.latest_pre_merge_batch = failed_batch(1);
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewUnreviewedAfterReap);
        assert_eq!(d.action, Action::NeedsHuman);
        assert!(!d.reason.contains("re-admitted"), "{}", d.reason);
    }

    #[test]
    fn review_gate_stalls_each_get_a_specific_reason() {
        // Clean review, human approval still required.
        let mut f = review_ready();
        f.review.head_verdict = verdict("completed_clean", false);
        assert_eq!(diagnose(&f), None, "clean and nothing else required is healthy");
        f.review.review_required_state = Some("required".into());
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewAwaitingApproval);
        assert_eq!(d.action, Action::NeedsHuman);

        // Findings with no revision to address them.
        let mut f = review_ready();
        f.review.head_verdict = verdict("completed_with_findings", false);
        assert_eq!(diagnose(&f).unwrap().category, Category::ReviewFindingsUnaddressed);
        f.review.head_verdict = verdict("completed_with_findings", true);
        assert_eq!(diagnose(&f), None, "a findings revision owns it");

        // Revision creation failed / reviewer gave up.
        f.review.head_verdict = verdict("revision_creation_failed", false);
        assert_eq!(diagnose(&f).unwrap().category, Category::ReviewFindingsUnaddressed);
        f.review.head_verdict = verdict("gave_up", false);
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewGaveUp);
        assert_eq!(d.action, Action::NeedsHuman);
    }

    #[test]
    fn an_owed_review_is_a_not_admitted_stall_whatever_the_human_gate_says() {
        for human in [None, Some("approved"), Some("unknown"), Some("required")] {
            let mut f = review_ready();
            f.review.admission_owed = true;
            f.review.review_required_state = human.map(str::to_owned);
            let d = diagnose(&f).unwrap_or_else(|| panic!("human state {human:?} must still get a reason"));
            assert_eq!(d.category, Category::ReviewNotAdmitted, "{human:?}");
            assert_eq!(d.action, Action::None, "{human:?}");
        }
        // Batch review off: nobody will admit it.
        let mut f = review_ready();
        f.review.admission_owed = true;
        f.review.fanout_enabled = false;
        let d = diagnose(&f).unwrap();
        assert_eq!(
            (d.category, d.action),
            (Category::ReviewNotAdmitted, Action::NeedsHuman)
        );
        // Not green yet: CI is the thing being waited on, not the review.
        let mut f = review_ready();
        f.review.admission_owed = true;
        f.ci_required_state = Some("pending".into());
        assert_eq!(diagnose(&f), None);
        // No observed head: nothing to say.
        let mut f = in_review();
        f.review.admission_owed = true;
        assert_eq!(diagnose(&f), None);
    }

    #[test]
    fn a_required_human_approval_alone_is_not_evidence_of_a_missing_review() {
        let mut f = review_ready();
        assert_eq!(diagnose(&f), None, "no review owed and no approval needed is healthy");
        f.review.review_required_state = Some("required".into());
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewAwaitingApproval);
        assert_eq!(d.action, Action::NeedsHuman);
        // Same with automated review turned off.
        f.review.fanout_enabled = false;
        assert_eq!(diagnose(&f).unwrap().category, Category::ReviewAwaitingApproval);
    }

    #[test]
    fn awaiting_approval_claim_requires_a_green_head() {
        let mut f = review_ready();
        f.review.head_verdict = verdict("completed_clean", false);
        f.review.review_required_state = Some("required".into());
        assert!(diagnose(&f).is_some());
        f.ci_required_state = Some("pending".into());
        assert_eq!(
            diagnose(&f),
            None,
            "'every check passed' must not be claimed while CI is pending"
        );
    }

    #[test]
    fn a_reviewer_stuck_in_dispatch_is_reported_after_the_threshold_only() {
        for stuck in ["claimed", "workspace_recovery", "cube_workspace_lease_attempted"] {
            let mut f = review_ready();
            f.review.latest_pre_merge_batch = Some(ReviewBatchFacts {
                status: "collecting".into(),
                generation: 1,
            });
            f.review.reviewers = vec![ExecutionFacts {
                dispatch_wait_reason: Some("cube_workspace_lease".into()),
                ..exec(stuck, DISPATCH_STALL_SECS - 1)
            }];
            assert_eq!(diagnose(&f), None, "a young {stuck} reviewer is not a stall");
            f.review.reviewers[0].age_secs = 45 * 60;
            let d = diagnose(&f).unwrap();
            assert_eq!(d.category, Category::ReviewerStalled, "{stuck}");
            assert_eq!(d.action, Action::None);
            assert!(d.reason.contains("45m") && d.reason.contains(stuck), "{}", d.reason);
        }
        // A running reviewer is healthy however old it is.
        let mut f = review_ready();
        f.review.latest_pre_merge_batch = Some(ReviewBatchFacts {
            status: "collecting".into(),
            generation: 1,
        });
        f.review.reviewers = vec![exec("running", 3 * 3600)];
        assert_eq!(diagnose(&f), None);
    }

    #[test]
    fn cancelled_or_abandoned_revisions_are_never_redispatched() {
        for status in ["cancelled", "abandoned"] {
            // A queued revision whose run was cancelled before it started,
            // and a running one the user stopped (task demoted to todo).
            for (rev_status, age) in [("todo", 30), ("todo", 4_000)] {
                let mut f = in_review();
                f.revisions = vec![revision(rev_status, RevisionOrigin::Review, Some(exec(status, age)))];
                let d = diagnose(&f).unwrap();
                assert_eq!(d.category, Category::RevisionHalted, "{status}");
                assert_eq!(d.action, Action::NeedsHuman, "{status}");
                assert!(d.reason.contains(status), "{}", d.reason);
            }
        }
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.revisions = vec![revision("todo", RevisionOrigin::CiFix, Some(exec("cancelled", 60)))];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.action, Action::NeedsHuman);
        assert!(
            d.reason.starts_with("Needs you: CI failing on required checks: "),
            "{}",
            d.reason
        );
    }

    #[test]
    fn redispatch_budget_is_per_pr_head_across_revisions() {
        let mut f = in_review();
        f.revisions = vec![revision("todo", RevisionOrigin::Review, Some(exec("failed", 600)))];
        // A brand-new revision (0 used itself) on a head already at the cap.
        f.head_redispatches_used = DEAD_REVISION_REDISPATCH_BUDGET;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.action, Action::NeedsHuman);
        assert!(d.reason.contains("on this PR head"), "{}", d.reason);
        // A head change resets the head count; the revision's own count
        // still caps it.
        f.head_redispatches_used = 0;
        assert!(matches!(
            diagnose(&f).unwrap().action,
            Action::RedispatchRevision { .. }
        ));
        f.revisions[0].redispatches_used = DEAD_REVISION_REDISPATCH_BUDGET;
        assert_eq!(diagnose(&f).unwrap().action, Action::NeedsHuman);
    }

    #[test]
    fn stalled_fix_revisions_are_not_reported_as_queued() {
        let stalled = |origin| {
            revision(
                "todo",
                origin,
                Some(ExecutionFacts {
                    dispatch_wait_reason: Some("cube_workspace_lease".into()),
                    ..exec("workspace_recovery", DISPATCH_STALL_SECS + 60)
                }),
            )
        };
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.failing_checks = vec!["checks".into()];
        f.revisions = vec![stalled(RevisionOrigin::CiFix)];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::DispatchStalled);
        assert!(d.reason.starts_with("CI failing on checks: "), "{}", d.reason);

        let mut f = in_review();
        f.pr_mergeable_state = Some("conflicting".into());
        f.conflict.active_attempt = Some(AttemptFacts {
            status: "running".into(),
            revision_task_id: Some("rev_1".into()),
            ..Default::default()
        });
        f.revisions = vec![stalled(RevisionOrigin::MergeConflict)];
        assert_eq!(diagnose(&f).unwrap().category, Category::DispatchStalled);
    }

    #[test]
    fn final_in_budget_fix_revision_is_live_not_exhausted_and_can_be_redispatched() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.attempts_used = 3;
        f.ci.budget = 3;
        f.revisions = vec![revision("active", RevisionOrigin::CiFix, Some(exec("running", 60)))];
        assert_eq!(diagnose(&f).unwrap().category, Category::CiFixRevisionLive);
        f.revisions = vec![revision("todo", RevisionOrigin::CiFix, Some(exec("orphaned", 600)))];
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::CiFixRevisionDead);
        assert!(matches!(d.action, Action::RedispatchRevision { .. }));
    }

    #[test]
    fn stale_transient_row_does_not_override_a_live_fix_revision() {
        let mut f = in_review();
        f.ci_required_state = Some("fail".into());
        f.ci.failing_checks = vec!["buildkite/mono/checks".into()];
        f.ci.attempts_used = 1;
        f.revisions = vec![revision("active", RevisionOrigin::CiFix, Some(exec("running", 60)))];
        for outcome in ["retried", "budget_exhausted", "retry_failed"] {
            f.ci.last_transient_retry = Some(TransientRetryFacts {
                check_name: "buildkite/mono/checks".into(),
                summary: "HTTP 504".into(),
                outcome: outcome.into(),
                age_secs: 5,
                attempt: 2,
                budget: 2,
            });
            let d = diagnose(&f).unwrap();
            assert_eq!(d.category, Category::CiFixRevisionLive, "{outcome}: {}", d.reason);
        }
        // Without a fix revision, a row for a check that is no longer
        // failing says nothing about the current failure.
        f.revisions.clear();
        f.ci.failing_checks = vec!["buildkite/mono/other".into()];
        assert_eq!(diagnose(&f).unwrap().category, Category::CiFailingNoRemediation);
    }

    #[test]
    fn origin_parses_created_via_prefixes() {
        assert_eq!(RevisionOrigin::from_created_via("ci-fix:crm_1"), RevisionOrigin::CiFix);
        assert_eq!(
            RevisionOrigin::from_created_via("merge-conflict:crz_1"),
            RevisionOrigin::MergeConflict
        );
        assert_eq!(RevisionOrigin::from_created_via("pr_review:x"), RevisionOrigin::Review);
        assert_eq!(RevisionOrigin::from_created_via("human"), RevisionOrigin::Other);
    }

    #[test]
    fn every_category_key_is_unique_and_snake_case() {
        let all = [
            Category::ReviewFeedback,
            Category::MergeConflictResolving,
            Category::MergeConflictRevisionDead,
            Category::MergeConflictNoRemediation,
            Category::CiTransientRetrying,
            Category::CiTransientExhausted,
            Category::CiFixExhausted,
            Category::CiFixRevisionLive,
            Category::CiFixRevisionDead,
            Category::CiFixRevisionBlocked,
            Category::CiFailingNoRemediation,
            Category::RevisionBlocked,
            Category::RevisionDead,
            Category::DispatchStalled,
            Category::RevisionHalted,
            Category::ReviewUnreviewedAfterReap,
            Category::ReviewReadmissionPending,
            Category::ReviewAwaitingApproval,
            Category::ReviewFindingsUnaddressed,
            Category::ReviewGaveUp,
            Category::ReviewNotAdmitted,
            Category::BlockedOther,
        ];
        let keys: std::collections::BTreeSet<&str> = all.iter().map(|c| c.key()).collect();
        assert_eq!(keys.len(), all.len());
        assert!(
            keys.iter()
                .all(|k| k.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
    }
}
