//! Same-PR revision dispatch and per-comment outcomes for review-guide
//! feedback. Design: `tools/boss/docs/designs/automatic-pr-review-guides.md`
//! §"Comments target the implementation".

use super::feedback_target::FeedbackTarget;
use super::revise_doc::{ClaimOutcome, claim_revisable_comments_in_tx, push_comment_directive_block};
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

        let directive = compose_guide_comment_directive(self, &canonical_pr, &series_id, &candidates);
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
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                    outcome.response,
                    request_regeneration as i64,
                    now
                ],
            )?;
            // A re-record for the same comment (retry after a transport
            // failure, or the worker re-running `boss comment guide-outcome`)
            // must update the existing thread entry rather than append a
            // duplicate — the outcome upsert above is already idempotent on
            // `comment_id`, so the thread entry should be too.
            let existing_entry_id: Option<String> = tx
                .query_row(
                    &format!(
                        "SELECT id FROM comment_thread_entries
                         WHERE comment_id = ?1 AND revise_task_id = ?2 AND entry_kind = '{THREAD_ENTRY_KIND_ANSWER}'"
                    ),
                    params![comment_id, revise_task_id],
                    |row| row.get(0),
                )
                .optional()?;
            match existing_entry_id {
                Some(entry_id) => {
                    tx.execute(
                        "UPDATE comment_thread_entries SET body = ?2, created_at = ?3 WHERE id = ?1",
                        params![entry_id, outcome.response, now],
                    )?;
                }
                None => {
                    let entry_id = next_id("cte");
                    tx.execute(
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
                }
            }
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

        if request_regeneration && let Some(root) = self.root_task_id_for_review_guide_series(&series_id)? {
            let token = format!("guide-regen:{revise_task_id}");
            if let Err(err) = self.retry_pr_review_guide(&root, Some(&token), boss_review_guide::PROMPT_VERSION) {
                tracing::warn!(
                    root_task_id = %root,
                    revise_task_id,
                    err = %err,
                    "guide comment outcome: regeneration request failed",
                );
            }
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

/// Assemble the worker directive from every addressed guide comment: the
/// comment id, its guide version triple, then its block from
/// [`push_comment_directive_block`] (quoted section, body, and any
/// answer-agent-reply / operator-follow-up bridge context — the same
/// bridging `compose_doc_comment_directive` supplies for the document path,
/// which matters here because `spawn_followup_classifier` can reclassify a
/// guide follow-up into a revision). The same-PR revision instructions and
/// the `boss comment guide-outcome` usage line are NOT repeated here — they
/// live in `compose_revision_directive`'s `CREATED_VIA_GUIDE_COMMENT_PREFIX`
/// branch (`runner/prompt.rs`), which fires for every dispatch of this
/// revision, so keeping one copy avoids the two prose blocks drifting apart.
fn compose_guide_comment_directive(
    db: &WorkDb,
    canonical_pr: &str,
    series_id: &str,
    comments: &[WorkComment],
) -> String {
    let mut out = format!(
        "Reviewer comment{} on review guide `{series_id}` for `{canonical_pr}` request{} a change to the PR implementation/tests.\n\n",
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
        push_comment_directive_block(db, &mut out, comment);
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
        assert!(task.description.contains("a change to the PR implementation/tests"));
        assert!(task.description.contains(&c1.id));
        let reloaded = db.get_comment(&c1.id).unwrap().unwrap();
        assert_eq!(reloaded.status, COMMENT_STATUS_IN_REVISION);
        assert_eq!(db.get_comment(&question.id).unwrap().unwrap().status, "active");
    }

    #[test]
    fn directive_includes_bridged_bucket2_context_when_present() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);

        // A guide comment that started as a `question`, got an answer-agent
        // reply, then an operator follow-up that reclassified it into a
        // revision — this PR is what wires `spawn_followup_classifier` to
        // classify guide follow-ups against `ClassifierSubject::PullRequest`,
        // so this bridge must reach the guide directive the same way it
        // reaches the document directive (`revise_doc`'s
        // `directive_includes_bridged_bucket2_context_when_present`).
        let c1 = make_guide_comment(&db, &series, "why retry?");
        db.set_comment_intent(&c1.id, "question", 0.9).unwrap();
        db.transition_comment_to_answering(&c1.id).unwrap();
        let run = db
            .create_answer_agent_run(&c1.id, "pr_review_guide", &series, "v0", 0)
            .unwrap();
        db.complete_answer_agent_run(&run.id, "replied", Some("It stops after 3 attempts."), None)
            .unwrap();
        db.create_comment_thread_entry(
            &c1.id,
            THREAD_ENTRY_KIND_ANSWER,
            "engine",
            "It stops after 3 attempts.",
            None,
            Some(&run.id),
        )
        .unwrap();
        db.transition_comment_to_answered(&c1.id).unwrap();
        db.transition_comment_to_awaiting_followup(&c1.id).unwrap();
        db.create_comment_thread_entry(
            &c1.id,
            THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
            "user:test@example.com",
            "please make it stop after 5 instead",
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
        assert!(task.description.contains("It stops after 3 attempts."));
        assert!(task.description.contains("please make it stop after 5 instead"));
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
    fn source_changed_and_answered_dispositions_are_persisted() {
        let (_dir, db) = open_db();
        let (_root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "fix retry");
        db.set_comment_intent(&c1.id, "revision", 0.9).unwrap();
        let c2 = make_guide_comment(&db, &series, "why not exponential backoff?");
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
                .response("Switched retries to stop after 3 attempts.")
                .build(),
        )
        .unwrap();
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c2.id.clone())
                .disposition(GuideCommentDisposition::Answered)
                .response("Exponential backoff isn't needed here; the endpoint is idempotent.")
                .build(),
        )
        .unwrap();

        // Scoped so the connection guard (a single shared `Mutex<Connection>`,
        // not a pool — see `WorkDb::conn`) is released before the
        // `list_comment_thread_entries` calls below reacquire it; holding it
        // open across those calls deadlocks on the same thread.
        {
            let conn = db.connect().unwrap();
            let d1: String = conn
                .query_row(
                    "SELECT disposition FROM guide_comment_outcomes WHERE comment_id = ?1",
                    [&c1.id],
                    |row| row.get(0),
                )
                .unwrap();
            let d2: String = conn
                .query_row(
                    "SELECT disposition FROM guide_comment_outcomes WHERE comment_id = ?1",
                    [&c2.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(d1, GuideCommentDisposition::SourceChanged.as_str());
            assert_eq!(d2, GuideCommentDisposition::Answered.as_str());
        }

        let entries1 = db.list_comment_thread_entries(&c1.id).unwrap();
        assert!(
            entries1
                .iter()
                .any(|e| e.body.contains("Switched retries to stop after 3 attempts."))
        );
        let entries2 = db.list_comment_thread_entries(&c2.id).unwrap();
        assert!(entries2.iter().any(|e| e.body.contains("the endpoint is idempotent")));
    }

    #[test]
    fn re_recording_an_outcome_updates_the_existing_thread_entry_without_duplicating() {
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
                .disposition(GuideCommentDisposition::NoChange)
                .response("first pass response")
                .build(),
        )
        .unwrap();
        // A worker re-running `boss comment guide-outcome` for the same
        // comment — after a transport failure, or simply re-recording with
        // a refined response — must update the existing thread entry rather
        // than append a duplicate.
        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::SourceChanged)
                .response("refined response after re-record")
                .build(),
        )
        .unwrap();

        let entries = db.list_comment_thread_entries(&c1.id).unwrap();
        let answer_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.entry_kind == THREAD_ENTRY_KIND_ANSWER)
            .collect();
        assert_eq!(
            answer_entries.len(),
            1,
            "re-recording must not duplicate the thread entry: {entries:?}"
        );
        assert_eq!(answer_entries[0].body, "refined response after re-record");

        let conn = db.connect().unwrap();
        let disposition: String = conn
            .query_row(
                "SELECT disposition FROM guide_comment_outcomes WHERE comment_id = ?1",
                [&c1.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(disposition, GuideCommentDisposition::SourceChanged.as_str());
    }

    #[test]
    fn request_regeneration_triggers_a_new_guide_attempt() {
        let (_dir, db) = open_db();
        let (root, series, _) = seed_open_guide(&db);
        let c1 = make_guide_comment(&db, &series, "the quoted section is stale prose");
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

        let count_before: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_review_guide_attempts", [], |row| row.get(0))
            .unwrap();

        db.record_guide_comment_outcome(
            &task_id,
            GuideCommentOutcome::builder()
                .comment_id(c1.id.clone())
                .disposition(GuideCommentDisposition::SourceChanged)
                .response("Confirmed prose error; regenerating the guide.")
                .request_regeneration(true)
                .build(),
        )
        .unwrap();

        let count_after: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_review_guide_attempts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count_after,
            count_before + 1,
            "request_regeneration must admit a new guide attempt for {root}"
        );

        let conn = db.connect().unwrap();
        let request_regeneration: i64 = conn
            .query_row(
                "SELECT request_regeneration FROM guide_comment_outcomes WHERE comment_id = ?1",
                [&c1.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(request_regeneration, 1);
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
