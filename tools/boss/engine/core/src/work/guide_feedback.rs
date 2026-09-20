//! Same-PR revision dispatch and per-comment outcomes for review-guide
//! feedback. Design: `tools/boss/docs/designs/automatic-pr-review-guides.md`
//! §"Comments target the implementation".

use super::feedback_target::FeedbackTarget;
use super::revise_doc::{ClaimOutcome, claim_revisable_comments_in_tx};
use super::revision_helpers::assert_parent_revisable_and_insert;
use super::*;
use boss_protocol::{GuideCommentOutcome, THREAD_ENTRY_AUTHOR_ENGINE};

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
            root_task_id,
            series_id,
            canonical_pr,
            chain_root_id,
            pr_lifecycle,
            pr_url,
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

        let candidates = {
            let conn = self.connect()?;
            comments::query_revisable_comments(
                &conn,
                &input.artifact_kind,
                &input.artifact_id,
                input.comment_ids.as_deref(),
            )?
        };
        if candidates.is_empty() {
            return Ok(ReviseDocOutcome::NoUnresolvedComments);
        }

        let directive = compose_guide_comment_directive(&canonical_pr, &series_id, &candidates);
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

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                let _ = root_task_id;
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
    pub fn record_guide_comment_outcome(
        &self,
        revise_task_id: &str,
        outcome: GuideCommentOutcome,
    ) -> Result<WorkComment> {
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
        {
            let conn = self.connect()?;
            conn.execute(
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
                    outcome.response,
                    request_regeneration as i64,
                    now
                ],
            )?;
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
                    outcome.response,
                    revise_task_id,
                    now
                ],
            )?;
            if self.guide_revision_is_terminal_on(&conn, revise_task_id)? {
                conn.execute(
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
        }

        if request_regeneration && let Some(root) = self.root_task_id_for_review_guide_series(&series_id)? {
            let token = format!("guide-regen:{revise_task_id}");
            let _ = self.retry_pr_review_guide(&root, Some(&token), boss_review_guide::PROMPT_VERSION);
        }

        self.get_comment(&comment_id)?
            .with_context(|| format!("missing comment after outcome: {comment_id}"))
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

fn compose_guide_comment_directive(canonical_pr: &str, series_id: &str, comments: &[WorkComment]) -> String {
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
        out.push_str("Quoted section:\n> ");
        out.push_str(&comment.anchor.exact);
        out.push_str("\n\nComment:\n> ");
        out.push_str(&comment.body);
        out.push_str("\n\n");
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
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::{
        CommentAnchor, CreateCommentInput, GuideCommentDisposition, GuideCommentOutcome, TaskKind, WorkItem,
        WorkItemPatch,
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

    #[test]
    fn duplicate_submit_returns_existing_task() {
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
        let ReviseDocOutcome::Created { task_id, .. } = first else {
            panic!("expected Created");
        };
        let second = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_review_guide")
                    .artifact_id(series)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        match second {
            ReviseDocOutcome::AlreadyInFlight { task_id: winner } => assert_eq!(winner, task_id),
            ReviseDocOutcome::NoUnresolvedComments => {}
            other => panic!("expected duplicate, got {other:?}"),
        }
    }

    #[test]
    fn no_change_outcome_resolves_only_that_comment() {
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
        {
            let now = now_string();
            let conn = db.connect().unwrap();
            super::resolve_guide_aware_comments(&conn, &task_id, &now).unwrap();
        }
        assert_eq!(db.get_comment(&c1.id).unwrap().unwrap().status, COMMENT_STATUS_RESOLVED);
        assert_eq!(
            db.get_comment(&c2.id).unwrap().unwrap().status,
            COMMENT_STATUS_IN_REVISION,
            "missing disposition must remain outstanding"
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
