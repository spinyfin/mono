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

    /// Return (postmortem, created). The immediate transaction serializes
    /// automatic and operator requests across connections, and rechecks open
    /// work at the insertion boundary. Tombstones also count as existing.
    pub fn start_project_postmortem(&self, project_id: &str) -> Result<(Task, bool)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let project = query_project(&tx, project_id).require("project", project_id)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design_postmortem'
             ORDER BY created_at, id LIMIT 1",
                [project_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok((query_task(&tx, &id).require("task", &id)?, false));
        }
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
             AND kind != 'design_postmortem' AND status = 'done' AND pr_url IS NOT NULL
             ORDER BY created_at, id",
        )?;
        let prs = stmt
            .query_map([project_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
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
