//! Durable persona leases. SQLite serializes allocation with the spawn record;
//! the partial unique index protects local and remote workers alike. Terminal
//! status does not release a lease: resource cleanup does, preserving history.

use std::collections::HashSet;

use super::*;

crate::register_counter!(
    ROSTER_EXHAUSTED,
    "persona_roster_exhausted",
    "Persona allocations that exhausted the crew roster and used an Ensign name."
);

pub(crate) fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&ROSTER_EXHAUSTED);
}

pub(super) fn new_metrics_registry() -> Arc<crate::metrics::Registry> {
    let registry = Arc::new(crate::metrics::Registry::new());
    register_metrics(&registry);
    registry
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "ALTER TABLE work_runs ADD COLUMN persona TEXT;
         ALTER TABLE work_runs ADD COLUMN persona_lease_active INTEGER NOT NULL DEFAULT 0
             CHECK (persona_lease_active IN (0, 1));
         CREATE UNIQUE INDEX work_runs_live_persona ON work_runs(persona)
             WHERE persona_lease_active = 1;
         CREATE UNIQUE INDEX work_runs_execution_persona_lease ON work_runs(execution_id)
             WHERE persona_lease_active = 1;",
    )?;
    Ok(())
}

/// Called inside the spawn/restoration transaction, after inserting the row.
/// Returns whether this call allocated a new overflow lease.
pub(super) fn allocate(conn: &Connection, run_id: &str) -> Result<bool> {
    let (mut previous, active): (Option<String>, bool) = conn.query_row(
        "SELECT persona, persona_lease_active FROM work_runs WHERE id = ?1",
        [run_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if active {
        return Ok(false);
    }
    let held: Option<(String, String)> = conn
        .query_row(
            "SELECT id, persona FROM work_runs WHERE persona_lease_active = 1
         AND execution_id = (SELECT execution_id FROM work_runs WHERE id = ?1)",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((old_run_id, persona)) = held {
        conn.execute(
            "UPDATE work_runs SET persona_lease_active = 0 WHERE id = ?1",
            [old_run_id],
        )?;
        previous = Some(persona);
    }
    let used: HashSet<String> = conn
        .prepare("SELECT persona FROM work_runs WHERE persona_lease_active = 1 AND persona IS NOT NULL")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let restored = previous.filter(|name| !name.is_empty() && !used.contains(name));
    let persona = restored
        .clone()
        .or_else(|| {
            boss_protocol::ROSTER
                .iter()
                .find(|name| !used.contains(**name))
                .map(|name| (*name).to_owned())
        })
        .unwrap_or_else(|| {
            (1u64..)
                .map(|n| format!("Ensign {n}"))
                .find(|name| !used.contains(name))
                .expect("finite live roster has a free overflow name")
        });
    conn.execute(
        "UPDATE work_runs SET persona = ?2, persona_lease_active = 1 WHERE id = ?1",
        params![run_id, persona],
    )?;
    Ok(restored.is_none() && persona.starts_with("Ensign "))
}

impl WorkDb {
    /// Apply the authoritative startup death verdict before dispatch reconciliation.
    pub(crate) fn reap_startup_dead_execution(
        &self,
        execution_id: &str,
        verdict: &crate::run_reconcile::RunReconcileVerdict,
    ) -> Result<Option<WorkExecution>> {
        if !matches!(verdict, crate::run_reconcile::RunReconcileVerdict::Dead) {
            return Ok(None);
        }
        if let Err(error) = self.release_persona(execution_id) {
            tracing::error!(execution_id, %error, "startup reaper: could not release persona of dead worker");
        }
        self.mark_execution_orphaned(
            execution_id,
            "engine startup: recovery probe proved worker dead across restart",
        )
        .map(Some)
    }

    pub fn with_persona_metrics(mut self, registry: Arc<crate::metrics::Registry>) -> Self {
        self.persona_metrics = registry;
        self
    }

    pub(super) fn record_persona_overflow(&self, count: u64) {
        if count > 0 {
            ROSTER_EXHAUSTED.inc_by(&self.persona_metrics, count);
            tracing::warn!(count, "persona roster exhausted; allocated unique Ensign leases");
        }
    }

    /// Existing leases are already reserved in SQLite, including remote and
    /// terminal workers awaiting cleanup. Restore stored historical personas
    /// first, then backfill older rows in the adoption query's created_at/id order.
    pub fn restore_tmux_personas(&self, runs: &[TmuxRunHandle]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut existing = Vec::new();
        let mut missing = Vec::new();
        for run in runs {
            let persona: Option<String> =
                tx.query_row("SELECT persona FROM work_runs WHERE id = ?1", [&run.run_id], |row| {
                    row.get(0)
                })?;
            if persona.is_some() {
                existing.push(&run.run_id);
            } else {
                missing.push(&run.run_id);
            }
        }
        let mut overflow = 0;
        for run_id in existing.into_iter().chain(missing) {
            overflow += u64::from(allocate(&tx, run_id)?);
        }
        tx.commit()?;
        self.record_persona_overflow(overflow);
        Ok(())
    }

    /// Restore the worker row, preferring an already-held lease or tmux identity
    /// over later bookkeeping-only siblings. Same-run adoption is idempotent.
    pub fn lease_persona_for_execution(&self, execution_id: &str) -> Result<String> {
        let mut conn = self.connect()?;
        // Restoration of a held lease is read-only, including when the DB can
        // no longer accept writes but tmux proves that the worker is alive.
        let held: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs WHERE execution_id = ?1 AND persona_lease_active = 1)",
            [execution_id],
            |row| row.get(0),
        )?;
        if held {
            return persona_display_name(&conn, execution_id)?.context("held persona missing");
        }
        let tx = conn.transaction()?;
        let run_id: String = tx.query_row(
            "SELECT id FROM work_runs WHERE execution_id = ?1
             ORDER BY persona_lease_active DESC, (tmux_spawn_token IS NOT NULL) DESC,
                      (persona IS NOT NULL) DESC, created_at DESC, id DESC LIMIT 1",
            [execution_id],
            |row| row.get(0),
        )?;
        let overflow = allocate(&tx, &run_id)?;
        let name = persona_display_name(&tx, execution_id)?.context("allocated persona missing")?;
        tx.commit()?;
        self.record_persona_overflow(u64::from(overflow));
        Ok(name)
    }

    /// Pure name projection by execution identity, including historical husks.
    /// A viewer report never acquires a persona or infers one from its slot.
    pub fn persona_display_name(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        persona_display_name(&conn, execution_id)
    }

    /// Terminal remote rows can predate registry registration or survive a
    /// crash between terminalization and cleanup. Never reclaim tmux owners.
    pub(crate) fn terminal_remote_persona_executions(&self) -> Result<Vec<String>> {
        let candidates = {
            let conn = self.connect()?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT r.execution_id FROM work_runs r
                 WHERE r.persona_lease_active = 1 AND r.host_id != 'local'
                 AND NOT EXISTS (SELECT 1 FROM work_runs t
                     WHERE t.execution_id = r.execution_id AND t.tmux_spawn_token IS NOT NULL)",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        candidates
            .into_iter()
            .filter_map(|id| match self.get_execution(&id) {
                Ok(execution) if execution.status.is_terminal() => Some(Ok(id)),
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            })
            .collect()
    }

    pub fn release_persona(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE work_runs SET persona_lease_active = 0 WHERE execution_id = ?1 AND persona_lease_active = 1",
            [execution_id],
        )?;
        Ok(())
    }
}

fn persona_display_name(conn: &Connection, execution_id: &str) -> Result<Option<String>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT persona, host_id FROM work_runs WHERE execution_id = ?1 AND persona IS NOT NULL
         ORDER BY persona_lease_active DESC, created_at DESC, id DESC LIMIT 1",
            [execution_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(persona, host)| {
        if host == "local" {
            persona
        } else {
            format!("{persona} (Remote)")
        }
    }))
}

#[cfg(test)]
mod tests;
