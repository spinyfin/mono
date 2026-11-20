//! Additive storage for cancellation attribution; existing writers remain valid.

use super::{Connection, Result, table_has_column};

pub(super) const MIGRATION_ID: &str = "20261120_1032_cancel_provenance_columns";
const COLUMNS: [&str; 3] = ["cancel_source", "cancel_actor", "cancel_reason"];

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for column in COLUMNS {
        if !table_has_column(conn, "work_executions", column)? {
            conn.execute_batch(&format!("ALTER TABLE work_executions ADD COLUMN {column} TEXT"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_schema_and_replay_preserve_data_and_allow_unattributed_writes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE work_executions (id TEXT PRIMARY KEY, status TEXT, cancel_source TEXT);
             INSERT INTO work_executions VALUES ('old', 'cancelled', 'app');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "UPDATE work_executions SET cancel_actor = 'operator', cancel_reason = 'stop' WHERE id = 'old';
             INSERT INTO work_executions (id, status) VALUES ('new', 'running');
             UPDATE work_executions SET status = 'abandoned' WHERE id = 'new';",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let stored: (String, String, String) = conn
            .query_row(
                "SELECT cancel_source, cancel_actor, cancel_reason FROM work_executions WHERE id = 'old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, ("app".into(), "operator".into(), "stop".into()));
        let absent: bool = conn
            .query_row(
                "SELECT cancel_source IS NULL AND cancel_actor IS NULL AND cancel_reason IS NULL
                 FROM work_executions WHERE id = 'new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(absent);
    }

    #[test]
    fn current_database_upgrades_without_changing_existing_cancellations() {
        let (_dir, db) = crate::test_support::open_db();
        let path = db.path().to_path_buf();
        {
            let conn = db.connect().unwrap();
            for column in COLUMNS {
                assert!(table_has_column(&conn, "work_executions", column).unwrap());
                conn.execute_batch(&format!("ALTER TABLE work_executions DROP COLUMN {column}"))
                    .unwrap();
            }
            conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
                .unwrap();
        }
        drop(db);
        let upgraded = crate::WorkDb::open(path.clone()).unwrap();
        let conn = upgraded.connect().unwrap();
        for column in COLUMNS {
            assert!(table_has_column(&conn, "work_executions", column).unwrap());
        }
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = ?1",
                [MIGRATION_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let migration = super::super::registry::all()
            .into_iter()
            .find(|entry| entry.id == MIGRATION_ID)
            .unwrap();
        assert_eq!(migration.legacy_version, None);
        drop(conn);
        drop(upgraded);
        crate::WorkDb::open(path).unwrap();
    }
}
