//! Same-PR revision dispatch and per-comment outcomes for review-guide
//! feedback. Design: `tools/boss/docs/designs/automatic-pr-review-guides.md`
//! §"Comments target the implementation".

use super::feedback_target::FeedbackTarget;
use super::revise_doc::{ClaimOutcome, append_comment_directive_body, claim_revisable_comments_in_tx};
use super::revision_helpers::assert_parent_revisable_and_insert;
use super::*;
use boss_protocol::{GuideCommentOutcome, THREAD_ENTRY_AUTHOR_ENGINE};

/// Result of recording a grounded per-comment guide outcome. Regeneration
/// is attempted after the outcome writes commit, so a retry failure does
/// not roll back the recorded disposition — and, because the outcome is
/// already durably recorded at that point, a regeneration failure is
/// reported as a warning inside this `Ok` result rather than surfacing as
/// an `Err` from [`WorkDb::record_guide_comment_outcome`]: the caller must
/// still treat the outcome as successfully recorded (publish the comment
/// invalidation, etc.) even when regeneration failed.
#[derive(Debug)]
pub struct RecordedGuideCommentOutcome {
    pub comment: WorkComment,
    /// `Some(Ok(_))` when regeneration was requested and queued
    /// successfully, `Some(Err(_))` when it was requested but the retry
    /// call itself failed (the error's `Display` text), `None` when
    /// regeneration was not requested at all.
    pub regeneration: Option<Result<RetryReviewGuideOutcome, String>>,
}

pub(crate) fn migrate_guide_feedback_outcomes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS guide_comment_outcomes (
             comment_id TEXT PRIMARY KEY,
             revise_task_id TEXT NOT NULL,
             disposition TEXT NOT NULL,
             response TEXT NOT NULL,
             request_regeneration INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS guide_comment_outcomes_by_task
             ON guide_comment_outcomes(revise_task_id);",
    )?;
    Ok(())
}

impl WorkDb {
    /// Same-PR revision for a `pr_review_guide` artifact. Claims comments and
    /// inserts the revision in one transaction so a losing concurrent claim
    /// creates no spare task. Closed/merged/missing PRs return `PrClosed`
    /// with no chore fallback.
    pub(crate) fn revise_guide_pr(
        &self,
        input: ReviseDocInput,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<ReviseDocOutcome> {
        let Some(FeedbackTarget::PullRequestImplementation {
            series_id,
            canonical_pr,
            chain_root_id,
            pr_lifecycle,
            pr_url,
            ..
        }) = self.resolve_feedback_target(&input.artifact_kind, &input.artifact_id)?
        else {
            return Ok(ReviseDocOutcome::NotApplicable {
                reason: format!(
                    "{}:{} is not a review-guide series bound to an implementation PR",
                    input.artifact_kind, input.artifact_id
                ),
            });
        };
        if pr_lifecycle != DocOwnerPrLifecycle::Open {
            return Ok(ReviseDocOutcome::PrClosed {
                reason: "This PR can no longer be revised.".to_owned(),
            });
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = comments::query_revisable_comments(
            &tx,
            &input.artifact_kind,
            &input.artifact_id,
            input.comment_ids.as_deref(),
        )?;
        if candidates.is_empty() {
            return Ok(ReviseDocOutcome::NoUnresolvedComments);
        }

        let directive = compose_guide_comment_directive(&tx, &canonical_pr, &series_id, &candidates);
        let name = format!(
            "Address {} reviewer comment{}",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" }
        );
        let created_via = format!("{CREATED_VIA_GUIDE_COMMENT_PREFIX}{series_id}");
        let revision_input = CreateRevisionInput::builder()
            .parent_task_id(chain_root_id)
            .description(directive)
            .name(name)
            .created_via(created_via)
            .force_duplicate(true)
            .build();

        let mut pending = PendingEvents::new();
        let task = match assert_parent_revisable_and_insert(&mut pending, &tx, revision_input, pr_checker) {
            Ok(task) => task,
            Err(err) if err.downcast_ref::<RevisionGateError>().is_some() => {
                return Ok(ReviseDocOutcome::PrClosed {
                    reason: "This PR can no longer be revised.".to_owned(),
                });
            }
            Err(err) => return Err(err),
        };

        match claim_revisable_comments_in_tx(&tx, &candidates, &task.id)? {
            ClaimOutcome::Claimed(addressed_comment_ids) => {
                let excluded_comment_ids =
                    comments::query_excluded_revisable_comment_ids(&tx, &input.artifact_kind, &input.artifact_id)?
                        .into_iter()
                        .filter(|id| !addressed_comment_ids.contains(id))
                        .collect::<Vec<_>>();
                let task_id = task.id.clone();
                commit_and_publish(tx, pending, self.event_bus())?;
                Ok(ReviseDocOutcome::Created {
                    task_id,
                    task_kind: "revision".to_owned(),
                    addressed_comment_ids,
                    excluded_comment_ids,
                    pr_url,
                })
            }
            ClaimOutcome::AlreadyInFlight(winner_task_id) => {
                drop(tx);
                Ok(ReviseDocOutcome::AlreadyInFlight {
                    task_id: winner_task_id,
                })
            }
            ClaimOutcome::NoneLeft => {
                drop(tx);
                Ok(ReviseDocOutcome::NoUnresolvedComments)
            }
        }
    }

    /// Record a grounded per-comment outcome for a guide-feedback revision.
    /// The comment must already be claimed by `revise_task_id`. If that task
    /// has already completed, the comment resolves immediately.
    ///
    /// Outcome row, thread entry, and optional immediate resolve run in one
    /// transaction. Re-recording the same comment replaces the existing
    /// engine answer thread entry instead of appending another.
    pub fn record_guide_comment_outcome(
        &self,
        revise_task_id: &str,
        outcome: GuideCommentOutcome,
    ) -> Result<RecordedGuideCommentOutcome> {
        let disposition = outcome.disposition.as_str();
        anyhow::ensure!(
            !outcome.response.trim().is_empty(),
            "guide comment outcome response may not be empty"
        );
        let comment = self
            .get_comment(&outcome.comment_id)?
            .with_context(|| format!("unknown comment: {}", outcome.comment_id))?;
        anyhow::ensure!(
            comment.artifact_kind == "pr_review_guide",
            "guide outcomes apply only to review-guide comments"
        );
        anyhow::ensure!(
            comment.revise_task_id.as_deref() == Some(revise_task_id),
            "comment {} is not claimed by revision {revise_task_id}",
            comment.id
        );
        anyhow::ensure!(
            comment.status == COMMENT_STATUS_IN_REVISION,
            "comment {} is not in_revision",
            comment.id
        );

        let now = now_string();
        let request_regeneration = outcome.request_regeneration;
        let series_id = comment.artifact_id.clone();
        let comment_id = comment.id.clone();
        let response = outcome.response.clone();
        {
            let mut conn = self.connect()?;
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO guide_comment_outcomes
                 (comment_id, revise_task_id, disposition, response, request_regeneration, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(comment_id) DO UPDATE SET
                     revise_task_id = excluded.revise_task_id,
                     disposition = excluded.disposition,
                     response = excluded.response,
                     request_regeneration = excluded.request_regeneration,
                     created_at = excluded.created_at",
                params![
                    comment_id,
                    revise_task_id,
                    disposition,
                    response,
                    request_regeneration as i64,
                    now
                ],
            )?;
            upsert_guide_outcome_thread_entry(&tx, &comment_id, revise_task_id, &response, &now)?;
            if self.guide_revision_is_terminal_on(&tx, revise_task_id)? {
                tx.execute(
                    &format!(
                        "UPDATE work_comments
                         SET status = '{COMMENT_STATUS_RESOLVED}',
                             status_actor = 'engine',
                             updated_at = ?2,
                             dismissed_at = ?2
                         WHERE id = ?1 AND status = '{COMMENT_STATUS_IN_REVISION}'"
                    ),
                    params![comment_id, now],
                )?;
            }
            tx.commit()?;
        }

        let regeneration = if request_regeneration {
            match self.root_task_id_for_review_guide_series(&series_id)? {
                Some(root) => {
                    let token = format!("guide-regen:{revise_task_id}");
                    match self.retry_pr_review_guide(&root, Some(&token), boss_review_guide::PROMPT_VERSION) {
                        Ok(outcome) => Some(Ok(outcome)),
                        Err(err) => {
                            // The outcome row, thread entry, and any
                            // resolve above already committed — this
                            // comment's disposition is durably recorded
                            // regardless of what happens here. Report the
                            // failure inside the `Ok` result (see the
                            // field doc) instead of returning `Err`, so the
                            // caller doesn't treat an already-recorded
                            // outcome as failed and skip the comment
                            // invalidation.
                            tracing::error!(
                                revise_task_id,
                                comment_id = %comment_id,
                                series_id = %series_id,
                                error = %format!("{err:#}"),
                                "guide outcome recorded but regeneration request failed"
                            );
                            Some(Err(format!("{err:#}")))
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        revise_task_id,
                        comment_id = %comment_id,
                        series_id = %series_id,
                        "guide outcome requested regeneration but the series has no root task"
                    );
                    None
                }
            }
        } else {
            None
        };

        let comment = self
            .get_comment(&comment_id)?
            .with_context(|| format!("missing comment after outcome: {comment_id}"))?;
        Ok(RecordedGuideCommentOutcome { comment, regeneration })
    }

    fn guide_revision_is_terminal_on(&self, conn: &Connection, task_id: &str) -> Result<bool> {
        let Some(task) = query_task(conn, task_id)? else {
            return Ok(false);
        };
        Ok(task.status.is_terminal())
    }
}

/// Resolve-side of guide-aware reconciliation: document comments still
/// resolve on task completion; guide comments resolve only when a
/// disposition was recorded. Missing dispositions stay `in_revision`.
pub(crate) fn resolve_guide_aware_comments(conn: &Connection, task_id: &str, now: &str) -> Result<usize> {
    conn.execute(
        &format!(
            "UPDATE work_comments
             SET status = '{COMMENT_STATUS_RESOLVED}',
                 status_actor = 'engine',
                 updated_at = ?2,
                 dismissed_at = ?2
             WHERE revise_task_id = ?1
               AND status = '{COMMENT_STATUS_IN_REVISION}'
               AND (
                   artifact_kind != 'pr_review_guide'
                   OR EXISTS (
                       SELECT 1 FROM guide_comment_outcomes o
                       WHERE o.comment_id = work_comments.id
                         AND o.revise_task_id = ?1
                   )
               )"
        ),
        params![task_id, now],
    )
    .map_err(Into::into)
}

fn upsert_guide_outcome_thread_entry(
    conn: &Connection,
    comment_id: &str,
    revise_task_id: &str,
    body: &str,
    now: &str,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE comment_thread_entries
         SET body = ?1, created_at = ?2
         WHERE comment_id = ?3
           AND revise_task_id = ?4
           AND entry_kind = ?5
           AND author = ?6",
        params![
            body,
            now,
            comment_id,
            revise_task_id,
            THREAD_ENTRY_KIND_ANSWER,
            THREAD_ENTRY_AUTHOR_ENGINE
        ],
    )?;
    if updated == 0 {
        let entry_id = next_id("cte");
        conn.execute(
            "INSERT INTO comment_thread_entries \
             (id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
            params![
                entry_id,
                comment_id,
                THREAD_ENTRY_KIND_ANSWER,
                THREAD_ENTRY_AUTHOR_ENGINE,
                body,
                revise_task_id,
                now
            ],
        )?;
    }
    Ok(())
}

fn compose_guide_comment_directive(
    conn: &Connection,
    canonical_pr: &str,
    series_id: &str,
    comments: &[WorkComment],
) -> String {
    let mut out = format!(
        "Reviewer comment{} on review guide `{series_id}` for `{canonical_pr}` request{} a change to the PR implementation/tests.\n\n\
         This is a REVISION of the existing PR {canonical_pr}. Do NOT open a new PR. \
         Editing generated Markdown cannot satisfy an implementation request. \
         An outdated quoted guide is context, not authority over the current code. \
         Inspect the actual current PR, address implementation and tests, validate with the repository's normal workflow, and update that PR.\n\n\
         For each submitted comment, record a grounded outcome with:\n\
         `boss comment guide-outcome --comment-id <id> --disposition source_changed|answered|no_change --body \"<grounded response>\"`\n\
         Add `--regenerate` only after a confirmed prose error; regeneration goes through guide reconciliation and does not resolve the comment by itself.\n\n",
        if comments.len() == 1 { "" } else { "s" },
        if comments.len() == 1 { "s" } else { "" },
    );
    for comment in comments {
        out.push_str(&format!("Comment {}:\n", comment.id));
        if let Some(context) = &comment.guide_context {
            out.push_str(&format!(
                "Guide version {} (comparison {}, head {}).\n",
                context.version_id, context.comparison_id, context.head_sha
            ));
        }
        append_comment_directive_body(&mut out, conn, comment);
    }
    out.push_str(
        "Please update the PR implementation and tests. Do not treat a regenerated guide as completing this work.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};
    use crate::work::{FakePrStateChecker, PrOpenState, RetryReviewGuideOutcome, WorkerPrCompletionTarget};
    use boss_protocol::{
        CommentAnchor, CreateCommentInput, CreateExecutionInput, ExecutionKind, ExecutionStatus,
        FinishExecutionRunInput, GuideCommentDisposition, GuideCommentOutcome, THREAD_ENTRY_KIND_ANSWER,
        THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP, TaskKind, WorkItem, WorkItemPatch,
    };

    fn open_checker() -> FakePrStateChecker {
        FakePrStateChecker::always(PrOpenState::Open)
    }

    fn seed_open_guide(db: &WorkDb) -> (String, String, String) {
        let root = create_active_chore(db, &create_product(db), "impl");
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let (series, comparison) = seed_review_guide_series(db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
            .unwrap();
        let PublishReviewGuideOutcome::Published(_) = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
            .unwrap()
        else {
            panic!("expected published guide")
        };
        (root, series, "https://github.com/acme/widget/pull/9".to_owned())
    }

    fn make_guide_comment(db: &WorkDb, series: &str, body: &str) -> WorkComment {
        let version_id = db
            .get_pr_review_guide_summary_for_root(
                &db.root_task_id_for_review_guide_series(series)
                    .unwrap()
                    .expect("series root"),
            )
            .unwrap()
            .expect("summary")
            .readable_version_id
            .expect("readable version");
        db.create_comment_with_guide_version(
            CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series)
                .anchor(CommentAnchor {
                    exact: "Original quote".into(),
                    ..Default::default()
                })
                .body(body)
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            Some(&version_id),
        )
        .unwrap()
    }

    #[test]
    fn revise_guide_creates_same_pr_revision_and_claims_comments() {
        let (_dir, db) = open_db();
        let (root, series, pr_url) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let question = make_guide_comment(&db, &series, "why retry?");
        db.set_comment_intent(&question.id, "question", 0.9).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series.clone())
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created {
            task_id,
            task_kind,
            addressed_comment_ids,
            pr_url: outcome_pr,
            ..
        } = outcome
        else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(task_kind, "revision");
        assert_eq!(outcome_pr.as_deref(), Some(pr_url.as_str()));
        assert_eq!(addressed_comment_ids, vec![c1.id.clone()]);
        let (WorkItem::Task(task) | WorkItem::Chore(task)) = db.get_work_item(&task_id).unwrap() else {
            panic!("expected task");
        };
        assert_eq!(task.parent_task_id.as_deref(), Some(root.as_str()));
        assert!(task.created_via.starts_with(CREATED_VIA_GUIDE_COMMENT_PREFIX));
        assert!(task.description.contains("cannot satisfy an implementation request"));
        assert!(task.description.contains(&c1.id));
        let reloaded = db.get_comment(&c1.id).unwrap().unwrap();
        assert_eq!(reloaded.status, COMMENT_STATUS_IN_REVISION);
        assert_eq!(db.get_comment(&question.id).unwrap().unwrap().status, "active");
    }

    #[test]
    fn closed_pr_refuses_without_chore() {
        let (_dir, db) = open_db();
        let (root, series, _) = seed_open_guide(&db);
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(outcome, ReviseDocOutcome::PrClosed { .. }));
        let (WorkItem::Task(root_task) | WorkItem::Chore(root_task)) = db.get_work_item(&root).unwrap() else {
            panic!("expected root");
        };
        let chores = db.list_tasks(&root_task.product_id, None, None, false).unwrap();
        assert_eq!(chores.iter().filter(|t| t.kind == TaskKind::Chore).count(), 1);
    }

    #[test]
    fn raced_merge_refuses_without_chore() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let checker = FakePrStateChecker::always(PrOpenState::Merged);
        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &checker,
            )
            .unwrap();
        assert!(matches!(outcome, ReviseDocOutcome::PrClosed { .. }));
    }

    fn start_live_revision_execution(db: &WorkDb, task_id: &str) -> String {
        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(task_id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("https://github.com/test/repo")
                    .build(),
            )
            .unwrap();
        let (exec, run) = db
            .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        exec.id
    }

    #[test]
    fn sequential_duplicate_submit_finds_no_unresolved_comments() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let first = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series.clone())
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(first, ReviseDocOutcome::Created { .. }));
        let second = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(
            matches!(second, ReviseDocOutcome::NoUnresolvedComments),
            "a sequential duplicate after in-tx claim sees no remaining revisable comments, got {second:?}"
        );
    }

    #[test]
    fn claim_returns_already_in_flight_when_candidates_already_claimed() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let first = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = first else {
            panic!("expected Created");
        };
        let claimed = db.get_comment(&c1.id).unwrap().unwrap();
        let conn = db.connect().unwrap();
        match super::super::revise_doc::claim_revisable_comments_in_tx(&conn, &[claimed], "task_other").unwrap() {
            super::super::revise_doc::ClaimOutcome::AlreadyInFlight(winner) => assert_eq!(winner, task_id),
            other => panic!("expected AlreadyInFlight, got {other:?}"),
        }
    }

    #[test]
    fn no_op_completion_resolves_dispositioned_guide_comment_only() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let c2 = make_guide_comment(&db, &series, "also fix timeout");
        db.set_comment_intent(&c2.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::NoChange)
                .response("Current retry already stops on permission errors.")
                .build(),
        )
        .unwrap();
        let exec_id = start_live_revision_execution(&db, &task_id);
        db.record_worker_no_op_completion(&exec_id, "no code change needed", None)
            .unwrap()
            .expect("live execution must complete");
        assert_eq!(db.get_comment(&c1.id).unwrap().unwrap().status, COMMENT_STATUS_RESOLVED);
        assert_eq!(
            db.get_comment(&c2.id).unwrap().unwrap().status,
            COMMENT_STATUS_IN_REVISION,
            "missing disposition must remain outstanding"
        );
    }

    #[test]
    fn pr_completion_resolves_source_changed_guide_comment_only() {
        let (_dir, db) = open_db();
        let (_root, series, pr_url) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let c2 = make_guide_comment(&db, &series, "also fix timeout");
        db.set_comment_intent(&c2.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::SourceChanged)
                .response("Retry now stops on permission errors.")
                .build(),
        )
        .unwrap();
        let exec_id = start_live_revision_execution(&db, &task_id);
        db.record_worker_pr_completion(&exec_id, &pr_url, None, None, WorkerPrCompletionTarget::InReview, None)
            .unwrap()
            .expect("live execution must complete");
        assert_eq!(db.get_comment(&c1.id).unwrap().unwrap().status, COMMENT_STATUS_RESOLVED);
        assert_eq!(
            db.get_comment(&c2.id).unwrap().unwrap().status,
            COMMENT_STATUS_IN_REVISION,
            "missing disposition must remain outstanding"
        );
    }

    #[test]
    fn terminal_revision_resolves_outcome_immediately() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        db.update_work_item(
            &task_id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let recorded = db
            .record_guide_comment_outcome(
                &task_id,
                GuideCommentOutcome::builder()
                    .comment_id(c1.id.clone())
                    .disposition(GuideCommentDisposition::Answered)
                    .response("Already documented in the current PR.")
                    .build(),
            )
            .unwrap();
        assert_eq!(recorded.comment.status, COMMENT_STATUS_RESOLVED);
    }

    #[test]
    fn outcome_rejects_comment_claimed_by_another_revision() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        let err = db
            .record_guide_comment_outcome(
                "task_other",
                GuideCommentOutcome::builder()
                    .comment_id(c1.id.clone())
                    .disposition(GuideCommentDisposition::Answered)
                    .response("wrong revision")
                    .build(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains(&task_id) || err.to_string().contains("not claimed"),
            "got {err}"
        );
    }

    #[test]
    fn outcome_rejects_comment_no_longer_in_revision() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        db.update_work_item(
            &task_id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::Answered)
                .response("done")
                .build(),
        )
        .unwrap();
        let err = db
            .record_guide_comment_outcome(
                &task_id,
                GuideCommentOutcome::builder()
                    .comment_id(c1.id)
                    .disposition(GuideCommentDisposition::NoChange)
                    .response("retry after resolve")
                    .build(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("not in_revision"), "got {err}");
    }

    #[test]
    fn request_regeneration_queues_a_retry() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        let recorded = db
            .record_guide_comment_outcome(
                &task_id,
                GuideCommentOutcome::builder()
                    .comment_id(c1.id)
                    .disposition(GuideCommentDisposition::SourceChanged)
                    .response("Fixed; the quoted guide is stale.")
                    .request_regeneration(true)
                    .build(),
            )
            .unwrap();
        assert!(
            matches!(recorded.regeneration, Some(Ok(RetryReviewGuideOutcome::Created(_)))),
            "got {:?}",
            recorded.regeneration
        );
    }

    /// A retry failure (no source comparison for the series) must not turn
    /// the whole outcome recording into an error — the disposition is
    /// already committed by the time regeneration is attempted, so the
    /// caller must still get `Ok` with the failure carried inside
    /// `regeneration`.
    #[test]
    fn regeneration_failure_does_not_fail_the_outcome_recording() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series.clone())
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        // No comparison at all makes `retry_pr_review_guide` return
        // `Ok(NoComparison)`, not an error, so force a genuine failure
        // instead: point the series' `selected_comparison_id` at a
        // comparison row that doesn't exist. `admit_pr_review_guide_attempt`
        // re-reads that column inside its transaction and sees it still
        // selected (matching what it just read), so the `ensure!` guard
        // passes — but the subsequent INSERT into `pr_review_guide_attempts`
        // trips its `REFERENCES pr_review_guide_source_comparisons(id)`
        // foreign key and returns a real `Err`.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_guide_source_series SET selected_comparison_id = 'prgc_missing' WHERE id = ?1",
                [&series],
            )
            .unwrap();
        let recorded = db
            .record_guide_comment_outcome(
                &task_id,
                GuideCommentOutcome::builder()
                    .comment_id(c1.id.clone())
                    .disposition(GuideCommentDisposition::SourceChanged)
                    .response("Fixed; the quoted guide is stale.")
                    .request_regeneration(true)
                    .build(),
            )
            .expect("recording the outcome must succeed even though regeneration will fail");
        assert_eq!(recorded.comment.id, c1.id, "the disposition must still be recorded");
        assert!(
            matches!(recorded.regeneration, Some(Err(_))),
            "got {:?}",
            recorded.regeneration
        );
        // The outcome row itself is durable, independent of the failed retry.
        let outcome_row: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM guide_comment_outcomes WHERE comment_id = ?1",
                [&c1.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome_row, 1);
    }

    #[test]
    fn re_recording_outcome_replaces_thread_entry() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap()
        else {
            panic!("expected Created");
        };
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::Answered)
                .response("first response")
                .build(),
        )
        .unwrap();
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::NoChange)
                .response("second response")
                .build(),
        )
        .unwrap();
        let entries = db.list_comment_thread_entries(&c1.id).unwrap();
        let answers: Vec<_> = entries
            .iter()
            .filter(|e| e.entry_kind == THREAD_ENTRY_KIND_ANSWER)
            .collect();
        assert_eq!(answers.len(), 1, "re-record must replace, not append: {answers:?}");
        assert_eq!(answers[0].body, "second response");
    }

    #[test]
    fn guide_directive_includes_followup_thread_context() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "why does this retry?");
        db.set_comment_intent(&c1.id, "question", 0.9).unwrap();
        db.transition_comment_to_answering(&c1.id).unwrap();
        let run = db
            .create_answer_agent_run(&c1.id, "pr_review_guide", &series, "hash", 0)
            .unwrap();
        db.complete_answer_agent_run(&run.id, "replied", Some("It retries on IO errors."), None)
            .unwrap();
        db.create_comment_thread_entry(
            &c1.id,
            THREAD_ENTRY_KIND_ANSWER,
            "engine",
            "It retries on IO errors.",
            None,
            Some(&run.id),
        )
        .unwrap();
        db.transition_comment_to_answered(&c1.id).unwrap();
        db.transition_comment_to_awaiting_followup(&c1.id).unwrap();
        db.create_comment_thread_entry(
            &c1.id,
            THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
            "user:test",
            "ok, then make it stop on permission errors",
            None,
            None,
        )
        .unwrap();
        db.reclassify_comment_intent(&c1.id, "revision", 0.85).unwrap();
        db.transition_comment_awaiting_followup_to_active(&c1.id).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        let (WorkItem::Task(task) | WorkItem::Chore(task)) = db.get_work_item(&task_id).unwrap() else {
            panic!("expected task");
        };
        assert!(
            task.description.contains("ok, then make it stop on permission errors"),
            "follow-up must appear in the directive:\n{}",
            task.description
        );
        assert!(
            task.description.contains("It retries on IO errors."),
            "bridged answer-agent reply must appear in the directive:\n{}",
            task.description
        );
    }

    #[test]
    fn banner_is_revisable_for_open_guide_and_closed_when_pr_done() {
        let (_dir, db) = open_db();
        let (root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let open = db.comments_banner_state("pr_review_guide", &series).unwrap();
        assert!(open.revisable);
        assert!(!open.pr_closed);
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let closed = db.comments_banner_state("pr_review_guide", &series).unwrap();
        assert!(!closed.revisable);
        assert!(closed.pr_closed);
    }
}
