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
use crate::comments::query_comment;
use crate::schema_init::table_has_column;
use boss_protocol::{AnswerProposedEdit, ProposedEditInput, ProposedEditState};

/// Migration that adds the stored-proposal column and the trigger that
/// supersedes a still-open proposal when its answered question is followed up
/// or reclassified.
pub(crate) const PROPOSED_EDIT_MIGRATION_ID: &str = "20261118_1030_answer_proposed_edits";

/// Idempotent: the column and trigger are each created only when absent.
pub(crate) fn migrate_proposed_edits(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "comment_thread_entries", "proposed_edit")? {
        conn.execute_batch("ALTER TABLE comment_thread_entries ADD COLUMN proposed_edit TEXT")?;
    }
    // Only `answered` -> `awaiting_followup` (operator follow-up) and
    // `answered` -> `active` (reclassified to a revision) supersede. Resolving
    // or dismissing the question leaves the proposal approvable.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS supersede_answer_proposals
         AFTER UPDATE OF status ON work_comments
         WHEN OLD.status = 'answered' AND NEW.status IN ('awaiting_followup', 'active')
         BEGIN
             UPDATE comment_thread_entries
             SET proposed_edit = json_set(proposed_edit, '$.state', 'superseded')
             WHERE comment_id = NEW.id AND json_extract(proposed_edit, '$.state') = 'proposed';
         END",
    )?;
    Ok(())
}

/// Column list for every `comment_thread_entries` SELECT. Order must
/// match [`map_comment_thread_entry`]. `pub` so callers that already
/// hold a connection (e.g.
/// [`crate::comment_thread_entries::list_comment_thread_entries_on`]) can build the
/// same SELECT without duplicating the column list.
pub(crate) fn comment_thread_entry_columns() -> &'static str {
    "id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at, proposed_edit"
}

/// Append a thread entry on an already-open connection. Shared by
/// [`WorkDb::create_comment_thread_entry`] and by
/// [`WorkDb::apply_answer_agent_reply`]'s reply transaction, including the
/// `entry_kind` validation the reply path must not drop.
pub(crate) fn insert_comment_thread_entry_on(
    conn: &Connection,
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

pub(crate) fn load_entry(conn: &Connection, id: &str) -> Result<CommentThreadEntry> {
    let cols = comment_thread_entry_columns();
    let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1");
    conn.query_row(&sql, [id], map_comment_thread_entry)
        .optional()?
        .with_context(|| format!("unknown thread entry: {id}"))
}

pub(crate) fn save_proposal(conn: &Connection, id: &str, proposal: &AnswerProposedEdit) -> Result<()> {
    // `revision_status` is hydrated on read; never persist a stale copy.
    let mut stored = proposal.clone();
    stored.revision_status = None;
    conn.execute(
        "UPDATE comment_thread_entries SET proposed_edit = ?2 WHERE id = ?1",
        params![id, serde_json::to_string(&stored)?],
    )?;
    Ok(())
}

/// Validate an answer agent's proposal against the comment it answers and
/// build the value to store. A proposal is only meaningful on a review-guide
/// question; one for a comment already classified as a revision is stored
/// `Superseded`, because that comment goes straight to revision on its own.
pub(crate) fn new_stored_proposal(
    conn: &Connection,
    comment_id: &str,
    proposal: ProposedEditInput,
) -> Result<AnswerProposedEdit> {
    anyhow::ensure!(proposal.version == 1, "unsupported proposed edit version");
    anyhow::ensure!(
        !proposal.title.trim().is_empty() && !proposal.instruction.trim().is_empty(),
        "proposal title and instruction must not be empty"
    );
    anyhow::ensure!(
        proposal.replacements.iter().all(|r| !r.path.trim().is_empty()),
        "replacement path must not be empty"
    );
    let comment = query_comment(conn, comment_id)?.context("unknown comment")?;
    anyhow::ensure!(
        comment.artifact_kind == "pr_review_guide",
        "proposed edits require a review-guide question"
    );
    let state = if comment.intent.as_deref() == Some(boss_protocol::INTENT_REVISION) {
        ProposedEditState::Superseded
    } else {
        ProposedEditState::Proposed
    };
    Ok(AnswerProposedEdit {
        proposal,
        state,
        revision_id: None,
        revision_status: None,
    })
}

/// Prepare an approved proposal inside the revision transaction: persist the
/// (optionally edited) instruction as `Approved` and reopen the answered
/// question as a revision so the shared revision path can claim it. The caller
/// rolls everything back if the revision is not created.
pub(crate) fn prepare_approval(conn: &Connection, entry_id: &str, instruction: Option<String>) -> Result<()> {
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
    // Saved before the status change so the supersede trigger sees `approved`.
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
        let conn = self.connect()?;
        insert_comment_thread_entry_on(
            &conn,
            comment_id,
            entry_kind,
            author,
            body,
            revise_task_id,
            answer_agent_run_id,
        )
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

    /// The comment a proposal's thread entry belongs to.
    pub fn comment_for_proposed_edit(&self, entry_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let entry = load_entry(&conn, entry_id)?;
        query_comment(&conn, &entry.comment_id)?.context("unknown comment")
    }

    /// Keep a proposal for audit while removing its actions. Dismissing an
    /// already-dismissed proposal is a no-op.
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
        let comment = query_comment(&tx, &entry.comment_id)?.context("unknown comment")?;
        tx.commit()?;
        Ok(comment)
    }

    /// Approve a proposal into the review-guide same-PR revision path. The
    /// approval, the comment's reopening and the revision insert commit
    /// together; a repeat approval returns the revision already created, and a
    /// closed PR leaves the proposal untouched.
    pub fn approve_proposed_edit(
        &self,
        entry_id: &str,
        instruction: Option<String>,
        pr_checker: &dyn PrStateChecker,
        actor: &str,
    ) -> Result<ReviseDocOutcome> {
        let entry = {
            let conn = self.connect()?;
            load_entry(&conn, entry_id)?
        };
        let comment = self.get_comment(&entry.comment_id)?.context("unknown comment")?;
        self.revise_guide_pr_with_approval(
            ReviseDocInput::builder()
                .artifact_kind(comment.artifact_kind)
                .artifact_id(comment.artifact_id)
                .comment_ids(vec![comment.id])
                .build(),
            pr_checker,
            actor,
            Some((entry_id, instruction)),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::WorkDb;
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
