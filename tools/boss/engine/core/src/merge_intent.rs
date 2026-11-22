//! What a standing merge request means once its item can be merged again.
//!
//! A merge request outlives the attempt it started (see
//! `boss_work_db::merge_intent` for what records and ends one, and
//! `tools/boss/docs/merge-intent.md` for the lifecycle). When that attempt
//! has ended, whatever ended it has been fixed, and the card is back in
//! Review's Ready section, [`settle_after_clean_probe`] decides what the
//! request is now worth.
//!
//! Today that decision has one outcome: the card stays in Review and the
//! request ends with a sentence saying what changed since the approved head.
//! [`Outcome`] has no variant that merges, so nothing here can re-request a
//! merge. A later change adds one behind a judgement of the diff between
//! the two heads; a judgement that is missing, fails or is unsure must keep
//! producing [`Outcome::Return`].

use boss_execution_contracts::ExecutionPublisher;
use boss_protocol::{
    MERGE_INTENT_LAPSED, MERGE_INTENT_NEEDS_REVIEW, MERGE_INTENT_REASON_ATTEMPT_ENDED,
    MERGE_INTENT_REASON_CHANGED_OUTSIDE_REMEDIATION, MERGE_INTENT_REASON_NO_DECISION, MergeIntent, ReviewSection,
    WorkItem,
};

use crate::work::{MergeIntentEnd, MergeIntentRemediation, PendingMergeCheck, WorkDb};

/// What to do with a standing request whose item is mergeable again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// A fix the engine owes has not started yet. Look again next probe.
    Wait,
    /// The request ends and the card stays in Review with this explanation.
    Return(MergeIntentEnd),
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// How long the engine waits for a CI-fix attempt to be minted after a
/// failed-checks dequeue before treating the obligation as abandoned.
/// Rebounce is expected on the same sweep as the dequeue; ten minutes
/// covers a deferred mint while a rebase or conflict resolution finishes.
pub(crate) const FIX_OWED_GIVE_UP_AFTER_SECS: i64 = 10 * 60;

/// Decide from what the engine holds: the approved head, the head it last
/// probed, and the fixes it started in between.
fn decide(intent: &MergeIntent, current_head: Option<&str>, remediation: MergeIntentRemediation) -> Outcome {
    if remediation.fix_owed {
        // Age of the unfulfilled obligation alone: an earlier, unrelated CI
        // attempt does not satisfy it, and one for this obligation would have
        // cleared `fix_owed`.
        let stale = remediation
            .fix_owed_age_secs
            .is_some_and(|age| age >= FIX_OWED_GIVE_UP_AFTER_SECS);
        if stale {
            return Outcome::Return(MergeIntentEnd {
                status: MERGE_INTENT_NEEDS_REVIEW,
                reason: MERGE_INTENT_REASON_NO_DECISION,
                detail: Some(
                    "Review before merging again: the merge left the queue on failed checks and no CI fix was started."
                        .to_owned(),
                ),
                decided_head_sha: current_head.map(str::to_owned),
            });
        }
        return Outcome::Wait;
    }
    let (Some(approved), Some(current)) = (intent.approved_head_sha.as_deref(), current_head) else {
        // Without both heads nothing can be shown to be unchanged.
        return Outcome::Return(MergeIntentEnd {
            status: MERGE_INTENT_NEEDS_REVIEW,
            reason: MERGE_INTENT_REASON_NO_DECISION,
            detail: Some(
                "Review before merging again: the merge ended and the PR could not be confirmed unchanged.".to_owned(),
            ),
            decided_head_sha: current_head.map(str::to_owned),
        });
    };
    if approved == current {
        return Outcome::Return(MergeIntentEnd {
            status: MERGE_INTENT_LAPSED,
            reason: MERGE_INTENT_REASON_ATTEMPT_ENDED,
            detail: Some(format!(
                "Merge again to retry: the merge at {} did not complete and the PR is unchanged.",
                short(approved)
            )),
            decided_head_sha: Some(current.to_owned()),
        });
    }
    let (reason, what) = match (remediation.ci_fixes > 0, remediation.conflict_resolutions > 0) {
        (true, true) => (
            MERGE_INTENT_REASON_NO_DECISION,
            "automatic CI and conflict fixes changed the PR",
        ),
        (true, false) => (MERGE_INTENT_REASON_NO_DECISION, "an automatic CI fix changed the PR"),
        (false, true) => (
            MERGE_INTENT_REASON_NO_DECISION,
            "an automatic conflict fix changed the PR",
        ),
        (false, false) => (
            MERGE_INTENT_REASON_CHANGED_OUTSIDE_REMEDIATION,
            "the PR changed without an automatic fix",
        ),
    };
    // What to do and why come first: a card shows about two lines of this.
    Outcome::Return(MergeIntentEnd {
        status: MERGE_INTENT_NEEDS_REVIEW,
        reason,
        detail: Some(format!(
            "Review before merging again: {what} ({} → {}).",
            short(approved),
            short(current)
        )),
        decided_head_sha: Some(current.to_owned()),
    })
}

/// Settle the standing merge request for `candidate`, if it has one and its
/// card is back in Review's Ready section with no merge attempt and no fix
/// in flight. Called on a definitive clean probe of an item that is not
/// parked. Ready is the engine's own definition of "could be merged right
/// now" (`classify_review_section`): in review, not blocked, no revision
/// queued or running, no AI review pending or missing a current-head
/// verdict, required CI green, mergeable.
pub async fn settle_after_clean_probe(db: &WorkDb, publisher: &dyn ExecutionPublisher, candidate: &PendingMergeCheck) {
    match settle(db, candidate) {
        Ok(Some(end)) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                status = end.status,
                reason = end.reason,
                decided_head_sha = ?end.decided_head_sha,
                "merge intent: item is mergeable again; request ended and card left in review",
            );
            publisher
                .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "merge_intent_settled")
                .await;
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(?err, work_item_id = %candidate.work_item_id, "merge intent: could not settle the standing merge request")
        }
    }
}

fn settle(db: &WorkDb, candidate: &PendingMergeCheck) -> anyhow::Result<Option<MergeIntentEnd>> {
    let Some(intent) = db.active_merge_intent(&candidate.work_item_id, &candidate.pr_url)? else {
        return Ok(None);
    };
    if db.is_parked_or_assume_not(&candidate.work_item_id, "settle merge intent") {
        return Ok(None);
    }
    let (WorkItem::Task(task) | WorkItem::Chore(task)) = db.get_work_item(&candidate.work_item_id)? else {
        return Ok(None);
    };
    if task.review_section != Some(ReviewSection::Ready) {
        return Ok(None);
    }
    // A submission that is still live owns what happens next: GitHub's
    // queue or armed auto-merge, or Trunk's resubmit after an eviction fix.
    if db
        .get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url)?
        .is_some()
        || db.get_active_trunk_merge_intent(&candidate.work_item_id)?.is_some()
    {
        return Ok(None);
    }
    if db
        .active_ci_remediation_for_work_item(&candidate.work_item_id)?
        .is_some()
        || db
            .active_conflict_resolution_for_work_item(&candidate.work_item_id)?
            .is_some()
    {
        return Ok(None);
    }
    let current_head = db
        .get_pr_status_snapshot(&candidate.work_item_id)?
        .and_then(|snapshot| snapshot.head_sha)
        .filter(|sha| !sha.is_empty());
    match decide(&intent, current_head.as_deref(), db.merge_intent_remediation(&intent)?) {
        Outcome::Wait => Ok(None),
        Outcome::Return(end) => Ok(db.end_merge_intent(&intent.id, &end)?.then_some(end)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{
        CiRemediationInsertInput, CreateExecutionInput, ExecutionStatus, GithubMergeIntentInsertInput,
        PrPollStateInput, REVIEW_GATE_OUTCOME_COMPLETED_CLEAN, ReviewVerdictInput, WorkItemPatch,
    };
    use boss_execution_contracts::NoopExecutionPublisher;
    use boss_protocol::{ExecutionKind, MERGE_INTENT_ACTIVE, MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE};

    const PR: &str = "https://github.com/example/repo/pull/42";
    const APPROVED: &str = "aaaaaaa1111111111111111111111111111111aa";
    const FIXED: &str = "bbbbbbb2222222222222222222222222222222bb";

    fn intent(approved: Option<&str>) -> MergeIntent {
        MergeIntent::builder()
            .id("mi_1")
            .work_item_id("task_1")
            .pr_url(PR)
            .status(MERGE_INTENT_ACTIVE)
            .maybe_approved_head_sha(approved)
            .requested_at("100")
            .build()
    }

    fn fixes(ci_fixes: usize, conflict_resolutions: usize) -> MergeIntentRemediation {
        MergeIntentRemediation {
            fix_owed: false,
            fix_owed_age_secs: None,
            ci_fixes,
            conflict_resolutions,
        }
    }

    fn returned(outcome: Outcome) -> MergeIntentEnd {
        match outcome {
            Outcome::Return(end) => end,
            Outcome::Wait => panic!("expected the request to end"),
        }
    }

    #[test]
    fn an_owed_fix_is_waited_for_whatever_the_heads_say() {
        let owed = MergeIntentRemediation {
            fix_owed: true,
            ..Default::default()
        };
        assert_eq!(decide(&intent(Some(APPROVED)), Some(APPROVED), owed), Outcome::Wait);
        assert_eq!(decide(&intent(Some(APPROVED)), Some(FIXED), owed), Outcome::Wait);
        let still_fresh = MergeIntentRemediation {
            fix_owed: true,
            fix_owed_age_secs: Some(FIX_OWED_GIVE_UP_AFTER_SECS - 1),
            ..Default::default()
        };
        assert_eq!(
            decide(&intent(Some(APPROVED)), Some(APPROVED), still_fresh),
            Outcome::Wait
        );
    }

    #[test]
    fn an_owed_fix_that_is_never_started_returns_to_review() {
        let stale = MergeIntentRemediation {
            fix_owed: true,
            fix_owed_age_secs: Some(FIX_OWED_GIVE_UP_AFTER_SECS),
            ..Default::default()
        };
        let end = returned(decide(&intent(Some(APPROVED)), Some(APPROVED), stale));
        assert_eq!(end.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(end.reason, MERGE_INTENT_REASON_NO_DECISION);
        assert!(end.detail.unwrap().contains("no CI fix was started"));
        let earlier_attempt = MergeIntentRemediation {
            fix_owed: true,
            fix_owed_age_secs: Some(FIX_OWED_GIVE_UP_AFTER_SECS),
            ci_fixes: 1,
            conflict_resolutions: 0,
        };
        let end = returned(decide(&intent(Some(APPROVED)), Some(APPROVED), earlier_attempt));
        assert_eq!(
            end.status, MERGE_INTENT_NEEDS_REVIEW,
            "an earlier attempt does not satisfy the obligation"
        );
    }

    #[test]
    fn a_pr_changed_by_automatic_fixes_needs_review_and_says_which_fix() {
        for (remediation, what) in [
            (fixes(1, 0), "an automatic CI fix changed the PR"),
            (fixes(0, 2), "an automatic conflict fix changed the PR"),
            (fixes(1, 1), "automatic CI and conflict fixes changed the PR"),
        ] {
            let end = returned(decide(&intent(Some(APPROVED)), Some(FIXED), remediation));
            assert_eq!(end.status, MERGE_INTENT_NEEDS_REVIEW);
            assert_eq!(end.reason, MERGE_INTENT_REASON_NO_DECISION);
            assert_eq!(end.decided_head_sha.as_deref(), Some(FIXED));
            let detail = end.detail.unwrap();
            assert!(detail.contains(what), "{detail}");
            assert!(detail.starts_with("Review before merging again: "), "{detail}");
            assert!(detail.contains("(aaaaaaa → bbbbbbb)"), "{detail}");
            assert!(!detail.contains(APPROVED), "heads are abbreviated: {detail}");
        }
    }

    #[test]
    fn a_pr_changed_with_no_fix_to_account_for_it_needs_review() {
        let end = returned(decide(&intent(Some(APPROVED)), Some(FIXED), fixes(0, 0)));
        assert_eq!(end.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(end.reason, MERGE_INTENT_REASON_CHANGED_OUTSIDE_REMEDIATION);
    }

    #[test]
    fn an_unchanged_pr_lapses_rather_than_claiming_a_review_is_needed() {
        for remediation in [fixes(0, 0), fixes(1, 0)] {
            let end = returned(decide(&intent(Some(APPROVED)), Some(APPROVED), remediation));
            assert_eq!(end.status, MERGE_INTENT_LAPSED);
            assert_eq!(end.reason, MERGE_INTENT_REASON_ATTEMPT_ENDED);
        }
    }

    #[test]
    fn a_head_that_cannot_be_compared_is_never_treated_as_unchanged() {
        for (approved, current) in [(None, Some(FIXED)), (Some(APPROVED), None), (None, None)] {
            let end = returned(decide(&intent(approved), current, fixes(1, 0)));
            assert_eq!(end.status, MERGE_INTENT_NEEDS_REVIEW);
            assert_eq!(end.reason, MERGE_INTENT_REASON_NO_DECISION);
        }
    }

    struct Fixture {
        db: WorkDb,
        candidate: PendingMergeCheck,
    }

    /// A chore in review on [`PR`], probed green and mergeable at `head`.
    fn fixture(head: &str) -> Fixture {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Merge intent");
        let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Ship it");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let fixture = Fixture {
            db,
            candidate: PendingMergeCheck {
                work_item_id: task.id,
                product_id: product.id,
                pr_url: PR.into(),
            },
        };
        fixture.probe(head, "success");
        fixture
    }

    impl Fixture {
        fn probe(&self, head: &str, ci: &'static str) {
            self.db
                .update_task_pr_poll_state(
                    &self.candidate.work_item_id,
                    PrPollStateInput {
                        caller: "merge_intent_test",
                        ci_required_state: ci,
                        review_required_state: "approved",
                        pr_mergeable_state: "mergeable",
                        pr_head_sha: Some(head),
                        ..Default::default()
                    },
                )
                .unwrap();
            // Ready requires a verdict that matches the current head. A green
            // probe without one lands in AI Review, and settle is a no-op.
            if ci == "success" {
                self.mark_head_reviewed_clean(head);
            }
        }

        fn mark_head_reviewed_clean(&self, head_sha: &str) {
            let execution = self
                .db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(&self.candidate.work_item_id)
                        .kind(ExecutionKind::PrReview)
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            let conn = self.db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
            WorkDb::insert_review_verdict_in_tx(
                &conn,
                &execution.id,
                &self.candidate.work_item_id,
                &ReviewVerdictInput {
                    head_sha: Some(head_sha.to_owned()),
                    findings_count: 0,
                    revision_warranted: false,
                    gate_outcome: REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
                },
            )
            .unwrap();
        }

        async fn settle(&self) {
            settle_after_clean_probe(&self.db, &NoopExecutionPublisher, &self.candidate).await;
        }

        fn standing(&self) -> Option<MergeIntent> {
            self.db.active_merge_intent(&self.candidate.work_item_id, PR).unwrap()
        }

        fn on_card(&self) -> Option<MergeIntent> {
            match self.db.get_work_item(&self.candidate.work_item_id).unwrap() {
                WorkItem::Task(task) | WorkItem::Chore(task) => task.merge_intent,
                other => panic!("expected a chore, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_late_request_on_a_parked_row_is_not_settled_by_a_clean_probe() {
        let f = fixture(APPROVED);
        f.db.park_work_item_as_actor(&f.candidate.work_item_id, true, Some("pause merge"), "human")
            .unwrap();
        // Parking ends existing requests; model a request recorded by a
        // concurrent writer after the park, before the poller's confirmation.
        f.db.record_merge_intent(&f.candidate.work_item_id, PR, Some(APPROVED))
            .unwrap();
        assert!(f.standing().is_some());
        f.settle().await;
        assert!(f.standing().is_some());
        f.db.park_work_item_as_actor(&f.candidate.work_item_id, false, None, "human")
            .unwrap();
        f.probe(APPROVED, "success");
        f.settle().await;
        assert!(f.standing().is_none());
    }

    #[tokio::test]
    async fn readiness_alone_never_creates_or_ends_a_request() {
        let f = fixture(APPROVED);
        f.settle().await;
        assert!(f.on_card().is_none());
    }

    #[tokio::test]
    async fn a_live_submission_keeps_the_request_standing() {
        let f = fixture(APPROVED);
        f.db.record_merge_intent(&f.candidate.work_item_id, PR, Some(APPROVED))
            .unwrap();
        // GitHub accepted the request: the card is in Merging, not Ready.
        f.db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(&f.candidate.work_item_id)
                .pr_url(PR)
                .head_sha(APPROVED)
                .build(),
            "{}",
        )
        .unwrap();
        f.settle().await;
        assert!(f.standing().is_some());
        // A lane demotion alone (armed auto-merge under red CI clears the
        // lane but not the submission) is still a live submission.
        f.db.set_task_merge_queue_state(&f.candidate.work_item_id, None, None)
            .unwrap();
        f.settle().await;
        assert!(f.standing().is_some());
    }

    #[tokio::test]
    async fn a_request_waits_through_the_fix_and_ends_in_review_once_it_lands() {
        let f = fixture(APPROVED);
        let id = &f.candidate.work_item_id;
        let requested = f.db.record_merge_intent(id, PR, Some(APPROVED)).unwrap();
        // The merge queue rejected the PR on failed checks.
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2",
                [],
            )
            .unwrap();
        f.db.note_merge_intent_left_queue(id, PR, "failed_checks", boss_engine_utils::epoch_time::now_epoch_secs())
            .unwrap();
        f.settle().await;
        assert!(f.standing().is_some(), "the CI fix that is owed has not started");
        // CI remediation starts as `ci_watch` starts it: the attempt is
        // minted, the card is blocked, then unblocked for its revision.
        let attempt =
            f.db.insert_ci_remediation(
                CiRemediationInsertInput::builder()
                    .product_id(&f.candidate.product_id)
                    .work_item_id(id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .head_sha_at_trigger(format!("mq:{APPROVED}"))
                    .attempt_kind("fix")
                    .consumes_budget(1)
                    .failed_checks("[]")
                    .failure_kind("merge_queue_rebounce")
                    .before_commit_sha("queue-commit")
                    .build(),
            )
            .unwrap()
            .unwrap();
        f.db.mark_chore_blocked_ci_failure(id, PR, Some(&attempt.id)).unwrap();
        f.settle().await;
        assert!(f.standing().is_some(), "blocked on the CI failure being fixed");
        f.db.clear_chore_blocked_ci_failure(id, PR).unwrap();
        f.settle().await;
        assert!(f.standing().is_some(), "the fix attempt is still running");
        // The fix lands on a new head, which retires the queue-side attempt
        // the way the poller's reaper does, but required CI has not finished
        // on the new head yet.
        f.db.mark_ci_remediation_abandoned(&attempt.id, "merge_queue_trigger_superseded")
            .unwrap();
        f.probe(FIXED, "in_progress");
        f.settle().await;
        assert!(f.standing().is_some(), "CI is still running on the fixed head");
        f.probe(FIXED, "success");
        f.settle().await;
        assert!(f.standing().is_none());
        let shown = f.on_card().unwrap();
        assert_eq!(shown.id, requested.id);
        assert_eq!(shown.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(shown.reason.as_deref(), Some(MERGE_INTENT_REASON_NO_DECISION));
        assert_eq!(shown.approved_head_sha.as_deref(), Some(APPROVED));
        assert_eq!(shown.decided_head_sha.as_deref(), Some(FIXED));
        assert!(shown.detail.unwrap().contains("an automatic CI fix changed the PR"));
        // Settling again changes nothing.
        f.settle().await;
        assert_eq!(f.on_card().unwrap().status, MERGE_INTENT_NEEDS_REVIEW);
    }

    #[tokio::test]
    async fn a_request_removed_from_the_queue_by_hand_is_not_settled_again() {
        let f = fixture(APPROVED);
        let id = &f.candidate.work_item_id;
        f.db.record_merge_intent(id, PR, Some(APPROVED)).unwrap();
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2",
                [],
            )
            .unwrap();
        f.db.note_merge_intent_left_queue(
            id,
            PR,
            "manual_removal",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        f.settle().await;
        let shown = f.on_card().unwrap();
        assert_eq!(shown.status, MERGE_INTENT_LAPSED);
        assert_eq!(
            shown.reason.as_deref(),
            Some(MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE)
        );
    }

    #[tokio::test]
    async fn a_stale_owed_fix_with_no_attempt_ends_in_review() {
        let f = fixture(APPROVED);
        let id = &f.candidate.work_item_id;
        f.db.record_merge_intent(id, PR, Some(APPROVED)).unwrap();
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2",
                [],
            )
            .unwrap();
        f.db.note_merge_intent_left_queue(id, PR, "failed_checks", boss_engine_utils::epoch_time::now_epoch_secs())
            .unwrap();
        assert!(f.standing().is_some());
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents
                 SET fix_owed_since = CAST(CAST(strftime('%s','now') AS INTEGER) - 700 AS TEXT)
                 WHERE work_item_id = ?1 AND status = 'active'",
                [id],
            )
            .unwrap();
        f.settle().await;
        assert!(f.standing().is_none());
        let shown = f.on_card().unwrap();
        assert_eq!(shown.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(shown.reason.as_deref(), Some(MERGE_INTENT_REASON_NO_DECISION));
        assert!(shown.detail.unwrap().contains("no CI fix was started"));
    }

    #[tokio::test]
    async fn a_declined_ci_fix_ends_rather_than_waiting() {
        let f = fixture(APPROVED);
        let id = &f.candidate.work_item_id;
        f.db.set_ci_attempt_budget(id, Some(0)).unwrap();
        f.db.record_merge_intent(id, PR, Some(APPROVED)).unwrap();
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2",
                [],
            )
            .unwrap();
        f.db.note_merge_intent_left_queue(id, PR, "failed_checks", boss_engine_utils::epoch_time::now_epoch_secs())
            .unwrap();
        f.db.decline_owed_merge_intent_ci_fix(id, PR, crate::work::MergeIntentCiFixDecline::BudgetExhausted)
            .unwrap();
        f.settle().await;
        assert!(f.standing().is_none(), "nothing will mint a CI fix");
        assert_eq!(f.on_card().unwrap().status, MERGE_INTENT_NEEDS_REVIEW);
    }

    #[tokio::test]
    async fn a_stale_owed_fix_ends_even_after_an_earlier_ci_attempt_finished() {
        let f = fixture(APPROVED);
        let id = &f.candidate.work_item_id;
        f.db.record_merge_intent(id, PR, Some(APPROVED)).unwrap();
        // A PR-branch CI fix that ran and finished after the request, before the
        // failed-checks dequeue that owes a second one.
        let earlier =
            f.db.insert_ci_remediation(
                CiRemediationInsertInput::builder()
                    .product_id(&f.candidate.product_id)
                    .work_item_id(id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .head_sha_at_trigger("earlier-head")
                    .attempt_kind("fix")
                    .consumes_budget(1)
                    .failed_checks("[]")
                    .failure_kind("pr_branch_ci")
                    .build(),
            )
            .unwrap()
            .unwrap();
        f.db.mark_ci_remediation_abandoned(&earlier.id, "done").unwrap();
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2",
                [],
            )
            .unwrap();
        f.db.note_merge_intent_left_queue(id, PR, "failed_checks", boss_engine_utils::epoch_time::now_epoch_secs())
            .unwrap();
        // Order: request < earlier attempt < fix owed since < now - 600.
        let conn = f.db.connect().unwrap();
        conn.execute(
            "UPDATE merge_intents
             SET created_at = CAST(CAST(strftime('%s','now') AS INTEGER) - 2000 AS TEXT)
             WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )
        .unwrap();
        conn.execute(
            "UPDATE ci_remediations SET created_at = CAST(CAST(strftime('%s','now') AS INTEGER) - 1500 AS TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE merge_intents
             SET fix_owed_since = CAST(CAST(strftime('%s','now') AS INTEGER) - 700 AS TEXT)
             WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )
        .unwrap();
        drop(conn);
        let standing = f.standing().unwrap();
        let remediation = f.db.merge_intent_remediation(&standing).unwrap();
        assert_eq!(remediation.ci_fixes, 1, "the earlier attempt postdates the request");
        assert!(remediation.fix_owed, "no attempt was minted for the dequeue");
        f.settle().await;
        assert!(f.standing().is_none());
        assert_eq!(f.on_card().unwrap().status, MERGE_INTENT_NEEDS_REVIEW);
    }
}
