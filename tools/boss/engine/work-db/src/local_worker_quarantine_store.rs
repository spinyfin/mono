//! Storage for the hold on local workers the engine cannot verify: the
//! metadata record that names them, and the connection-based readers dispatch
//! admission runs inside its own transaction.
//!
//! Deciding *who* to hold needs process and kernel observations, so the scan
//! lives with the host services in `boss_engine::local_worker_quarantine`; it
//! hands its verdict in through [`WorkDb::write_local_worker_quarantine`].
//!
//! Two rules keep one unverifiable worker from stopping unrelated work:
//!
//! - **A held execution holds only its own work item.** Other work items
//!   dispatch normally, locally and remotely. Only an inventory that could
//!   not be read at all ([`LocalWorkerQuarantine::scan_error`]) pauses local
//!   dispatch as a whole, because then the engine cannot name which work
//!   items are affected.
//! - **A hold lasts only while its execution is non-terminal.** Every reader
//!   here checks the execution's current status, so cancelling or otherwise
//!   terminalizing a held execution releases its work item on the very next
//!   dispatch decision, with no engine restart and no rescan.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::WorkDb;

const METADATA_KEY: &str = "local_worker_startup_quarantine";

/// Where an operator looks to see what is held and why.
const INSPECT_HINT: &str = "`bossctl dispatch quarantine status` shows the evidence";

/// The persisted hold: executions whose workers are not proven dead, and the
/// error that aborted the scan, if any. A scan error holds every work item.
///
/// This is the record as written. An entry stops holding once its execution
/// is terminal even before the record is rewritten; read
/// [`WorkDb::held_local_workers`] for what currently holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalWorkerQuarantine {
    pub executions: BTreeMap<String, String>,
    pub scan_error: Option<String>,
}

impl LocalWorkerQuarantine {
    pub fn is_empty(&self) -> bool {
        self.executions.is_empty() && self.scan_error.is_none()
    }
}

/// A non-terminal execution whose latest local run lacks a complete tmux
/// identity (server label, session name and spawn token).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnidentifiedLocalRun {
    pub execution_id: String,
    pub work_item_id: String,
    /// The run was created for a tmux spawn that never asked tmux for a
    /// session, so no worker process ever existed for it.
    ///
    /// A local run is stamped `tmux_hosted` in the same transaction that
    /// creates it, and the spawn writes the session identity before tmux
    /// receives any command. A stamped run with no identity column, no
    /// recorded pid, and no sibling run holding a spawn token therefore
    /// stopped between those two writes: the engine process that was
    /// spawning it went away first. That is a spawn that never started, not
    /// a worker of unknown liveness.
    pub never_reached_tmux: bool,
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn read_local_worker_quarantine(&self) -> Result<LocalWorkerQuarantine> {
        self.get_metadata(METADATA_KEY)?
            .map(|value| serde_json::from_str(&value).context("reading local worker quarantine"))
            .unwrap_or_else(|| Ok(LocalWorkerQuarantine::default()))
    }

    /// Replace the persisted hold with `quarantine`. The scan writes its
    /// complete verdict in one statement so dispatch admission never observes
    /// a half-built record.
    pub fn write_local_worker_quarantine(&self, quarantine: &LocalWorkerQuarantine) -> Result<()> {
        self.set_metadata(METADATA_KEY, &serde_json::to_string(quarantine)?)
    }

    /// The hold as it applies right now: the persisted record minus every
    /// execution that has since reached a terminal status.
    pub fn held_local_workers(&self) -> Result<LocalWorkerQuarantine> {
        let conn = self.connect()?;
        held_local_workers_in(&conn)
    }

    /// Why local dispatch as a whole is paused, or `None` when it is not.
    ///
    /// Only an unreadable run inventory pauses all local dispatch. A held
    /// execution never does: it holds its own work item, which
    /// [`Self::ensure_work_item_not_quarantined`] reports.
    pub fn local_dispatch_quarantine_reason(&self) -> Result<Option<String>> {
        Ok(self
            .read_local_worker_quarantine()?
            .scan_error
            .map(|error| inventory_unreadable_reason(&error)))
    }

    pub fn is_execution_quarantined(&self, execution_id: &str) -> Result<bool> {
        let quarantine = self.held_local_workers()?;
        Ok(quarantine.scan_error.is_some() || quarantine.executions.contains_key(execution_id))
    }

    pub fn ensure_work_item_not_quarantined(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        ensure_work_item_not_quarantined_in(&conn, work_item_id)
    }

    /// Epoch seconds of the newest durable write on the latest local run of
    /// `execution_id`: the greatest of `created_at`, `started_at` and
    /// `finished_at`. `None` when the execution has no local run.
    pub fn latest_local_run_last_write_epoch(&self, execution_id: &str) -> Result<Option<i64>> {
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

    pub fn local_workers_without_tmux_identity(&self) -> Result<Vec<UnidentifiedLocalRun>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.work_item_id,
                    (r.tmux_hosted = 1
                     AND r.tmux_session_name IS NULL
                     AND r.tmux_spawn_token IS NULL
                     AND r.tmux_server_label IS NULL
                     AND r.shell_pid IS NULL
                     AND r.tmux_pane_pid IS NULL
                     AND NOT EXISTS (
                         SELECT 1 FROM work_runs sibling
                         WHERE sibling.execution_id = e.id AND sibling.tmux_spawn_token IS NOT NULL
                     ))
             FROM work_executions e
             JOIN work_runs r ON r.id = (
                 SELECT latest.id FROM work_runs latest WHERE latest.execution_id = e.id
                 ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1
             )
             WHERE e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND r.host_id = 'local'
               AND (r.tmux_session_name IS NULL OR r.tmux_spawn_token IS NULL OR r.tmux_server_label IS NULL)",
        )?;
        stmt.query_map([], |row| {
            Ok(UnidentifiedLocalRun {
                execution_id: row.get(0)?,
                work_item_id: row.get(1)?,
                never_reached_tmux: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
    }
}

fn inventory_unreadable_reason(error: &str) -> String {
    format!(
        "local dispatch paused: the engine could not read its local run inventory, so it cannot tell which \
         work items have an unverified worker ({error}). The inventory is re-read on every dispatch decision and \
         the pause lifts as soon as a read succeeds; {INSPECT_HINT}"
    )
}

fn held_local_workers_in(conn: &Connection) -> Result<LocalWorkerQuarantine> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key = ?1", [METADATA_KEY], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(value) = value else {
        return Ok(LocalWorkerQuarantine::default());
    };
    let mut quarantine: LocalWorkerQuarantine =
        serde_json::from_str(&value).context("reading local worker quarantine")?;
    if quarantine.executions.is_empty() {
        return Ok(quarantine);
    }
    // A hold protects a worker that might still be running. Once its
    // execution is terminal there is nothing left for dispatch to duplicate,
    // so the entry stops holding immediately; the next re-evaluation drops it
    // from the record. An entry whose row cannot be found keeps holding: absence of the
    // row is not a terminal status.
    let mut terminal = conn.prepare(
        "SELECT 1 FROM work_executions
         WHERE id = ?1 AND status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
    )?;
    let mut held = BTreeMap::new();
    for (execution_id, work_item_id) in std::mem::take(&mut quarantine.executions) {
        if !terminal.exists([&execution_id])? {
            held.insert(execution_id, work_item_id);
        }
    }
    quarantine.executions = held;
    Ok(quarantine)
}

pub(super) fn ensure_work_item_not_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<()> {
    let quarantine = held_local_workers_in(conn)?;
    if let Some(error) = &quarantine.scan_error {
        bail!(inventory_unreadable_reason(error));
    }
    let held: Vec<&str> = quarantine
        .executions
        .iter()
        .filter(|(_, item)| item.as_str() == work_item_id)
        .map(|(execution, _)| execution.as_str())
        .collect();
    if !held.is_empty() {
        bail!(
            "work item {work_item_id} has a quarantined historical local worker: execution {} has no complete \
             tmux identity and its process is not proven dead, so nothing else is dispatched for this work \
             item. Other work items are unaffected. The hold lifts when that execution is cancelled or reaches \
             any other terminal state, or when its process is proven gone; {INSPECT_HINT}",
            held.join(", "),
        );
    }
    Ok(())
}

pub(super) fn work_item_is_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<bool> {
    let quarantine = held_local_workers_in(conn)?;
    Ok(quarantine.scan_error.is_some() || quarantine.executions.values().any(|id| id == work_item_id))
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

    fn insert_execution(db: &WorkDb, id: &str, work_item_id: &str, status: &str) {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
                 VALUES (?1, ?2, 'chore_implementation', ?3, 'https://github.com/test/repo', '100')",
                [id, work_item_id, status],
            )
            .unwrap();
    }

    fn set_status(db: &WorkDb, id: &str, status: &str) {
        db.connect()
            .unwrap()
            .execute("UPDATE work_executions SET status = ?2 WHERE id = ?1", [id, status])
            .unwrap();
    }

    /// Insert a local run for `execution_id`. `columns` is extra
    /// `name = value` SQL applied after the insert.
    fn insert_local_run(db: &WorkDb, run_id: &str, execution_id: &str, created_at: &str, columns: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, host_id)
             VALUES (?1, ?2, 'worker-1', 'active', ?3, ?3, 'local')",
            [run_id, execution_id, created_at],
        )
        .unwrap();
        if !columns.is_empty() {
            conn.execute(&format!("UPDATE work_runs SET {columns} WHERE id = ?1"), [run_id])
                .unwrap();
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
        assert!(db.held_local_workers().unwrap().is_empty());
    }

    #[test]
    fn a_held_execution_holds_its_own_work_item_and_nothing_else() {
        let db = open_db();
        insert_execution(&db, "exec-1", "item-1", "running");
        db.write_local_worker_quarantine(&held(&[("exec-1", "item-1")], None))
            .unwrap();

        let conn = db.connect().unwrap();
        assert!(work_item_is_quarantined_in(&conn, "item-1").unwrap());
        let refusal = ensure_work_item_not_quarantined_in(&conn, "item-1")
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("exec-1"), "{refusal}");
        assert!(
            refusal.contains("cancelled"),
            "the refusal must name what releases it: {refusal}"
        );
        assert!(refusal.contains("bossctl dispatch quarantine"), "{refusal}");
        assert!(!work_item_is_quarantined_in(&conn, "item-2").unwrap());
        ensure_work_item_not_quarantined_in(&conn, "item-2").unwrap();
        drop(conn);

        assert!(db.is_execution_quarantined("exec-1").unwrap());
        assert!(!db.is_execution_quarantined("exec-2").unwrap());
        // One held worker is not a reason to stop local dispatch for everyone.
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    }

    #[test]
    fn a_terminal_execution_stops_holding_without_the_record_being_rewritten() {
        for terminal in ["cancelled", "failed", "completed", "abandoned", "orphaned"] {
            let db = open_db();
            insert_execution(&db, "exec-1", "item-1", "running");
            insert_execution(&db, "exec-2", "item-2", "waiting_human");
            let recorded = held(&[("exec-1", "item-1"), ("exec-2", "item-2")], None);
            db.write_local_worker_quarantine(&recorded).unwrap();
            assert!(db.ensure_work_item_not_quarantined("item-1").is_err());

            set_status(&db, "exec-1", terminal);

            db.ensure_work_item_not_quarantined("item-1")
                .unwrap_or_else(|err| panic!("{terminal} execution still holds its work item: {err:#}"));
            assert!(!db.is_execution_quarantined("exec-1").unwrap(), "{terminal}");
            let conn = db.connect().unwrap();
            assert!(!work_item_is_quarantined_in(&conn, "item-1").unwrap(), "{terminal}");
            // The other entry is untouched, and the stored record is unchanged:
            // the release came from the status read, not from a rewrite.
            assert!(work_item_is_quarantined_in(&conn, "item-2").unwrap(), "{terminal}");
            drop(conn);
            assert_eq!(db.read_local_worker_quarantine().unwrap(), recorded);
            assert_eq!(db.held_local_workers().unwrap(), held(&[("exec-2", "item-2")], None));
        }
    }

    #[test]
    fn an_entry_whose_execution_row_is_missing_keeps_holding() {
        let db = open_db();
        db.write_local_worker_quarantine(&held(&[("exec-gone", "item-1")], None))
            .unwrap();
        assert!(db.is_execution_quarantined("exec-gone").unwrap());
        assert!(db.ensure_work_item_not_quarantined("item-1").is_err());
        db.ensure_work_item_not_quarantined("item-2").unwrap();
    }

    #[test]
    fn a_scan_error_holds_every_work_item_and_every_execution() {
        let db = open_db();
        db.write_local_worker_quarantine(&held(&[], Some("inventory unreadable")))
            .unwrap();

        let conn = db.connect().unwrap();
        assert!(work_item_is_quarantined_in(&conn, "any-item").unwrap());
        let refusal = ensure_work_item_not_quarantined_in(&conn, "any-item")
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("inventory unreadable"), "{refusal}");
        drop(conn);
        assert!(db.is_execution_quarantined("any-execution").unwrap());
        let reason = db.local_dispatch_quarantine_reason().unwrap().unwrap();
        assert!(reason.contains("inventory unreadable"), "{reason}");
        assert!(reason.contains("re-read on every dispatch decision"), "{reason}");
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
        assert!(db.held_local_workers().unwrap().is_empty());
    }

    #[test]
    fn last_write_epoch_is_the_newest_local_run_timestamp_and_none_without_a_run() {
        let db = open_db();
        assert_eq!(db.latest_local_run_last_write_epoch("missing").unwrap(), None);
        insert_execution(&db, "exec-1", "wi-1", "queued");
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at, finished_at, host_id)
                 VALUES ('run-1', 'exec-1', 'worker-1', 'finished', '100', '250', '400', 'local')",
                [],
            )
            .unwrap();
        assert_eq!(db.latest_local_run_last_write_epoch("exec-1").unwrap(), Some(400));
    }

    fn unidentified(db: &WorkDb) -> BTreeMap<String, bool> {
        db.local_workers_without_tmux_identity()
            .unwrap()
            .into_iter()
            .map(|run| (run.execution_id, run.never_reached_tmux))
            .collect()
    }

    #[test]
    fn a_stamped_run_with_no_identity_and_no_pid_never_reached_tmux() {
        let db = open_db();
        // The spawn window: run row created for a tmux spawn, nothing else yet.
        insert_execution(&db, "in-window", "item-1", "running");
        insert_local_run(&db, "run-1", "in-window", "100", "tmux_hosted = 1");
        // Created before tmux hosting was stamped: could be an app-hosted worker.
        insert_execution(&db, "unstamped", "item-2", "running");
        insert_local_run(&db, "run-2", "unstamped", "100", "");
        // A pid was recorded, so a process existed at some point.
        insert_execution(&db, "with-pid", "item-3", "running");
        insert_local_run(&db, "run-3", "with-pid", "100", "tmux_hosted = 1, shell_pid = 4242");
        insert_execution(&db, "with-pane-pid", "item-4", "running");
        insert_local_run(
            &db,
            "run-4",
            "with-pane-pid",
            "100",
            "tmux_hosted = 1, tmux_pane_pid = 4242",
        );
        // Part of an identity is not "never asked tmux".
        insert_execution(&db, "partial", "item-5", "running");
        insert_local_run(
            &db,
            "run-5",
            "partial",
            "100",
            "tmux_hosted = 1, tmux_session_name = 'boss-1-abc'",
        );

        assert_eq!(
            unidentified(&db),
            BTreeMap::from([
                ("in-window".to_owned(), true),
                ("unstamped".to_owned(), false),
                ("with-pid".to_owned(), false),
                ("with-pane-pid".to_owned(), false),
                ("partial".to_owned(), false),
            ])
        );
    }

    #[test]
    fn a_sibling_run_holding_a_spawn_token_means_tmux_may_have_been_asked() {
        let db = open_db();
        insert_execution(&db, "exec-1", "item-1", "running");
        insert_local_run(
            &db,
            "run-old",
            "exec-1",
            "100",
            "tmux_hosted = 1, tmux_server_label = 'boss', tmux_session_name = 'boss-1-abc', \
             tmux_spawn_token = 'token-1', tmux_spawn_state = 'intended'",
        );
        insert_local_run(&db, "run-new", "exec-1", "200", "tmux_hosted = 1");
        assert_eq!(unidentified(&db), BTreeMap::from([("exec-1".to_owned(), false)]));
    }

    #[test]
    fn identified_remote_and_terminal_runs_are_not_listed() {
        let db = open_db();
        insert_execution(&db, "identified", "item-1", "running");
        insert_local_run(
            &db,
            "run-1",
            "identified",
            "100",
            "tmux_hosted = 1, tmux_server_label = 'boss', tmux_session_name = 'boss-1-abc', \
             tmux_spawn_token = 'token-1', tmux_spawn_state = 'intended'",
        );
        insert_execution(&db, "terminal", "item-2", "cancelled");
        insert_local_run(&db, "run-2", "terminal", "100", "tmux_hosted = 1");
        insert_execution(&db, "remote", "item-3", "running");
        insert_local_run(&db, "run-3", "remote", "100", "");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO hosts (id, pool_size, created_at) VALUES ('remote', 1, '0')",
                [],
            )
            .unwrap();
            conn.execute("UPDATE work_runs SET host_id = 'remote' WHERE id = 'run-3'", [])
                .unwrap();
        }
        assert!(unidentified(&db).is_empty());
    }
}
