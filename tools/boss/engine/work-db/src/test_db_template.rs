//! An immutable, migrated schema for independent storage test fixtures.
use std::{path::Path, sync::LazyLock};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, backup};
use tempfile::TempDir;

use crate::WorkDb;

static TEMPLATE: LazyLock<TempDir> = LazyLock::new(|| {
    let directory = TempDir::new().unwrap();
    // Use production startup, including every migration and runtime seed.
    // Closing the only connection checkpoints WAL before anyone copies it.
    let db = WorkDb::open(directory.path().join("schema.db")).unwrap();
    drop(db);
    directory
});

pub(super) fn copy_to(path: &Path) {
    std::fs::copy(TEMPLATE.path().join("schema.db"), path).unwrap();
}

pub(crate) fn open_memory_db() -> Result<WorkDb> {
    let db = WorkDb::empty_in_memory()?;
    // Each caller gets its own read-only source connection, so copying the
    // template does not serialize otherwise independent tests on a mutex.
    let source = Connection::open_with_flags(TEMPLATE.path().join("schema.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut destination = db.connect()?;
    let copy = backup::Backup::new(&source, &mut destination)?;
    anyhow::ensure!(copy.step(-1)? == backup::StepResult::Done, "copy test schema");
    drop(copy);
    drop(destination);
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_fixtures_keep_wal_constraints_and_independent_state() {
        let (first_dir, first) = crate::test_support::open_db();
        let (_second_dir, second) = crate::test_support::open_db();
        first.set_metadata("fixture-only", "first").unwrap();
        assert_eq!(second.get_metadata("fixture-only").unwrap(), None);
        let connection = first.connect().unwrap();
        let journal: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let error = connection.execute("INSERT INTO projects (id, product_id, name, slug, status, priority, created_at, updated_at) VALUES ('orphan', 'missing', 'orphan', 'orphan', 'planned', 'medium', '1', '1')", []).unwrap_err();
        let rusqlite::Error::SqliteFailure(code, _) = error else {
            panic!("expected foreign key violation: {error}");
        };
        assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
        drop(connection);
        drop(first);
        let reopened = WorkDb::open_existing(first_dir.path().join("state.db")).unwrap();
        assert_eq!(reopened.get_metadata("fixture-only").unwrap().as_deref(), Some("first"));
    }

    #[test]
    fn memory_fixtures_keep_shared_connections_without_sharing_test_state() {
        let first = WorkDb::open(":memory:".into()).unwrap();
        let second = WorkDb::open(":memory:".into()).unwrap();
        assert!(first.is_in_memory());
        first.set_metadata("fixture-only", "first").unwrap();
        assert_eq!(second.get_metadata("fixture-only").unwrap(), None);
        let reader = first.connect_new().unwrap();
        let value: String = reader
            .query_row("SELECT value FROM metadata WHERE key = 'fixture-only'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(value, "first");
        let foreign_keys: bool = reader.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
        assert!(foreign_keys);
        drop(reader);
        first.init().unwrap();
        assert_eq!(first.get_metadata("fixture-only").unwrap().as_deref(), Some("first"));
    }
}
