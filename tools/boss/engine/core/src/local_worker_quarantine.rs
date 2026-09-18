//! Startup quarantine for historical local workers without tmux identity.
//!
//! Lease loss and missing app inventory do not prove process death. Preserve
//! these executions and hold local dispatch until rollback/drain and restart.
//! The metadata record also prevents later recovery sweeps undoing the hold.
//!
//! Two kinds of evidence prove death. The durable pid probe is one. The other
//! is the kernel boot: a run whose last durable write predates the current
//! boot (by more than [`PRE_BOOT_MARGIN_SECS`]) cannot own a live process,
//! because no process survives a reboot. Without that second proof a
//! historical row that never recorded a pid could never leave the hold, and
//! neither rollback nor drain can act on a worker nobody can name.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::durable_liveness::{WorkerProcess, probe_execution_worker};
use crate::work::WorkDb;

const METADATA_KEY: &str = "local_worker_startup_quarantine";
pub(crate) const ATTENTION_KIND: &str = "local_worker_startup_quarantine";

/// How far before the kernel boot a run's last durable write must fall before
/// the boot alone proves its worker dead. The margin absorbs a wall clock that
/// was behind when the row was written and corrected after boot (the kernel
/// re-anchors its boot time on clock corrections, the row does not move).
/// The failure direction of a margin that is too small is a duplicate worker,
/// so it is generous; a row inside the margin simply stays held until the
/// pid probe or a later boot can prove it.
pub(crate) const PRE_BOOT_MARGIN_SECS: i64 = 3600;

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

    /// Epoch seconds of the newest durable write on the latest local run of
    /// `execution_id`: the greatest of `created_at`, `started_at` and
    /// `finished_at`. `None` when the execution has no local run.
    fn latest_local_run_last_write_epoch(&self, execution_id: &str) -> Result<Option<i64>> {
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
    let boot = kernel_boot_epoch_secs();
    if boot.is_none() {
        tracing::warn!("kernel boot time is unavailable; pre-boot historical runs cannot be proven dead this startup");
    }
    quarantine_with_probe(db, |id| probe_execution_worker(db, id), boot)
}

/// Epoch seconds at which the running kernel booted, or `None` when the
/// platform cannot say. `None` is never treated as evidence.
pub(crate) fn kernel_boot_epoch_secs() -> Option<i64> {
    kernel_boot_epoch_secs_impl()
}

#[cfg(target_os = "macos")]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    let mut boottime = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>();
    let name = c"kern.boottime";
    // SAFETY: `boottime` is a valid, writable timeval of exactly `len` bytes,
    // and sysctl writes at most `len` bytes into it.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boottime as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != std::mem::size_of::<libc::timeval>() || boottime.tv_sec <= 0 {
        return None;
    }
    Some(boottime.tv_sec)
}

#[cfg(target_os = "linux")]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel_boot_epoch_secs_impl() -> Option<i64> {
    None
}

/// Whether a run whose last durable write was at `last_write_epoch` is proven
/// dead by a kernel that booted at `boot_epoch`.
fn run_predates_boot(last_write_epoch: Option<i64>, boot_epoch: Option<i64>) -> bool {
    match (last_write_epoch, boot_epoch) {
        (Some(last_write), Some(boot)) => last_write.saturating_add(PRE_BOOT_MARGIN_SECS) < boot,
        _ => false,
    }
}

fn quarantine_with_probe(
    db: &WorkDb,
    probe: impl Fn(&str) -> WorkerProcess,
    boot_epoch_secs: Option<i64>,
) -> Result<StartupQuarantineReport> {
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
                // A run written entirely before the kernel booted has no
                // process left to probe: whatever pid it recorded (or did
                // not) belongs to a previous boot. This outranks the pid
                // probe, which after a reboot can only report pid reuse.
                let last_write = match db.latest_local_run_last_write_epoch(&execution_id) {
                    Ok(last_write) => last_write,
                    Err(err) => {
                        tracing::warn!(
                            execution_id,
                            error = %format!("{err:#}"),
                            "failed to read the historical run's last write time; treating boot evidence as unknown"
                        );
                        None
                    }
                };
                let (dead, evidence) = if run_predates_boot(last_write, boot_epoch_secs) {
                    (true, "run_predates_kernel_boot")
                } else {
                    let process = probe(&execution_id);
                    (matches!(process, WorkerProcess::Gone { .. }), process.reason())
                };
                if dead {
                    tracing::info!(
                        execution_id,
                        evidence,
                        "historical local worker has no tmux identity and its process is proven dead; permitting orphan recovery"
                    );
                    report.dead_execution_ids.insert(execution_id);
                } else {
                    tracing::error!(
                        execution_id,
                        evidence,
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
    // The hold above is what dispatch admission reads; the attention items
    // below are the operator-facing explanation. Filing them is best-effort:
    // a held execution can belong to a work item that was soft-deleted after
    // the worker spawned, and `upsert_external_tracker_attention` refuses a
    // tombstoned task. That refusal must not take the engine down at startup
    // (the hold is already durable), so it is logged and the sweep continues.
    for work_item_id in previous.executions.values().collect::<HashSet<_>>() {
        let still_held = quarantine.scan_error.is_some() || quarantine.executions.values().any(|id| id == work_item_id);
        if still_held {
            continue;
        }
        if let Err(err) = db.resolve_external_tracker_attention(work_item_id, ATTENTION_KIND) {
            tracing::error!(
                work_item_id,
                error = %format!("{err:#}"),
                "failed to resolve the historical local worker attention item; the hold itself is already lifted"
            );
        }
    }
    for (execution_id, work_item_id) in &quarantine.executions {
        if let Err(err) = db.upsert_external_tracker_attention(
            work_item_id,
            ATTENTION_KIND,
            "Local dispatch paused for a historical worker",
            &format!("Execution `{execution_id}` has no complete tmux identity and its process is live or unknown. An older app-hosted worker may still exist. Local dispatch is paused to prevent duplicate workers. Roll back to the prior release to stop or drain the worker, then restart the tmux-only engine. Missing app inventory or an expired workspace lease does not prove death."),
        ) {
            tracing::error!(
                execution_id,
                work_item_id,
                error = %format!("{err:#}"),
                "failed to file the historical local worker attention item; the hold itself is already durable"
            );
        }
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
        let report = quarantine_with_probe(
            &db,
            |id| {
                if id == live {
                    WorkerProcess::Alive { shell_pid: 42 }
                } else if id == dead {
                    WorkerProcess::Gone { shell_pid: 43 }
                } else {
                    WorkerProcess::Unknown
                }
            },
            None,
        )
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
        let drained = quarantine_with_probe(&reopened, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
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
        quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, None).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                [&execution],
            )
            .unwrap();
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
        quarantine_with_probe(&db, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
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
        let report = quarantine_with_probe(&db, |_| panic!("excluded rows must never be probed"), None).unwrap();
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

    #[test]
    fn tombstoned_work_item_keeps_its_hold_without_aborting_startup() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "deleted after spawn");
        db.delete_work_item(&item).unwrap();
        // Filing attention against a tombstoned task is refused; the hold
        // must still land and the sweep must still return `Ok`.
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(report.protected_execution_ids.contains(&execution));
        assert!(db.is_execution_quarantined(&execution).unwrap());
        assert!(db.ensure_work_item_not_quarantined(&item).is_err());
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_some());
        // ...and proven death still lifts it, again without an attention error
        // surfacing as a startup failure.
        let drained = quarantine_with_probe(&db, |_| WorkerProcess::Gone { shell_pid: 42 }, None).unwrap();
        assert!(drained.dead_execution_ids.contains(&execution));
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    }

    #[test]
    fn run_written_before_the_kernel_booted_is_proven_dead() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, item) = seed(&db, "pre-boot");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        // Establish the hold first, as a build that lacked boot evidence
        // would have, so this also covers lifting a persisted hold.
        quarantine_with_probe(&db, |_| WorkerProcess::Unknown, None).unwrap();
        assert!(db.is_execution_quarantined(&execution).unwrap());
        // A kernel that booted well after the row's last write outranks a
        // probe that claims the (recycled) pid is alive.
        let boot = now + PRE_BOOT_MARGIN_SECS * 2;
        let report = quarantine_with_probe(&db, |_| WorkerProcess::Alive { shell_pid: 42 }, Some(boot)).unwrap();
        assert_eq!(report.dead_execution_ids, HashSet::from([execution.clone()]));
        assert!(report.protected_execution_ids.is_empty());
        assert!(!db.is_execution_quarantined(&execution).unwrap());
        assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
        db.ensure_work_item_not_quarantined(&item).unwrap();
        db.mark_execution_orphaned(&execution, "run predates kernel boot")
            .unwrap();
    }

    #[test]
    fn run_inside_the_pre_boot_margin_or_without_boot_evidence_stays_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let (execution, _) = seed(&db, "recent");
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        for boot in [None, Some(now + PRE_BOOT_MARGIN_SECS / 2), Some(now - 1)] {
            let report = quarantine_with_probe(&db, |_| WorkerProcess::Unknown, boot).unwrap();
            assert!(report.protected_execution_ids.contains(&execution), "boot={boot:?}");
            assert!(report.dead_execution_ids.is_empty(), "boot={boot:?}");
            assert!(db.is_execution_quarantined(&execution).unwrap(), "boot={boot:?}");
        }
    }

    #[test]
    fn kernel_boot_time_is_in_the_past_on_supported_platforms() {
        let boot = kernel_boot_epoch_secs();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let boot = boot.expect("boot time readable");
            assert!(boot > 0 && boot <= boss_engine_utils::epoch_time::now_epoch_secs());
        } else {
            assert!(boot.is_none());
        }
    }
}
