//! `ideas` persistence: markdown drafts authored over time and later
//! graduated into a chore or project. Deliberately **not** a work
//! item — not dispatchable, no execution, no PR, no attentions, no
//! dependency edges, not on the kanban.

use super::*;

/// Column list shared by every `ideas` SELECT. Order must match [`map_idea`].
const IDEA_COLUMNS: &str = "id, short_id, product_id, name, body, status, graduated_to_id, \
     created_via, created_at, updated_at";

fn map_idea(row: &Row<'_>) -> rusqlite::Result<Idea> {
    let status_raw: String = row.get(5)?;
    let status = status_raw
        .parse::<IdeaStatus>()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into()))?;
    Ok(Idea {
        id: row.get(0)?,
        short_id: row.get(1)?,
        product_id: row.get(2)?,
        name: row.get(3)?,
        body: row.get(4)?,
        status,
        graduated_to_id: row.get::<_, Option<String>>(6)?.filter(|s| !s.is_empty()),
        created_via: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn query_idea(conn: &Connection, id: &str) -> Result<Option<Idea>> {
    let sql = format!("SELECT {IDEA_COLUMNS} FROM ideas WHERE id = ?1");
    conn.query_row(&sql, [id], map_idea).optional().map_err(Into::into)
}

/// List ideas for a product against an existing connection, newest first.
/// Shared by [`WorkDb::list_ideas`] and by
/// [`WorkDb::get_work_tree_instrumented`] (`ideas` rides the same
/// worktree fetch and `work.product.<id>` invalidation topic every other
/// product-scoped entity uses), which already holds the shared connection
/// guard and cannot re-enter `self.connect()` to call the public method.
pub(crate) fn list_ideas_in_tx(conn: &Connection, product_id: &str, status: Option<IdeaStatus>) -> Result<Vec<Idea>> {
    let mut sql = format!("SELECT {IDEA_COLUMNS} FROM ideas WHERE product_id = ?1");
    if status.is_some() {
        sql.push_str(" AND status = ?2");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(status) = status {
        stmt.query_map(params![product_id, status.as_str()], map_idea)?
    } else {
        stmt.query_map(params![product_id], map_idea)?
    };
    collect_rows(rows)
}

impl WorkDb {
    /// Create a new `draft` idea and return the row.
    pub fn create_idea(&self, input: CreateIdeaInput) -> Result<Idea> {
        if input.name.trim().is_empty() {
            bail!("idea name may not be empty");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("idea");
        let now = now_string();
        let short_id = allocate_idea_short_id(&tx, &input.product_id)?;
        let created_via = input
            .created_via
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| CREATED_VIA_UNKNOWN.to_owned());
        let body = input.body.unwrap_or_default();

        tx.execute(
            "INSERT INTO ideas
                 (id, short_id, product_id, name, body, status, graduated_to_id,
                  created_via, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'draft', NULL, ?6, ?7, ?7)",
            params![
                id,
                short_id,
                input.product_id,
                input.name.trim(),
                body,
                created_via,
                now
            ],
        )?;

        let idea = query_idea(&tx, &id)?.with_context(|| format!("missing idea after insert: {id}"))?;
        tx.commit()?;
        Ok(idea)
    }

    /// Fetch a single idea by canonical id.
    pub fn get_idea(&self, id: &str) -> Result<Option<Idea>> {
        let conn = self.connect()?;
        query_idea(&conn, id)
    }

    /// Fetch an idea by per-product `I<n>` short id.
    pub fn get_idea_by_short_id(&self, product_id: &str, short_id: i64) -> Result<Option<Idea>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;
        let sql = format!("SELECT {IDEA_COLUMNS} FROM ideas WHERE product_id = ?1 AND short_id = ?2");
        conn.query_row(&sql, params![product_id, short_id], map_idea)
            .optional()
            .map_err(Into::into)
    }

    /// List ideas for a product, newest first. `status` filters to one
    /// lifecycle state; `None` returns every idea regardless of status.
    pub fn list_ideas(&self, product_id: &str, status: Option<IdeaStatus>) -> Result<Vec<Idea>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;
        list_ideas_in_tx(&conn, product_id, status)
    }

    /// Apply a patch to an idea. Only `Some` fields are updated. Allowed
    /// regardless of `status` — editing a graduated or archived idea's
    /// name/body does not change its lifecycle state.
    pub fn update_idea(&self, id: &str, patch: IdeaPatch) -> Result<Idea> {
        let conn = self.connect()?;
        let existing = query_idea(&conn, id).require("idea", id)?;

        let name = patch
            .name
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or(existing.name);
        let body = patch.body.unwrap_or(existing.body);
        let now = now_string();

        conn.execute(
            "UPDATE ideas SET name = ?2, body = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, name, body, now],
        )?;

        query_idea(&conn, id)?.with_context(|| format!("missing idea after update: {id}"))
    }

    /// Permanently delete an idea. Unconditional — deleting a graduated
    /// idea does not touch what it graduated into. Graduation itself never
    /// deletes the idea; an explicit later human delete is a separate action.
    pub fn delete_idea(&self, id: &str) -> Result<()> {
        let conn = self.connect()?;
        let _existing = query_idea(&conn, id).require("idea", id)?;
        conn.execute("DELETE FROM ideas WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Graduate a `draft` idea into a chore or project, atomically:
    /// the target row and the idea's `status = 'graduated'` /
    /// `graduated_to_id` update commit in the same transaction, or neither
    /// does. Refuses non-`draft` ideas — graduation is a one-way transition
    /// and re-graduating (or graduating an archived idea) would silently
    /// orphan the first target or overwrite `graduated_to_id`.
    ///
    /// `effort_level` / `reasoning` apply only to `target = Chore`; passing
    /// either with `target = Project` is rejected — a project's auto-minted
    /// design task has no such knobs (only the unrelated
    /// `design_reasoning_effort_xhigh` boolean escalation flag).
    ///
    /// Graduating to a project passes `autostart = false` on the design
    /// seed task: a gesture on a draft must not silently dispatch a
    /// design worker.
    pub fn graduate_idea(
        &self,
        id: &str,
        target: IdeaGraduationKind,
        name: Option<String>,
        effort_level: Option<EffortLevel>,
        reasoning: Option<ReasoningMode>,
    ) -> Result<(Idea, Option<Task>, Option<Project>)> {
        if target == IdeaGraduationKind::Project && (effort_level.is_some() || reasoning.is_some()) {
            bail!("--effort / --reasoning only apply when graduating an idea to a chore, not a project");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let idea = query_idea(&tx, id).require("idea", id)?;
        if idea.status != IdeaStatus::Draft {
            bail!(
                "idea {id} is {} and cannot be graduated (only a draft idea can be graduated)",
                idea.status.as_str()
            );
        }

        let title = name
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| idea.name.clone());

        let (graduated_to_id, chore, project) = match target {
            IdeaGraduationKind::Chore => {
                let created_via = format!("{CREATED_VIA_IDEA_GRADUATION_PREFIX}{id}");
                let chore = insert_chore_in_tx(
                    &tx,
                    CreateChoreInput::builder()
                        .product_id(idea.product_id.clone())
                        .name(title)
                        .description(idea.body.clone())
                        .maybe_effort_level(effort_level)
                        .maybe_reasoning(reasoning)
                        .created_via(created_via)
                        .force_duplicate(true)
                        .build(),
                )?;
                (chore.id.clone(), Some(chore), None)
            }
            IdeaGraduationKind::Project => {
                let project = create_project_in_tx(
                    &tx,
                    CreateProjectInput::builder()
                        .product_id(idea.product_id.clone())
                        .name(title)
                        .autostart(false)
                        .build(),
                    &idea.body,
                )?;
                (project.id.clone(), None, Some(project))
            }
        };

        let now = now_string();
        tx.execute(
            "UPDATE ideas SET status = 'graduated', graduated_to_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, graduated_to_id, now],
        )?;
        let idea = query_idea(&tx, id)?.with_context(|| format!("missing idea after graduate: {id}"))?;
        tx.commit()?;
        Ok((idea, chore, project))
    }
}
