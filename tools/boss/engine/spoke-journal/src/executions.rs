//! Durable execution records keyed by `(run_id, spawn_token)`. Survives compaction.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::Journal;

impl Journal {
    pub fn load_execution(&self, run_id: &str, spawn_token: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT bytes FROM executions WHERE run_id=?1 AND spawn_token=?2",
                params![run_id, spawn_token],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_execution(&mut self, run_id: &str, spawn_token: &str, bytes: &[u8]) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO executions(run_id, spawn_token, bytes) VALUES(?1, ?2, ?3)
             ON CONFLICT(run_id, spawn_token) DO UPDATE SET bytes=excluded.bytes",
            params![run_id, spawn_token, bytes],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_executions(&self) -> Result<Vec<Vec<u8>>> {
        let mut statement = self
            .db
            .prepare("SELECT bytes FROM executions ORDER BY run_id, spawn_token")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        let mut bytes = Vec::new();
        for row in rows {
            bytes.push(row?);
        }
        Ok(bytes)
    }
}
