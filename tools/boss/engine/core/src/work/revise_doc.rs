//! `CommentsReviseDoc` — batch-address every unaddressed `directive`/
//! `larger_change` comment on a design/investigation-owned `pr_doc`
//! artifact (the unified buckets 1 & 3 path). Design:
//! `tools/boss/docs/designs/comment-triggered-document-revisions.md`
//! §"Buckets 1 & 3 — unified (directive / larger change)".

use super::*;

/// Outcome of the guarded batch UPDATE that claims comments for a freshly
/// created revision/chore. See [`WorkDb::claim_revisable_comments`].
enum ClaimOutcome {
    /// The comments actually claimed by this call's task (may be a subset
    /// of the candidates under a partial race).
    Claimed(Vec<String>),
    /// Every candidate was already claimed by a concurrent
    /// `CommentsReviseDoc` call between the read and this update; carries
    /// that call's task id.
    AlreadyInFlight(String),
    /// Nothing left to claim and no other claim was found either (the
    /// candidates were resolved/dismissed out-of-band in the interim).
    NoneLeft,
}

impl WorkDb {
    /// Resolve the doc's owner, apply the revision-vs-chore decision table,
    /// create the work item, and stamp/transition the addressed comments —
    /// the full `handle_comments_revise_doc` recipe from the design.
    ///
    /// `pr_checker` is threaded straight through to [`WorkDb::create_revision`]
    /// (production: [`GhPrStateChecker`]; tests: a fake).
    pub fn revise_doc(&self, input: ReviseDocInput, pr_checker: &dyn PrStateChecker) -> Result<ReviseDocOutcome> {
        let Some(owner) = self.resolve_doc_owner(&input.artifact_kind, &input.artifact_id)? else {
            return Ok(ReviseDocOutcome::NotApplicable {
                reason: format!(
                    "{}:{} is not a design/investigation-owned document",
                    input.artifact_kind, input.artifact_id
                ),
            });
        };

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

        let directive = compose_doc_comment_directive(&input.artifact_id, &candidates);
        let name = format!(
            "Address {} reviewer comment{}",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" }
        );
        let created_via = format!(
            "{CREATED_VIA_DOC_COMMENT_PREFIX}{}:{}",
            input.artifact_kind, input.artifact_id
        );

        let (task_id, task_kind, pr_url) = match owner.pr_lifecycle {
            DocOwnerPrLifecycle::Open => {
                match self.create_revision(
                    CreateRevisionInput::builder()
                        .parent_task_id(owner.chain_root_id.clone())
                        .description(directive.clone())
                        .name(name.clone())
                        .created_via(created_via.clone())
                        // Two racing CommentsReviseDoc calls on the same
                        // artifact produce identical task names; the
                        // guarded UPDATE in `claim_revisable_comments` is
                        // the single source of race arbitration
                        // (AlreadyInFlight), so the recent-duplicate guard
                        // must not preempt it here.
                        .force_duplicate(true)
                        .build(),
                    pr_checker,
                ) {
                    Ok(task) => (task.id, "revision".to_owned(), owner.pr_url.clone()),
                    Err(err) if err.downcast_ref::<RevisionGateError>().is_some() => {
                        // Raced merge/close between render and click:
                        // `assert_parent_revisable` is the backstop — fall
                        // through to the chore branch (design §"Edge cases").
                        tracing::info!(
                            artifact_id = %input.artifact_id,
                            owner_task_id = %owner.task_id,
                            error = %format!("{err:#}"),
                            "revise_doc: create_revision gate refused; falling through to a chore",
                        );
                        let task = self.create_doc_comment_chore(&owner, &directive, &name, &created_via)?;
                        (task.id, "chore".to_owned(), None)
                    }
                    Err(err) => return Err(err),
                }
            }
            DocOwnerPrLifecycle::Merged | DocOwnerPrLifecycle::NoPr => {
                let task = self.create_doc_comment_chore(&owner, &directive, &name, &created_via)?;
                (task.id, "chore".to_owned(), None)
            }
        };

        match self.claim_revisable_comments(&candidates, &task_id)? {
            ClaimOutcome::Claimed(addressed_comment_ids) => Ok(ReviseDocOutcome::Created {
                task_id,
                task_kind,
                addressed_comment_ids,
                pr_url,
            }),
            ClaimOutcome::AlreadyInFlight(winner_task_id) => Ok(ReviseDocOutcome::AlreadyInFlight {
                task_id: winner_task_id,
            }),
            ClaimOutcome::NoneLeft => Ok(ReviseDocOutcome::NoUnresolvedComments),
        }
    }

    /// Create the general-task (chore) vehicle for the merged/closed/no-PR
    /// branches of the decision table, inheriting `product_id` from the
    /// doc's owning task.
    fn create_doc_comment_chore(
        &self,
        owner: &DocOwner,
        directive: &str,
        name: &str,
        created_via: &str,
    ) -> Result<Task> {
        let owner_task = {
            let conn = self.connect()?;
            query_task(&conn, &owner.task_id)?
                .with_context(|| format!("doc owner task not found: {}", owner.task_id))?
        };
        self.create_chore(
            CreateChoreInput::builder()
                .product_id(owner_task.product_id)
                .name(name.to_owned())
                .description(directive.to_owned())
                .created_via(created_via.to_owned())
                // See the matching comment on the revision path in
                // `revise_doc`: the guarded UPDATE in
                // `claim_revisable_comments` is the single source of race
                // arbitration, so the recent-duplicate guard must not
                // preempt it here either.
                .force_duplicate(true)
                .build(),
        )
    }

    /// The guarded batch UPDATE: claim exactly the still-`active`,
    /// still-revisable comments among `candidates` for `task_id`. Design
    /// §"Concurrency/idempotency" — "a racing second call finds nothing
    /// left and returns `AlreadyInFlight{task_id}`".
    fn claim_revisable_comments(&self, candidates: &[WorkComment], task_id: &str) -> Result<ClaimOutcome> {
        let conn = self.connect()?;
        let now = now_string();
        let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let placeholders = std::iter::repeat("?")
            .take(candidate_ids.len())
            .collect::<Vec<_>>()
            .join(",");

        let update_sql = format!(
            "UPDATE work_comments
             SET status = '{COMMENT_STATUS_IN_REVISION}', revise_task_id = ?, status_actor = 'engine', updated_at = ?
             WHERE id IN ({placeholders}) AND status = '{COMMENT_STATUS_ACTIVE}'
               AND intent IN ('{INTENT_DIRECTIVE}', '{INTENT_LARGER_CHANGE}')"
        );
        let mut update_params: Vec<&dyn rusqlite::ToSql> = vec![&task_id, &now];
        for id in &candidate_ids {
            update_params.push(id);
        }
        let affected = conn.execute(&update_sql, update_params.as_slice())?;

        if affected == 0 {
            // Only a genuinely in-flight claim counts as a winner here.
            // `revise_task_id` is intentionally left un-cleared when a
            // comment transitions out of `in_revision` (see its doc
            // comment), so without the status filter a comment carrying a
            // stale id from a prior, already-completed batch would be
            // misreported as `AlreadyInFlight`.
            let winner_sql = format!(
                "SELECT revise_task_id FROM work_comments
                 WHERE id IN ({placeholders}) AND revise_task_id IS NOT NULL
                   AND status = '{COMMENT_STATUS_IN_REVISION}' LIMIT 1"
            );
            let winner_params: Vec<&dyn rusqlite::ToSql> =
                candidate_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            let winner: Option<String> = conn
                .query_row(&winner_sql, winner_params.as_slice(), |row| row.get(0))
                .optional()?;
            return Ok(match winner {
                Some(winner_task_id) => ClaimOutcome::AlreadyInFlight(winner_task_id),
                None => ClaimOutcome::NoneLeft,
            });
        }

        let select_sql = format!("SELECT id FROM work_comments WHERE id IN ({placeholders}) AND revise_task_id = ?");
        let mut select_params: Vec<&dyn rusqlite::ToSql> =
            candidate_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        select_params.push(&task_id);
        let mut stmt = conn.prepare(&select_sql)?;
        let addressed: Vec<String> = collect_rows(stmt.query_map(select_params.as_slice(), |row| row.get(0))?)?;
        Ok(ClaimOutcome::Claimed(addressed))
    }
}

/// Assemble the worker directive from every addressed comment: the doc's
/// artifact id, each comment's quoted anchor, and its body (design
/// §"Risks" — "directive assembly includes doc path, quoted anchors, and
/// comment bodies").
fn compose_doc_comment_directive(artifact_id: &str, comments: &[WorkComment]) -> String {
    let mut out = format!(
        "Reviewer comment{} on `{artifact_id}` request{} the following change{}:\n\n",
        if comments.len() == 1 { "" } else { "s" },
        if comments.len() == 1 { "s" } else { "" },
        if comments.len() == 1 { "" } else { "s" },
    );
    for comment in comments {
        out.push_str("Quoted section:\n> ");
        out.push_str(&comment.anchor.exact);
        out.push_str("\n\nComment:\n> ");
        out.push_str(&comment.body);
        out.push_str("\n\n");
    }
    out.push_str("Please update the document accordingly.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{FakePrStateChecker, PrOpenState, WorkDb};
    use boss_protocol::{
        CommentAnchor, CreateCommentInput, CreateProductInput, CreateProjectInput, SetProjectDesignDocInput, TaskKind,
        WorkItemPatch,
    };
    use std::path::PathBuf;

    const REPO: &str = "git@github.com:o/r.git";
    const DOC_PATH: &str = "tools/boss/docs/designs/x.md";

    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn open_checker() -> FakePrStateChecker {
        FakePrStateChecker::always(PrOpenState::Open)
    }

    /// Stand up a product + project, point the project's design-doc
    /// pointer at `DOC_PATH` on `main`, and return the auto-created
    /// design task plus the `pr_doc:*` artifact id that resolves to it
    /// (mirrors `resolve_doc_owner_matches_project_design_doc_pointer` in
    /// `work/tests/t11.rs`).
    fn seed_design_owned_artifact(db: &WorkDb) -> (Task, String) {
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("proto")
                    .repo_remote_url(REPO.to_owned())
                    .build(),
            )
            .unwrap();
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("proj")
                    .build(),
            )
            .unwrap();
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(REPO.to_owned()),
            design_doc_branch: Some("main".to_owned()),
            design_doc_path: Some(DOC_PATH.to_owned()),
            unset: false,
        })
        .unwrap();
        let design = db
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("project should have an auto-created design task");
        let artifact_id = format!("pr_doc:{REPO}:main:{DOC_PATH}");
        (design, artifact_id)
    }

    fn make_comment(db: &WorkDb, artifact_id: &str, exact: &str) -> WorkComment {
        db.create_comment(CreateCommentInput {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: exact.to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: format!("please change {exact}"),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        })
        .unwrap()
    }

    #[test]
    fn not_applicable_for_work_item_artifact_kind() {
        let db = mem_db();
        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("work_item")
                    .artifact_id("task_x")
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(outcome, ReviseDocOutcome::NotApplicable { .. }));
    }

    #[test]
    fn no_unresolved_comments_when_none_classified() {
        let db = mem_db();
        let (_design, artifact_id) = seed_design_owned_artifact(&db);
        // No comments at all on this artifact.
        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(outcome, ReviseDocOutcome::NoUnresolvedComments));
    }

    #[test]
    fn creates_revision_for_open_pr_and_claims_comments() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        let pr_url = "https://github.com/o/r/pull/1".to_owned();
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();
        let c2 = make_comment(&db, &artifact_id, "beta");
        db.set_comment_intent(&c2.id, "larger_change", 0.9).unwrap();
        // A question-intent comment must never be swept into the batch.
        let c3 = make_comment(&db, &artifact_id, "gamma");
        db.set_comment_intent(&c3.id, "question", 0.9).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created {
            task_id,
            task_kind,
            addressed_comment_ids,
            pr_url: outcome_pr_url,
        } = outcome
        else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(task_kind, "revision");
        assert_eq!(outcome_pr_url.as_deref(), Some(pr_url.as_str()));
        assert_eq!(addressed_comment_ids.len(), 2);
        assert!(addressed_comment_ids.contains(&c1.id));
        assert!(addressed_comment_ids.contains(&c2.id));
        assert!(!addressed_comment_ids.contains(&c3.id));

        let reloaded1 = db.get_comment(&c1.id).unwrap().unwrap();
        assert_eq!(reloaded1.status, "in_revision");
        assert_eq!(reloaded1.revise_task_id.as_deref(), Some(task_id.as_str()));
        let reloaded3 = db.get_comment(&c3.id).unwrap().unwrap();
        assert_eq!(
            reloaded3.status, "active",
            "question-intent comment must stay untouched"
        );
    }

    #[test]
    fn banner_state_reflects_revise_doc_lifecycle() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        let pr_url = "https://github.com/o/r/pull/1".to_owned();
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        // No comments yet: doc has an owner, but nothing unresolved.
        let state = db.comments_banner_state("pr_doc", &artifact_id).unwrap();
        assert!(!state.revisable);
        assert_eq!(state.unresolved_count, 0);
        assert_eq!(state.in_revision_count, 0);
        assert_eq!(state.doc_kind, Some(TaskKind::Design));

        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();
        let c2 = make_comment(&db, &artifact_id, "beta");
        db.set_comment_intent(&c2.id, "question", 0.9).unwrap();

        // One directive comment: revisable, and the question-intent one
        // must not count toward `unresolved_count`.
        let state = db.comments_banner_state("pr_doc", &artifact_id).unwrap();
        assert!(state.revisable);
        assert_eq!(state.unresolved_count, 1);
        assert_eq!(state.in_revision_count, 0);

        db.revise_doc(
            ReviseDocInput::builder()
                .artifact_kind("pr_doc")
                .artifact_id(artifact_id.clone())
                .build(),
            &open_checker(),
        )
        .unwrap();

        // Claimed by the revision: no longer unresolved, now in_revision.
        let state = db.comments_banner_state("pr_doc", &artifact_id).unwrap();
        assert!(!state.revisable);
        assert_eq!(state.unresolved_count, 0);
        assert_eq!(state.in_revision_count, 1);
        assert_eq!(state.doc_kind, Some(TaskKind::Design));
    }

    #[test]
    fn creates_chore_when_pr_merged() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_kind, pr_url, .. } = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(task_kind, "chore");
        assert!(pr_url.is_none());
    }

    #[test]
    fn creates_chore_when_no_pr() {
        let db = mem_db();
        let (_design, artifact_id) = seed_design_owned_artifact(&db);
        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_kind, pr_url, .. } = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(task_kind, "chore");
        assert!(pr_url.is_none());
    }

    #[test]
    fn no_op_on_double_revise() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        let first = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id.clone())
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(first, ReviseDocOutcome::Created { .. }));

        // Nothing new landed since; a second click finds no active
        // directive/larger_change comments left and is a no-op.
        let second = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &open_checker(),
            )
            .unwrap();
        assert!(matches!(second, ReviseDocOutcome::NoUnresolvedComments));
    }

    #[test]
    fn manual_subset_via_comment_ids() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();
        let c2 = make_comment(&db, &artifact_id, "beta");
        db.set_comment_intent(&c2.id, "directive", 0.9).unwrap();

        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .comment_ids(vec![c1.id.clone()])
                    .build(),
                &open_checker(),
            )
            .unwrap();
        let ReviseDocOutcome::Created {
            addressed_comment_ids, ..
        } = outcome
        else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(addressed_comment_ids, vec![c1.id.clone()]);

        // c2 was never a candidate for this batch, so it's still active.
        let reloaded2 = db.get_comment(&c2.id).unwrap().unwrap();
        assert_eq!(reloaded2.status, "active");
    }

    #[test]
    fn falls_through_to_chore_when_gate_refuses_at_click_time() {
        let db = mem_db();
        let (design, artifact_id) = seed_design_owned_artifact(&db);
        let pr_url = "https://github.com/o/r/pull/1".to_owned();
        // DB lifecycle still reads Open (in_review + pr_url set)...
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        // ...but the live checker reports the PR merged, so the gate
        // refuses at click time and this must fall through to a chore
        // rather than propagating the RevisionGateError.
        let checker = FakePrStateChecker::always(PrOpenState::Merged);
        let outcome = db
            .revise_doc(
                ReviseDocInput::builder()
                    .artifact_kind("pr_doc")
                    .artifact_id(artifact_id)
                    .build(),
                &checker,
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_kind, pr_url, .. } = outcome else {
            panic!("expected Created, got {outcome:?}");
        };
        assert_eq!(task_kind, "chore");
        assert!(pr_url.is_none());
    }

    #[test]
    fn already_in_flight_when_comment_claimed_by_concurrent_call() {
        let db = mem_db();
        let (_design, artifact_id) = seed_design_owned_artifact(&db);
        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        // Exercise `claim_revisable_comments` directly with a candidate
        // list captured while the comment was still `active` (as
        // `revise_doc` would read it), then simulate the loser of a
        // concurrent `CommentsReviseDoc` race: the comment is claimed
        // (still `in_revision`) for some other task's batch before this
        // call's guarded UPDATE runs.
        let candidate = db.get_comment(&c1.id).unwrap().unwrap();
        let winner_task_id = "task_winner".to_owned();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_comments SET status = 'in_revision', revise_task_id = ? WHERE id = ?",
                rusqlite::params![winner_task_id, c1.id],
            )
            .unwrap();
        }

        let outcome = db.claim_revisable_comments(&[candidate], "task_loser").unwrap();
        assert!(matches!(
            outcome,
            ClaimOutcome::AlreadyInFlight(task_id) if task_id == winner_task_id
        ));
    }

    #[test]
    fn stale_revise_task_id_does_not_report_already_in_flight() {
        let db = mem_db();
        let (_design, artifact_id) = seed_design_owned_artifact(&db);
        let c1 = make_comment(&db, &artifact_id, "alpha");
        db.set_comment_intent(&c1.id, "directive", 0.9).unwrap();

        // Exercise `claim_revisable_comments` directly with a candidate
        // list captured (as `revise_doc` would) while the comment was
        // still `active`, but the row has since gone non-active out of
        // band, carrying a `revise_task_id` from a prior, already
        // completed batch. The guarded UPDATE therefore affects 0 rows,
        // and the stale id must not be mistaken for a genuinely
        // in-flight claim.
        let candidate = db.get_comment(&c1.id).unwrap().unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_comments SET status = 'resolved', revise_task_id = 'task_old' WHERE id = ?",
                rusqlite::params![c1.id],
            )
            .unwrap();
        }

        let outcome = db.claim_revisable_comments(&[candidate], "task_new").unwrap();
        assert!(matches!(outcome, ClaimOutcome::NoneLeft));
    }
}
