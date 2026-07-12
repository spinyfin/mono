//! DB operations for the `editorial_actions` audit table.
//!
//! Every PreToolUse hook decision (allow / rewrite / deny) on a
//! `gh pr|issue` invocation is written here via
//! [`WorkDb::insert_editorial_action`] and read back via
//! [`WorkDb::list_editorial_actions`].

use super::*;

/// Maximum `tool_command` length stored in the DB. Longer commands are
/// truncated with a trailing `…` marker so a runaway body-inline command
/// can't cause a multi-MB row.
const COMMAND_MAX_BYTES: usize = 4096;

/// Default row limit for [`WorkDb::list_editorial_actions`].
pub const LIST_EDITORIAL_ACTIONS_DEFAULT_LIMIT: u32 = 50;

impl WorkDb {
    /// Load the product_id, compiled EditorialRules, and workspace_path for
    /// an execution in one synchronous DB round-trip. Used by the PreToolUse
    /// audit handler.
    ///
    /// Returns `("", default_rules, None)` when the execution or product does
    /// not exist — the caller should treat that as "skip, no product to audit
    /// against."
    pub fn get_editorial_context(
        &self,
        execution_id: &str,
    ) -> Result<(String, boss_protocol::EditorialRules, Option<String>)> {
        let conn = self.connect()?;
        let row: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT p.id, p.editorial_rules, e.workspace_path
                   FROM work_executions e
                   JOIN tasks t ON t.id = e.work_item_id
                   JOIN products p ON p.id = t.product_id
                  WHERE e.id = ?1",
                [execution_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (product_id, rules_json, workspace_path) = row.unwrap_or_default();
        let rules = rules_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<boss_protocol::EditorialRules>(s).ok())
            .unwrap_or_default();
        Ok((product_id, rules, workspace_path))
    }

    /// Insert one row into `editorial_actions` and return the new row id.
    /// `tool_command` is truncated to [`COMMAND_MAX_BYTES`].
    pub fn insert_editorial_action(
        &self,
        product_id: &str,
        execution_id: &str,
        pr_url: Option<&str>,
        tool_command: &str,
        action: &str,
        reason: Option<&str>,
    ) -> Result<i64> {
        let conn = self.connect()?;
        let created_at = now_string();
        let truncated_command = truncate_command(tool_command);
        conn.execute(
            "INSERT INTO editorial_actions
                 (product_id, execution_id, pr_url, tool_command, action, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                product_id,
                execution_id,
                pr_url,
                truncated_command,
                action,
                reason,
                created_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return up to `limit` editorial-action rows for `product_id`, ordered
    /// freshest first (`created_at DESC`). Optionally filter by `pr_url`
    /// prefix/exact-match when the caller passes `--pr`. `limit` defaults to
    /// [`LIST_EDITORIAL_ACTIONS_DEFAULT_LIMIT`] when `None`.
    pub fn list_editorial_actions(
        &self,
        product_id: &str,
        limit: Option<u32>,
        pr_url_filter: Option<&str>,
    ) -> Result<Vec<boss_protocol::EditorialAction>> {
        let conn = self.connect()?;
        let cap = limit.unwrap_or(LIST_EDITORIAL_ACTIONS_DEFAULT_LIMIT) as i64;
        let rows = if let Some(pr_url) = pr_url_filter {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, execution_id, pr_url, tool_command, action, reason, created_at
                   FROM editorial_actions
                  WHERE product_id = ?1 AND pr_url = ?2
                  ORDER BY created_at DESC, id DESC
                  LIMIT ?3",
            )?;
            collect_rows(stmt.query_map(params![product_id, pr_url, cap], map_editorial_action)?)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, execution_id, pr_url, tool_command, action, reason, created_at
                   FROM editorial_actions
                  WHERE product_id = ?1
                  ORDER BY created_at DESC, id DESC
                  LIMIT ?2",
            )?;
            collect_rows(stmt.query_map(params![product_id, cap], map_editorial_action)?)?
        };
        Ok(rows)
    }
}

fn map_editorial_action(row: &Row<'_>) -> rusqlite::Result<boss_protocol::EditorialAction> {
    let id: i64 = row.get(0)?;
    Ok(boss_protocol::EditorialAction::builder()
        .id(id.to_string())
        .product_id(row.get::<_, String>(1)?)
        .execution_id(row.get::<_, String>(2).unwrap_or_default())
        .maybe_pr_url(row.get::<_, Option<String>>(3)?)
        .tool_command(row.get::<_, String>(4)?)
        .action(row.get::<_, String>(5)?)
        .reason(row.get::<_, String>(6).unwrap_or_default())
        .created_at(row.get::<_, String>(7)?)
        .build())
}

fn truncate_command(cmd: &str) -> String {
    if cmd.len() <= COMMAND_MAX_BYTES {
        return cmd.to_owned();
    }
    // Truncate at a UTF-8 boundary.
    let truncated = cmd
        .char_indices()
        .take_while(|(i, _)| *i < COMMAND_MAX_BYTES - 1)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &cmd[..truncated])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trailing marker `truncate_command` appends when it cuts a command.
    const ELLIPSIS: char = '…';

    #[test]
    fn short_command_returned_verbatim() {
        let cmd = "gh pr view 42";
        let out = truncate_command(cmd);
        assert_eq!(out, cmd);
        assert!(!out.ends_with(ELLIPSIS));
    }

    #[test]
    fn command_exactly_at_limit_is_not_truncated() {
        let cmd = "a".repeat(COMMAND_MAX_BYTES);
        assert_eq!(cmd.len(), COMMAND_MAX_BYTES);
        let out = truncate_command(&cmd);
        // Exactly at the limit is the boundary case for `<=`: it must be
        // returned unchanged, with no ellipsis.
        assert_eq!(out, cmd);
        assert!(!out.ends_with(ELLIPSIS));
    }

    #[test]
    fn long_ascii_command_is_truncated_with_ellipsis() {
        let cmd = "a".repeat(COMMAND_MAX_BYTES + 500);
        let out = truncate_command(&cmd);
        // A too-long command gets an ellipsis appended...
        assert!(out.ends_with(ELLIPSIS));
        assert_ne!(out, cmd);
        // ...and the body preceding it is a prefix of the original input.
        let body = out.strip_suffix(ELLIPSIS).unwrap();
        assert!(cmd.starts_with(body));
        // The stored byte length stays bounded: at most the cap plus the
        // 3-byte ellipsis (it never blows past the intended size).
        assert!(out.len() <= COMMAND_MAX_BYTES + ELLIPSIS.len_utf8());
    }

    #[test]
    fn truncation_in_middle_of_multibyte_char_stays_valid_utf8() {
        // Pack ASCII up to just before the cut point, then multibyte chars,
        // so the naive byte cut at `COMMAND_MAX_BYTES - 1` would land in the
        // middle of an emoji. The boundary logic must back off to a whole
        // char instead of slicing mid-codepoint (which would panic).
        let mut cmd = "a".repeat(COMMAND_MAX_BYTES - 2);
        // Each 😀 is 4 bytes; append enough to push well past the limit.
        for _ in 0..10 {
            cmd.push('😀');
        }
        assert!(cmd.len() > COMMAND_MAX_BYTES);

        // Must not panic on the mid-codepoint slice.
        let out = truncate_command(&cmd);

        // The result is a valid Rust String, so it is valid UTF-8 by
        // construction; assert the observable behavior instead: it ends with
        // the ellipsis and its body is a whole-char prefix of the input.
        assert!(out.ends_with(ELLIPSIS));
        let body = out.strip_suffix(ELLIPSIS).unwrap();
        assert!(cmd.starts_with(body));
        // No partial emoji leaked through: every char in the body is intact.
        assert!(body.chars().all(|c| c == 'a' || c == '😀'));
    }

    #[test]
    fn truncation_with_accented_chars_near_limit_does_not_panic() {
        // Two-byte chars (é = 2 bytes) straddling the cut point exercise the
        // same boundary logic with a different codepoint width. Placing the
        // first é at byte 4094 means the naive cut at COMMAND_MAX_BYTES - 1
        // (byte 4095) lands inside it.
        let mut cmd = "a".repeat(COMMAND_MAX_BYTES - 2);
        for _ in 0..10 {
            cmd.push('é');
        }
        assert!(cmd.len() > COMMAND_MAX_BYTES);

        let out = truncate_command(&cmd);

        assert!(out.ends_with(ELLIPSIS));
        let body = out.strip_suffix(ELLIPSIS).unwrap();
        assert!(cmd.starts_with(body));
        assert!(out.len() <= COMMAND_MAX_BYTES + ELLIPSIS.len_utf8());
    }
}
