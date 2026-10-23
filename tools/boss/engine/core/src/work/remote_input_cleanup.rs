//! Durable, attempt-scoped cleanup obligations for remote input.
use super::WorkDb;
use anyhow::Result;
use rusqlite::params;

const KEY_PREFIX: &str = "remote_input_cleanup:";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingRemoteInputCleanup {
    pub host_id: String,
    /// Unique input attempt nonce, also the directory basename.
    pub run_id: String,
    pub workspace: String,
    pub execution_id: String,
    pub safe_to_remove: bool,
}

fn key(host_id: &str, run_id: &str) -> String {
    format!("{KEY_PREFIX}{host_id}:{run_id}")
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub(crate) fn mark_remote_input_cleanup_pending(
        &self,
        host_id: &str,
        run_id: &str,
        workspace: &str,
        execution_id: &str,
    ) -> Result<PendingRemoteInputCleanup> {
        let item = PendingRemoteInputCleanup {
            host_id: host_id.into(),
            run_id: run_id.into(),
            workspace: workspace.into(),
            execution_id: execution_id.into(),
            safe_to_remove: false,
        };
        self.connect()?.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params![key(host_id, run_id), serde_json::to_string(&item)?],
        )?;
        Ok(item)
    }

    pub(crate) fn mark_remote_input_safe(&self, item: &mut PendingRemoteInputCleanup) -> Result<()> {
        let previous = serde_json::to_string(item)?;
        item.safe_to_remove = true;
        self.connect()?.execute(
            "UPDATE metadata SET value = ?2 WHERE key = ?1 AND value = ?3",
            params![key(&item.host_id, &item.run_id), serde_json::to_string(item)?, previous],
        )?;
        Ok(())
    }

    pub(crate) fn clear_remote_input_cleanup(&self, item: &PendingRemoteInputCleanup) -> Result<()> {
        self.connect()?.execute(
            "DELETE FROM metadata WHERE key = ?1 AND value = ?2",
            params![key(&item.host_id, &item.run_id), serde_json::to_string(item)?],
        )?;
        Ok(())
    }

    pub(crate) fn pending_remote_input_cleanups(&self) -> Result<Vec<PendingRemoteInputCleanup>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT value FROM metadata WHERE substr(key, 1, ?1) = ?2")?;
        let rows = statement.query_map(params![KEY_PREFIX.len(), KEY_PREFIX], |row| row.get::<_, String>(0))?;
        let mut pending = Vec::new();
        for row in rows {
            let raw = row?;
            // Rows that do not decode as a PendingRemoteInputCleanup are skipped
            // and left in place: without an attempt id there is no directory
            // this pass can safely remove.
            if let Ok(item) = serde_json::from_str(&raw) {
                pending.push(item);
            }
        }
        Ok(pending)
    }

    pub(crate) fn pending_remote_input_cleanups_for_execution(
        &self,
        execution_id: &str,
    ) -> Result<Vec<PendingRemoteInputCleanup>> {
        Ok(self
            .pending_remote_input_cleanups()?
            .into_iter()
            .filter(|item| item.execution_id == execution_id)
            .collect())
    }
}
