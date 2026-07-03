//! `comment_thread_entries` persistence — the shared engine-authored
//! nudge/answer/follow-up table (P2b of
//! `comment-triggered-document-revisions.md` § "Reply/link mechanics").
//!
//! Only the `nudge` insert is wired up so far (P2b: post one at
//! `directive`/`larger_change` classification time, before `[Revise]` is
//! even clicked); `answer` and `operator_followup` land with the bucket-2
//! phases. Mirrors `answer_agent_runs.rs`'s insert-then-reselect shape.

use super::*;

impl WorkDb {
    /// Column list for every `comment_thread_entries` SELECT. Order must
    /// match [`map_comment_thread_entry`].
    fn comment_thread_entry_columns() -> &'static str {
        "id, comment_id, entry_kind, author, body, revise_task_id, \
         answer_agent_run_id, created_at"
    }

    /// Post an `entry_kind='nudge'` thread entry on `comment_id`. Called
    /// immediately on `directive`/`larger_change` classification (design §
    /// "Buckets 1 & 3 — unified"); `revise_task_id` starts `NULL` and is
    /// filled in later, once a `[Revise]` batch actually claims the comment.
    pub fn create_nudge_thread_entry(&self, comment_id: &str, body: &str) -> Result<CommentThreadEntry> {
        let conn = self.connect()?;
        let id = next_id("cte");
        let now = now_string();
        conn.execute(
            "INSERT INTO comment_thread_entries \
             (id, comment_id, entry_kind, author, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                comment_id,
                THREAD_ENTRY_KIND_NUDGE,
                THREAD_ENTRY_AUTHOR_ENGINE,
                body,
                now
            ],
        )?;
        let cols = Self::comment_thread_entry_columns();
        let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1");
        conn.query_row(&sql, [&id], map_comment_thread_entry)
            .map_err(Into::into)
    }

    /// Thread entries for a comment, oldest first — the chronological order
    /// the sidebar renders them in.
    pub fn list_thread_entries_for_comment(&self, comment_id: &str) -> Result<Vec<CommentThreadEntry>> {
        let conn = self.connect()?;
        let cols = Self::comment_thread_entry_columns();
        let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE comment_id = ?1 ORDER BY created_at, id");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([comment_id], map_comment_thread_entry)?;
        collect_rows(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput};
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
            body: "this typo should say beta".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap()
        .id
    }

    #[test]
    fn create_nudge_entry_round_trips() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let entry = db
            .create_nudge_thread_entry(
                &comment,
                "This looks like it wants a doc change — click [Revise] to start one.",
            )
            .unwrap();
        assert_eq!(entry.comment_id, comment);
        assert_eq!(entry.entry_kind, "nudge");
        assert_eq!(entry.author, "engine");
        assert!(entry.revise_task_id.is_none());
        assert!(entry.answer_agent_run_id.is_none());

        let entries = db.list_thread_entries_for_comment(&comment).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, entry.id);
    }

    #[test]
    fn entries_list_oldest_first() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let first = db.create_nudge_thread_entry(&comment, "first").unwrap();
        let second = db.create_nudge_thread_entry(&comment, "second").unwrap();

        let entries = db.list_thread_entries_for_comment(&comment).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first.id);
        assert_eq!(entries[1].id, second.id);
    }
}
