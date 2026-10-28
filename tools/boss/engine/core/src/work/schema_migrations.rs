//! Named migration bookkeeping inside the engine's startup transaction.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction};

pub(super) struct Migration {
    pub id: String,
    // Only the frozen integer history has a version. New entries use None.
    pub legacy_version: Option<u32>,
    pub apply: fn(&Connection) -> Result<()>,
}

pub(super) const CREATE_LEDGER: &str = "CREATE TABLE schema_migrations (
    id TEXT PRIMARY KEY NOT NULL,
    applied_at TEXT NOT NULL
)";

pub(super) fn applied(conn: &Connection) -> Result<BTreeSet<String>> {
    Ok(conn
        .prepare("SELECT id FROM schema_migrations ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn record(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_migrations (id, applied_at) VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        [id],
    )?;
    Ok(())
}

/// The caller commits only after all migrations and startup data succeed.
/// Bootstrap is permitted only when no ledger exists, never on ordinary opens.
pub(super) fn apply(tx: &Transaction<'_>, migrations: &[Migration], legacy: Option<u32>) -> Result<()> {
    let mut known = BTreeMap::new();
    for migration in migrations {
        ensure!(
            known.insert(migration.id.as_str(), migration).is_none(),
            "duplicate database migration id: {}",
            migration.id
        );
    }
    if let Some(version) = legacy {
        let last = migrations.iter().filter_map(|m| m.legacy_version).max().unwrap_or(0);
        ensure!(
            version <= last,
            "database schema version {version} is newer than this engine's last known integer version {last}; upgrade the engine"
        );
        tx.execute_batch(CREATE_LEDGER)?;
        for migration in known.values() {
            if migration.legacy_version.is_some_and(|v| v <= version) {
                record(tx, &migration.id)?;
            }
        }
        // A pre-ledger binary must reject this DB as unversioned rather than
        // accepting a stale integer and writing against an unfamiliar schema.
        tx.execute("DELETE FROM metadata WHERE key = 'schema_version'", [])?;
    }
    let applied = applied(tx)?;
    let unknown: Vec<_> = applied.iter().filter(|id| !known.contains_key(id.as_str())).collect();
    ensure!(
        unknown.is_empty(),
        "database has applied migrations unknown to this engine: {unknown:?}; the database is newer than this binary; upgrade the engine"
    );
    for migration in known.values() {
        if !applied.contains(&migration.id) {
            (migration.apply)(tx).with_context(|| format!("applying database migration {}", migration.id))?;
            record(tx, &migration.id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "schema_migration_tests.rs"]
mod tests;
