//! Fresh host observations are admission budgets, never evidence of worker death.
use crate::*;
use boss_host_execution::capacity::Resources;

pub const HOST_FRESHNESS_MS: i64 = 15_000;
pub const HOST_UNREACHABLE_ATTENTION_MS: i64 = 6 * 60 * 60 * 1000;

#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct HostCapacityReport {
    pub generation: u64,
    pub sequence: u64,
    pub boot_id: String,
    pub awake_ns: u64,
    pub total: u32,
    pub occupied: u32,
    pub resources: Resources,
    pub applied_command_through: u64,
    pub preflights_ok: bool,
}

pub(crate) const MIGRATION_ID: &str = "20261112_2303_host_capacity";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_capacity (
           host_id TEXT PRIMARY KEY, connection_generation INTEGER NOT NULL, hub_boot TEXT NOT NULL,
           backlog INTEGER NOT NULL, pulse_ms INTEGER, pulse_boot TEXT, pulse_awake INTEGER,
           inventory_sequence INTEGER, report_generation INTEGER NOT NULL DEFAULT 0,
           report_ms INTEGER, total INTEGER NOT NULL DEFAULT 0, occupied INTEGER NOT NULL DEFAULT 0,
           cpu INTEGER NOT NULL DEFAULT 0, memory INTEGER NOT NULL DEFAULT 0, disk INTEGER NOT NULL DEFAULT 0,
           command_through INTEGER NOT NULL DEFAULT 0, unreachable_since_ms INTEGER
         );",
    )?;
    Ok(())
}

impl WorkDb {
    /// Reconnect invalidates readiness without releasing any assignment.
    pub fn open_host_capacity(&self, host: &str, generation: u64, backlog: u64, now: i64) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO host_capacity(host_id, connection_generation, backlog, unreachable_since_ms, hub_boot)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(host_id) DO UPDATE SET
               connection_generation = ?2, backlog = ?3, hub_boot = ?5, pulse_ms = NULL, report_ms = NULL,
               inventory_sequence = NULL, unreachable_since_ms = COALESCE(unreachable_since_ms, ?4)
             WHERE connection_generation < ?2",
            params![host, generation as i64, backlog as i64, now, hub_boot()],
        )?;
        Ok(())
    }

    pub fn host_capacity_pulse(&self, host: &str, generation: u64, boot: &str, awake: u64, now: i64) -> Result<()> {
        self.connect()?.execute(
            "UPDATE host_capacity SET pulse_ms = ?5, pulse_boot = ?3, pulse_awake = ?4,
               unreachable_since_ms = NULL
             WHERE host_id = ?1 AND connection_generation = ?2",
            params![host, generation as i64, boot, awake as i64, now],
        )?;
        Ok(())
    }

    pub fn host_capacity_disconnected(&self, host: &str, generation: u64, now: i64) -> Result<()> {
        self.connect()?.execute(
            "UPDATE host_capacity SET pulse_ms = NULL, report_ms = NULL,
               unreachable_since_ms = COALESCE(unreachable_since_ms, ?3)
             WHERE host_id = ?1 AND connection_generation = ?2",
            params![host, generation as i64, now],
        )?;
        Ok(())
    }

    /// Only the first inventory after a (re)connect sets the readiness barrier;
    /// later periodic inventories must not make a ready budget unschedulable
    /// until the projector catches up.
    pub fn host_capacity_inventory(&self, host: &str, generation: u64, sequence: u64, through: u64) -> Result<()> {
        self.connect()?.execute(
            "UPDATE host_capacity SET inventory_sequence = ?3
             WHERE host_id = ?1 AND connection_generation = ?2 AND ?3 > backlog AND ?4 >= backlog
               AND inventory_sequence IS NULL",
            params![host, generation as i64, sequence as i64, through as i64],
        )?;
        Ok(())
    }

    /// A replay cannot renew a report: generation must advance and the same-boot
    /// sample must have been taken after a fresh, authenticated nonce reply.
    pub fn record_host_capacity(
        &self,
        host: &str,
        connection: u64,
        report: &HostCapacityReport,
        now: i64,
    ) -> Result<()> {
        let conn = self.connect()?;
        let through = report.applied_command_through as i64;
        let issued: i64 = conn.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM host_commands WHERE host_id = ?1",
            [host],
            |r| r.get(0),
        )?;
        if through < 0 || through > issued {
            // Fail closed for the budget only: the receipt is already durable
            // and an inconsistent prefix must not tear down the stream.
            tracing::error!(
                host_id = %host,
                applied_command_through = through,
                issued,
                "capacity report claims a command prefix beyond the issued commands; budget not updated"
            );
            return Ok(());
        }
        conn.execute(
            "UPDATE host_capacity SET report_generation = ?3, report_ms = ?4, total = ?5,
               occupied = ?6, cpu = ?7, memory = ?8, disk = ?9, command_through = ?10
             WHERE host_id = ?1 AND connection_generation = ?2 AND report_generation < ?3
               AND ?11 > backlog AND pulse_boot = ?12 AND pulse_awake <= ?13
               AND pulse_ms > ?4 - ?14 AND pulse_ms <= ?4 AND command_through <= ?10",
            params![
                host,
                connection as i64,
                report.generation as i64,
                now,
                if report.preflights_ok { report.total } else { 0 },
                report.occupied,
                report.resources.cpu_millis as i64,
                report.resources.memory_bytes as i64,
                report.resources.disk_bytes as i64,
                through,
                report.sequence as i64,
                report.boot_id,
                report.awake_ns as i64,
                HOST_FRESHNESS_MS
            ],
        )?;
        Ok(())
    }

    /// Selection is advisory; admission repeats this inside the ownership transaction.
    pub fn available_host_budget(&self, host: &str) -> Result<u64> {
        available(&*self.connect()?, host, now_ms())
    }

    /// Silence produces a deduplicated attention, never an orphan or lease release.
    pub fn reconcile_unreachable_hosts(&self, now: i64) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // A host with unresolved assignments that has not connected since an
        // upgrade has no row yet; track it from now, with zero readiness.
        tx.execute(
            "INSERT OR IGNORE INTO host_capacity(host_id, connection_generation, hub_boot, backlog, unreachable_since_ms)
             SELECT DISTINCT a.host_id, 0, '', 0, ?1 FROM spoke_assignments a
             JOIN hosts h ON h.id = a.host_id
             WHERE a.resolved_at_ms IS NULL",
            params![now],
        )?;
        tx.execute(
            "UPDATE host_capacity SET unreachable_since_ms = COALESCE(unreachable_since_ms, pulse_ms + ?2, ?1)
             WHERE pulse_ms IS NULL OR pulse_ms <= ?1 - ?2",
            params![now, HOST_FRESHNESS_MS],
        )?;
        let runs: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                "SELECT a.execution_id, a.host_id FROM spoke_assignments a
                 JOIN hosts h ON h.id = a.host_id
                 JOIN host_capacity c ON c.host_id = a.host_id
                 JOIN work_executions e ON e.id = a.execution_id
                 WHERE a.resolved_at_ms IS NULL AND c.unreachable_since_ms <= ?1
                   AND e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                   AND NOT EXISTS (SELECT 1 FROM work_attention_items i WHERE i.execution_id = a.execution_id
                       AND i.kind = 'host_unreachable' AND i.status = 'open')",
            )?;
            statement
                .query_map([now - HOST_UNREACHABLE_ATTENTION_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (execution, host) in runs {
            crate::workitems::insert_attention_item_row(&tx, &CreateAttentionItemInput {
                execution_id: Some(execution), work_item_id: None, kind: "host_unreachable".into(),
                title: format!("Host {host} is unreachable"),
                body_markdown: "No fresh host challenge response for six hours. The worker may still be running; its assignment and lease remain held. Reconnect the host or explicitly abandon the run after fencing the worker.".into(),
                status: None, resolved_at: None,
            })?;
        }
        // An item stays open only while its execution is live, its newest
        // assignment is unresolved on a host that still exists, and that host
        // has not answered a fresh challenge. A terminal run, a resolved
        // assignment or a removed host therefore clears it.
        tx.execute(
            "UPDATE work_attention_items SET status = 'resolved', resolved_at = ?1
             WHERE kind = 'host_unreachable' AND status = 'open' AND NOT EXISTS (
                SELECT 1 FROM spoke_assignments a
                JOIN hosts h ON h.id = a.host_id
                JOIN work_executions e ON e.id = a.execution_id
                LEFT JOIN host_capacity c ON c.host_id = a.host_id
                WHERE a.execution_id = work_attention_items.execution_id
                  AND a.assignment_generation = (SELECT MAX(newest.assignment_generation)
                      FROM spoke_assignments newest WHERE newest.execution_id = a.execution_id)
                  AND a.resolved_at_ms IS NULL
                  AND e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                  AND COALESCE(c.pulse_ms, 0) <= ?2)",
            params![now_string(), now - HOST_FRESHNESS_MS],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

fn hub_boot() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| crate::next_id("capacity-hub"))
}

/// Admission lost the race for a seat: another dispatcher took it, or the
/// freshness or inventory window closed after selection. The work is deferred,
/// never failed.
#[derive(Debug, thiserror::Error)]
#[error("host {host_id} has no fresh resource budget")]
pub struct HostBudgetExhausted {
    pub host_id: String,
}

pub(crate) fn require_budget(conn: &Connection, host: &str) -> Result<()> {
    if available(conn, host, now_ms())? == 0 {
        return Err(HostBudgetExhausted {
            host_id: host.to_owned(),
        }
        .into());
    }
    Ok(())
}

/// Seats a ready spoke host can take now. A host on the legacy SSH transport,
/// or a `spoke_pending` host that still launches over SSH, has no reports to
/// age, so it has no budget here: its seats are still the configured pool
/// minus its active runs, counted by the caller.
fn available(conn: &Connection, host: &str, now: i64) -> Result<u64> {
    let config: Option<(bool, i64)> = conn
        .query_row("SELECT enabled, pool_size FROM hosts WHERE id = ?1", [host], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((true, pool)) = config else {
        return Ok(0);
    };
    let open: u64 = conn.query_row(
        "SELECT COUNT(*) FROM spoke_assignments WHERE host_id = ?1 AND resolved_at_ms IS NULL",
        [host],
        |r| r.get(0),
    )?;
    let report: Option<(u64, u64, Resources, i64)> = conn.query_row(
        "SELECT total, occupied, cpu, memory, disk, command_through FROM host_capacity c
         WHERE host_id = ?1 AND hub_boot = ?4 AND pulse_ms > ?2 AND report_ms > ?2 AND pulse_ms <= ?3 AND report_ms <= ?3
           AND inventory_sequence IS NOT NULL AND inventory_sequence <= (
               SELECT applied FROM spoke_projection WHERE host_id = c.host_id AND halt_reason IS NULL)
           AND connection_generation = (SELECT generation FROM spoke_journals WHERE host_id = c.host_id)",
        params![host, now - HOST_FRESHNESS_MS, now, hub_boot()], |r| Ok((r.get(0)?, r.get(1)?, Resources {
            cpu_millis: r.get(2)?, memory_bytes: r.get(3)?, disk_bytes: r.get(4)? }, r.get(5)?))).optional()?;
    let Some((total, occupied, resources, through)) = report else {
        return Ok(0);
    };
    let outstanding: u64 = conn.query_row(
        "SELECT COUNT(*) FROM spoke_assignments a WHERE host_id = ?1 AND resolved_at_ms IS NULL
           AND NOT EXISTS(SELECT 1 FROM host_commands c WHERE c.host_id = a.host_id
               AND c.command_id = ?3 || a.run_id AND c.sequence <= ?2)",
        params![host, through, crate::spoke_hub::HOST_COMMAND_SPAWN_PREFIX],
        |r| r.get(0),
    )?;
    Ok(resources
        .workers(outstanding)
        .min(total.saturating_sub(occupied).saturating_sub(outstanding))
        .min((pool.max(0) as u64).saturating_sub(open)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_host_execution::capacity::{DISK_FLOOR_BYTES, WORKER_DISK_BYTES, WORKER_MEMORY_BYTES};

    fn fixture() -> (WorkDb, i64) {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        db.add_host("host", "", 4, &[]).unwrap();
        db.set_host_transport("host", HostTransport::SpokeReady).unwrap();
        db.open_spoke_journal("host", "journal", 0, 1).unwrap();
        let now = now_ms();
        db.open_host_capacity("host", 1, 0, now).unwrap();
        db.host_capacity_inventory("host", 1, 1, 0).unwrap();
        // The projector has applied the inventory barrier.
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO spoke_projection(host_id, journal_id, applied) VALUES ('host', 'journal', 1)",
                [],
            )
            .unwrap();
        (db, now)
    }

    fn report() -> HostCapacityReport {
        HostCapacityReport::builder()
            .generation(2)
            .sequence(2)
            .boot_id("boot")
            .awake_ns(20)
            .total(4)
            .occupied(0)
            .resources(Resources {
                cpu_millis: 4000,
                memory_bytes: 4 * WORKER_MEMORY_BYTES,
                disk_bytes: DISK_FLOOR_BYTES + 4 * WORKER_DISK_BYTES,
            })
            .applied_command_through(0)
            .preflights_ok(true)
            .build()
    }

    fn ready(db: &WorkDb, now: i64) {
        db.host_capacity_pulse("host", 1, "boot", 10, now).unwrap();
        db.record_host_capacity("host", 1, &report(), now).unwrap();
    }

    #[test]
    fn capacity_requires_current_challenge_replay_and_projection_barriers() {
        let (db, now) = fixture();
        db.record_host_capacity("host", 1, &report(), now).unwrap();
        assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
        ready(&db, now);
        assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 4);
        db.connect()
            .unwrap()
            .execute("UPDATE spoke_projection SET applied = 0", [])
            .unwrap();
        assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
        db.connect()
            .unwrap()
            .execute("UPDATE spoke_projection SET applied = 1", [])
            .unwrap();
        db.host_capacity_disconnected("host", 1, now).unwrap();
        assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
    }

    #[test]
    fn simultaneous_admissions_cannot_spend_the_same_budget() {
        let (db, now) = fixture();
        ready(&db, now);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let db = db.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        db.admit_spoke_assignment(&format!("execution-{i}"), "host")
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap().ok())
                .collect::<Vec<_>>()
        });
        assert_eq!(winners.len(), 4);
        assert_eq!(db.available_host_budget("host").unwrap(), 0);
        let winner = &winners[0];
        assert_eq!(
            db.admit_spoke_assignment(&winner.execution_id, "host").unwrap(),
            *winner
        );
        assert!(
            db.resolve_spoke_assignment(&winner.execution_id, winner.assignment_generation, "never_started")
                .unwrap()
        );
        assert!(
            !db.resolve_spoke_assignment(&winner.execution_id, winner.assignment_generation, "never_started")
                .unwrap()
        );
        assert_eq!(db.available_host_budget("host").unwrap(), 1);
    }

    #[test]
    fn prefix_accounts_for_running_work_once_and_pending_assignments_separately() {
        let (db, now) = fixture();
        ready(&db, now);
        let first = db.admit_spoke_assignment("first", "host").unwrap();
        let key = SpokeRunKey {
            execution_id: first.execution_id,
            assignment_generation: first.assignment_generation,
            run_id: first.run_id.clone(),
            spawn_token: first.spawn_token,
        };
        db.resume_host_command_journal("host", None).unwrap();
        let command = db
            .append_host_command(
                "host",
                &crate::spoke_hub::host_command_spawn_id(&first.run_id),
                Some(&key),
                RunAdmission::Start,
                b"spawn",
                &|_| b"spawn".to_vec(),
            )
            .unwrap();
        db.admit_spoke_assignment("second", "host").unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 2);
        let mut updated = report();
        updated.generation = 3;
        updated.sequence = 3;
        updated.applied_command_through = command.sequence;
        updated.occupied = 1;
        updated.resources.cpu_millis = 3000;
        db.record_host_capacity("host", 1, &updated, now).unwrap();
        assert_eq!(
            db.available_host_budget("host").unwrap(),
            2,
            "count the first run only in the report"
        );
        db.record_host_capacity("host", 1, &report(), now).unwrap();
        assert_eq!(
            db.available_host_budget("host").unwrap(),
            2,
            "older capacity cannot restore spent resources"
        );
    }

    #[test]
    fn replay_staleness_boot_and_connection_fences_fail_closed() {
        let (db, now) = fixture();
        ready(&db, now);
        let later = now + HOST_FRESHNESS_MS;
        db.host_capacity_pulse("host", 1, "boot", 30, later).unwrap();
        db.record_host_capacity("host", 1, &report(), later).unwrap();
        assert_eq!(
            available(&db.connect().unwrap(), "host", later).unwrap(),
            0,
            "duplicate reports do not renew freshness"
        );
        let mut old_sample = report();
        old_sample.generation = 3;
        db.record_host_capacity("host", 1, &old_sample, later).unwrap();
        assert_eq!(
            available(&db.connect().unwrap(), "host", later).unwrap(),
            0,
            "buffered sample predates the challenge"
        );
        db.open_host_capacity("host", 2, 10, later).unwrap();
        ready(&db, later);
        assert_eq!(
            available(&db.connect().unwrap(), "host", later).unwrap(),
            0,
            "previous stream is fenced"
        );
        db.host_capacity_pulse("host", 2, "new-boot", 1, later).unwrap();
        old_sample.sequence = 11;
        db.record_host_capacity("host", 2, &old_sample, later).unwrap();
        assert_eq!(
            available(&db.connect().unwrap(), "host", later).unwrap(),
            0,
            "different boot cannot borrow a fresh challenge"
        );
    }

    #[test]
    fn disk_pressure_preflight_failure_and_enablement_are_independent() {
        let (db, now) = fixture();
        ready(&db, now);
        db.set_host_enabled("host", false).unwrap();
        db.host_capacity_pulse("host", 1, "boot", 10, now).unwrap();
        assert!(!db.get_host("host").unwrap().unwrap().enabled);
        assert_eq!(db.available_host_budget("host").unwrap(), 0);
        db.set_host_enabled("host", true).unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 4);
        let mut pressure = report();
        pressure.generation = 3;
        pressure.resources.disk_bytes = DISK_FLOOR_BYTES;
        db.record_host_capacity("host", 1, &pressure, now).unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 0);
        pressure = report();
        pressure.generation = 4;
        pressure.preflights_ok = false;
        db.record_host_capacity("host", 1, &pressure, now).unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 0);
        assert!(db.get_host("host").unwrap().unwrap().enabled);
    }

    #[test]
    fn a_later_inventory_does_not_zero_a_ready_budget() {
        let (db, now) = fixture();
        ready(&db, now);
        assert_eq!(db.available_host_budget("host").unwrap(), 4);
        // A periodic inventory lands before the projector has applied it.
        db.host_capacity_inventory("host", 1, 50, 0).unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 4);
    }

    #[test]
    fn an_inconsistent_command_prefix_withholds_the_budget_without_failing() {
        let (db, now) = fixture();
        db.host_capacity_pulse("host", 1, "boot", 10, now).unwrap();
        let mut bad = report();
        bad.applied_command_through = 99;
        db.record_host_capacity("host", 1, &bad, now).unwrap();
        assert_eq!(db.available_host_budget("host").unwrap(), 0);
    }

    #[test]
    fn losing_the_admission_race_is_a_typed_budget_error() {
        let (db, now) = fixture();
        ready(&db, now);
        for i in 0..4 {
            db.admit_spoke_assignment(&format!("e{i}"), "host").unwrap();
        }
        let error = db.admit_spoke_assignment("e4", "host").unwrap_err();
        assert!(error.downcast_ref::<HostBudgetExhausted>().is_some(), "{error:#}");
    }

    #[test]
    fn a_removed_host_is_not_seeded_or_raised() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        db.add_host("seeded", "", 4, &[]).unwrap();
        db.reserve_spoke_assignment("seeded-run", "seeded").unwrap();
        db.remove_host("seeded").unwrap();
        db.add_host("raised", "", 4, &[]).unwrap();
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
                 VALUES ('orphan', 'item', 'task_implementation', 'running', 'https://example.test/r', '1')",
                [],
            )
            .unwrap();
        db.reserve_spoke_assignment("orphan", "raised").unwrap();
        let now = now_ms();
        db.open_host_capacity("raised", 0, 0, now - HOST_UNREACHABLE_ATTENTION_MS - 1)
            .unwrap();
        db.remove_host("raised").unwrap();
        db.reconcile_unreachable_hosts(now).unwrap();
        db.reconcile_unreachable_hosts(now + HOST_UNREACHABLE_ATTENTION_MS + 1)
            .unwrap();
        let seeded: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM host_capacity WHERE host_id = 'seeded'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(seeded, 0, "a removed host must not be inserted into host_capacity");
        assert!(
            db.list_open_attention_items_of_kind("host_unreachable")
                .unwrap()
                .is_empty(),
            "a removed host must not raise host_unreachable"
        );
    }
}
