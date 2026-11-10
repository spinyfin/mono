//! An immutable, migrated schema for independent storage test fixtures.
use std::{
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, backup};
use tempfile::TempDir;

use crate::WorkDb;

static TEMPLATE: LazyLock<TempDir> = LazyLock::new(|| {
    let directory = TempDir::new().unwrap();
    // Use production startup, including every migration and runtime seed.
    // Closing the only connection checkpoints WAL before anyone copies it.
    // Not `WorkDb::open`: that would seed this new file from this template.
    let db = WorkDb::open_and_migrate(directory.path().join("schema.db")).unwrap();
    drop(db);
    directory
});

pub(crate) fn copy_to(path: &Path) {
    std::fs::copy(TEMPLATE.path().join("schema.db"), path).unwrap();
}

/// Give a database file that does not exist yet the migrated schema, so
/// opening it replays no migrations. Returns false, leaving the path alone,
/// whenever startup has to see what is there instead: a database, a leftover
/// journal, or a location the copy could not be written to.
pub(crate) fn seed_new_file_db(path: &Path) -> bool {
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let present = |path: PathBuf| path.symlink_metadata().is_ok();
    if present(path.to_path_buf()) || ["-wal", "-shm", "-journal"].into_iter().any(|s| present(beside(s))) {
        return false;
    }
    static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);
    let staged = beside(&format!(
        ".seed-{}-{}",
        std::process::id(),
        NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed)
    ));
    // Linking never replaces a file, and the copy is complete before it is
    // linked: of two openers racing on one new path exactly one seeds it, and
    // the other runs startup against a whole database.
    let seeded = std::fs::copy(TEMPLATE.path().join("schema.db"), &staged)
        .and_then(|_| std::fs::hard_link(&staged, path))
        .is_ok();
    let _ = std::fs::remove_file(&staged);
    seeded
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

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Every schema object's definition, then every applied migration id.
    fn schema_and_ledger(connection: &Connection) -> Vec<String> {
        connection
            .prepare(
                "SELECT type || ' ' || name || ' ' || COALESCE(sql, '') FROM sqlite_master
                 UNION ALL
                 SELECT 'migration ' || id FROM schema_migrations
                 ORDER BY 1",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn opening_a_new_file_starts_from_the_migrated_schema_and_reopening_runs_startup() {
        let dir = TempDir::new().unwrap();
        // A parent that does not exist yet is still created by `open`.
        let path = dir.path().join("nested/state.db");
        let first = WorkDb::open(path.clone()).unwrap();
        let second = WorkDb::open(dir.path().join("other.db")).unwrap();
        assert_eq!(first.path(), path);
        first.set_metadata("fixture-only", "first").unwrap();
        assert_eq!(second.get_metadata("fixture-only").unwrap(), None);
        let connection = first.connect().unwrap();
        let journal: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let foreign_keys: bool = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert!(foreign_keys);
        // The copy is what startup builds: the same schema objects and the
        // same migration ledger as a database that was started cold.
        let cold = WorkDb::open_in_memory().unwrap();
        assert!(!schema_and_ledger(&connection).is_empty());
        assert_eq!(
            schema_and_ledger(&connection),
            schema_and_ledger(&cold.connect().unwrap())
        );
        drop(connection);
        drop(first);
        assert_eq!(
            names_in(path.parent().unwrap()),
            ["state.db"],
            "no staging copy is left beside it"
        );
        // The file exists now, so this open is real startup over the same data.
        let reopened = WorkDb::open(path).unwrap();
        assert_eq!(reopened.get_metadata("fixture-only").unwrap().as_deref(), Some("first"));
    }

    #[test]
    fn a_path_that_already_holds_something_is_left_for_real_startup() {
        let dir = TempDir::new().unwrap();
        // An empty file is an empty database, which startup has to build.
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        assert!(!seed_new_file_db(&empty));
        assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0, "the file is not replaced");
        let db = WorkDb::open(empty).unwrap();
        assert!(!schema_and_ledger(&db.connect().unwrap()).is_empty());
        // A journal without its database is startup's to interpret, too.
        for suffix in ["-wal", "-shm", "-journal"] {
            let orphan = dir.path().join(format!("orphan{suffix}.db"));
            let journal = dir.path().join(format!("orphan{suffix}.db{suffix}"));
            std::fs::write(&journal, b"").unwrap();
            assert!(!seed_new_file_db(&orphan), "{suffix}");
            assert!(!orphan.exists(), "{suffix}");
        }
        // Nowhere to write the copy: `open` falls through and reports it.
        assert!(!seed_new_file_db(&dir.path().join("missing/state.db")));
    }

    #[test]
    fn openers_racing_on_one_new_path_seed_it_once_and_all_see_a_whole_database() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        let seeded: usize = std::thread::scope(|scope| {
            let racers: Vec<_> = (0..16).map(|_| scope.spawn(|| seed_new_file_db(&path))).collect();
            racers.into_iter().map(|racer| usize::from(racer.join().unwrap())).sum()
        });
        assert_eq!(seeded, 1);
        assert_eq!(names_in(dir.path()), ["state.db"]);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(TEMPLATE.path().join("schema.db")).unwrap()
        );

        let shared = dir.path().join("shared.db");
        std::thread::scope(|scope| {
            let openers: Vec<_> = (0..4)
                .map(|n| {
                    let shared = &shared;
                    scope.spawn(move || {
                        let db = WorkDb::open(shared.clone()).unwrap();
                        db.set_metadata(&format!("opener-{n}"), "seen").unwrap();
                    })
                })
                .collect();
            for opener in openers {
                opener.join().unwrap();
            }
        });
        let db = WorkDb::open(shared).unwrap();
        for n in 0..4 {
            assert_eq!(
                db.get_metadata(&format!("opener-{n}")).unwrap().as_deref(),
                Some("seen")
            );
        }
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
