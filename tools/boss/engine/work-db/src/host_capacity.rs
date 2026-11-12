//! Fresh host observations are admission budgets, never evidence of worker death.
use crate::*;
use anyhow::ensure;
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

    pub fn host_capacity_inventory(&self, host: &str, generation: u64, sequence: u64, through: u64) -> Result<()> {
        self.connect()?.execute(
            "UPDATE host_capacity SET inventory_sequence = ?3
             WHERE host_id = ?1 AND connection_generation = ?2 AND ?3 > backlog AND ?4 >= backlog
               AND (inventory_sequence IS NULL OR inventory_sequence < ?3)",
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
        ensure!(
            through >= 0 && through <= issued,
            "capacity command prefix exceeds issued commands"
        );
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
        tx.execute(
            "UPDATE host_capacity SET unreachable_since_ms = COALESCE(unreachable_since_ms, pulse_ms + ?2, ?1)
             WHERE pulse_ms IS NULL OR pulse_ms <= ?1 - ?2",
            params![now, HOST_FRESHNESS_MS],
        )?;
        let runs: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                "SELECT a.execution_id, a.host_id FROM spoke_assignments a
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
        tx.execute(
            "UPDATE work_attention_items SET status = 'resolved', resolved_at = ?1
            WHERE kind = 'host_unreachable' AND status = 'open' AND EXISTS (
                SELECT 1 FROM spoke_assignments a JOIN host_capacity c ON c.host_id = a.host_id
                WHERE a.execution_id = work_attention_items.execution_id
                  AND a.assignment_generation = (SELECT MAX(newest.assignment_generation)
                      FROM spoke_assignments newest WHERE newest.execution_id = a.execution_id)
                  AND (a.resolved_at_ms IS NOT NULL OR c.pulse_ms > ?2))",
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

pub(crate) fn require_budget(conn: &Connection, host: &str) -> Result<()> {
    ensure!(
        available(conn, host, now_ms())? > 0,
        "host {host} has no fresh resource budget"
    );
    Ok(())
}

/// Seats a spoke-transport host can take now. A host on the legacy transport
/// has no reports to age, so it has no budget here: its seats are still the
/// configured pool minus its active runs, counted by the caller.
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
#[path = "host_capacity_tests.rs"]
mod tests;
