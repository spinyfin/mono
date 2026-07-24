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

/// Stage `Event::ProjectImplDrained { project_id }` onto `pending` if this
/// status transition was the project's *last* non-terminal trigger task
/// (`project_task`/`design`/`investigation`) reaching terminal. Must be
/// called with the transaction that performed the status-write UPDATE,
/// after that UPDATE has run but before the transaction commits — see
/// `crate::event_publish`'s publish-after-commit seam, which is what
/// actually flushes `pending` onto the bus once the caller's `tx.commit()`
/// succeeds.
///
/// Deliberately does not replicate `project_postmortem_sweep::evaluate_project`'s
/// dedup gate, archived-project skip, or "since last postmortem" cutoff —
/// this is only the trigger-count check; the event-driven subscriber
/// re-derives everything else from current DB state when it reacts to the
/// event (events are hints, not commands — see the event-bus design doc).
pub(crate) fn stage_project_impl_drained_on_terminal_transition(
    tx: &Connection,
    pending: &mut crate::event_publish::PendingEvents,
    task_kind: TaskKind,
    project_id: Option<&str>,
    previous_status: &TaskStatus,
    new_status: &TaskStatus,
) -> Result<()> {
    if previous_status.is_terminal() || !new_status.is_terminal() {
        return Ok(());
    }
    if !matches!(
        task_kind,
        TaskKind::ProjectTask | TaskKind::Design | TaskKind::Investigation
    ) {
        return Ok(());
    }
    let Some(project_id) = project_id else {
        return Ok(());
    };
    if project_has_open_trigger_tasks_in_tx(tx, project_id)? {
        return Ok(());
    }
    pending.push(boss_event_bus::Event::ProjectImplDrained {
        project_id: project_id.to_owned(),
    });
    Ok(())
}

/// Stage `Event::ProjectImplDrained { project_id }` onto `pending` when
/// soft-deleting a task removes the project's *last* open trigger task.
/// Deletion is not a status write, so it cannot go through
/// [`stage_project_impl_drained_on_terminal_transition`]'s
/// previous-status/new-status transition check — this is the deletion-path
/// sibling: it fires whenever a trigger-kind task that was itself still
/// open (`status` not terminal) is tombstoned and, after that tombstone,
/// [`project_has_open_trigger_tasks_in_tx`] finds no more open trigger
/// tasks (the `deleted_at IS NULL` filter that query and
/// `list_project_trigger_tasks` share means the just-deleted row no longer
/// counts). Must be called with the transaction that performed the
/// `deleted_at` UPDATE, after that UPDATE has run but before the
/// transaction commits — same publish-after-commit contract as the
/// terminal-transition sibling.
pub(crate) fn stage_project_impl_drained_on_trigger_task_deleted(
    tx: &Connection,
    pending: &mut crate::event_publish::PendingEvents,
    task_kind: TaskKind,
    project_id: Option<&str>,
    status: &TaskStatus,
) -> Result<()> {
    if status.is_terminal() {
        // Already excluded from the open-trigger-task count before the
        // delete — tombstoning it changes nothing about whether the
        // project's impl work just drained.
        return Ok(());
    }
    if !matches!(
        task_kind,
        TaskKind::ProjectTask | TaskKind::Design | TaskKind::Investigation
    ) {
        return Ok(());
    }
    let Some(project_id) = project_id else {
        return Ok(());
    };
    if project_has_open_trigger_tasks_in_tx(tx, project_id)? {
        return Ok(());
    }
    pending.push(boss_event_bus::Event::ProjectImplDrained {
        project_id: project_id.to_owned(),
    });
    Ok(())
}

/// True if `project_id` still has at least one live, non-terminal trigger
/// task (`project_task`/`design`/`investigation`). Mirrors the WHERE clause
/// of [`WorkDb::list_project_trigger_tasks`] but runs directly against an
/// already-open `Connection`/`Transaction` so callers holding the shared
/// connection's mutex guard (e.g. `update_task`, mid-transaction) don't
/// deadlock trying to `WorkDb::connect()` again — see that method's doc
/// comment on why a second `connect()` call from the same call stack
/// deadlocks.
fn project_has_open_trigger_tasks_in_tx(conn: &Connection, project_id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks
         WHERE project_id = ?1
           AND kind IN ('project_task', 'design', 'investigation')
           AND deleted_at IS NULL
           AND status NOT IN ('done', 'archived', 'cancelled')",
        params![project_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
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
    /// Most recently created `design_postmortem` task for `project_id`
    /// (deleted or not), or `None` if the project has never had one.
    ///
    /// Used by `project_postmortem_sweep` solely as the timestamp cutoff for
    /// "implementation work completed since the last postmortem" (a wave of
    /// zero net work must not spawn another one). For the dedup gate itself
    /// (skip scheduling while a postmortem is still open), use
    /// [`Self::last_live_design_postmortem_for_project`] instead — mixing
    /// the two concerns into one row let a newer *deleted* postmortem mask
    /// an older *live, still-open* one and defeat the gate. Deliberately
    /// includes soft-deleted rows: excluding them would let deleting an
    /// unwanted postmortem erase its "already reviewed up to here" boundary,
    /// re-arming the trigger and causing the sweep to immediately re-
    /// backfill the very history the deletion was meant to dismiss.
    pub fn last_design_postmortem_for_project(&self, project_id: &str) -> Result<Option<Task>> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM tasks
                 WHERE project_id = ?1 AND kind = 'design_postmortem'
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

    /// Most recently created *live* (non-deleted) `design_postmortem` task
    /// for `project_id`, or `None` if there isn't one.
    ///
    /// This is `project_postmortem_sweep`'s dedup gate: a live, non-terminal
    /// postmortem blocks scheduling a duplicate; a soft-deleted one never
    /// does. Sibling to [`Self::last_design_postmortem_for_project`], which
    /// answers a different question (the cutoff anchor, which must keep
    /// considering tombstones, see its doc comment) and must not be reused
    /// for this purpose: a newer deleted row from that query would mask an
    /// older, still-live, still-open one and defeat the gate.
    pub fn last_live_design_postmortem_for_project(&self, project_id: &str) -> Result<Option<Task>> {
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
    ///
    /// `effort_level` is stamped explicitly to `Medium` rather than left
    /// `NULL` — see incident postmortem-archived-fanout-2026-07-20: an
    /// unclassified engine-created row silently falls through to
    /// `resolve_spawn_config`'s untagged-row fallback with no visibility
    /// into why. That fallback is `engine_default = "opus"` at
    /// HEAD (never Fable — see `claude.rs`'s `ModelMenu` doc comment), but
    /// an explicit level makes the choice visible on the row itself
    /// (`boss task show`) and stops it drifting with whatever the fallback
    /// happens to be in a future refactor. `Medium` matches the task's
    /// actual shape — reviewing a bounded set of already-merged PRs and
    /// updating one doc, not an open-ended design or large implementation
    /// — and resolves to Sonnet, not Opus.
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
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id, effort_level)
         VALUES (?1, ?2, ?3, 'design_postmortem', ?6, ?7, 'todo', NULL, NULL, NULL, ?4, ?4, 1, 'medium', ?5, ?8, ?9)",
        params![
            id,
            product_id,
            project_id,
            now,
            CREATED_VIA_ENGINE_AUTO,
            name,
            description,
            short_id,
            EffortLevel::Medium.as_str()
        ],
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
        assert_eq!(
            task.effort_level,
            Some(EffortLevel::Medium),
            "must be classified at creation, not left null to fall through to whatever the \
             untagged-row model fallback happens to be — incident postmortem-archived-fanout-2026-07-20"
        );
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

    fn new_project(db: &WorkDb, product_id: &str) -> boss_protocol::Project {
        db.create_project(
            CreateProjectInput::builder()
                .product_id(product_id)
                .name("Alpha")
                .no_design_task(true)
                .build(),
        )
        .unwrap()
    }

    /// Negative branch: a `chore`/`revision`-kind task going terminal must
    /// never stage `ProjectImplDrained`, even with a project id and a
    /// genuine non-terminal→terminal transition — only
    /// `project_task`/`design`/`investigation` count toward the trigger.
    #[test]
    fn terminal_transition_ignores_non_trigger_task_kinds() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = new_project(&db, &product.id);
        let conn = db.connect().unwrap();

        for kind in [
            TaskKind::Chore,
            TaskKind::Revision,
            TaskKind::Followup,
            TaskKind::DesignPostmortem,
        ] {
            let mut pending = crate::event_publish::PendingEvents::new();
            stage_project_impl_drained_on_terminal_transition(
                &conn,
                &mut pending,
                kind.clone(),
                Some(project.id.as_str()),
                &TaskStatus::Todo,
                &TaskStatus::Done,
            )
            .unwrap();
            assert!(
                pending.is_empty(),
                "non-trigger kind {kind:?} must never stage an event"
            );
        }
    }

    /// Negative branch: a status write that is not a genuine
    /// non-terminal→terminal transition — either the previous status was
    /// already terminal (terminal→terminal re-write) or the new status is
    /// not terminal at all — must never stage `ProjectImplDrained`,
    /// regardless of task kind or open-trigger-task state.
    #[test]
    fn terminal_transition_ignores_non_transitions() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = new_project(&db, &product.id);
        let conn = db.connect().unwrap();

        // terminal -> terminal re-write (e.g. `done` -> `archived`).
        let mut pending = crate::event_publish::PendingEvents::new();
        stage_project_impl_drained_on_terminal_transition(
            &conn,
            &mut pending,
            TaskKind::ProjectTask,
            Some(project.id.as_str()),
            &TaskStatus::Done,
            &TaskStatus::Archived,
        )
        .unwrap();
        assert!(pending.is_empty(), "terminal -> terminal must never stage an event");

        // non-terminal -> non-terminal (no completion happened at all).
        let mut pending = crate::event_publish::PendingEvents::new();
        stage_project_impl_drained_on_terminal_transition(
            &conn,
            &mut pending,
            TaskKind::ProjectTask,
            Some(project.id.as_str()),
            &TaskStatus::Todo,
            &TaskStatus::Active,
        )
        .unwrap();
        assert!(
            pending.is_empty(),
            "non-terminal -> non-terminal must never stage an event"
        );
    }
}
