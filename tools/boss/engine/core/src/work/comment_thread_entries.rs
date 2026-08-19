//! `comment_thread_entries` persistence — engine-authored (and
//! operator-authored) turns in a comment's thread, shared by the bucket-2
//! answer/follow-up paths (P3b of
//! `comment-triggered-document-revisions.md` §"Reply/link mechanics").
//!
//! Writes `entry_kind = 'answer'` rows (via
//! [`WorkDb::create_comment_thread_entry`], called from the
//! `CommentsPostAnswer` handler and from `finalize_answer_agent`'s
//! no-reply-posted path) and `operator_followup` rows (phase 3c). A prior
//! `nudge` kind (phase 2b — an engine-authored "this looks like a doc
//! change" thread entry, superseded by the sidebar's intent badge, which
//! already surfaces the same classification) is no longer written; existing
//! `nudge` rows remain in the table as inert history and are simply not
//! rendered by the app.
use super::*;

impl WorkDb {
    /// Column list for every `comment_thread_entries` SELECT. Order must
    /// match [`map_comment_thread_entry`].
    fn comment_thread_entry_columns() -> &'static str {
        "id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at"
    }

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
        let cols = Self::comment_thread_entry_columns();
        let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1");
        conn.query_row(&sql, [&id], map_comment_thread_entry)
            .map_err(Into::into)
    }

    /// List a comment's thread entries in chronological order. Not yet
    /// consumed by any handler in P3b (no thread-read RPC exists until the
    /// UI phase wires `CommentsList` to include them) — added now so the
    /// table has symmetric CRUD from day one.
    pub fn list_comment_thread_entries(&self, comment_id: &str) -> Result<Vec<CommentThreadEntry>> {
        let conn = self.connect()?;
        let cols = Self::comment_thread_entry_columns();
        let sql =
            format!("SELECT {cols} FROM comment_thread_entries WHERE comment_id = ?1 ORDER BY created_at ASC, id ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([comment_id], map_comment_thread_entry)?;
        collect_rows(rows)
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
