//! Schema migration for Ideas: markdown drafts that graduate into a chore
//! or project. Own table, own `I<n>` short-id namespace.

use anyhow::Result;
use rusqlite::Connection;

/// Create `ideas` and its own dense per-product `I<n>` short-id sequence
/// (own table, own namespace — not the shared `short_id_sequences`
/// counter tasks/chores/projects use). Deliberately not a `tasks` row: an
/// idea is not dispatchable, has no execution, PR, attentions, or
/// dependency edges, and is not on the kanban.
///
/// `graduated_to_id` is a soft pointer to the chore/project this idea
/// became once `status = 'graduated'`; never cleared, since a graduated
/// idea is kept, not deleted.
///
/// Idempotent — `CREATE TABLE / INDEX IF NOT EXISTS`.
pub(crate) fn migrate_ideas_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ideas (
             id                TEXT PRIMARY KEY,
             short_id          INTEGER,
             product_id        TEXT NOT NULL,
             name              TEXT NOT NULL,
             body              TEXT NOT NULL DEFAULT '',
             status            TEXT NOT NULL DEFAULT 'draft',
             graduated_to_id   TEXT,
             created_via       TEXT NOT NULL DEFAULT 'unknown',
             created_at        TEXT NOT NULL,
             updated_at        TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS ideas_product_short_id_idx
             ON ideas(product_id, short_id) WHERE short_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS ideas_product_status_idx
             ON ideas(product_id, status, created_at);
         CREATE TABLE IF NOT EXISTS idea_short_id_sequences (
             product_id TEXT PRIMARY KEY,
             next_value INTEGER NOT NULL
         );",
    )?;
    Ok(())
}
