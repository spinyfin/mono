//! DB-layer support for `kind = 'design_postmortem'` tasks — the
//! auto-scheduled task that reviews a project's merged PRs against its
//! design doc once the project's implementation work drains to zero.
//! The edge-trigger / dedup / precondition decision logic lives in
//! `crate::project_postmortem_sweep`; this module only owns the two DB
//! operations that sweep needs: finding the most recent postmortem for a
//! project (dedup gate + "since last postmortem" cutoff) and inserting a
//! new one.

use super::*;

/// A narrow projection of a project's trigger-count task
/// (`project_task`/`design`/`investigation`) — just the fields
/// `project_postmortem_sweep` needs to decide whether to fire. Deliberately
/// not a full [`Task`]: `WorkDb::list_tasks` — the obvious-looking
/// alternative — runs the shared 32-column base query, which omits
/// `completed_at` entirely (see `mappers::map_task`'s doc comment); every
/// `Task` it returns carries `completed_at: None` regardless of the actual
/// column value. The sweep's cutoff comparison needs the real value, so it
/// gets its own query instead of a wider change to the shared, widely-used
/// `list_tasks`/`map_task` pairing.
#[derive(Debug, Clone)]
pub(crate) struct TriggerTaskSnapshot {
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub name: String,
    pub pr_url: Option<String>,
    pub completed_at: Option<String>,
}

impl WorkDb {
    /// Every live (non-deleted) trigger-count task
    /// (`project_task`/`design`/`investigation`) for a project, with
    /// `completed_at` populated. See [`TriggerTaskSnapshot`] for why this
    /// is a dedicated query rather than `list_tasks`.
    pub(crate) fn list_project_trigger_tasks(
        &self,
        product_id: &str,
        project_id: &str,
    ) -> Result<Vec<TriggerTaskSnapshot>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT kind, status, name, pr_url, completed_at
             FROM tasks
             WHERE product_id = ?1 AND project_id = ?2
               AND kind IN ('project_task', 'design', 'investigation')
               AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![product_id, project_id], |row| {
            let kind_raw: String = row.get(0)?;
            let kind = kind_raw.parse::<TaskKind>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                )
            })?;
            let status_raw: String = row.get(1)?;
            let status = status_raw.parse::<TaskStatus>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                )
            })?;
            Ok(TriggerTaskSnapshot {
                kind,
                status,
                name: row.get(2)?,
                pr_url: row.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
                completed_at: row.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
            })
        })?;
        collect_rows(rows)
    }
}

impl WorkDb {
    /// Most recently created (non-deleted) `design_postmortem` task for
    /// `project_id`, or `None` if the project has never had one.
    ///
    /// Used by `project_postmortem_sweep` both as the dedup gate (skip
    /// scheduling while the returned task is still open) and as the
    /// timestamp cutoff for "implementation work completed since the last
    /// postmortem" (a wave of zero net work must not spawn another one).
    pub fn last_design_postmortem_for_project(&self, project_id: &str) -> Result<Option<Task>> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM tasks
                 WHERE project_id = ?1 AND kind = 'design_postmortem' AND deleted_at IS NULL
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![project_id],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => query_task(&conn, &id),
            None => Ok(None),
        }
    }

    /// Create the auto-scheduled `design_postmortem` task for a project.
    /// `description` carries the full remit brief (merged-PR list,
    /// instructions) — mirrors how every other `engine_auto` task stamps
    /// its brief onto `description` for the worker prompt's `- details:`
    /// block (see `runner::work_item_details`).
    ///
    /// No `repo_remote_url` override is set, matching the auto-created
    /// `design` seed task: the row inherits the project's design-doc repo
    /// via the same product-level resolution
    /// (`exec_status_helpers::resolve_repo_for_work_item`'s `Design |
    /// DesignPostmortem` arm).
    pub fn create_design_postmortem(
        &self,
        product_id: &str,
        project_id: &str,
        project_name: &str,
        description: String,
    ) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let task = insert_design_postmortem_in_tx(&tx, product_id, project_id, project_name, description)?;
        tx.commit()?;
        Ok(task)
    }
}

pub(crate) fn insert_design_postmortem_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    project_name: &str,
    description: String,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let name = format!("Design postmortem: {project_name}");
    let short_id = allocate_short_id(conn, product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
         VALUES (?1, ?2, ?3, 'design_postmortem', ?6, ?7, 'todo', NULL, NULL, NULL, ?4, ?4, 1, 'medium', ?5, ?8)",
        params![id, product_id, project_id, now, CREATED_VIA_ENGINE_AUTO, name, description, short_id],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing design postmortem task after insert: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_product_named, open_db};

    #[test]
    fn create_design_postmortem_inserts_project_scoped_task() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();

        let task = db
            .create_design_postmortem(&product.id, &project.id, &project.name, "review these PRs".to_owned())
            .unwrap();

        assert_eq!(task.kind, TaskKind::DesignPostmortem);
        assert_eq!(task.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(task.status, TaskStatus::Todo);
        assert!(task.autostart);
        assert_eq!(task.description, "review these PRs");
        assert_eq!(task.created_via, CREATED_VIA_ENGINE_AUTO);
    }

    #[test]
    fn last_design_postmortem_for_project_returns_most_recent() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();

        assert!(db.last_design_postmortem_for_project(&project.id).unwrap().is_none());

        let first = db
            .create_design_postmortem(&product.id, &project.id, &project.name, "first".to_owned())
            .unwrap();
        let found = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(found.id, first.id);

        let second = db
            .create_design_postmortem(&product.id, &project.id, &project.name, "second".to_owned())
            .unwrap();
        let found = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(found.id, second.id, "must return the most recently created postmortem");
    }
}
