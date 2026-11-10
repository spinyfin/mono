//! Persisted dispatch hold vocabulary shared by scheduling and reporting.
//!
//! Every work item that is waiting rather than running carries one of the
//! reasons below. The board, `bossctl dispatch stuck` and `bossctl dispatch
//! diagnose` all read the reason the engine's wait classifier produces
//! (`boss_work_db`'s `queued_wait`), and render the phrase from
//! [`dispatch_wait_phrase`]: there is no second vocabulary in a client.
//!
//! A reason is either one of the codes in [`DISPATCH_WAIT_REASON_CODES`] or a
//! sentence the scheduler wrote for one hold (the interactive cap, a workspace
//! lease, a chain hold naming the blocking task). [`DISPATCH_WAIT_REASON_UNCLASSIFIED`]
//! is the one code that is not a reason: it marks a wait the engine could not
//! name, which is a defect to surface rather than a state to explain.
//!
//! `tools/boss/docs/dispatch-wait-reasons.md` describes the reasons and what
//! happens to an unclassified wait.

use boss_protocol::WorkExecution;

/// The per-repository workspace lease limiter is at capacity.
pub const WAITING_FOR_WORKSPACE_REASON: &str = "Waiting for a workspace";

/// Worker pool has no free slot this drain pass.
pub const DISPATCH_WAIT_REASON_POOL_EXHAUSTED: &str = "pool_exhausted";

/// Global dispatch pause is holding this ready row.
pub const DISPATCH_WAIT_REASON_DISPATCH_PAUSED: &str = "dispatch_paused";

/// Automation pause is holding this automation-pool row.
pub const DISPATCH_WAIT_REASON_AUTOMATION_PAUSED: &str = "automation_paused";

/// A `progression` row is held `ready` because the engine cannot yet spawn a
/// progression agent (no worker kind of its own). Not a capacity wait: nothing
/// is short of slots, so it is deliberately absent from
/// [`is_capacity_wait_reason`].
pub const DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE: &str = "progression_unavailable";

/// A resume or successor row is held `ready` because the execution it recovers
/// from is finished but still holds its cube workspace lease, so that run's
/// unpublished work has not been preserved yet. Dispatching now would start
/// the row from trunk and strand the work. Not a capacity wait: nothing is
/// short of slots or workspaces, and the row stays held until the lease is
/// released (which preserves first), so it is deliberately absent from
/// [`is_capacity_wait_reason`].
pub const DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION: &str = "awaiting_predecessor_preservation";

/// Dispatch focus is holding an out-of-scope ready row.
pub use boss_protocol::DISPATCH_WAIT_REASON_FOCUS_MODE;

/// Another run on the same pull request is live, so this one waits its turn.
/// The scheduler persists a sentence naming the blocking task for this hold;
/// the bare code is its event-stream name and is kept labelled for rows that
/// carry it.
pub const DISPATCH_WAIT_REASON_CHAIN_SERIALIZED: &str = "chain_serialized";

/// [`DISPATCH_WAIT_REASON_CHAIN_SERIALIZED`] where the live run is an
/// automated review.
pub const DISPATCH_WAIT_REASON_CHAIN_SERIALIZED_REVIEW_HELD: &str = "chain_serialized_review_held";

/// The interactive pool is at its configured concurrency cap. Persisted as
/// [`interactive_cap_wait_reason`]'s sentence; the bare code is its
/// event-stream name.
pub const DISPATCH_WAIT_REASON_INTERACTIVE_CAP: &str = "interactive_concurrency_cap";

/// An unmet `blocks` prerequisite gates the row.
pub const DISPATCH_WAIT_REASON_WAITING_DEPENDENCY: &str = "waiting_dependency";

/// The row is a later task of a project whose earlier task has not finished
/// starting: the reconciler holds every project task but the first incomplete
/// one in `waiting_dependency`.
pub const DISPATCH_WAIT_REASON_PROJECT_ORDER: &str = "project_order";

/// The row is `ready` but its `dispatch_not_before` deadline is still ahead.
pub const DISPATCH_WAIT_REASON_NOT_BEFORE: &str = "not_before";

/// The row is `ready` and no scheduler pass has held it: the next pass either
/// claims a worker for it or records the hold that stops it. A row that stays
/// here across scheduler passes is reclassified as
/// [`DISPATCH_WAIT_REASON_UNCLASSIFIED`].
pub const DISPATCH_WAIT_REASON_AWAITING_DISPATCH: &str = "awaiting_dispatch";

/// Startup preflight (boot recovery, the local capability probe, or a failed
/// runtime check) is holding every local dispatch.
pub const DISPATCH_WAIT_REASON_PREFLIGHT_HOLD: &str = "dispatch_preflight";

/// The row's task has automatic start switched off, so the reconciler leaves
/// its queued execution alone until it is started explicitly.
pub const DISPATCH_WAIT_REASON_AUTOSTART_OFF: &str = "autostart_off";

/// Nothing is queued because the item is human-driven: no agent ever starts.
pub const DISPATCH_WAIT_REASON_HUMAN_DRIVEN: &str = "human_driven";

/// Nothing is queued because the item is deferred until it is approved.
pub const DISPATCH_WAIT_REASON_DEFERRED: &str = "deferred";

/// Nothing is queued because the item, its project or its parent is parked.
pub const DISPATCH_WAIT_REASON_PARKED: &str = "parked";

/// Nothing is queued because no repository resolves for the item.
pub const DISPATCH_WAIT_REASON_REPO_UNRESOLVED: &str = "repo_unresolved";

/// Nothing is queued because the item's pull request is already open: review
/// and merge handling own the next step.
pub const DISPATCH_WAIT_REASON_PR_OPEN: &str = "pr_open";

/// Nothing is queued because the item's last run ended and the engine never
/// starts a replacement on its own.
pub const DISPATCH_WAIT_REASON_RUN_ENDED: &str = "run_ended";

/// Nothing is queued because the revision's parent task has no pull request
/// to push to yet.
pub const DISPATCH_WAIT_REASON_AWAITING_PARENT_PR: &str = "awaiting_parent_pr";

/// The item is `active` with no run pending or live: its worker ended and the
/// orphan sweep will start a replacement.
pub const DISPATCH_WAIT_REASON_AWAITING_REDISPATCH: &str = "awaiting_redispatch";

/// The item is `active` with no run pending or live because its pull request
/// is with review and merge handling, which owns the next step.
pub const DISPATCH_WAIT_REASON_AWAITING_REVIEW: &str = "awaiting_review";

/// The item is `active` with no run pending or live because a dispatch
/// failure (the churn guard, a deliberate park) is holding it.
pub const DISPATCH_WAIT_REASON_DISPATCH_FAILED: &str = "dispatch_failed";

/// The item is `active` with no run pending or live because worker recovery
/// gave up and raised an attention item for a person.
pub const DISPATCH_WAIT_REASON_RECOVERY_ATTENTION: &str = "recovery_attention";

/// The engine could not name why the item is waiting. Never a reason in its
/// own right: it is reported as an anomaly (logged, and raised as a
/// `dispatch_wait_unclassified` attention item), and the card says the item is
/// in an unexpected state.
pub use boss_protocol::DISPATCH_WAIT_REASON_UNCLASSIFIED;

/// Every code a waiting item can carry, paired with the phrase shown for it.
/// A code added without a phrase here does not compile into a label: it is
/// reported as [`DISPATCH_WAIT_REASON_UNCLASSIFIED`], so a new hold cannot
/// reach a card as a bare identifier.
pub const DISPATCH_WAIT_REASON_CODES: &[(&str, &str)] = &[
    (DISPATCH_WAIT_REASON_POOL_EXHAUSTED, "worker pool full"),
    (DISPATCH_WAIT_REASON_DISPATCH_PAUSED, "dispatch paused"),
    (DISPATCH_WAIT_REASON_AUTOMATION_PAUSED, "automation paused"),
    (DISPATCH_WAIT_REASON_FOCUS_MODE, "outside the dispatch focus set"),
    (
        DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE,
        "the engine cannot start progression agents yet",
    ),
    (
        DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION,
        "waiting for the previous run's work to be preserved",
    ),
    (
        DISPATCH_WAIT_REASON_CHAIN_SERIALIZED,
        "another run on the same pull request is in progress",
    ),
    (
        DISPATCH_WAIT_REASON_CHAIN_SERIALIZED_REVIEW_HELD,
        "an automated review of the same pull request is in progress",
    ),
    (
        DISPATCH_WAIT_REASON_INTERACTIVE_CAP,
        "the interactive worker cap is reached",
    ),
    (DISPATCH_WAIT_REASON_WAITING_DEPENDENCY, "waiting for a dependency"),
    (
        DISPATCH_WAIT_REASON_PROJECT_ORDER,
        "waiting for an earlier task in its project",
    ),
    (
        DISPATCH_WAIT_REASON_NOT_BEFORE,
        "held until its scheduled dispatch time",
    ),
    (
        DISPATCH_WAIT_REASON_AWAITING_DISPATCH,
        "ready for the dispatcher's next pass",
    ),
    (
        DISPATCH_WAIT_REASON_PREFLIGHT_HOLD,
        "engine startup checks are holding dispatch",
    ),
    (
        DISPATCH_WAIT_REASON_AUTOSTART_OFF,
        "automatic start is off; start it to queue it",
    ),
    (DISPATCH_WAIT_REASON_HUMAN_DRIVEN, "human-driven; no agent will start"),
    (DISPATCH_WAIT_REASON_DEFERRED, "deferred until it is approved to start"),
    (DISPATCH_WAIT_REASON_PARKED, "parked"),
    (
        DISPATCH_WAIT_REASON_REPO_UNRESOLVED,
        "no repository resolves for dispatch",
    ),
    (
        DISPATCH_WAIT_REASON_PR_OPEN,
        "its pull request is open, so no new run is queued",
    ),
    (
        DISPATCH_WAIT_REASON_RUN_ENDED,
        "its last run ended; start it again to queue a new run",
    ),
    (
        DISPATCH_WAIT_REASON_AWAITING_PARENT_PR,
        "waiting for the parent task to open its pull request",
    ),
    (
        DISPATCH_WAIT_REASON_AWAITING_REDISPATCH,
        "its worker ended; the engine will start a new run",
    ),
    (
        DISPATCH_WAIT_REASON_AWAITING_REVIEW,
        "its pull request is with review and merge handling",
    ),
    (
        DISPATCH_WAIT_REASON_DISPATCH_FAILED,
        "held after a dispatch failure; see the failure on the card",
    ),
    (
        DISPATCH_WAIT_REASON_RECOVERY_ATTENTION,
        "worker recovery gave up; waiting for a person",
    ),
    (
        DISPATCH_WAIT_REASON_UNCLASSIFIED,
        "the engine has no reason recorded for this wait",
    ),
];

/// The phrase for a wait code, or `None` when `reason` is not a known code.
pub fn dispatch_wait_code_label(reason: &str) -> Option<&'static str> {
    DISPATCH_WAIT_REASON_CODES
        .iter()
        .find(|(code, _)| *code == reason)
        .map(|(_, label)| *label)
}

/// Whether `reason` has the shape of a code (`snake_case`) but no phrase. Such
/// a value is a hold nobody named: it must be reported as unclassified, never
/// rendered as an identifier.
pub fn is_unlabelled_wait_code(reason: &str) -> bool {
    dispatch_wait_code_label(reason).is_none()
        && !reason.is_empty()
        && reason
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The phrase a waiting item shows for `reason`: the registered phrase for a
/// code, and the scheduler's own sentence otherwise.
pub fn dispatch_wait_phrase(reason: &str) -> String {
    match dispatch_wait_code_label(reason) {
        Some(label) => label.to_owned(),
        None if is_unlabelled_wait_code(reason) => format!(
            "{} (unrecognised wait code `{reason}`)",
            dispatch_wait_code_label(DISPATCH_WAIT_REASON_UNCLASSIFIED).unwrap_or_default()
        ),
        None => reason.to_owned(),
    }
}

/// User-visible interactive-cap hold text. Lease-pending slots are
/// never called "working".
pub fn interactive_cap_wait_reason(working: usize, lease_pending: usize) -> String {
    if lease_pending == 0 {
        format!("Waiting for an available worker ({working} working)")
    } else {
        format!("Waiting for an available worker ({working} working, {lease_pending} waiting for a workspace)")
    }
}

/// Whether a persisted hold is due to capacity or an operator scheduling pause.
pub fn is_capacity_wait_reason(reason: &str) -> bool {
    matches!(
        reason,
        WAITING_FOR_WORKSPACE_REASON
            | DISPATCH_WAIT_REASON_POOL_EXHAUSTED
            | DISPATCH_WAIT_REASON_DISPATCH_PAUSED
            | DISPATCH_WAIT_REASON_AUTOMATION_PAUSED
            | DISPATCH_WAIT_REASON_FOCUS_MODE
    ) || reason.starts_with("Waiting for an available worker (")
}

/// A waiting item the engine has not explained: either it could not be
/// classified at all, or it is a `ready` row no scheduler pass has claimed or
/// held yet. The scheduler heartbeat reports one that stays unexplained.
#[derive(Debug, Clone)]
pub struct UnexplainedWait {
    pub work_item_id: String,
    /// The pending execution, or `None` when the item is queued on the board
    /// with no execution at all.
    pub execution: Option<WorkExecution>,
    /// [`DISPATCH_WAIT_REASON_UNCLASSIFIED`] or
    /// [`DISPATCH_WAIT_REASON_AWAITING_DISPATCH`].
    pub reason: String,
    /// The owning task's status, when the work item is a task.
    pub task_status: Option<String>,
}

impl UnexplainedWait {
    /// Stable identity of this wait across heartbeats: the execution when
    /// there is one, the work item otherwise.
    pub fn key(&self) -> String {
        match &self.execution {
            Some(execution) => execution.id.clone(),
            None => format!("item:{}", self.work_item_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_recognizes_persisted_text_and_not_event_codes() {
        for reason in [
            WAITING_FOR_WORKSPACE_REASON.to_owned(),
            interactive_cap_wait_reason(8, 0),
            interactive_cap_wait_reason(6, 2),
            DISPATCH_WAIT_REASON_POOL_EXHAUSTED.to_owned(),
            DISPATCH_WAIT_REASON_DISPATCH_PAUSED.to_owned(),
            DISPATCH_WAIT_REASON_AUTOMATION_PAUSED.to_owned(),
            DISPATCH_WAIT_REASON_FOCUS_MODE.to_owned(),
        ] {
            assert!(is_capacity_wait_reason(&reason), "{reason}");
        }
        for reason in [
            "interactive_concurrency_cap",
            "slot_busy",
            "waiting_dependency",
            DISPATCH_WAIT_REASON_PROGRESSION_UNAVAILABLE,
            DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION,
        ] {
            assert!(!is_capacity_wait_reason(reason), "{reason}");
        }
    }

    /// Words that say an item is waiting without saying why. A phrase built
    /// only from these would be the "reason unknown" line under another name.
    const VAGUE: &[&str] = &["unknown", "waiting", "pending", "queued", "held", "blocked"];

    #[test]
    fn every_wait_code_has_one_specific_phrase() {
        let mut seen = std::collections::HashSet::new();
        for (code, label) in DISPATCH_WAIT_REASON_CODES {
            assert!(seen.insert(*code), "wait code {code} is registered twice");
            assert_eq!(dispatch_wait_phrase(code), *label);
            assert!(!label.to_lowercase().contains("unknown"), "{code}: {label}");
            assert!(
                !VAGUE.contains(&label.to_lowercase().trim_end_matches('.')),
                "{code} is labelled with the bare word `{label}`, which names no reason"
            );
            assert!(
                *code == DISPATCH_WAIT_REASON_PARKED || label.split_whitespace().count() >= 2,
                "{code}: `{label}` is too short to name a reason"
            );
        }
    }

    #[test]
    fn scheduler_sentences_pass_through_and_unnamed_codes_do_not() {
        let sentence = interactive_cap_wait_reason(3, 0);
        assert_eq!(dispatch_wait_phrase(&sentence), sentence);
        assert_eq!(
            dispatch_wait_phrase(WAITING_FOR_WORKSPACE_REASON),
            WAITING_FOR_WORKSPACE_REASON
        );
        assert!(!is_unlabelled_wait_code(&sentence));

        // A hold recorded under a code nobody gave a phrase is not rendered
        // as that identifier: it reads as the unclassified anomaly it is.
        assert!(is_unlabelled_wait_code("some_new_hold"));
        let phrase = dispatch_wait_phrase("some_new_hold");
        assert!(
            phrase.starts_with(dispatch_wait_code_label(DISPATCH_WAIT_REASON_UNCLASSIFIED).unwrap()),
            "{phrase}"
        );
        assert!(phrase.contains("some_new_hold"), "{phrase}");
        assert!(!is_unlabelled_wait_code(DISPATCH_WAIT_REASON_FOCUS_MODE));
    }
}
