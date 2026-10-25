//! Schema 56: bounded, transactionally ordered board input changes.
//! The journal is a recovery aid, not an event bus: readers that miss its
//! retained window rebuild once from a consistent snapshot. Run inputs reuse
//! schema 49's exhaustive runtime epochs rather than duplicate run triggers.
use super::*;

pub(crate) const JOURNAL_LIMIT: u64 = 4096;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS board_changes (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            item_id TEXT,
            source TEXT NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS board_changes_bound AFTER INSERT ON board_changes
         BEGIN DELETE FROM board_changes WHERE sequence <= NEW.sequence - {JOURNAL_LIMIT}; END;"
    ))?;
    for (table, column) in [
        ("tasks", "id"),
        ("board_runtime_epoch", "work_item_id"),
        ("work_attachments", "work_item_id"),
        ("worker_proposals", "work_item_id"),
        ("pr_review_verdicts", "work_item_id"),
        ("operator_questions", "work_item_id"),
    ] {
        for (operation, rows) in [
            ("INSERT", vec!["NEW"]),
            ("UPDATE", vec!["OLD", "NEW"]),
            ("DELETE", vec!["OLD"]),
        ] {
            let mut body = String::new();
            for row in rows {
                let distinct = if operation == "UPDATE" && row == "OLD" {
                    format!(" AND OLD.{column} IS NOT NEW.{column}")
                } else {
                    String::new()
                };
                body.push_str(&format!(
                    "INSERT INTO board_changes(item_id, source) SELECT {row}.{column}, '{table}' WHERE {row}.{column} IS NOT NULL{distinct};"
                ));
                if table == "tasks" {
                    // Keep the severed edge even for a hard deletion, reparent,
                    // or move across products before any reader sees it.
                    let distinct = if operation == "UPDATE" && row == "OLD" {
                        " AND OLD.parent_task_id IS NOT NEW.parent_task_id"
                    } else {
                        ""
                    };
                    body.push_str(&format!("INSERT INTO board_changes(item_id, source) SELECT {row}.parent_task_id, 'tasks' WHERE {row}.parent_task_id IS NOT NULL{distinct};"));
                }
            }
            conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS board_changes_{table}_{operation} AFTER {operation} ON {table} BEGIN {body} END;"))?;
        }
    }
    // Every non-card input read by complete board assembly or its derivations.
    // These less frequent changes invalidate the whole projection. In
    // particular dependencies can gate cards in a DIFFERENT product.
    for table in [
        "products",
        "projects",
        "ideas",
        "work_item_dependencies",
        "work_attention_items",
        "attention_groups",
        "attentions",
        "planner_runs",
    ] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS board_changes_{table}_{operation} AFTER {operation} ON {table} BEGIN INSERT INTO board_changes(source) VALUES ('{table}'); END;"))?;
        }
    }
    // Journal review badge and guide-update status inputs at their owning root.
    // Job-only bookkeeping does not affect card derivations.
    for (table, columns, select) in [
        (
            "pr_review_batch_members",
            "execution_id, role",
            "SELECT work_item_id FROM work_executions WHERE id = ROW.execution_id",
        ),
        (
            "pr_review_guide_source_series",
            "id, root_task_id, canonical_pr_url, guide_lifecycle, readable_version_id, selected_comparison_id, latest_observation_sequence, last_capture_error, request_epoch",
            "SELECT ROW.root_task_id",
        ),
        (
            "pr_review_guide_revision_heads",
            "root_task_id, revision_task_id, observation_sequence, head_sha, pr_url",
            "SELECT ROW.root_task_id",
        ),
        (
            "pr_review_guide_updates",
            "attempt_id",
            "SELECT s.root_task_id FROM pr_review_guide_attempts a JOIN pr_review_guide_source_series s ON s.id = a.series_id WHERE a.id = ROW.attempt_id",
        ),
        (
            "pr_review_guide_source_comparisons",
            "id, series_id, head_sha",
            "SELECT root_task_id FROM pr_review_guide_source_series WHERE id = ROW.series_id",
        ),
        (
            "pr_review_guide_versions",
            "id, comparison_id",
            "SELECT root_task_id FROM pr_review_guide_source_series WHERE readable_version_id = ROW.id",
        ),
        (
            "pr_review_guide_attempts",
            "id, series_id, status, error, finished_at, request_epoch, comparison_id, prompt_version",
            "SELECT root_task_id FROM pr_review_guide_source_series WHERE id = ROW.series_id",
        ),
    ] {
        for (operation, rows) in [
            ("INSERT", vec!["NEW"]),
            ("UPDATE", vec!["OLD", "NEW"]),
            ("DELETE", vec!["OLD"]),
        ] {
            let event = if operation == "UPDATE" {
                format!("UPDATE OF {columns}")
            } else {
                operation.to_owned()
            };
            let selects = rows
                .iter()
                .map(|row| select.replace("ROW", row))
                .collect::<Vec<_>>()
                .join(" UNION ");
            conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS board_changes_{table}_{operation} AFTER {event} ON {table} BEGIN INSERT INTO board_changes(item_id, source) SELECT value, '{table}' FROM (WITH affected(value) AS ({selects}) SELECT value FROM affected) WHERE value IS NOT NULL; END;"))?;
        }
    }
    // Board review badges consult this migration watermark. Scheduler and
    // coordinator metadata do not affect the board and must not force scans.
    let key = super::review_verdicts::PR_REVIEW_VERDICTS_SINCE_METADATA_KEY;
    for (operation, condition) in [
        ("INSERT", format!("NEW.key = '{key}'")),
        ("UPDATE", format!("OLD.key = '{key}' OR NEW.key = '{key}'")),
        ("DELETE", format!("OLD.key = '{key}'")),
    ] {
        conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS board_changes_metadata_{operation} AFTER {operation} ON metadata WHEN {condition} BEGIN INSERT INTO board_changes(source) VALUES ('metadata'); END;"))?;
    }
    Ok(())
}

/// Replace schema 56 triggers without discarding its recovery watermark.
pub(super) fn refine_triggers(conn: &Connection) -> Result<()> {
    let names: Vec<String> = collect_rows(conn.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'board_changes_%' AND name != 'board_changes_bound'")?.query_map([], |row| row.get(0))?)?;
    for name in names {
        conn.execute_batch(&format!("DROP TRIGGER [{name}];"))?;
    }
    migrate(conn)
}
