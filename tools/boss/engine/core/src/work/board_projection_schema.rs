//! Schema 56: bounded, transactionally ordered board input changes.
//! The journal is a recovery aid, not an event bus: readers that miss its
//! retained window rebuild once from a consistent snapshot. Run inputs reuse
//! schema 49's exhaustive runtime epochs rather than duplicate run triggers.
use super::*;

pub(super) const JOURNAL_LIMIT: u64 = 4096;

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
                body.push_str(&format!(
                    "INSERT INTO board_changes(item_id, source) VALUES ({row}.{column}, '{table}');"
                ));
                if table == "tasks" {
                    // Keep the severed edge even for a hard deletion, reparent,
                    // or move across products before any reader sees it.
                    body.push_str(&format!("INSERT INTO board_changes(item_id, source) SELECT {row}.parent_task_id, 'tasks' WHERE {row}.parent_task_id IS NOT NULL;"));
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
        "pr_review_batch_members",
        "pr_review_guide_source_series",
        "pr_review_guide_source_comparisons",
        "pr_review_guide_versions",
        "pr_review_guide_attempts",
        "pr_review_guide_updates",
        "pr_review_guide_revision_heads",
        "pr_review_guide_automatic_attempts",
    ] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS board_changes_{table}_{operation} AFTER {operation} ON {table} BEGIN INSERT INTO board_changes(source) VALUES ('{table}'); END;"))?;
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
