//! `comment_thread_entries` persistence — engine-authored (and
//! operator-authored) turns in a comment's thread, shared by the answer and
//! follow-up paths (`comment-triggered-document-revisions.md` §"Reply/link
//! mechanics").
//!
//! Writes `entry_kind = 'answer'` rows (via
//! [`WorkDb::create_comment_thread_entry`], called from the
//! `CommentsPostAnswer` handler and from `finalize_answer_agent`'s
//! no-reply-posted path) and `operator_followup` rows. The retired `nudge`
//! kind (an engine-authored "this looks like a doc change" entry, superseded
//! by the sidebar's intent badge, which already surfaces the same
//! classification) is no longer written; existing `nudge` rows remain in the
//! table as inert history and are filtered out at read time (see
//! `list_comment_thread_entries` below).
use super::*;
use boss_protocol::{AnswerProposedEdit, ProposedEditInput, ProposedEditState};

/// Column list for every `comment_thread_entries` SELECT. Order must
/// match [`map_comment_thread_entry`]. `pub(crate)` so callers that already
/// hold a connection (e.g.
/// [`crate::work::comment_thread_entries::list_comment_thread_entries_on`]) can build the
/// same SELECT without duplicating the column list.
pub(crate) fn comment_thread_entry_columns() -> &'static str {
    "id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at, proposed_edit"
}

/// List a comment's thread entries in chronological order, excluding retired
/// `nudge` rows, on an already-open connection. Shared by
/// [`WorkDb::list_comment_thread_entries`] — every reader (the app, `bossctl`,
/// the CLI, the revision worker's directive, and the follow-up classifier's
/// prompt) goes through this same filter and column list, so a caller that
/// already holds WorkDb's single pooled connection (an Immediate transaction)
/// gets the identical query instead of a hand-rolled copy.
pub(crate) fn list_comment_thread_entries_on(conn: &Connection, comment_id: &str) -> Result<Vec<CommentThreadEntry>> {
    let cols = comment_thread_entry_columns();
    let sql = format!(
        "SELECT {cols} FROM comment_thread_entries \
         WHERE comment_id = ?1 AND entry_kind <> ?2 \
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![comment_id, boss_protocol::THREAD_ENTRY_KIND_NUDGE],
        map_comment_thread_entry,
    )?;
    let mut entries = collect_rows(rows)?;
    for entry in &mut entries {
        if let Some(proposal) = &mut entry.proposed_edit
            && let Some(id) = &proposal.revision_id
        {
            proposal.revision_status = conn
                .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
        }
    }
    Ok(entries)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Append a thread entry to a comment. `entry_kind` must be one of
    /// `answer` / `operator_followup` ([`boss_protocol::THREAD_ENTRY_KIND_ANSWER`]
    /// et al.). Unvalidated against comment state — callers own the
    /// state-machine guard (e.g. `CommentsPostAnswer` only calls this after
    /// confirming a `running` answer-agent run exists for the comment).
    pub fn create_comment_thread_entry(
        &self,
        comment_id: &str,
        entry_kind: &str,
        author: &str,
        body: &str,
        revise_task_id: Option<&str>,
        answer_agent_run_id: Option<&str>,
    ) -> Result<CommentThreadEntry> {
        match entry_kind {
            boss_protocol::THREAD_ENTRY_KIND_ANSWER | boss_protocol::THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP => {}
            other => bail!("invalid comment thread entry_kind: {other}"),
        }
        let conn = self.connect()?;
        let id = next_id("cte");
        let now = now_string();
        conn.execute(
            "INSERT INTO comment_thread_entries \
             (id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                comment_id,
                entry_kind,
                author,
                body,
                revise_task_id,
                answer_agent_run_id,
                now
            ],
        )?;
        let cols = comment_thread_entry_columns();
        let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1");
        conn.query_row(&sql, [&id], map_comment_thread_entry)
            .map_err(Into::into)
    }

    /// List a comment's thread entries in chronological order, excluding
    /// retired `nudge` rows: every reader (the app, `bossctl`, the CLI, the
    /// revision worker's directive, and the follow-up classifier's prompt)
    /// goes through this method, so filtering here — rather than in each
    /// client — keeps all of them in agreement about what counts as thread
    /// history.
    pub fn list_comment_thread_entries(&self, comment_id: &str) -> Result<Vec<CommentThreadEntry>> {
        let conn = self.connect()?;
        list_comment_thread_entries_on(&conn, comment_id)
    }
}

#[cfg(test)]
mod tests {
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput, THREAD_ENTRY_KIND_ANSWER};
    use std::path::PathBuf;

    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn make_comment(db: &WorkDb, artifact_id: &str) -> String {
        db.create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why does this retry three times?".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap()
        .id
    }

    #[test]
    fn create_and_list_answer_entry() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let entry = db
            .create_comment_thread_entry(
                &comment,
                THREAD_ENTRY_KIND_ANSWER,
                "engine",
                "The retry backoff is exponential because…",
                None,
                Some(&run.id),
            )
            .unwrap();
        assert_eq!(entry.comment_id, comment);
        assert_eq!(entry.entry_kind, THREAD_ENTRY_KIND_ANSWER);
        assert_eq!(entry.author, "engine");
        assert_eq!(entry.answer_agent_run_id.as_deref(), Some(run.id.as_str()));
        assert!(entry.revise_task_id.is_none());

        let listed = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
    }

    #[test]
    fn rejects_unknown_entry_kind() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        assert!(
            db.create_comment_thread_entry(&comment, "bogus", "engine", "x", None, None)
                .is_err()
        );
    }

    /// The retired nudge kind is no longer a writable entry_kind — only
    /// pre-existing rows carry it, and they arrive via raw SQL/migration, not
    /// this constructor.
    #[test]
    fn rejects_the_retired_nudge_entry_kind() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        assert!(
            db.create_comment_thread_entry(&comment, "nudge", "engine", "x", None, None)
                .is_err()
        );
    }

    /// Pre-existing `nudge` rows arrive via raw SQL (migration-era data, not
    /// this constructor — see `rejects_the_retired_nudge_entry_kind` above)
    /// and must not surface through the shared read path any reader uses.
    #[test]
    fn list_excludes_retired_nudge_rows() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let kept = db
            .create_comment_thread_entry(&comment, THREAD_ENTRY_KIND_ANSWER, "engine", "kept", None, None)
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO comment_thread_entries \
                 (id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at) \
                 VALUES ('cte_nudge', ?1, 'nudge', 'engine', 'legacy nudge', NULL, NULL, '2020-01-01T00:00:00Z')",
                [&comment],
            )
            .unwrap();

        let entries = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, kept.id);
    }

    #[test]
    fn entries_list_oldest_first() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let first = db
            .create_comment_thread_entry(&comment, THREAD_ENTRY_KIND_ANSWER, "engine", "first", None, None)
            .unwrap();
        let second = db
            .create_comment_thread_entry(&comment, THREAD_ENTRY_KIND_ANSWER, "engine", "second", None, None)
            .unwrap();

        let entries = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first.id);
        assert_eq!(entries[1].id, second.id);
    }
}

pub(super) fn load_entry(conn: &Connection, id: &str) -> Result<CommentThreadEntry> {
    let cols = comment_thread_entries::comment_thread_entry_columns();
    Ok(conn.query_row(
        &format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1"),
        [id],
        map_comment_thread_entry,
    )?)
}

pub(super) fn save_proposal(conn: &Connection, id: &str, proposal: &AnswerProposedEdit) -> Result<()> {
    conn.execute(
        "UPDATE comment_thread_entries SET proposed_edit = ?2 WHERE id = ?1",
        params![id, serde_json::to_string(proposal)?],
    )?;
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Answer, proposal, run completion, and answered status commit together.
    pub fn post_answer_with_proposal(
        &self,
        execution_id: &str,
        run_id: &str,
        comment_id: &str,
        body: &str,
        proposal: Option<ProposedEditInput>,
    ) -> Result<WorkComment> {
        anyhow::ensure!(!body.trim().is_empty(), "reply body may not be empty");
        if let Some(p) = &proposal {
            anyhow::ensure!(p.version == 1, "unsupported proposed edit version");
            anyhow::ensure!(
                !p.title.trim().is_empty() && !p.instruction.trim().is_empty(),
                "proposal title and instruction must not be empty"
            );
            anyhow::ensure!(
                p.replacements.iter().all(|r| !r.path.trim().is_empty()),
                "replacement path must not be empty"
            );
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let comment = comments::query_comment(&tx, comment_id)?.context("unknown comment")?;
        anyhow::ensure!(
            proposal.is_none() || comment.artifact_kind == "pr_review_guide",
            "proposed edits require a review-guide question"
        );
        let now = now_string();
        let updated = tx.execute(
            "UPDATE answer_agent_runs SET status = 'replied', reply_body = ?4, completed_at = ?5
             WHERE id = ?1 AND comment_id = ?2 AND status = 'running'
               AND (execution_id = ?3 OR execution_id IS NULL)",
            params![run_id, comment_id, execution_id, body, now],
        )?;
        anyhow::ensure!(
            updated == 1,
            "answer run is no longer running or belongs to another execution"
        );
        let proposal = proposal.map(|proposal| AnswerProposedEdit {
            proposal,
            state: if comment.intent.as_deref() == Some("revision") {
                ProposedEditState::Superseded
            } else {
                ProposedEditState::Proposed
            },
            revision_id: None,
            revision_status: None,
        });
        tx.execute(
            "INSERT INTO comment_thread_entries
            (id, comment_id, entry_kind, author, body, answer_agent_run_id, created_at, proposed_edit)
            VALUES (?1, ?2, 'answer', 'engine', ?3, ?4, ?5, ?6)",
            params![
                next_id("cte"),
                comment_id,
                body,
                run_id,
                now,
                proposal.map(|p| serde_json::to_string(&p)).transpose()?
            ],
        )?;
        let comment = comments::transition_comment_to_answered_on(&tx, comment_id)?;
        tx.commit()?;
        Ok(comment)
    }

    pub fn comment_for_proposed_edit(&self, entry_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let entry = load_entry(&conn, entry_id)?;
        comments::query_comment(&conn, &entry.comment_id)?.context("unknown comment")
    }

    pub fn dismiss_proposed_edit(&self, entry_id: &str) -> Result<WorkComment> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entry = load_entry(&tx, entry_id)?;
        let mut proposal = entry.proposed_edit.context("answer has no proposal")?;
        anyhow::ensure!(
            matches!(
                proposal.state,
                ProposedEditState::Proposed | ProposedEditState::Dismissed
            ),
            "only a proposed edit can be dismissed"
        );
        proposal.state = ProposedEditState::Dismissed;
        save_proposal(&tx, entry_id, &proposal)?;
        let comment = comments::query_comment(&tx, &entry.comment_id)?.context("unknown comment")?;
        tx.commit()?;
        Ok(comment)
    }

    pub fn approve_proposed_edit(
        &self,
        entry_id: &str,
        instruction: Option<String>,
        checker: &dyn PrStateChecker,
    ) -> Result<ReviseDocOutcome> {
        let entry = {
            let conn = self.connect()?;
            load_entry(&conn, entry_id)?
        };
        let comment = self.get_comment(&entry.comment_id)?.context("unknown comment")?;
        self.revise_guide_pr_with_proposal(
            ReviseDocInput::builder()
                .artifact_kind(comment.artifact_kind)
                .artifact_id(comment.artifact_id)
                .comment_ids(vec![comment.id])
                .build(),
            checker,
            Some((entry_id, instruction)),
        )
    }
}

/// Prepare an approved proposal inside the revision transaction. An approved
/// retry is handled before parent lifecycle checks, even after the PR closes.
pub(super) fn prepare_approval(conn: &Connection, entry_id: &str, instruction: Option<String>) -> Result<()> {
    let entry = load_entry(conn, entry_id)?;
    let mut proposal = entry.proposed_edit.context("answer has no proposal")?;
    anyhow::ensure!(
        proposal.state == ProposedEditState::Proposed,
        "proposal is no longer actionable"
    );
    if let Some(instruction) = instruction {
        anyhow::ensure!(!instruction.trim().is_empty(), "instruction must not be empty");
        proposal.proposal.instruction = instruction;
    }
    // Persist the approved instruction before composing the directive.
    proposal.state = ProposedEditState::Approved;
    save_proposal(conn, entry_id, &proposal)?;
    let updated = conn.execute(
        "UPDATE work_comments SET status = 'active', intent = 'revision',
        intent_confidence = NULL, intent_classified_at = ?2, intent_overridden_by = 'user',
        intent_classification_failed_at = NULL, intent_classification_error = NULL,
        reopened_at = NULL, status_actor = 'user', updated_at = ?2
        WHERE id = ?1 AND status = 'answered'",
        params![entry.comment_id, now_string()],
    )?;
    anyhow::ensure!(updated == 1, "question is no longer answered");
    Ok(())
}

#[cfg(test)]
mod proposed_edit_tests {
    use super::*;
    use crate::test_support::{add_guide_comment, open_db, seed_published_guide};
    use boss_protocol::{ProposedEditInput, ProposedEditState, ProposedTextReplacement, WorkItemPatch};

    fn answer(db: &WorkDb, series: &str) -> (String, String) {
        let comment = add_guide_comment(db, series, "Could this explanation be clearer?");
        db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        let run = db
            .create_answer_agent_run(&comment.id, "pr_review_guide", series, "v0", 0)
            .unwrap();
        db.post_answer_with_proposal("test-execution", &run.id, &comment.id, "Use change-proportional maintenance.",
        Some(ProposedEditInput {
            version: 1, title: "Clarify maintenance cost".into(),
            instruction: "Update docs/design.md to describe change-proportional maintenance because DB scope alone is insufficient.".into(),
            replacements: vec![ProposedTextReplacement { path: "docs/design.md".into(), old_text: "delta-only".into(), new_text: "change-proportional".into() }],
        })).unwrap();
        let entries = db.list_comment_thread_entries(&comment.id).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "Use change-proportional maintenance.");
        assert_eq!(
            entries[0].proposed_edit.as_ref().unwrap().state,
            ProposedEditState::Proposed
        );
        (comment.id, entries[0].id.clone())
    }

    #[test]
    fn proposal_approval_is_atomic_idempotent_and_carries_instruction_and_origin() {
        let (_dir, db) = open_db();
        let (root, series) = seed_published_guide(&db, 9);
        let (comment, entry) = answer(&db, &series);
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let result = db
            .approve_proposed_edit(
                &entry,
                Some("Change docs/design.md and explain assembly cost.".into()),
                &checker,
            )
            .unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = result else {
            panic!("expected revision")
        };
        let item = db.get_work_item(&task_id).unwrap();
        let task = match item {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            _ => panic!("expected revision task"),
        };
        assert!(
            task.description
                .contains("Change docs/design.md and explain assembly cost.")
        );
        assert!(task.description.contains(&comment));
        assert!(task.description.contains(&entry));
        assert!(task.description.contains("delta-only"));
        assert!(task.description.contains("change-proportional"));
        assert_eq!(
            db.get_comment(&comment).unwrap().unwrap().revise_task_id,
            Some(task_id.clone())
        );
        let proposal = db.list_comment_thread_entries(&comment).unwrap()[0]
            .proposed_edit
            .clone()
            .unwrap();
        assert_eq!(proposal.state, ProposedEditState::Approved);
        assert_eq!(proposal.revision_id, Some(task_id.clone()));
        assert!(proposal.revision_status.is_some());
        let before: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            matches!(db.approve_proposed_edit(&entry, None, &checker).unwrap(), ReviseDocOutcome::AlreadyInFlight { task_id: id } if id == task_id)
        );
        let after: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "duplicate approval must not create spare revisions");
    }

    #[test]
    fn dismissed_and_superseded_proposals_cannot_be_approved() {
        let (_dir, db) = open_db();
        let (_, series) = seed_published_guide(&db, 9);
        let (comment, entry) = answer(&db, &series);
        db.dismiss_proposed_edit(&entry).unwrap();
        db.dismiss_proposed_edit(&entry).unwrap();
        assert_eq!(
            db.list_comment_thread_entries(&comment).unwrap()[0]
                .proposed_edit
                .as_ref()
                .unwrap()
                .state,
            ProposedEditState::Dismissed
        );
        assert!(
            db.approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Open))
                .is_err()
        );
        let (comment, entry) = answer(&db, &series);
        db.override_comment_intent(&comment, "revision").unwrap();
        assert_eq!(
            db.list_comment_thread_entries(&comment).unwrap()[0]
                .proposed_edit
                .as_ref()
                .unwrap()
                .state,
            ProposedEditState::Superseded
        );
        assert!(
            db.approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Open))
                .is_err()
        );
    }

    #[test]
    fn merged_parent_rejects_approval_without_changing_proposal_or_creating_chore() {
        let (_dir, db) = open_db();
        let (_, series) = seed_published_guide(&db, 9);
        let (comment, entry) = answer(&db, &series);
        let before: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert!(matches!(
            db.approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Merged))
                .unwrap(),
            ReviseDocOutcome::PrClosed { .. }
        ));
        let after: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(db.get_comment(&comment).unwrap().unwrap().status, "answered");
        assert_eq!(
            db.list_comment_thread_entries(&comment).unwrap()[0]
                .proposed_edit
                .as_ref()
                .unwrap()
                .state,
            ProposedEditState::Proposed
        );
    }

    #[test]
    fn invalid_proposal_does_not_complete_answer_run() {
        let (_dir, db) = open_db();
        let (_, series) = seed_published_guide(&db, 9);
        let comment = add_guide_comment(&db, &series, "Why?");
        db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        let run = db
            .create_answer_agent_run(&comment.id, "pr_review_guide", &series, "v0", 0)
            .unwrap();
        assert!(
            db.post_answer_with_proposal(
                "test",
                &run.id,
                &comment.id,
                "Answer",
                Some(ProposedEditInput {
                    version: 99,
                    title: "Edit".into(),
                    instruction: "Change the doc".into(),
                    replacements: vec![],
                })
            )
            .is_err()
        );
        assert!(db.running_answer_agent_run_for_comment(&comment.id).unwrap().is_some());
        assert!(db.list_comment_thread_entries(&comment.id).unwrap().is_empty());
    }

    #[test]
    fn proposal_survives_reopen_and_concurrent_approval_creates_one_revision() {
        let (dir, db) = open_db();
        let (_, series) = seed_published_guide(&db, 9);
        let (comment, entry) = answer(&db, &series);
        drop(db);
        let db = Arc::new(WorkDb::open(dir.path().join("state.db")).unwrap());
        assert_eq!(
            db.list_comment_thread_entries(&comment).unwrap()[0]
                .proposed_edit
                .as_ref()
                .unwrap()
                .state,
            ProposedEditState::Proposed
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                let entry = entry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    db.approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Open))
                        .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, ReviseDocOutcome::Created { .. }))
                .count(),
            1
        );
        let ids: Vec<_> = outcomes
            .into_iter()
            .map(|o| match o {
                ReviseDocOutcome::Created { task_id, .. } | ReviseDocOutcome::AlreadyInFlight { task_id } => task_id,
                other => panic!("unexpected approval result: {other:?}"),
            })
            .collect();
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn approval_does_not_replay_a_previously_approved_instruction() {
        let (_dir, db) = open_db();
        let (root, series) = seed_published_guide(&db, 9);
        let (comment, entry) = answer(&db, &series);
        let historical = db
            .create_comment_thread_entry(
                &comment,
                THREAD_ENTRY_KIND_ANSWER,
                "engine",
                "Historical answer",
                None,
                None,
            )
            .unwrap();
        save_proposal(
            &db.connect().unwrap(),
            &historical.id,
            &AnswerProposedEdit {
                proposal: ProposedEditInput {
                    version: 1,
                    title: "Earlier edit".into(),
                    instruction: "An earlier instruction that must not be replayed".into(),
                    replacements: vec![],
                },
                state: ProposedEditState::Approved,
                revision_id: Some(root),
                revision_status: None,
            },
        )
        .unwrap();
        let ReviseDocOutcome::Created { task_id, .. } = db
            .approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Open))
            .unwrap()
        else {
            panic!("expected revision")
        };
        let item = db.get_work_item(&task_id).unwrap();
        let task = match item {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            _ => panic!("expected revision"),
        };
        assert!(task.description.contains("Update docs/design.md"));
        assert!(
            !task
                .description
                .contains("An earlier instruction that must not be replayed")
        );
    }
}
