//! Which labelled section of the Review column a card is grouped under.
//!
//! The Review column always shows every card, grouped into sections. The
//! grouping is engine state — it is derived from facts only the engine has
//! resolved (`ready_for_review`, `ai_review_state`, the in-progress revision
//! flags, the polled CI and mergeability facts) — so it is computed here and
//! shipped on `Task::review_section`. The macOS client renders the key it is
//! given and never re-derives it.

use boss_engine_board_gesture::BoardRow;
use boss_protocol::{BoardColumn, ReviewSection, Task, TaskStatus};

use super::{AI_REVIEW_STATE_REVIEW_QUEUED, AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS, AI_REVIEW_STATE_REVIEWING};

/// Resolve the Review-column section of `task`, or `None` when the row is not
/// a Review-lane card (its board position is some other column, or it sits in
/// Done's "Merging" group).
///
/// Each card lands in exactly one section — the first rule that matches, in
/// this order:
///
/// 1. **Blocked** — needs a human or a fix before it can progress: the work
///    item is blocked (`status = blocked` or a `blocked_reason`), a required
///    CI check failed, GitHub reports a merge conflict, or every open
///    revision is itself blocked (nothing is running).
/// 2. **In Revision** — a revision is running or queued on the PR
///    (`has_in_progress_revision`, the orange "in revision" badge).
/// 3. **AI Review** — an AI review is queued or running, or it reported
///    findings that no revision has started on yet (a started revision would
///    have matched rule 2).
/// 4. **Ready** — `ready_for_review`, the same fact the old Ready filter read.
/// 5. **Pending** — everything else: an open PR whose required CI is still
///    running or not yet polled, whose mergeability is unknown, or that has
///    no PR bound yet. Nothing is wrong and nobody needs to act; the card is
///    just not ready. It gets its own section rather than being forced into
///    Blocked (which would claim a problem that does not exist) or Ready
///    (which would claim it can be merged now).
///
/// Blocked outranks In Revision because a revision does not make a failing
/// check or a conflict any less blocking: until the revision lands and the
/// facts clear, the operator cannot merge, and the failure is the more
/// important thing to see. In Revision outranks AI Review because a card with
/// a revision running is not waiting on the reviewer, and AI Review outranks
/// Ready because a clean-CI PR with a review still in flight is not yet safe
/// to merge — `ready_for_review` deliberately knows nothing about the AI
/// review, so the ordering here is what keeps such a card out of Ready.
///
/// Reads `ready_for_review`, `has_in_progress_revision`,
/// `in_revision_blocked_only` and `ai_review_state`, so it must run after
/// every one of them has been attached.
pub(crate) fn classify_review_section(task: &Task) -> Option<ReviewSection> {
    let position = BoardRow {
        status: task.status.clone(),
        autostart: task.autostart,
        blocked_reason: task.blocked_reason.clone(),
        merge_queue_state: task.merge_queue_state.clone(),
    }
    .position();
    if position.column != BoardColumn::Review || position.group.is_some() {
        return None;
    }

    let blocked = task.status == TaskStatus::Blocked
        || task.blocked_reason.is_some()
        || task.ci_required_state.as_deref() == Some("fail")
        || task.pr_mergeable_state.as_deref() == Some("conflicting")
        || (task.has_in_progress_revision && task.in_revision_blocked_only);
    if blocked {
        return Some(ReviewSection::Blocked);
    }
    if task.has_in_progress_revision {
        return Some(ReviewSection::InRevision);
    }
    if matches!(
        task.ai_review_state.as_deref(),
        Some(AI_REVIEW_STATE_REVIEWING | AI_REVIEW_STATE_REVIEW_QUEUED | AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS)
    ) {
        return Some(ReviewSection::AiReview);
    }
    if task.ready_for_review {
        return Some(ReviewSection::Ready);
    }
    Some(ReviewSection::Pending)
}

/// Set `review_section` on every task and chore. Called by `get_work_tree`
/// and by the single-row projection path after the attaches it reads.
pub(crate) fn attach_review_section(tasks: &mut [Task], chores: &mut [Task]) {
    for task in tasks.iter_mut().chain(chores.iter_mut()) {
        task.review_section = classify_review_section(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::TaskKind;

    /// A Review-lane card that is ready by every measure: open PR, CI green,
    /// mergeable, nothing running. Each test perturbs one fact from here.
    fn ready_card() -> Task {
        let mut t = Task::builder()
            .id("t1")
            .product_id("prod_1")
            .kind(TaskKind::Chore)
            .name("n")
            .description("")
            .status(TaskStatus::InReview)
            .created_at("")
            .updated_at("")
            .pr_url("https://github.com/org/repo/pull/1")
            .build();
        t.ci_required_state = Some("success".to_owned());
        t.pr_mergeable_state = Some("mergeable".to_owned());
        t.ready_for_review = true;
        t
    }

    fn with_ai_state(mut t: Task, state: &str) -> Task {
        t.ai_review_state = Some(state.to_owned());
        t
    }

    fn in_revision(mut t: Task) -> Task {
        t.has_in_progress_revision = true;
        t.ready_for_review = false;
        t
    }

    #[test]
    fn ready_card_is_ready() {
        assert_eq!(classify_review_section(&ready_card()), Some(ReviewSection::Ready));
    }

    #[test]
    fn ready_card_with_clean_ai_review_stays_ready() {
        for state in ["reviewed_all_clear", "review_not_required", "not_reviewed"] {
            assert_eq!(
                classify_review_section(&with_ai_state(ready_card(), state)),
                Some(ReviewSection::Ready),
                "ai_review_state={state}"
            );
        }
    }

    #[test]
    fn in_progress_revision_is_in_revision() {
        assert_eq!(
            classify_review_section(&in_revision(ready_card())),
            Some(ReviewSection::InRevision)
        );
    }

    #[test]
    fn ai_review_queued_running_or_findings_is_ai_review() {
        for state in ["reviewing", "review_queued", "reviewed_with_findings"] {
            assert_eq!(
                classify_review_section(&with_ai_state(ready_card(), state)),
                Some(ReviewSection::AiReview),
                "ai_review_state={state}"
            );
        }
    }

    #[test]
    fn failing_ci_is_blocked() {
        let mut t = ready_card();
        t.ci_required_state = Some("fail".to_owned());
        t.ready_for_review = false;
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn merge_conflict_is_blocked_even_before_reconciliation_catches_up() {
        // The poller already reads CONFLICTING but status/blocked_reason have
        // not been reconciled yet: the raw fact alone must block the card.
        let mut t = ready_card();
        t.pr_mergeable_state = Some("conflicting".to_owned());
        t.ready_for_review = false;
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn blocked_status_with_review_phase_reason_is_blocked() {
        for reason in [
            "merge_conflict",
            "ci_failure",
            "ci_failure_exhausted",
            "review_feedback",
        ] {
            let mut t = ready_card();
            t.status = TaskStatus::Blocked;
            t.blocked_reason = Some(reason.to_owned());
            t.ready_for_review = false;
            assert_eq!(
                classify_review_section(&t),
                Some(ReviewSection::Blocked),
                "blocked_reason={reason}"
            );
        }
    }

    #[test]
    fn only_blocked_revisions_is_blocked_not_in_revision() {
        let mut t = in_revision(ready_card());
        t.in_revision_blocked_only = true;
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn ci_running_unknown_or_unpolled_is_pending() {
        for ci_state in [Some("in_progress"), Some("unknown"), None] {
            let mut t = ready_card();
            t.ci_required_state = ci_state.map(str::to_owned);
            t.ready_for_review = false;
            assert_eq!(
                classify_review_section(&t),
                Some(ReviewSection::Pending),
                "ci_required_state={ci_state:?}"
            );
        }
    }

    #[test]
    fn unknown_mergeability_is_pending() {
        let mut t = ready_card();
        t.pr_mergeable_state = Some("unknown".to_owned());
        t.ready_for_review = false;
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Pending));
    }

    #[test]
    fn clean_review_waiting_on_ci_is_pending() {
        let mut t = with_ai_state(ready_card(), "reviewed_clean_pending");
        t.ci_required_state = Some("in_progress".to_owned());
        t.ready_for_review = false;
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Pending));
    }

    // ── precedence between overlapping states ───────────────────────────────

    #[test]
    fn failing_ci_outranks_in_revision() {
        let mut t = in_revision(ready_card());
        t.ci_required_state = Some("fail".to_owned());
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn merge_conflict_outranks_in_revision() {
        let mut t = in_revision(ready_card());
        t.pr_mergeable_state = Some("conflicting".to_owned());
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn failing_ci_outranks_ai_review() {
        let mut t = with_ai_state(ready_card(), "reviewing");
        t.ci_required_state = Some("fail".to_owned());
        assert_eq!(classify_review_section(&t), Some(ReviewSection::Blocked));
    }

    #[test]
    fn in_revision_outranks_ai_review() {
        let t = with_ai_state(in_revision(ready_card()), "reviewed_with_findings");
        assert_eq!(classify_review_section(&t), Some(ReviewSection::InRevision));
    }

    #[test]
    fn ai_review_outranks_ready() {
        // `ready_for_review` is true here: it knows nothing about the AI
        // review, so a review still in flight must pull the card out of Ready.
        let t = with_ai_state(ready_card(), "reviewing");
        assert!(t.ready_for_review);
        assert_eq!(classify_review_section(&t), Some(ReviewSection::AiReview));
    }

    // ── cards that are not in the Review column ─────────────────────────────

    #[test]
    fn non_review_rows_have_no_section() {
        let mut active = ready_card();
        active.status = TaskStatus::Active;
        assert_eq!(classify_review_section(&active), None);

        let mut done = ready_card();
        done.status = TaskStatus::Done;
        assert_eq!(classify_review_section(&done), None);

        let mut dependency_blocked = ready_card();
        dependency_blocked.status = TaskStatus::Blocked;
        dependency_blocked.blocked_reason = Some("dependency".to_owned());
        assert_eq!(classify_review_section(&dependency_blocked), None);
    }

    #[test]
    fn merging_row_belongs_to_done_not_review() {
        let mut t = ready_card();
        t.merge_queue_state = Some("queued".to_owned());
        assert_eq!(classify_review_section(&t), None);
    }

    #[test]
    fn attach_sets_section_on_tasks_and_chores_alike() {
        let mut tasks = vec![ready_card()];
        let mut blocked = ready_card();
        blocked.id = "t2".to_owned();
        blocked.ci_required_state = Some("fail".to_owned());
        let mut chores = vec![blocked];
        attach_review_section(&mut tasks, &mut chores);
        assert_eq!(tasks[0].review_section, Some(ReviewSection::Ready));
        assert_eq!(chores[0].review_section, Some(ReviewSection::Blocked));
    }
}
