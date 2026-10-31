//! Storage for the startup quarantine of historical local workers: the
//! metadata record that holds local dispatch, and the connection-based
//! readers dispatch admission runs inside its own transaction.
//!
//! Deciding *who* to quarantine needs process and kernel observations, so the
//! scan lives with the host services in [`crate::local_worker_quarantine`];
//! it hands its verdict in through [`WorkDb::write_local_worker_quarantine`].

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::WorkDb;

const METADATA_KEY: &str = "local_worker_startup_quarantine";

/// The persisted hold: executions whose workers are not proven dead, and the
/// error that aborted the scan, if any. A scan error holds every work item.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct LocalWorkerQuarantine {
    pub(crate) executions: BTreeMap<String, String>,
    pub(crate) scan_error: Option<String>,
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub(crate) fn read_local_worker_quarantine(&self) -> Result<LocalWorkerQuarantine> {
        self.get_metadata(METADATA_KEY)?
            .map(|value| serde_json::from_str(&value).context("reading local worker quarantine"))
            .unwrap_or_else(|| Ok(LocalWorkerQuarantine::default()))
    }

    /// Replace the persisted hold with `quarantine`. The startup scan writes
    /// its complete verdict in one statement so dispatch admission never
    /// observes a half-built record.
    pub(crate) fn write_local_worker_quarantine(&self, quarantine: &LocalWorkerQuarantine) -> Result<()> {
        self.set_metadata(METADATA_KEY, &serde_json::to_string(quarantine)?)
    }

    pub(crate) fn local_dispatch_quarantine_reason(&self) -> Result<Option<String>> {
        let quarantine = self.read_local_worker_quarantine()?;
        if quarantine.executions.is_empty() && quarantine.scan_error.is_none() {
            return Ok(None);
        }
        Ok(Some(format!(
            "local dispatch quarantined: historical workers lack tmux identity; roll back or drain them and restart (executions: {}; scan error: {})",
            quarantine.executions.keys().cloned().collect::<Vec<_>>().join(", "),
            quarantine.scan_error.as_deref().unwrap_or("none"),
        )))
    }

    pub(crate) fn is_execution_quarantined(&self, execution_id: &str) -> Result<bool> {
        let quarantine = self.read_local_worker_quarantine()?;
        Ok(quarantine.scan_error.is_some() || quarantine.executions.contains_key(execution_id))
    }

    pub(crate) fn ensure_work_item_not_quarantined(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        ensure_work_item_not_quarantined_in(&conn, work_item_id)
    }

    /// Epoch seconds of the newest durable write on the latest local run of
    /// `execution_id`: the greatest of `created_at`, `started_at` and
    /// `finished_at`. `None` when the execution has no local run.
    pub(crate) fn latest_local_run_last_write_epoch(&self, execution_id: &str) -> Result<Option<i64>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT MAX(
                 CAST(created_at AS INTEGER),
                 CAST(COALESCE(started_at, '0') AS INTEGER),
                 CAST(COALESCE(finished_at, '0') AS INTEGER)
             ) FROM work_runs
             WHERE execution_id = ?1 AND host_id = 'local'
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            [execution_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    pub(crate) fn local_workers_without_tmux_identity(&self) -> Result<Vec<(String, String)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.work_item_id FROM work_executions e
             JOIN work_runs r ON r.id = (
                 SELECT latest.id FROM work_runs latest WHERE latest.execution_id = e.id
                 ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1
             )
             WHERE e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND r.host_id = 'local'
               AND (r.tmux_session_name IS NULL OR r.tmux_spawn_token IS NULL OR r.tmux_server_label IS NULL)",
        )?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub(super) fn ensure_work_item_not_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<()> {
    if work_item_is_quarantined_in(conn, work_item_id)? {
        bail!("work item has a quarantined historical local worker; roll back or drain it and restart");
    }
    Ok(())
}

pub(super) fn work_item_is_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<bool> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key = ?1", [METADATA_KEY], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(value) = value {
        let quarantine: LocalWorkerQuarantine = serde_json::from_str(&value)?;
        if quarantine.scan_error.is_some() || quarantine.executions.values().any(|id| id == work_item_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    fn held(executions: &[(&str, &str)], scan_error: Option<&str>) -> LocalWorkerQuarantine {
        LocalWorkerQuarantine {
            executions: executions
                .iter()
                .map(|(execution, item)| ((*execution).to_owned(), (*item).to_owned()))
                .collect(),
            scan_error: scan_error.map(str::to_owned),
        }
    }

    #[test]
    fn a_fresh_database_holds_nothing() {
        let db = open_db();
        let conn = db.connect().unwrap();
        assert!(!work_item_is_quarantined_in(&conn, "item-1").unwrap());
        ensure_work_item_not_quarantined_in(&conn, "item-1").unwrap();
        drop(conn);
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        assert!(!db.is_execution_quarantined("exec-1").unwrap());
    }

    #[test]
    fn the_connection_helpers_hold_exactly_the_recorded_work_items() {
        let db = open_db();
        db.write_local_worker_quarantine(&held(&[("exec-1", "item-1")], None))
            .unwrap();

        let conn = db.connect().unwrap();
        assert!(work_item_is_quarantined_in(&conn, "item-1").unwrap());
        assert!(ensure_work_item_not_quarantined_in(&conn, "item-1").is_err());
        assert!(!work_item_is_quarantined_in(&conn, "item-2").unwrap());
        ensure_work_item_not_quarantined_in(&conn, "item-2").unwrap();
        drop(conn);

        assert!(db.is_execution_quarantined("exec-1").unwrap());
        assert!(!db.is_execution_quarantined("exec-2").unwrap());
        let reason = db.local_dispatch_quarantine_reason().unwrap().unwrap();
        assert!(reason.contains("exec-1"), "{reason}");
    }

    #[test]
    fn a_scan_error_holds_every_work_item_and_every_execution() {
        let db = open_db();
        db.write_local_worker_quarantine(&held(&[], Some("inventory unreadable")))
            .unwrap();

        let conn = db.connect().unwrap();
        assert!(work_item_is_quarantined_in(&conn, "any-item").unwrap());
        drop(conn);
        assert!(db.is_execution_quarantined("any-execution").unwrap());
        let reason = db.local_dispatch_quarantine_reason().unwrap().unwrap();
        assert!(reason.contains("inventory unreadable"), "{reason}");
    }

    #[test]
    fn writing_replaces_the_previous_hold_rather_than_merging() {
        let db = open_db();
        db.write_local_worker_quarantine(&held(&[("exec-1", "item-1")], None))
            .unwrap();
        db.write_local_worker_quarantine(&held(&[("exec-2", "item-2")], None))
            .unwrap();

        let conn = db.connect().unwrap();
        assert!(!work_item_is_quarantined_in(&conn, "item-1").unwrap());
        assert!(work_item_is_quarantined_in(&conn, "item-2").unwrap());
        drop(conn);

        db.write_local_worker_quarantine(&held(&[], None)).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    }

    #[test]
    fn last_write_epoch_is_the_newest_local_run_timestamp_and_none_without_a_run() {
        let db = open_db();
        assert_eq!(db.latest_local_run_last_write_epoch("missing").unwrap(), None);
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
             VALUES ('exec-1', 'wi-1', 'chore_implementation', 'queued', 'https://github.com/test/repo', '100')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, finished_at, host_id)
             VALUES ('run-1', 'exec-1', 'worker-1', 'finished', '100', '250', '400', 'local')",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.latest_local_run_last_write_epoch("exec-1").unwrap(), Some(400));
    }
}
