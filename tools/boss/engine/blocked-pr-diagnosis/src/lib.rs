//! Why a Review-lane card is blocked or stalled, as one operator-facing
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

/// How many times the sweep may re-dispatch one dead `todo` revision before
/// it stops and asks a human. Each re-dispatch is a fresh worker run; the
/// engine's own orphan sweep already caps consecutive dead runs at three,
/// and two sweep re-dispatches on top of the original dispatch is the same
/// "three runs died" budget seen from the PR's side.
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
    /// Sweep re-dispatches already recorded against this revision.
    pub redispatches_used: u32,
}

impl RevisionFacts {
    fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active" | "blocked")
    }

    /// Open, nothing non-terminal, and the newest run ended without the
    /// revision reaching Review: the worker died or was reaped.
    fn is_dead(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active")
            && self.live_execution_count == 0
            && self.latest_execution.as_ref().is_some_and(ExecutionFacts::is_terminal)
    }

    fn is_dispatch_stalled(&self) -> bool {
        matches!(self.status.as_str(), "todo" | "active")
            && self
                .latest_execution
                .as_ref()
                .is_some_and(|e| !e.is_terminal() && !e.is_running() && e.age_secs >= DISPATCH_STALL_SECS)
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
    /// `retried` (job re-run requested), `retry_failed` (Buildkite refused
    /// the retry), or `budget_exhausted`.
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewBatchFacts {
    pub status: String,
    pub generation: i64,
    pub has_settled_verdict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewFacts {
    pub latest_pre_merge_batch: Option<ReviewBatchFacts>,
    pub generation_cap: i64,
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
    ReviewUnreviewedAfterReap,
    ReviewReadmissionPending,
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
    if let Some(r) = open.iter().find(|r| r.is_dead()) {
        return Some(dead_revision(r, Category::RevisionDead));
    }
    if let Some(r) = open.iter().find(|r| r.is_dispatch_stalled()) {
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
            if rev.is_dead() {
                return dead_revision(rev, Category::MergeConflictRevisionDead);
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
    if let Some(t) = &f.ci.last_transient_retry {
        match t.outcome.as_str() {
            "retried" if t.age_secs < TRANSIENT_RETRY_FRESH_SECS => {
                return Diagnosis::waiting(
                    Category::CiTransientRetrying,
                    format!(
                        "CI retrying: {} in {} ({}/{})",
                        t.summary, t.check_name, t.attempt, t.budget
                    ),
                );
            }
            "retried" => {
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
        if rev.is_dead() {
            return dead_revision(rev, Category::CiFixRevisionDead);
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

fn diagnose_review(f: &BlockedFacts) -> Option<Diagnosis> {
    let batch = f.review.latest_pre_merge_batch.as_ref()?;
    if batch.status != "failed" || batch.has_settled_verdict {
        return None;
    }
    if batch.generation >= f.review.generation_cap {
        return Some(Diagnosis::human(
            Category::ReviewUnreviewedAfterReap,
            format!(
                "not reviewed: the automated review failed {} times on this head; start one by hand",
                batch.generation
            ),
        ));
    }
    Some(Diagnosis::waiting(
        Category::ReviewReadmissionPending,
        format!(
            "automated review failed (generation {}/{}); a replacement is being re-admitted",
            batch.generation, f.review.generation_cap
        ),
    ))
}

fn dead_revision(r: &RevisionFacts, category: Category) -> Diagnosis {
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
    if r.redispatches_used < DEAD_REVISION_REDISPATCH_BUDGET {
        Diagnosis::new(
            category,
            format!(
                "{} \"{}\" has no running worker (last run {last}); re-dispatching ({}/{})",
                r.origin.label(),
                r.name,
                r.redispatches_used + 1,
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
                "{} \"{}\" died {} times (last run {last}); start it by hand or close it",
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
        assert!(d.reason.contains("re-dispatching (1/2)"), "{}", d.reason);

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

    #[test]
    fn reaped_review_batch_is_readmitting_below_cap_and_human_at_cap() {
        let mut f = in_review();
        f.review.latest_pre_merge_batch = Some(ReviewBatchFacts {
            status: "failed".into(),
            generation: 1,
            has_settled_verdict: false,
        });
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewReadmissionPending);
        f.review.latest_pre_merge_batch.as_mut().unwrap().generation = 2;
        let d = diagnose(&f).unwrap();
        assert_eq!(d.category, Category::ReviewUnreviewedAfterReap);
        assert_eq!(d.action, Action::NeedsHuman);
        f.review.latest_pre_merge_batch.as_mut().unwrap().has_settled_verdict = true;
        assert_eq!(
            diagnose(&f),
            None,
            "a settled verdict is a review decision, not a stall"
        );
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
            Category::ReviewUnreviewedAfterReap,
            Category::ReviewReadmissionPending,
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
