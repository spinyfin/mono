//! Durable pending-cleanup markers for staged remote worker input.
//!
//! The brief is shipped to `<workspace>/<config>/runs/<run_id>` on the remote
//! before the worker exists. A marker is recorded before the first transfer
//! and cleared once the directory is removed (or a recorded remote pid hands
//! cleanup to the pid-gated paths), so a crash or transport failure never
//! leaves the only pointer to the brief in memory.

use anyhow::Result;
use rusqlite::params;

use super::WorkDb;

const KEY_PREFIX: &str = "remote_input_cleanup:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingRemoteInputCleanup {
    pub host_id: String,
    pub run_id: String,
    pub workspace: String,
}

fn key(host_id: &str, run_id: &str) -> String {
    format!("{KEY_PREFIX}{host_id}:{run_id}")
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub(crate) fn mark_remote_input_cleanup_pending(&self, host_id: &str, run_id: &str, workspace: &str) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key(host_id, run_id), workspace],
        )?;
        Ok(())
    }

    pub(crate) fn clear_remote_input_cleanup(&self, host_id: &str, run_id: &str) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM metadata WHERE key = ?1", [key(host_id, run_id)])?;
        Ok(())
    }

    pub(crate) fn pending_remote_input_cleanups(&self) -> Result<Vec<PendingRemoteInputCleanup>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT key, value FROM metadata WHERE key LIKE ?1 ESCAPE '\\'")?;
        let rows = statement.query_map([format!("{KEY_PREFIX}%")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut pending = Vec::new();
        for row in rows {
            let (key, workspace) = row?;
            // Host ids hold no ':'; run ids are alphanumeric, '-' or '_'.
            if let Some((host_id, run_id)) = key.strip_prefix(KEY_PREFIX).and_then(|rest| rest.split_once(':')) {
                pending.push(PendingRemoteInputCleanup {
                    host_id: host_id.to_owned(),
                    run_id: run_id.to_owned(),
                    workspace,
                });
            }
        }
        Ok(pending)
    }
}
