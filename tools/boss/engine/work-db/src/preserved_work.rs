use crate::schema_init::table_has_column;
use crate::*;
use boss_engine_recovery::execution_bookmark::PreservedTip;

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    for (column, ty) in [
        ("preserved_change_id", "TEXT"),
        ("preserved_commit_id", "TEXT"),
        ("preserved_commit_count", "INTEGER"),
    ] {
        if !table_has_column(conn, "work_executions", column)? {
            conn.execute_batch(&format!("ALTER TABLE work_executions ADD COLUMN {column} {ty}"))?;
        }
    }
    Ok(())
}

#[test]
fn upgrade_retains_old_rows_without_inventing_preserved_work() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE work_executions (id TEXT PRIMARY KEY, status TEXT);
        INSERT INTO work_executions VALUES ('old', 'orphaned');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap();
    let row = conn.query_row("SELECT status, preserved_change_id, preserved_commit_id, preserved_commit_count FROM work_executions WHERE id = 'old'", [], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<u64>>(3)?))
    }).unwrap();
    assert_eq!(row, ("orphaned".into(), None, None, None));
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Store location remains useful after the legacy pointer row is retired.
    pub fn preservation_source(
        &self,
        execution_id: &str,
    ) -> Result<Option<boss_engine_recovery::execution_bookmark::ExecutionBookmark>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT repo_path, host_id, base_commit FROM execution_bookmarks WHERE execution_id = ?1",
                [execution_id],
                |row| {
                    Ok(boss_engine_recovery::execution_bookmark::ExecutionBookmark {
                        execution_id: execution_id.to_owned(),
                        repo_path: PathBuf::from(row.get::<_, String>(0)?),
                        host_id: row.get(1)?,
                        base_commit: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn record_preserved_tip(&self, execution_id: &str, tip: &PreservedTip) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE work_executions SET preserved_change_id = ?2, preserved_commit_id = ?3,
                preserved_commit_count = ?4 WHERE id = ?1",
            params![execution_id, tip.change_id, tip.commit_id, tip.count],
        )?;
        anyhow::ensure!(changed == 1, "execution missing for preserved work");
        Ok(())
    }

    pub fn clear_preserved_tip(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE work_executions SET preserved_change_id = NULL, preserved_commit_id = NULL,
                preserved_commit_count = NULL WHERE id = ?1",
            params![execution_id],
        )?;
        Ok(())
    }

    pub fn preserved_tip(&self, execution_id: &str) -> Result<Option<PreservedTip>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT preserved_change_id, preserved_commit_id, preserved_commit_count
                FROM work_executions WHERE id = ?1 AND preserved_commit_id IS NOT NULL",
                [execution_id],
                |row| {
                    Ok(PreservedTip {
                        change_id: row.get(0)?,
                        commit_id: row.get(1)?,
                        count: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }
}
