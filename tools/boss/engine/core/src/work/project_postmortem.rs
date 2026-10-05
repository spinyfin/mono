//! Durable completion signals and the shared, serialized scheduling decision.
use super::*;

pub(crate) fn migrate_project_postmortem_signals(conn: &Connection) -> Result<()> {
    // Signals are written in the mutation's transaction, including direct SQL
    // writers (merge reconciliation and cascade deletion). No historical rows
    // are seeded: installing this feature must not backfill old projects.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_postmortem_signals (
            project_id TEXT PRIMARY KEY REFERENCES projects(id)
        );
        CREATE TRIGGER IF NOT EXISTS project_postmortem_task_changed
        AFTER UPDATE OF status, project_id, deleted_at ON tasks
        WHEN OLD.project_id IS NOT NULL AND OLD.deleted_at IS NULL
          AND OLD.kind != 'design_postmortem'
          AND (OLD.kind != 'design' OR EXISTS (
              SELECT 1 FROM tasks WHERE project_id = OLD.project_id
                AND kind IN ('project_task', 'investigation')
                AND status = 'done' AND deleted_at IS NULL))
          AND OLD.status NOT IN ('done', 'archived')
          AND (NEW.status IN ('done', 'archived') OR NEW.deleted_at IS NOT NULL
               OR NEW.project_id IS NOT OLD.project_id)
        BEGIN
            INSERT OR IGNORE INTO project_postmortem_signals VALUES (OLD.project_id);
        END;
        CREATE TRIGGER IF NOT EXISTS project_postmortem_project_done
        AFTER UPDATE OF status ON projects
        WHEN NEW.status = 'done' AND OLD.status != 'done'
        BEGIN
            INSERT OR IGNORE INTO project_postmortem_signals VALUES (NEW.id);
        END;",
    )?;
    Ok(())
}

impl WorkDb {
    pub(crate) fn has_project_postmortem_signal(&self, project_id: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_postmortem_signals WHERE project_id = ?1)",
            [project_id],
            |row| row.get(0),
        )?)
    }

    /// Start a postmortem for `boss project postmortem` / the
    /// `StartProjectPostmortem` RPC. Deleted postmortems are ignored, so the
    /// command can recover a project whose postmortem was removed.
    pub fn start_project_postmortem(&self, project_id: &str) -> Result<(Task, bool)> {
        self.start_postmortem(project_id, false)
    }

    /// Start a postmortem for the sweep. A deleted postmortem still anchors
    /// the "work completed since" cutoff, so deleting one dismisses the work
    /// it covered without blocking a later wave.
    pub(crate) fn schedule_project_postmortem(&self, project_id: &str) -> Result<(Task, bool)> {
        self.start_postmortem(project_id, true)
    }

    /// Return (postmortem, created). The immediate transaction serializes
    /// the sweep and the `StartProjectPostmortem` RPC across connections, and
    /// rechecks open work at the insertion boundary. A live, non-terminal
    /// postmortem is returned as-is; otherwise a new one needs a done
    /// `project_task`/`investigation` completed after the latest
    /// postmortem's cutoff (or any, if there has been none).
    fn start_postmortem(&self, project_id: &str, tombstones_anchor: bool) -> Result<(Task, bool)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let project = query_project(&tx, project_id).require("project", project_id)?;
        let latest: Option<(String, String, i64, bool)> = tx
            .query_row(
                "SELECT id, status,
                        COALESCE(CAST(NULLIF(completed_at, '') AS INTEGER), CAST(created_at AS INTEGER), 0),
                        deleted_at IS NOT NULL
                 FROM tasks WHERE project_id = ?1 AND kind = 'design_postmortem'
                   AND (?2 OR deleted_at IS NULL)
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                rusqlite::params![project_id, tombstones_anchor],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let live_open: Option<String> = tx
            .query_row(
                "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design_postmortem'
                   AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [project_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = live_open {
            return Ok((query_task(&tx, &id).require("task", &id)?, false));
        }
        let cutoff = latest.as_ref().map(|(_, _, cutoff, _)| *cutoff);
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tasks WHERE project_id = ?1 AND deleted_at IS NULL
             AND kind != 'design_postmortem' AND status NOT IN ('done', 'archived')",
            [project_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(open == 0, "cannot start project postmortem: {open} open task(s) remain");
        let planning: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM planner_runs WHERE project_id = ?1
             AND outcome IN ('running', 'staged'))",
            [project_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !planning,
            "cannot start project postmortem: planning or staged work remains"
        );
        anyhow::ensure!(
            project.status != ProjectStatus::Archived,
            "cannot start postmortem for an archived project"
        );
        anyhow::ensure!(
            project.design_doc_path.as_deref().is_some_and(|path| !path.is_empty()),
            "cannot start project postmortem: no design doc is set"
        );
        let mut stmt = tx.prepare(
            "SELECT name, pr_url FROM tasks WHERE project_id = ?1 AND deleted_at IS NULL
             AND kind IN ('project_task', 'investigation') AND status = 'done'
             AND CAST(NULLIF(completed_at, '') AS INTEGER) > ?2
             ORDER BY created_at, id",
        )?;
        let done = stmt
            .query_map(rusqlite::params![project_id, cutoff.unwrap_or(-1)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if done.is_empty() {
            if let Some((id, _, _, false)) = &latest {
                return Ok((query_task(&tx, id).require("task", id)?, false));
            }
            anyhow::bail!(
                "cannot start project postmortem: no implementation work completed since the last postmortem"
            );
        }
        let prs = done
            .into_iter()
            .filter_map(|(name, url)| url.filter(|u| !u.is_empty()).map(|u| (name, u)))
            .collect::<Vec<_>>();
        let refs = prs
            .iter()
            .map(|(name, url)| (name.as_str(), url.as_str()))
            .collect::<Vec<_>>();
        let description = crate::project_postmortem_sweep::compose_postmortem_brief(&project, &refs);
        let task = design_postmortem::insert_design_postmortem_in_tx(
            &tx,
            &project.product_id,
            project_id,
            &project.name,
            description,
        )?;
        tx.commit()?;
        Ok((task, true))
    }
}
