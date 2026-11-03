//! Dispatch focus mode, DB half: persistence of the focus set, target
//! resolution, and the scope rule that decides whether a ready execution is
//! inside it. See `ExecutionCoordinator::drain_ready_queue` for where the
//! verdict is applied.
//!
//! Focus is an admission gate and nothing else. Nothing here writes to a
//! work item: scope is *derived* from the row's existing project, product and
//! lineage columns, so clearing the focus set leaves every row as it was.
//!
//! # The scope rule
//!
//! A work item is in scope when any of these holds:
//!
//! 1. its `project_id` is a focused project (a project's design seed task is
//!    an ordinary task with `project_id` set, so it counts as the project);
//! 2. its `product_id` is a focused product (any project, or none);
//! 3. its lineage roots in an in-scope item. A `revision` (including the
//!    review, CI-fix and merge-conflict revisions the engine mints) resolves
//!    through its chain root; a `followup` (including a post-merge review
//!    followup) resolves through `origin_task_short_id` to the task it
//!    followed up. Both steps repeat, so a revision of a followup of an
//!    in-scope root is in scope.
//!
//! Executions whose `work_item_id` is not a task are attributed to the task
//! that owns them: a review-guide run to its series' root task, an
//! answer-agent run to the task owning the commented document or review guide, an automation triage run
//! to its product. A row that cannot be attributed to anything is out of
//! scope: focus mode admits only what it can prove is in scope.

use anyhow::Context;
use boss_protocol::{DispatchFocus, DispatchFocusTarget, DispatchFocusTargetKind, TaskKind};

use super::products_design::resolve_doc_owner_on;
use super::*;

/// Metadata KV key holding the JSON-encoded [`DispatchFocus`].
const METADATA_KEY_DISPATCH_FOCUS: &str = "dispatch_focus";

/// Resolves scope for a batch of work items against one focus set, memoising
/// per work item so a drain pass pays for each distinct item once.
struct ScopeResolver<'a> {
    conn: &'a Connection,
    focus: &'a DispatchFocus,
    memo: HashMap<String, bool>,
}

impl<'a> ScopeResolver<'a> {
    fn new(conn: &'a Connection, focus: &'a DispatchFocus) -> Self {
        Self {
            conn,
            focus,
            memo: HashMap::new(),
        }
    }

    fn in_scope(&mut self, work_item_id: &str) -> Result<bool> {
        if let Some(verdict) = self.memo.get(work_item_id) {
            return Ok(*verdict);
        }
        let verdict = self.resolve(work_item_id, 0)?;
        self.memo.insert(work_item_id.to_owned(), verdict);
        Ok(verdict)
    }

    fn resolve(&mut self, id: &str, depth: usize) -> Result<bool> {
        // Bounded like every other lineage walk: a cycle in corrupt data
        // resolves out of scope instead of looping.
        if depth > MAX_CHAIN_DEPTH {
            return Ok(false);
        }
        if id.starts_with("task_") {
            return self.task_in_scope(id, depth);
        }
        if id.starts_with("prod_") {
            return Ok(self.focus.product_ids.contains(id));
        }
        if id.starts_with("proj_") {
            return self.project_in_scope(id);
        }
        if id.starts_with("cmt_") {
            // An answer agent runs on a comment; attribute it to the task
            // that owns the commented artifact. Answer agents only spawn for
            // design/investigation-owned `pr_doc` comments and
            // `pr_review_guide` comments.
            let comment: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT artifact_kind, artifact_id FROM work_comments WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((kind, artifact_id)) = comment else {
                return Ok(false);
            };
            let owner: Option<String> = match kind.as_str() {
                "work_item" if artifact_id != id => Some(artifact_id),
                "pr_doc" => resolve_doc_owner_on(self.conn, &kind, &artifact_id)?.map(|owner| owner.task_id),
                "pr_review_guide" => self
                    .conn
                    .query_row(
                        "SELECT root_task_id FROM pr_review_guide_source_series WHERE id = ?1",
                        [&artifact_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?,
                _ => None,
            };
            return match owner {
                Some(owner_id) => self.resolve(&owner_id, depth + 1),
                None => Ok(false),
            };
        }
        if id.starts_with("prgc_") {
            let root: Option<String> = self
                .conn
                .query_row(
                    "SELECT s.root_task_id FROM pr_review_guide_source_comparisons c
                     JOIN pr_review_guide_source_series s ON s.id = c.series_id
                     WHERE c.id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            return match root {
                Some(root_id) => self.resolve(&root_id, depth + 1),
                None => Ok(false),
            };
        }
        // An automation triage run is keyed by the automation id.
        let automation_product: Option<String> = self
            .conn
            .query_row("SELECT product_id FROM automations WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(automation_product.is_some_and(|product_id| self.focus.product_ids.contains(&product_id)))
    }

    fn project_in_scope(&self, project_id: &str) -> Result<bool> {
        if self.focus.project_ids.contains(project_id) {
            return Ok(true);
        }
        let product_id: Option<String> = self
            .conn
            .query_row("SELECT product_id FROM projects WHERE id = ?1", [project_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(product_id.is_some_and(|product_id| self.focus.product_ids.contains(&product_id)))
    }

    fn task_in_scope(&mut self, task_id: &str, depth: usize) -> Result<bool> {
        let Some(task) = query_task(self.conn, task_id)? else {
            return Ok(false);
        };
        if self.focus.product_ids.contains(&task.product_id)
            || task
                .project_id
                .as_deref()
                .is_some_and(|project_id| self.focus.project_ids.contains(project_id))
        {
            return Ok(true);
        }
        if task.kind == TaskKind::Revision {
            let root = chain_root(self.conn, task_id)?;
            if root != task_id && self.resolve(&root, depth + 1)? {
                return Ok(true);
            }
        }
        if let Some(origin_short_id) = task.origin_task_short_id {
            let origin: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM tasks WHERE product_id = ?1 AND short_id = ?2
                     ORDER BY deleted_at IS NOT NULL, created_at LIMIT 1",
                    params![task.product_id, origin_short_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(origin_id) = origin
                && origin_id != task_id
                && self.resolve(&origin_id, depth + 1)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The persisted focus set against an open connection or transaction.
pub(crate) fn load_dispatch_focus_on(conn: &Connection) -> Result<DispatchFocus> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            [METADATA_KEY_DISPATCH_FOCUS],
            |row| row.get(0),
        )
        .optional()?;
    match raw {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).with_context(|| format!("unreadable persisted dispatch focus: {raw}"))
        }
        _ => Ok(DispatchFocus::default()),
    }
}

/// Whether `work_item_id` is inside `focus` against an open connection.
/// An inactive focus contains everything.
pub(crate) fn work_item_in_dispatch_focus_on(
    conn: &Connection,
    work_item_id: &str,
    focus: &DispatchFocus,
) -> Result<bool> {
    if !focus.is_active() {
        return Ok(true);
    }
    ScopeResolver::new(conn, focus).in_scope(work_item_id)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The persisted focus set. Absent means no focus mode.
    pub fn load_dispatch_focus(&self) -> Result<DispatchFocus> {
        let conn = self.connect()?;
        load_dispatch_focus_on(&conn)
    }

    /// Persist the focus set. An inactive (empty) set is stored as such
    /// rather than deleted, so a clear is a durable write like any other.
    pub fn save_dispatch_focus(&self, focus: &DispatchFocus) -> Result<()> {
        self.set_metadata(METADATA_KEY_DISPATCH_FOCUS, &serde_json::to_string(focus)?)
    }

    /// Resolve one `bossctl dispatch focus` target: a project or product id,
    /// a project short id (`P<n>`, `#<n>`, `<n>`, `<slug>/<n>`), or a product slug.
    /// Tasks and everything else are refused: focus scopes whole projects
    /// and products, and quietly accepting a task would scope nothing.
    pub fn resolve_dispatch_focus_target(&self, raw: &str) -> Result<(DispatchFocusTargetKind, String)> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty focus target");
        }
        let conn = self.connect()?;
        let exists =
            |sql: &str, id: &str| -> Result<bool> { Ok(conn.query_row(sql, [id], |row| row.get::<_, i64>(0))? != 0) };
        if raw.starts_with("prod_") {
            if exists("SELECT EXISTS(SELECT 1 FROM products WHERE id = ?1)", raw)? {
                return Ok((DispatchFocusTargetKind::Product, raw.to_owned()));
            }
            bail!("could not resolve focus target {raw}: no such product");
        }
        if raw.starts_with("proj_") {
            if exists("SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)", raw)? {
                return Ok((DispatchFocusTargetKind::Project, raw.to_owned()));
            }
            bail!("could not resolve focus target {raw}: no such project");
        }
        let by_slug: Option<String> = conn
            .query_row("SELECT id FROM products WHERE slug = ?1", [raw], |row| row.get(0))
            .optional()?;
        if let Some(product_id) = by_slug {
            return Ok((DispatchFocusTargetKind::Product, product_id));
        }
        match resolve_friendly_work_item_id(&conn, raw)? {
            Some(id) if id.starts_with("proj_") => Ok((DispatchFocusTargetKind::Project, id)),
            Some(id) => bail!(
                "focus target {raw} is {id}, which is not a project or product — focus mode scopes \
                 whole projects and products"
            ),
            None => {
                bail!("could not resolve focus target {raw}: not a project or product id, short id, or product slug")
            }
        }
    }

    /// The focus set resolved for display, projects first, in id order.
    pub fn describe_dispatch_focus(&self, focus: &DispatchFocus) -> Result<Vec<DispatchFocusTarget>> {
        let conn = self.connect()?;
        let mut targets = Vec::new();
        for id in &focus.project_ids {
            let row: Option<(Option<i64>, String)> = conn
                .query_row("SELECT short_id, name FROM projects WHERE id = ?1", [id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?;
            let (short_id, name) = row.unwrap_or((None, String::new()));
            targets.push(DispatchFocusTarget {
                kind: DispatchFocusTargetKind::Project,
                id: id.clone(),
                short_id,
                name,
            });
        }
        for id in &focus.product_ids {
            let slug: Option<String> = conn
                .query_row("SELECT slug FROM products WHERE id = ?1", [id], |row| row.get(0))
                .optional()?;
            targets.push(DispatchFocusTarget {
                kind: DispatchFocusTargetKind::Product,
                id: id.clone(),
                short_id: None,
                name: slug.unwrap_or_default(),
            });
        }
        Ok(targets)
    }

    /// Whether `work_item_id` is inside `focus`. An inactive focus contains
    /// everything.
    pub fn work_item_in_dispatch_focus(&self, work_item_id: &str, focus: &DispatchFocus) -> Result<bool> {
        let conn = self.connect()?;
        work_item_in_dispatch_focus_on(&conn, work_item_id, focus)
    }

    /// The ids of the executions in `executions` that lie outside `focus` —
    /// the ones focus mode holds. Empty while focus is inactive.
    ///
    /// A `pr_review` execution is attributed to the task it reviews
    /// (`work_item_id`), resolved through its chain root exactly like an
    /// implementation row; a review for an out-of-scope PR is held, never
    /// dropped.
    pub fn executions_outside_dispatch_focus(
        &self,
        executions: &[WorkExecution],
        focus: &DispatchFocus,
    ) -> Result<HashSet<String>> {
        if !focus.is_active() || executions.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.connect()?;
        let mut resolver = ScopeResolver::new(&conn, focus);
        let mut outside = HashSet::new();
        for execution in executions {
            if !resolver.in_scope(&execution.work_item_id)? {
                outside.insert(execution.id.clone());
            }
        }
        Ok(outside)
    }
}
