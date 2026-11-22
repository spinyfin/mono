//! Where a database file's bytes are: its largest tables and indexes, and
//! the pages no table uses.
//!
//! Measured from a finished backup snapshot, never from the live database:
//! the measurement visits every page of the file it is given, and a snapshot
//! is a private, consistent copy that no engine connection is waiting on.
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// One table or index and the bytes of the pages it owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectSize {
    pub name: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeReport {
    pub page_size: u64,
    pub pages: u64,
    /// Pages on the freelist. The engine never runs `VACUUM` on its live
    /// database and `auto_vacuum` is off, so pages freed by a delete stay in
    /// the file until an insert reuses them. A snapshot is written compacted,
    /// so this is zero for one; read it from the live database instead.
    pub free_pages: u64,
    /// Largest first.
    pub objects: Vec<ObjectSize>,
}

impl SizeReport {
    pub fn total_bytes(&self) -> u64 {
        self.pages * self.page_size
    }

    /// `name=<MiB>` for the `top` largest objects, for a log line.
    pub fn summary(&self, top: usize) -> String {
        self.objects
            .iter()
            .take(top)
            .map(|object| format!("{}={:.1}MiB", object.name, object.bytes as f64 / 1_048_576.0))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Measure the database file at `path`, which must not be in use by a
/// writer: it is opened immutable, with no locks and no journal.
pub fn measure_snapshot(path: &Path) -> Result<SizeReport> {
    let text = path
        .to_str()
        .with_context(|| format!("snapshot path is not valid UTF-8: {}", path.display()))?;
    // Only these characters are special in an SQLite URI filename.
    let escaped = text.replace('%', "%25").replace('?', "%3f").replace('#', "%23");
    let conn = Connection::open_with_flags(
        format!("file:{escaped}?immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening snapshot {} to measure it", path.display()))?;
    measure(&conn)
}

fn measure(conn: &Connection) -> Result<SizeReport> {
    let pragma = |name: &str| -> Result<u64> { Ok(conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?) };
    let mut stmt =
        conn.prepare("SELECT name, SUM(pgsize) AS bytes FROM dbstat GROUP BY name ORDER BY bytes DESC, name")?;
    let objects = stmt
        .query_map([], |row| {
            Ok(ObjectSize {
                name: row.get(0)?,
                bytes: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SizeReport {
        page_size: pragma("page_size")?,
        pages: pragma("page_count")?,
        free_pages: pragma("freelist_count")?,
        objects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_largest_objects_come_first_and_account_for_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory name with URI-special characters and a space.
        let nested = dir.path().join("state #1 100%?");
        std::fs::create_dir(&nested).unwrap();
        let path = nested.join("snapshot.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE big(id INTEGER PRIMARY KEY, filler TEXT);
             CREATE TABLE small(id INTEGER PRIMARY KEY, tag TEXT);
             CREATE INDEX small_tag ON small(tag);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400)
             INSERT INTO big SELECT i, printf('%.3000c', 'x') FROM n;
             INSERT INTO small SELECT id, 'tag' || id FROM big;",
        )
        .unwrap();
        drop(conn);

        let report = measure_snapshot(&path).unwrap();
        assert_eq!(report.objects[0].name, "big");
        assert!(report.objects[0].bytes > 400 * 3000, "{report:?}");
        assert!(report.objects.iter().any(|object| object.name == "small_tag"));
        let owned: u64 = report.objects.iter().map(|object| object.bytes).sum();
        assert_eq!(
            owned,
            report.total_bytes(),
            "every page of a compact file belongs to a table or index"
        );
        assert_eq!(report.free_pages, 0);
        assert!(report.summary(1).starts_with("big="), "{}", report.summary(1));
        assert!(!report.summary(1).contains("small"));
    }
}
