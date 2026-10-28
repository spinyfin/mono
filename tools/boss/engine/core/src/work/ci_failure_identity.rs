use super::schema_init::table_has_column;
use super::*;

/// Preserve historical attempts while allowing another check on the same head
/// to fail independently. Queue episodes retain their existing empty check key.
pub(super) fn migrate(tx: &rusqlite::Connection) -> Result<()> {
    if table_has_column(tx, "ci_remediations", "failure_check")? {
        return Ok(());
    }
    tx.execute_batch("ALTER TABLE ci_remediations ADD COLUMN failure_check TEXT NOT NULL DEFAULT ''")?;
    let schema: String = tx.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'ci_remediations'",
        [],
        |row| row.get(0),
    )?;
    let indexes = tx
        .prepare(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'ci_remediations' AND sql IS NOT NULL",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let schema = schema
        .replacen("CREATE TABLE ci_remediations", "CREATE TABLE ci_remediations_new", 1)
        .replace(
            "UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)",
            "UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind, failure_check)",
        );
    tx.execute_batch(&schema)?;
    tx.execute_batch(
        "INSERT INTO ci_remediations_new SELECT * FROM ci_remediations;
        DROP TABLE ci_remediations;
        ALTER TABLE ci_remediations_new RENAME TO ci_remediations;",
    )?;
    for index in indexes {
        tx.execute_batch(&index)?;
    }
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Include terminal and pre-migration rows: observing an old failure again
    /// must not create another revision or consume another budget slot.
    pub(crate) fn ci_check_attempted(&self, work_item_id: &str, head: &str, check: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ci_remediations c,
                 json_each(CASE WHEN json_valid(c.failed_checks) THEN c.failed_checks ELSE '[]' END) f
              WHERE c.work_item_id = ?1 AND c.head_sha_at_trigger = ?2
                AND c.failure_kind = 'pr_branch_ci' AND c.attempt_kind = 'fix'
                AND json_extract(CASE WHEN json_valid(f.value) THEN f.value ELSE '{}' END, '$.name') = ?3)",
            params![work_item_id, head, check],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_rows_indexes_and_independent_check_keys() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ci_remediations (
            id TEXT PRIMARY KEY, work_item_id TEXT, head_sha_at_trigger TEXT,
            attempt_kind TEXT, failed_checks TEXT,
            UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind));
            CREATE INDEX ci_remediations_work_item_idx ON ci_remediations(work_item_id);
            INSERT INTO ci_remediations VALUES ('old', 'task', 'head', 'fix', '[]');",
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        migrate(&tx).unwrap();
        migrate(&tx).unwrap();
        let count: i64 = tx
            .query_row("SELECT count(*) FROM ci_remediations WHERE id = 'old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let index: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'ci_remediations_work_item_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
        for check in ["mac-app-build", "bazel-build-test"] {
            tx.execute(
                "INSERT INTO ci_remediations VALUES (?1, 'task', 'head', 'fix', '[]', ?1)",
                [check],
            )
            .unwrap();
        }
        assert!(
            tx.execute(
                "INSERT INTO ci_remediations VALUES ('duplicate', 'task', 'head', 'fix', '[]', 'mac-app-build')",
                []
            )
            .is_err()
        );
        tx.commit().unwrap();
    }
}
