//! Startup quarantine for historical local workers without tmux identity.
//!
//! Lease loss and missing app inventory do not prove process death. Preserve
//! these executions and hold local dispatch until rollback/drain and restart.
//! The metadata record also prevents later recovery sweeps undoing the hold.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::durable_liveness::{WorkerProcess, probe_execution_worker};
use crate::work::WorkDb;

const METADATA_KEY: &str = "local_worker_startup_quarantine";
pub(crate) const ATTENTION_KIND: &str = "local_worker_startup_quarantine";

#[derive(Default, Serialize, Deserialize)]
struct Quarantine {
    executions: BTreeMap<String, String>,
    scan_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct StartupQuarantineReport {
    pub protected_execution_ids: HashSet<String>,
    pub dead_execution_ids: HashSet<String>,
    pub scan_failed: bool,
}

impl WorkDb {
    fn read_local_worker_quarantine(&self) -> Result<Quarantine> {
        self.get_metadata(METADATA_KEY)?
            .map(|value| serde_json::from_str(&value).context("reading local worker quarantine"))
            .unwrap_or_else(|| Ok(Quarantine::default()))
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

    fn local_workers_without_tmux_identity(&self) -> Result<Vec<(String, String)>> {
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

pub(crate) fn quarantine_historical_local_workers(db: &WorkDb) -> Result<StartupQuarantineReport> {
    quarantine_with_probe(db, |id| probe_execution_worker(db, id))
}

fn quarantine_with_probe(db: &WorkDb, probe: impl Fn(&str) -> WorkerProcess) -> Result<StartupQuarantineReport> {
    let previous = db.read_local_worker_quarantine()?;
    let mut quarantine = Quarantine::default();
    let mut report = StartupQuarantineReport::default();
    match db.local_workers_without_tmux_identity() {
        Ok(rows) => {
            // A status change from another recovery path is not proof of death.
            // Keep re-probing an established hold, even if its row terminalized.
            let mut rows: BTreeMap<_, _> = rows.into_iter().collect();
            rows.extend(previous.executions.clone());
            for (execution_id, work_item_id) in rows {
                let process = probe(&execution_id);
                if matches!(process, WorkerProcess::Gone { .. }) {
                    report.dead_execution_ids.insert(execution_id);
                } else {
                    tracing::error!(
                        execution_id,
                        evidence = process.reason(),
                        "historical local worker has no tmux identity; preserving execution and quarantining local dispatch for rollback/drain"
                    );
                    report.protected_execution_ids.insert(execution_id.clone());
                    quarantine.executions.insert(execution_id, work_item_id);
                }
            }
        }
        Err(err) => {
            tracing::error!(?err, "historical worker scan failed; quarantining local dispatch");
            quarantine.scan_error = Some(format!("{err:#}"));
            quarantine.executions = previous.executions.clone();
            report.scan_failed = true;
        }
    }
    db.set_metadata(METADATA_KEY, &serde_json::to_string(&quarantine)?)?;
    for work_item_id in previous.executions.values().collect::<HashSet<_>>() {
        if quarantine.scan_error.is_none() && !quarantine.executions.values().any(|id| id == work_item_id) {
            db.resolve_external_tracker_attention(work_item_id, ATTENTION_KIND)?;
        }
    }
    for (execution_id, work_item_id) in &quarantine.executions {
        db.upsert_external_tracker_attention(
            work_item_id,
            ATTENTION_KIND,
            "Local dispatch paused for a historical worker",
            &format!("Execution `{execution_id}` has no complete tmux identity and its process is live or unknown. An older app-hosted worker may still exist. Local dispatch is paused to prevent duplicate workers. Roll back to the prior release to stop or drain the worker, then restart the tmux-only engine. Missing app inventory or an expired workspace lease does not prove death."),
        )?;
    }
    Ok(report)
}

pub(crate) fn ensure_work_item_not_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<()> {
    if work_item_is_quarantined_in(conn, work_item_id)? {
        bail!("work item has a quarantined historical local worker; roll back or drain it and restart");
    }
    Ok(())
}

pub(crate) fn work_item_is_quarantined_in(conn: &Connection, work_item_id: &str) -> Result<bool> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key = ?1", [METADATA_KEY], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(value) = value {
        let quarantine: Quarantine = serde_json::from_str(&value)?;
        if quarantine.scan_error.is_some() || quarantine.executions.values().any(|id| id == work_item_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_old_execution, create_product};

    fn seed(db: &WorkDb, name: &str) -> (String, String) {
        let product = create_product(db);
        let item = create_active_chore(db, &product, name);
        let execution = create_old_execution(db, &item);
        db.start_execution_run(&execution, "worker-1", "repo", "lease", "workspace", "/tmp/workspace")
            .unwrap();
        (execution, item)
    }

    #[test]
    fn only_proven_death_permits_orphaning_and_redispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let db = WorkDb::open(path.clone()).unwrap();
        let (live, live_item) = seed(&db, "live");
        let (unknown, unknown_item) = seed(&db, "unknown");
        let (dead, dead_item) = seed(&db, "dead");
        let report = quarantine_with_probe(&db, |id| {
            if id == live {
                WorkerProcess::Alive { shell_pid: 42 }
            } else if id == dead {
                WorkerProcess::Gone { shell_pid: 43 }
            } else {
                WorkerProcess::Unknown
            }
        })
        .unwrap();
        assert_eq!(
            report.protected_execution_ids,
            HashSet::from([live.clone(), unknown.clone()])
        );
        assert_eq!(report.dead_execution_ids, HashSet::from([dead.clone()]));
        assert!(
            db.local_dispatch_quarantine_reason()
                .unwrap()
                .unwrap()
                .contains("roll back or drain")
        );
        for (execution, item) in [(&live, &live_item), (&unknown, &unknown_item)] {
            let attentions = db.list_attention_items_for_work_item(item).unwrap();
            assert!(
                attentions
                    .iter()
                    .any(|attention| attention.kind == ATTENTION_KIND && attention.status == "open")
            );
            assert!(db.mark_execution_orphaned(execution, "lease expired").is_err());
            assert!(db.ensure_work_item_not_quarantined(item).is_err());
            assert!(ensure_work_item_not_quarantined_in(&db.connect().unwrap(), item).is_err());
            assert!(!db.get_execution(execution).unwrap().status.is_terminal());
        }
        db.mark_execution_orphaned(&dead, "process proven gone").unwrap();
        db.ensure_work_item_not_quarantined(&dead_item).unwrap();
        // An engine restart cannot erase the hold before it re-probes.
        let reopened = WorkDb::open(path).unwrap();
        assert!(reopened.is_execution_quarantined(&live).unwrap());
        let drained = quarantine_with_probe(&reopened, |_| WorkerProcess::Gone { shell_pid: 42 }).unwrap();
        assert!(drained.protected_execution_ids.is_empty());
        assert!(reopened.local_dispatch_quarantine_reason().unwrap().is_none());
        assert!(
            reopened
                .list_attention_items_for_work_item(&live_item)
                .unwrap()
                .iter()
                .all(|attention| attention.kind != ATTENTION_KIND || attention.status == "resolved")
        );
    }

    #[test]
    fn missing_pid_is_unknown_even_when_no_tmux_worker_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "no pid");
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(report.dead_execution_ids.is_empty());
    }

    #[test]
    fn terminalizing_a_quarantined_row_does_not_prove_its_worker_died() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "still live");
        quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                [&execution],
            )
            .unwrap();
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
        quarantine_with_probe(&db, |_| WorkerProcess::Gone { shell_pid: 42 }).unwrap();
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    }

    #[test]
    fn live_pid_with_partial_identity_stays_quarantined_even_with_tmux_hosted_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "live partial identity");
        db.set_run_shell_pid_for_execution(&execution, i64::from(std::process::id()))
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET tmux_hosted = 1, tmux_session_name = 'historical' WHERE execution_id = ?1",
                [&execution],
            )
            .unwrap();
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.reconcile_active_dispatch(|_| false).unwrap().is_empty());
        assert!(!db.get_execution(&execution).unwrap().status.is_terminal());
    }

    #[test]
    fn tmux_remote_and_terminal_rows_do_not_enter_historical_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (tmux, _) = seed(&db, "tmux");
        db.record_tmux_spawn_intent_for_execution(&tmux, "boss", "boss-worker", "token")
            .unwrap();
        let (terminal, _) = seed(&db, "terminal");
        db.mark_execution_orphaned(&terminal, "already dead").unwrap();
        let (remote, _) = seed(&db, "remote");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO hosts (id, pool_size, created_at) VALUES ('remote', 1, '0')",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE work_runs SET host_id = 'remote' WHERE execution_id = ?1",
                [&remote],
            )
            .unwrap();
        }
        let report = quarantine_with_probe(&db, |_| panic!("excluded rows must never be probed")).unwrap();
        assert!(report.protected_execution_ids.is_empty());
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    }

    #[test]
    fn unreadable_historical_inventory_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "ALTER TABLE work_runs RENAME COLUMN tmux_session_name TO unavailable_identity",
                [],
            )
            .unwrap();
        let report = quarantine_historical_local_workers(&db).unwrap();
        assert!(report.scan_failed);
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        assert!(db.is_execution_quarantined("unknown-execution").unwrap());
        assert!(db.ensure_work_item_not_quarantined("unknown-item").is_err());
    }
}
