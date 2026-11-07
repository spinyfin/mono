//! Parking: the owner's explicit "not now" for a task, chore or project.
//!
//! A parked row keeps its lifecycle `status`, leaves the default board, and
//! is skipped by every automatic dispatch and upkeep path through one shared
//! predicate ([`is_parked`] / [`upkeep_eligible`]). Parking is a set of four
//! nullable columns (`parked_at`, `parked_by`, `parked_reason`, `parked_via`)
//! on `tasks` and `projects`, orthogonal to `status`, `deferred`,
//! `human_driven` and `autostart`; `parked_at IS NOT NULL` is the single
//! load-bearing fact.
//!
//! ## What "parked" means
//!
//! A task is parked when any of these hold, and it is still open (`done` and
//! `archived` rows are finished work, never parked):
//!
//! - it carries its own stamp (`parked_at`), set directly (`parked_via =
//!   'direct'`) or by its project's park cascading onto it (`parked_via =
//!   'project:<project_id>'`);
//! - its project is parked — the stamp is normally already there, because a
//!   task created in or moved into a parked project is stamped by a trigger,
//!   but the predicate does not depend on that;
//! - it is a revision and its chain root (or that root's project) is parked.
//!   A revision follows its parent PR, so it is never parked on its own; it
//!   is born parked with a copy of the chain root's stamp (`parked_via =
//!   'direct'` or `'project:<id>'`), and parent un-park clears those
//!   inherited stamps. The SQL predicate still treats a revision of a
//!   parked root as parked, as a backstop.
//!
//! ## What park and un-park do
//!
//! - **Park a task**: stamp it, stamp every open revision whose chain root
//!   is this task, and cancel never-started executions (`ready | queued |
//!   waiting_dependency`, reason `parked`) on the task and those revisions.
//!   A live execution (`claimed | running | waiting_human`) is never killed:
//!   parking is an attention decision, not a cancellation, and the in-flight
//!   run finishes normally.
//! - **Park a project**: stamp the project, then every open, non-deleted task
//!   in it (hidden revision rows and `deferred` rows included) with
//!   `project:<id>`, in one transaction. A task the owner parked directly
//!   keeps its own stamp.
//! - **Un-park a task**: clear the stamp and the inherited stamps on its
//!   open revisions. A task whose project is still parked cannot be
//!   un-parked on its own — un-park the project — which would otherwise
//!   leave a row that looks un-parked but is still skipped.
//! - **Un-park a project**: clear the project and only the rows whose stamp
//!   names it. A task parked directly before or during the project's park
//!   stays parked.
//!
//! Un-park only clears the columns and writes the ledger row; it does not
//! refresh the PR, route conflicts or red CI, or re-mint executions.
//! Section 1.5 of
//! `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! specifies that sequence.
//!
//! ## Atrophy: who consults the predicate
//!
//! A parked row (or one whose project or revision chain root is parked) is
//! never nominated by an upkeep producer. Wherever a producer has its own SQL
//! candidate listing, the exclusion is in that SQL
//! ([`upkeep_eligible_task_sql`]), so the row is never even considered:
//!
//! - orphan re-dispatch (`list_orphan_active_candidates`) and dispatch-failure
//!   recovery (`list_dispatch_failed_recovery_candidates`);
//! - AI review scheduling and recovery: dead-review recovery
//!   (`list_dead_pr_review_candidates`, `list_dead_review_batch_member_candidates`),
//!   the reviewer fallback (`list_tasks_with_stalled_reviewer`), deferred review
//!   admission (`list_tasks_awaiting_pre_merge_review_admission`) and PR-binding
//!   repair (`list_pr_binding_repair_candidates`);
//! - the abandoned-branch sweep (`list_abandoned_pushed_branch_candidates`);
//! - CI and conflict remediation rescue (`list_stranded_ci_remediation_attempts`,
//!   `list_chores_stranded_blocked_remediation`);
//! - the merge poller's speculative conflict prediction and stacked-PR offers,
//!   which act only on an open PR and so list through
//!   `list_chores_pending_upkeep_check`;
//! - the dependency cascade's follow-ups: the stuck-execution promotion
//!   (`promote_todo_autostart_stuck_executions`) and the waiting-writer wake-up
//!   (`promote_waiting_writers`).
//!
//! `tests/upkeep_producer_tests.rs` registers each of those listings, and the
//! listings that deliberately stay shared, and runs one parameterised test over
//! the registry: a producer added later must be registered there, and one whose
//! listing forgets the predicate fails the test instead of upkeeping parked work.
//!
//! Where a candidate is *shared with observing facts about the world*, the
//! listing cannot drop the row, so the consumer gates the reaction instead:
//!
//! - the merge poller's own lists (in-review PRs, PRs blocked on a conflict or
//!   on CI) still contain a parked row, because a parked PR can still merge or
//!   close. `sweep_one` records those two outcomes (merged → `done`, closed →
//!   the closed-unmerged path) and does nothing about an open PR: no conflict
//!   or CI remediation, moot-revision retirement, review-guide refresh,
//!   poll-state or merge-queue upkeep, Trunk adoption or eviction handling, and
//!   no post-merge deep review (`boss_engine_merge_poller::candidate_is_parked`).
//!   The merge-queue rebounce pass skips parked rows, and the CI and conflict
//!   watchers' shared opt-out gate refuses a parked row for any other caller;
//!   a conflict remediation that was queued before the park aborts when its
//!   turn comes;
//! - the dependency cascade (`list_dependency_blocked_candidates`) still flips a
//!   parked `blocked: dependency` row to `todo`, so its lifecycle position stays
//!   truthful, but the park check in `reconcile_work_item_execution` mints
//!   nothing, exactly as `deferred` behaves;
//! - the completion path that would enqueue the AI reviewer for a freshly
//!   detected PR (`finalize_pr_transition`, and the declared-delivery
//!   equivalent) advances the row to `in_review` without one;
//! - the project postmortem sweep skips a parked project;
//! - an explicit operator request (`RequestExecution`, `bossctl review start`)
//!   is refused with the typed "un-park it first" error
//!   ([`WorkDb::refuse_if_parked`]).
//!
//! Two paths are state repair, not producers, and are deliberately not gated:
//! `reconcile_orphaned_conflict_ladder_attempts` (startup only; it abandons an
//! orphaned attempt and re-arms detection, which is itself gated) and the
//! housekeeping that releases lease and pool reservations.
//!
//! ## Who may park
//!
//! Only the `human` and `boss` caller tiers. Every other actor (a worker, a
//! progression session, an engine sweep, Boothby) is refused at the write
//! boundary with [`ParkingRefusal::CallerTier`], the way a raise to
//! `critical` is: parking is the owner's decision.
//!
//! ## Ledger
//!
//! Every change to `parked_at` is a row in `task_status_audit` (property
//! `parked_at`). A trigger on `tasks.parked_at` writes a parking-owned sink
//! that does not name the ledger tables, so `ALTER TABLE` rewrite still
//! works after an upgrade test has dropped those tables; the write helper
//! copies the sink into the ledger inside the same savepoint. Writes that
//! fire the membership triggers without going through that helper leave
//! rows in the sink until the next helper call (or an explicit flush at
//! the write site). Table-existence is probed by touching the relation and
//! treating "no such table" as empty: upgrade tests drop the ledger (and
//! sometimes the sink) in-process before `ALTER TABLE`, so a cached
//! sqlite_master answer would go stale. The `basis`
//! names the producer: [`BASIS_PARKED`], [`BASIS_UNPARKED`],
//! [`BASIS_PROJECT_PARK_CASCADE`], [`BASIS_PROJECT_UNPARK_CASCADE`]; a task
//! born parked or moved into a parked project carries the basis of the write
//! that created or moved it.

use super::*;
use crate::executions_runs::CancelExecutionOpts;
use boss_protocol::{LAST_STATUS_ACTOR_BOSS, LAST_STATUS_ACTOR_HUMAN, PARKED_VIA_DIRECT, parked_via_project};

pub(super) const MIGRATION_ID: &str = "20261104_0145_task_parking";

/// Ledger basis: the owner parked a row directly.
pub const BASIS_PARKED: &str = "parked";
/// Ledger basis: the owner un-parked a row directly.
pub const BASIS_UNPARKED: &str = "unparked";
/// Ledger basis: a project's park stamped one of its tasks.
pub const BASIS_PROJECT_PARK_CASCADE: &str = "project_park_cascade";
/// Ledger basis: a project's un-park cleared one of its tasks.
pub const BASIS_PROJECT_UNPARK_CASCADE: &str = "project_unpark_cascade";

/// The reason recorded on a never-started execution cancelled by a park.
pub const CANCEL_REASON_PARKED: &str = "parked";

const PARKED_COLUMNS: [&str; 4] = ["parked_at", "parked_by", "parked_reason", "parked_via"];

/// Rows that are parked because of a project are stamped by these triggers, so
/// every insert path — the planner's auto-populate, a follow-up mint, `boss
/// task create --project`, a revision — is covered without each one knowing
/// about parking. The park time is the project's own: a child is "parked
/// since" the owner decided, not since the child was created.
const PARKING_MEMBERSHIP_TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS tasks_born_parked_in_parked_project
AFTER INSERT ON tasks
WHEN NEW.parked_at IS NULL AND NEW.project_id IS NOT NULL
 AND EXISTS (SELECT 1 FROM projects WHERE id = NEW.project_id AND parked_at IS NOT NULL)
BEGIN
    UPDATE tasks SET
        parked_at = (SELECT parked_at FROM projects WHERE id = NEW.project_id),
        parked_by = (SELECT parked_by FROM projects WHERE id = NEW.project_id),
        parked_reason = (SELECT parked_reason FROM projects WHERE id = NEW.project_id),
        parked_via = 'project:' || NEW.project_id
    WHERE id = NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS tasks_park_follows_project_membership
AFTER UPDATE OF project_id ON tasks
WHEN NEW.project_id IS NOT OLD.project_id
BEGIN
    UPDATE tasks SET parked_at = NULL, parked_by = NULL, parked_reason = NULL, parked_via = NULL
    WHERE id = NEW.id AND OLD.project_id IS NOT NULL AND parked_via = 'project:' || OLD.project_id;
    UPDATE tasks SET
        parked_at = (SELECT parked_at FROM projects WHERE id = NEW.project_id),
        parked_by = (SELECT parked_by FROM projects WHERE id = NEW.project_id),
        parked_reason = (SELECT parked_reason FROM projects WHERE id = NEW.project_id),
        parked_via = 'project:' || NEW.project_id
    WHERE id = NEW.id AND parked_at IS NULL AND NEW.project_id IS NOT NULL
      AND EXISTS (SELECT 1 FROM projects WHERE id = NEW.project_id AND parked_at IS NOT NULL);
END;
CREATE TRIGGER IF NOT EXISTS tasks_born_parked_with_parked_parent
AFTER INSERT ON tasks
WHEN NEW.kind = 'revision' AND NEW.parked_at IS NULL AND NEW.parent_task_id IS NOT NULL
 AND EXISTS (
    SELECT 1 FROM tasks parent
    WHERE parent.id = NEW.parent_task_id
      AND parent.parked_at IS NOT NULL
      AND parent.status NOT IN ('done', 'archived')
 )
BEGIN
    UPDATE tasks SET
        parked_at = (SELECT parked_at FROM tasks WHERE id = NEW.parent_task_id),
        parked_by = (SELECT parked_by FROM tasks WHERE id = NEW.parent_task_id),
        parked_reason = (SELECT parked_reason FROM tasks WHERE id = NEW.parent_task_id),
        parked_via = (SELECT parked_via FROM tasks WHERE id = NEW.parent_task_id)
    WHERE id = NEW.id;
END;";

/// Trigger SQL names only parking-owned tables, never `task_status_audit*`.
/// SQLite re-parses every trigger on `ALTER TABLE`; a body that named the
/// ledger would fail upgrade tests that drop those tables first.
const PARKING_AUDIT_TRIGGERS: &str = "
CREATE TABLE IF NOT EXISTS parking_audit_context (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1) DEFAULT 1,
    basis TEXT NOT NULL,
    actor TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS parking_audit_sink (
    work_item_id TEXT NOT NULL,
    old_value TEXT,
    new_value TEXT,
    actor TEXT NOT NULL,
    changed_at TEXT NOT NULL,
    basis TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS parking_audit_parked_at
AFTER UPDATE OF parked_at ON tasks
WHEN OLD.parked_at IS NOT NEW.parked_at
BEGIN
    INSERT INTO parking_audit_sink (work_item_id, old_value, new_value, actor, changed_at, basis)
    VALUES (
        NEW.id, OLD.parked_at, NEW.parked_at,
        COALESCE((SELECT actor FROM parking_audit_context), NEW.last_status_actor),
        strftime('%s','now'),
        COALESCE((SELECT basis FROM parking_audit_context), 'unattributed_write')
    );
END;";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for table in ["tasks", "projects"] {
        for column in PARKED_COLUMNS {
            if !crate::schema_init::table_has_column(conn, table, column)? {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"))?;
            }
        }
    }
    conn.execute_batch(PARKING_AUDIT_TRIGGERS)?;
    conn.execute_batch(PARKING_MEMBERSHIP_TRIGGERS)?;
    Ok(())
}

/// Replace membership triggers on existing databases as well as fresh installs.
pub(super) fn migrate_membership(conn: &Connection) -> Result<()> {
    conn.execute_batch("DROP TRIGGER IF EXISTS tasks_born_parked_in_parked_project;")?;
    conn.execute_batch(&PARKING_MEMBERSHIP_TRIGGERS.replacen(
        "WHEN NEW.parked_at IS NULL AND NEW.project_id IS NOT NULL",
        "WHEN NEW.parked_at IS NULL AND NEW.project_id IS NOT NULL
 AND NOT (NEW.kind = 'revision' AND EXISTS (
    SELECT 1 FROM tasks p WHERE p.id = NEW.parent_task_id
      AND p.parked_at IS NOT NULL AND p.status NOT IN ('done', 'archived')
 ))",
        1,
    ))?;
    conn.execute_batch(
        "
CREATE TRIGGER IF NOT EXISTS tasks_reopened_in_parked_project
AFTER UPDATE OF status, deleted_at ON tasks
WHEN NEW.status NOT IN ('done', 'archived') AND NEW.deleted_at IS NULL
 AND (OLD.status IN ('done', 'archived') OR OLD.deleted_at IS NOT NULL)
 AND NEW.parked_at IS NULL
 AND EXISTS (SELECT 1 FROM projects WHERE id = NEW.project_id AND parked_at IS NOT NULL)
BEGIN
    UPDATE tasks SET
        parked_at = (SELECT parked_at FROM projects WHERE id = NEW.project_id),
        parked_by = (SELECT parked_by FROM projects WHERE id = NEW.project_id),
        parked_reason = (SELECT parked_reason FROM projects WHERE id = NEW.project_id),
        parked_via = 'project:' || NEW.project_id
    WHERE id = NEW.id;
END;",
    )?;
    Ok(())
}

/// True when `err` is SQLite's "no such table: {table}". Upgrade tests drop
/// parking and ledger tables in-process before `ALTER TABLE` rewrite, so
/// callers probe by using the relation rather than caching `sqlite_master`.
pub(super) fn sqlite_missing_table(err: &rusqlite::Error, table: &str) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(_, Some(msg)) => msg.contains("no such table") && msg.contains(table),
        _ => false,
    }
}

/// Copy parked_at sink rows into the ledger. No-op when the sink is empty
/// or the ledger table is gone (upgrade tests drop the ledger, then
/// `ALTER TABLE` other columns). Membership triggers that fire from a
/// plain `conn.execute` (revision `project_id` cascade, a raw INSERT)
/// leave rows in the sink until this runs; call it at those write sites
/// so list/show and the stuck report see the ledger in the same
/// transaction rather than after an unrelated later write.
pub(super) fn flush_parked_at_ledger(conn: &Connection) -> rusqlite::Result<()> {
    let pending: i64 = match conn.query_row("SELECT EXISTS (SELECT 1 FROM parking_audit_sink LIMIT 1)", [], |row| {
        row.get(0)
    }) {
        Ok(value) => value,
        Err(err) if sqlite_missing_table(&err, "parking_audit_sink") => return Ok(()),
        Err(err) => return Err(err),
    };
    if pending == 0 {
        return Ok(());
    }
    match conn.execute_batch(
        "INSERT INTO task_status_audit (work_item_id, property, old_value, new_value, actor, changed_at, basis)
         SELECT work_item_id, 'parked_at', old_value, new_value, actor, changed_at, basis FROM parking_audit_sink;
         DELETE FROM parking_audit_sink;",
    ) {
        Ok(()) => Ok(()),
        Err(err) if sqlite_missing_table(&err, "task_status_audit") => Ok(()),
        Err(err) => Err(err),
    }
}

/// A refused parking operation. Typed so callers can tell the owner what to do
/// next instead of parsing prose.
#[derive(Debug, thiserror::Error)]
pub enum ParkingRefusal {
    /// Explicit dispatch of a parked row. The row stays skipped until it is
    /// un-parked, so starting it would only mint an execution nothing will run.
    #[error(
        "cannot start {work_item_id}: it is parked{detail} — un-park it first with `{unpark_command}`; \
         an explicit start does not override a park"
    )]
    UnparkFirst {
        work_item_id: String,
        /// The row whose park keeps `work_item_id` parked: itself, its
        /// project, or its revision chain root.
        holder_id: String,
        unpark_command: String,
        /// Pre-rendered ` (project …, "reason")` suffix.
        detail: String,
    },
    /// A caller tier that may not park or un-park.
    #[error(
        "parking is the owner's decision: refused for caller tier `{caller_tier}`. \
         Only a human (or the coordinator, on the owner's instruction) may park or un-park work"
    )]
    CallerTier { caller_tier: String },
}

/// The refusal every non-owner caller tier gets for a park or un-park.
pub fn parking_refusal(caller_tier: &str) -> anyhow::Error {
    ParkingRefusal::CallerTier {
        caller_tier: caller_tier.to_owned(),
    }
    .into()
}

fn authorize_actor(actor: &str) -> Result<()> {
    if actor == LAST_STATUS_ACTOR_HUMAN || actor == LAST_STATUS_ACTOR_BOSS {
        return Ok(());
    }
    Err(parking_refusal(actor))
}

/// The row whose park keeps an item parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkHolder {
    /// The item itself, its project, or its revision chain root.
    pub holder_id: String,
    pub holder_is_project: bool,
    /// When the owner parked the holder (epoch seconds), if recorded.
    pub parked_at: Option<i64>,
    pub reason: Option<String>,
}

impl ParkHolder {
    fn unpark_command(&self) -> String {
        let noun = if self.holder_is_project { "project" } else { "task" };
        format!("boss {noun} unpark {}", self.holder_id)
    }

    fn unpark_first(&self, work_item_id: &str) -> ParkingRefusal {
        let scope = if self.holder_id == work_item_id {
            String::new()
        } else if self.holder_is_project {
            format!(" because project {} is parked", self.holder_id)
        } else {
            format!(" because its parent {} is parked", self.holder_id)
        };
        let reason = self
            .reason
            .as_deref()
            .map(|reason| format!(" (\"{reason}\")"))
            .unwrap_or_default();
        ParkingRefusal::UnparkFirst {
            work_item_id: work_item_id.to_owned(),
            holder_id: self.holder_id.clone(),
            unpark_command: self.unpark_command(),
            detail: format!("{scope}{reason}"),
        }
    }
}

/// SQL predicate, over `tasks` aliased `alias`, that is true when the task is
/// open and parked: stamped itself, in a parked project, or a revision whose
/// chain root (or that root's project) is parked. This is the one definition
/// of "parked" every upkeep producer's candidate listing should embed;
/// [`is_parked`] runs exactly this text, so a producer and the dispatch gate
/// can never disagree.
pub fn parked_task_sql(alias: &str) -> String {
    let a = alias;
    format!(
        "({a}.status NOT IN ('done', 'archived') AND (
            {a}.parked_at IS NOT NULL
            OR EXISTS (SELECT 1 FROM projects pp WHERE pp.id = {a}.project_id
                       AND pp.parked_at IS NOT NULL AND pp.status NOT IN ('done', 'archived'))
            OR ({a}.kind = 'revision' AND EXISTS (
                SELECT 1 FROM tasks root WHERE root.id = {a}.parent_task_id AND (
                    root.parked_at IS NOT NULL
                    OR EXISTS (SELECT 1 FROM projects rp WHERE rp.id = root.project_id
                               AND rp.parked_at IS NOT NULL AND rp.status NOT IN ('done', 'archived')))))))"
    )
}

/// The negation of [`parked_task_sql`]: SQL, over `tasks` aliased `alias`, that
/// is true when upkeep may run for the task. Every upkeep producer's SQL
/// candidate listing ANDs this in, so a parked row is never even nominated —
/// the exclusion happens where candidates are chosen, not where an action is
/// finally attempted. The producers that do this are inventoried in the module
/// documentation; the SQL listings are also registered in
/// `tests/upkeep_producer_tests.rs`, which a parameterised test iterates.
pub fn upkeep_eligible_task_sql(alias: &str) -> String {
    format!("NOT {}", parked_task_sql(alias))
}

/// `true` when `work_item_id` is parked (see [`parked_task_sql`]). A project is
/// parked when it carries its own stamp and is still open. Products and unknown
/// ids are never parked.
pub fn is_parked(conn: &Connection, work_item_id: &str) -> Result<bool> {
    if work_item_id.starts_with("task_") {
        let sql = format!("SELECT {} FROM tasks t WHERE t.id = ?1", parked_task_sql("t"));
        return Ok(conn
            .query_row(&sql, [work_item_id], |row| row.get::<_, bool>(0))
            .optional()?
            .unwrap_or(false));
    }
    if work_item_id.starts_with("proj_") {
        return Ok(open_parked_project(conn, work_item_id)?.is_some());
    }
    Ok(false)
}

/// The inverse of [`is_parked`], named for the consumers that gate on it: an
/// upkeep producer (CI fix, conflict resolution, review scheduling, re-dispatch)
/// must act only on an item for which this is `true`.
pub fn upkeep_eligible(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(!is_parked(conn, work_item_id)?)
}

/// Who keeps `work_item_id` parked, or `None` when it is not parked.
pub fn parked_holder(conn: &Connection, work_item_id: &str) -> Result<Option<ParkHolder>> {
    if work_item_id.starts_with("proj_") {
        return open_parked_project(conn, work_item_id);
    }
    if !work_item_id.starts_with("task_") {
        return Ok(None);
    }
    let Some(row) = task_park_row(conn, work_item_id)? else {
        return Ok(None);
    };
    if row.terminal {
        return Ok(None);
    }
    if row.parked_at().is_some() {
        // A stamp a project's park cascaded onto the row is held by that
        // project: it is the project that has to be un-parked.
        if let Some(project_id) = row.parked_via().and_then(|via| via.strip_prefix("project:"))
            && let Some(holder) = open_parked_project(conn, project_id)?
        {
            return Ok(Some(holder));
        }
        // A revision inherits its parent's stamp and cannot be un-parked on
        // its own: the chain root is the holder even when the revision row
        // carries a copy of the stamp.
        if row.is_revision
            && let Some(root_id) = row.parent_task_id.as_deref()
            && let Some(root) = task_park_row(conn, root_id)?
        {
            return Ok(Some(root.into_holder(root_id)));
        }
        return Ok(Some(row.into_holder(work_item_id)));
    }
    if let Some(project_id) = row.project_id.as_deref()
        && let Some(holder) = open_parked_project(conn, project_id)?
    {
        return Ok(Some(holder));
    }
    if row.is_revision
        && let Some(root_id) = row.parent_task_id.as_deref()
        && let Some(root) = task_park_row(conn, root_id)?
    {
        if root.parked_at().is_some() {
            return Ok(Some(root.into_holder(root_id)));
        }
        if let Some(project_id) = root.project_id.as_deref() {
            return open_parked_project(conn, project_id);
        }
    }
    Ok(None)
}

struct TaskParkRow {
    terminal: bool,
    is_revision: bool,
    project_id: Option<String>,
    parent_task_id: Option<String>,
    /// `(parked_at, parked_reason, parked_via)` when the row is stamped.
    stamp: Option<(String, Option<String>, Option<String>)>,
}

impl TaskParkRow {
    fn parked_at(&self) -> Option<&str> {
        self.stamp.as_ref().map(|(at, _, _)| at.as_str())
    }

    fn parked_via(&self) -> Option<&str> {
        self.stamp.as_ref().and_then(|(_, _, via)| via.as_deref())
    }

    fn into_holder(self, id: &str) -> ParkHolder {
        let (parked_at, parked_reason) = match self.stamp {
            Some((at, reason, _)) => (Some(at), reason),
            None => (None, None),
        };
        ParkHolder {
            holder_id: id.to_owned(),
            holder_is_project: false,
            parked_at: parked_at.as_deref().and_then(|at| at.parse().ok()),
            reason: parked_reason.filter(|reason| !reason.is_empty()),
        }
    }
}

fn task_park_row(conn: &Connection, id: &str) -> Result<Option<TaskParkRow>> {
    Ok(conn
        .query_row(
            "SELECT status IN ('done', 'archived'), kind = 'revision', project_id, parent_task_id, parked_at, parked_reason,
                    parked_via
             FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            [id],
            |row| {
                let parked_at: Option<String> = row.get(4)?;
                let parked_reason: Option<String> = row.get(5)?;
                let parked_via: Option<String> = row.get(6)?;
                Ok(TaskParkRow {
                    terminal: row.get(0)?,
                    is_revision: row.get(1)?,
                    project_id: row.get(2)?,
                    parent_task_id: row.get(3)?,
                    stamp: parked_at.map(|at| (at, parked_reason, parked_via)),
                })
            },
        )
        .optional()?)
}

fn open_parked_project(conn: &Connection, project_id: &str) -> Result<Option<ParkHolder>> {
    Ok(conn
        .query_row(
            "SELECT parked_at, parked_reason FROM projects
             WHERE id = ?1 AND parked_at IS NOT NULL AND status NOT IN ('done', 'archived')",
            [project_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .map(|(parked_at, reason)| ParkHolder {
            holder_id: project_id.to_owned(),
            holder_is_project: true,
            parked_at: parked_at.parse().ok(),
            reason: reason.filter(|reason| !reason.is_empty()),
        }))
}

/// The refusal an explicit start of a parked item gets, or `None` when it is
/// not parked. Shared by the mutating request path and the read-only
/// admission evaluator so the two cannot disagree.
pub(crate) fn unpark_first_refusal(conn: &Connection, work_item_id: &str) -> Result<Option<ParkingRefusal>> {
    Ok(parked_holder(conn, work_item_id)?.map(|holder| holder.unpark_first(work_item_id)))
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Park (`parked = true`) or un-park a task, chore or project on behalf of
    /// `actor`, which must be the `human` or `boss` tier. Idempotent. See the
    /// module docs for what each operation does.
    pub fn park_work_item_as_actor(
        &self,
        id: &str,
        parked: bool,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<WorkItem> {
        authorize_actor(actor)?;
        let reason = reason.map(str::trim).filter(|reason| !reason.is_empty());
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        let now = now_string();
        let item = match classify_id(id)? {
            ItemKind::Task => {
                set_task_parked(&tx, &mut pending, id, parked, reason, actor, &now)?;
                let mut task = query_task(&tx, id).require("task", id)?;
                project_for_work_item_updated(&tx, &mut task, "park_work_item");
                task_to_item(task)
            }
            ItemKind::Project => {
                set_project_parked(&tx, &mut pending, id, parked, reason, actor, &now)?;
                WorkItem::Project(query_project(&tx, id).require("project", id)?)
            }
            ItemKind::Product => bail!("cannot park product {id}: parking applies to tasks, chores and projects"),
            ItemKind::Comment | ItemKind::Comparison => bail!("{id} is not a task, chore or project"),
        };
        commit_and_publish(tx, pending, self.event_bus())?;
        Ok(item)
    }

    /// `UpdateWorkItem` with `parked` set. A parking patch carries nothing else:
    /// park and un-park are their own request, so they cannot ride along with —
    /// or half-apply beside — an unrelated edit.
    pub(crate) fn apply_parking_patch(&self, id: &str, patch: &WorkItemPatch, actor: &str) -> Result<WorkItem> {
        anyhow::ensure!(
            patch.is_parking_only(),
            "`parked` cannot be combined with other fields in one update: send park and un-park as their own request"
        );
        let Some(parked) = patch.parked else {
            bail!("`parked_reason` needs `parked`: pass `parked = true` to park with a reason");
        };
        self.park_work_item_as_actor(id, parked, patch.parked_reason.as_deref(), actor)
    }

    /// See [`is_parked`].
    pub fn is_parked(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        is_parked(&conn, work_item_id)
    }

    /// See [`upkeep_eligible`].
    pub fn upkeep_eligible(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        upkeep_eligible(&conn, work_item_id)
    }

    /// Refuse, with the typed [`ParkingRefusal::UnparkFirst`], an explicit
    /// request to start work on a parked item (an operator-triggered review, for
    /// one). Returns `Ok(())` when the item is not parked. An explicit request
    /// does not override a park for the same reason `RequestExecution` does not:
    /// every automatic path would keep skipping the row, so whatever it started
    /// would only strand.
    pub fn refuse_if_parked(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        match unpark_first_refusal(&conn, work_item_id)? {
            Some(refusal) => Err(refusal.into()),
            None => Ok(()),
        }
    }
}

fn set_task_parked(
    tx: &Connection,
    pending: &mut PendingEvents,
    id: &str,
    parked: bool,
    reason: Option<&str>,
    actor: &str,
    now: &str,
) -> Result<()> {
    let task = query_task(tx, id).require("task", id)?;
    let verb = if parked { "park" } else { "un-park" };
    if task.deleted_at.is_some() {
        bail!("cannot {verb} {id}: it is deleted");
    }
    if task.kind == TaskKind::Revision {
        let parent = task.parent_task_id.as_deref().unwrap_or("its parent");
        bail!("cannot {verb} {id}: it is a revision, and a revision follows its parent PR — {verb} {parent} instead");
    }
    if parked {
        if task.status.is_terminal() {
            bail!(
                "cannot park {id}: it is `{}`, and finished work is never upkept — there is nothing to park",
                task.status
            );
        }
        if task.parked_at.is_some() {
            // Already parked. Parking a row that a project's park cascaded onto
            // makes it the owner's own decision, so it survives the project's
            // un-park; a supplied reason replaces the recorded one.
            if task.parked_via.as_deref() != Some(PARKED_VIA_DIRECT) || reason.is_some() {
                tx.execute(
                    "UPDATE tasks SET parked_via = ?2, parked_by = ?3,
                         parked_reason = COALESCE(?4, parked_reason), updated_at = ?5
                     WHERE id = ?1",
                    params![id, PARKED_VIA_DIRECT, actor, reason, now],
                )?;
            }
            inherit_park_stamp_onto_open_revisions(tx, pending, id, actor, now, BASIS_PARKED)?;
            return Ok(());
        }
        stamp_task(tx, id, actor, reason, PARKED_VIA_DIRECT, now, BASIS_PARKED)?;
        inherit_park_stamp_onto_open_revisions(tx, pending, id, actor, now, BASIS_PARKED)?;
        return Ok(());
    }
    if task.parked_at.is_none() {
        return Ok(());
    }
    if let Some(project_id) = task.project_id.as_deref()
        && open_parked_project(tx, project_id)?.is_some()
    {
        bail!(
            "cannot un-park {id}: it is parked because project {project_id} is parked — \
             run `boss project unpark {project_id}`. Rows the owner parked directly stay parked when the project is un-parked"
        );
    }
    crate::task_status_audit::execute(
        tx,
        BASIS_UNPARKED,
        actor,
        "UPDATE tasks SET parked_at = NULL, parked_by = NULL, parked_reason = NULL, parked_via = NULL, updated_at = ?2
         WHERE id = ?1",
        params![id, now],
    )?;
    crate::task_status_audit::execute(
        tx,
        BASIS_UNPARKED,
        actor,
        "UPDATE tasks SET parked_at = NULL, parked_by = NULL, parked_reason = NULL, parked_via = NULL, updated_at = ?2
         WHERE parent_task_id = ?1 AND kind = 'revision' AND parked_via = ?3
           AND deleted_at IS NULL AND status NOT IN ('done', 'archived')",
        params![id, now, PARKED_VIA_DIRECT],
    )?;
    Ok(())
}

fn stamp_task(
    tx: &Connection,
    id: &str,
    actor: &str,
    reason: Option<&str>,
    via: &str,
    now: &str,
    basis: &str,
) -> Result<()> {
    crate::task_status_audit::execute(
        tx,
        basis,
        actor,
        "UPDATE tasks SET parked_at = ?2, parked_by = ?3, parked_reason = ?4, parked_via = ?5, updated_at = ?2
         WHERE id = ?1",
        params![id, now, actor, reason, via],
    )?;
    Ok(())
}

/// Copy the chain root's park stamp onto its open revisions and cancel
/// their never-started executions. A revision follows its parent PR, so
/// parking the parent parks the chain; claimed/running/waiting_human rows
/// stay alive.
fn inherit_park_stamp_onto_open_revisions(
    tx: &Connection,
    pending: &mut PendingEvents,
    parent_id: &str,
    actor: &str,
    now: &str,
    basis: &str,
) -> Result<()> {
    crate::task_status_audit::execute(
        tx,
        basis,
        actor,
        "UPDATE tasks SET
            parked_at = (SELECT parked_at FROM tasks WHERE id = ?1),
            parked_by = (SELECT parked_by FROM tasks WHERE id = ?1),
            parked_reason = (SELECT parked_reason FROM tasks WHERE id = ?1),
            parked_via = (SELECT parked_via FROM tasks WHERE id = ?1),
            updated_at = ?2
         WHERE parent_task_id = ?1 AND kind = 'revision'
           AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
           AND (
                parked_at IS NULL
                OR parked_via IS NOT (SELECT parked_via FROM tasks WHERE id = ?1)
                OR parked_at IS NOT (SELECT parked_at FROM tasks WHERE id = ?1)
                OR parked_by IS NOT (SELECT parked_by FROM tasks WHERE id = ?1)
                OR parked_reason IS NOT (SELECT parked_reason FROM tasks WHERE id = ?1)
           )",
        params![parent_id, now],
    )?;
    cancel_pending_executions(tx, pending, parent_id)?;
    for revision_id in open_revision_ids(tx, parent_id)? {
        cancel_pending_executions(tx, pending, &revision_id)?;
    }
    Ok(())
}

fn open_revision_ids(tx: &Connection, parent_id: &str) -> Result<Vec<String>> {
    Ok(tx
        .prepare(
            "SELECT id FROM tasks
             WHERE parent_task_id = ?1 AND kind = 'revision'
               AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
             ORDER BY created_at, id",
        )?
        .query_map([parent_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn set_project_parked(
    tx: &Connection,
    pending: &mut PendingEvents,
    id: &str,
    parked: bool,
    reason: Option<&str>,
    actor: &str,
    now: &str,
) -> Result<()> {
    let project = query_project(tx, id).require("project", id)?;
    let via = parked_via_project(id);
    if parked {
        if matches!(project.status, ProjectStatus::Done | ProjectStatus::Archived) {
            bail!(
                "cannot park project {id}: it is `{}`, and finished work is never upkept — there is nothing to park",
                project.status
            );
        }
        if project.parked_at.is_some() {
            if let Some(reason) = reason {
                tx.execute(
                    "UPDATE projects SET parked_reason = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, reason, now],
                )?;
            }
            return Ok(());
        }
        tx.execute(
            "UPDATE projects SET parked_at = ?2, parked_by = ?3, parked_reason = ?4, parked_via = ?5, updated_at = ?2
             WHERE id = ?1",
            params![id, now, actor, reason, PARKED_VIA_DIRECT],
        )?;
        // Collect before the bulk stamp: the cascade skips rows already parked
        // directly, and only the rows it stamps need their executions cancelled.
        let children: Vec<String> = tx
            .prepare(
                "SELECT id FROM tasks
                 WHERE project_id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
                   AND parked_at IS NULL
                 ORDER BY created_at, id",
            )?
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        crate::task_status_audit::execute(
            tx,
            BASIS_PROJECT_PARK_CASCADE,
            actor,
            "UPDATE tasks SET parked_at = ?2, parked_by = ?3, parked_reason = ?4, parked_via = ?5, updated_at = ?2
             WHERE project_id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'archived')
               AND parked_at IS NULL",
            params![id, now, actor, reason, via],
        )?;
        for child in &children {
            cancel_pending_executions(tx, pending, child)?;
        }
        return Ok(());
    }
    if project.parked_at.is_some() {
        tx.execute(
            "UPDATE projects SET parked_at = NULL, parked_by = NULL, parked_reason = NULL, parked_via = NULL,
                 updated_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
    }
    // Always clear the cascaded rows, even when the project was already
    // un-parked: a stale stamp naming it would otherwise strand a hidden row.
    crate::task_status_audit::execute(
        tx,
        BASIS_PROJECT_UNPARK_CASCADE,
        actor,
        "UPDATE tasks SET parked_at = NULL, parked_by = NULL, parked_reason = NULL, parked_via = NULL, updated_at = ?2
         WHERE parked_via = ?1",
        params![via, now],
    )?;
    Ok(())
}

/// Cancel `work_item_id`'s never-started executions. A `claimed` row is
/// mid-spawn and a `running` / `waiting_human` one is a live worker: both are
/// left to finish.
pub(crate) fn cancel_pending_executions(
    tx: &Connection,
    pending: &mut PendingEvents,
    work_item_id: &str,
) -> Result<usize> {
    let ids: Vec<String> = tx
        .prepare(
            "SELECT id FROM work_executions
             WHERE work_item_id = ?1 AND status IN ('ready', 'queued', 'waiting_dependency')
             ORDER BY created_at, id",
        )?
        .query_map([work_item_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for execution_id in &ids {
        crate::executions_runs::cancel_execution_in_tx(
            tx,
            pending,
            execution_id,
            CancelExecutionOpts {
                reason: Some(CANCEL_REASON_PARKED.to_owned()),
                queued_only: true,
                record_failure_reason: false,
            },
        )?;
    }
    Ok(ids.len())
}

/// After a task changes `project_id`, the membership trigger may have parked
/// it (and the cascade its revisions). Cancel the never-started executions of
/// every chain member that is now parked, matching the explicit park path.
pub(crate) fn cancel_pending_for_parked_chain(
    tx: &Connection,
    pending: &mut PendingEvents,
    root_id: &str,
) -> Result<()> {
    let mut ids = vec![root_id.to_owned()];
    ids.extend(crate::chain_helpers::collect_chain_revision_ids_including_deleted(
        tx, root_id,
    )?);
    for id in ids {
        let parked: bool = tx
            .query_row("SELECT parked_at IS NOT NULL FROM tasks WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .unwrap_or(false);
        if parked {
            cancel_pending_executions(tx, pending, &id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_protocol::{
        CreateExecutionInput, CreateProjectInput, CreateTaskInput, ExecutionKind, ExecutionStatus, StuckBucket,
    };

    #[path = "regressions.rs"]
    mod regressions;

    type Fixture = (tempfile::TempDir, WorkDb, String);

    fn fixture() -> Fixture {
        let (dir, db) = open_db();
        let product = create_test_product(&db).id;
        (dir, db, product)
    }

    fn project(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_project(
            CreateProjectInput::builder()
                .product_id(product_id)
                .name(name)
                .no_design_task(true)
                .build(),
        )
        .unwrap()
        .id
    }

    /// A project task that no creation-time dispatch can race.
    fn project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> String {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap()
        .id
    }

    /// A hidden revision row, inserted directly: the revision gate needs an
    /// open PR that these tests have no reason to fake.
    fn revision_of(db: &WorkDb, parent: &str, name: &str) -> String {
        let parent = get_task(db, parent);
        let id = format!("task_rev_{name}");
        let conn = db.connect().unwrap();
        crate::task_status_audit::execute(
            &conn,
            "insert_revision_in_tx",
            LAST_STATUS_ACTOR_HUMAN,
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, ?3, 'revision', ?4, '', 'todo', '1', '1', ?5)",
            params![id, parent.product_id, parent.project_id, name, parent.id],
        )
        .unwrap();
        id
    }

    fn park(db: &WorkDb, id: &str) {
        db.park_work_item_as_actor(id, true, Some("not now"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
    }

    fn unpark(db: &WorkDb, id: &str) -> Result<WorkItem> {
        db.park_work_item_as_actor(id, false, None, LAST_STATUS_ACTOR_HUMAN)
    }

    fn get_task(db: &WorkDb, id: &str) -> Task {
        query_task(&db.connect().unwrap(), id).unwrap().unwrap()
    }

    fn project_row(db: &WorkDb, id: &str) -> Project {
        query_project(&db.connect().unwrap(), id).unwrap().unwrap()
    }

    fn execution_status(db: &WorkDb, execution_id: &str) -> String {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT status FROM work_executions WHERE id = ?1",
                [execution_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// `(old, new, actor, basis)` for every `parked_at` ledger row of `id`, oldest first.
    fn ledger(db: &WorkDb, id: &str) -> Vec<(Option<String>, Option<String>, String, String)> {
        let conn = db.connect().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT old_value, new_value, actor, basis FROM task_status_audit
                 WHERE work_item_id = ?1 AND property = 'parked_at' ORDER BY id",
            )
            .unwrap();
        stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn stuck_bucket(db: &WorkDb, id: &str) -> Option<StuckBucket> {
        db.stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs() + 10, &|_| false)
            .unwrap()
            .rows
            .into_iter()
            .find(|row| row.work_item_id == id)
            .map(|row| row.bucket)
    }

    #[test]
    fn migration_is_idempotent_and_adds_columns_to_both_tables() {
        let (_dir, db, _) = fixture();
        let conn = db.connect().unwrap();
        for table in ["tasks", "projects"] {
            for column in PARKED_COLUMNS {
                assert!(crate::schema_init::table_has_column(&conn, table, column).unwrap());
            }
        }
        migrate(&conn).unwrap();
    }

    #[test]
    fn park_and_unpark_a_task_keeps_status_and_writes_the_ledger() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "Parked chore");

        let WorkItem::Chore(parked) = db
            .park_work_item_as_actor(
                &chore.id,
                true,
                Some("  waiting on the offsite  "),
                LAST_STATUS_ACTOR_BOSS,
            )
            .unwrap()
        else {
            panic!("expected a chore")
        };
        assert!(parked.is_parked());
        assert_eq!(parked.status, chore.status, "parking is orthogonal to status");
        assert_eq!(parked.parked_by.as_deref(), Some("boss"));
        assert_eq!(parked.parked_reason.as_deref(), Some("waiting on the offsite"));
        assert_eq!(parked.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert!(db.is_parked(&chore.id).unwrap());
        assert!(!db.upkeep_eligible(&chore.id).unwrap());

        let WorkItem::Chore(back) = unpark(&db, &chore.id).unwrap() else {
            panic!("expected a chore")
        };
        assert!(!back.is_parked());
        assert_eq!(
            (&back.parked_at, &back.parked_by, &back.parked_reason, &back.parked_via),
            (&None, &None, &None, &None)
        );
        assert!(db.upkeep_eligible(&chore.id).unwrap());

        let rows = ledger(&db, &chore.id);
        assert_eq!(rows.len(), 2, "no NULL row for the insert: {rows:?}");
        assert_eq!(
            (rows[0].0.as_deref(), rows[0].2.as_str(), rows[0].3.as_str()),
            (None, "boss", BASIS_PARKED)
        );
        assert!(rows[0].1.is_some());
        assert_eq!(
            (rows[1].1.as_deref(), rows[1].2.as_str(), rows[1].3.as_str()),
            (None, "human", BASIS_UNPARKED)
        );
        assert_eq!(rows[1].0, rows[0].1);
    }

    #[test]
    fn parking_is_idempotent_and_direct_parking_takes_over_a_cascaded_stamp() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "Idempotent");
        park(&db, &chore.id);
        let first = get_task(&db, &chore.id).parked_at;
        park(&db, &chore.id);
        assert_eq!(get_task(&db, &chore.id).parked_at, first);
        assert_eq!(ledger(&db, &chore.id).len(), 1, "a repeat park writes no ledger row");
        unpark(&db, &chore.id).unwrap();
        unpark(&db, &chore.id).unwrap();
        assert_eq!(ledger(&db, &chore.id).len(), 2, "a repeat un-park writes no ledger row");

        let p = project(&db, &product, "Takeover");
        let t = project_task(&db, &product, &p, "Child");
        db.park_work_item_as_actor(&p, true, Some("project reason"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        assert_eq!(get_task(&db, &t).parked_via, Some(parked_via_project(&p)));
        db.park_work_item_as_actor(&t, true, Some("mine"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        let child = get_task(&db, &t);
        assert_eq!(child.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert_eq!(child.parked_reason.as_deref(), Some("mine"));
    }

    #[test]
    fn only_the_human_and_boss_tiers_may_park() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "Owner only");
        let p = project(&db, &product, "Owner only project");
        for actor in ["worker", "progression", "engine", "boothby", "unknown"] {
            for id in [&chore.id, &p] {
                for parked in [true, false] {
                    let error = db.park_work_item_as_actor(id, parked, None, actor).unwrap_err();
                    assert!(
                        matches!(
                            error.downcast_ref::<ParkingRefusal>(),
                            Some(ParkingRefusal::CallerTier { caller_tier }) if caller_tier == actor
                        ),
                        "{actor} must be refused with a typed error, got {error:#}"
                    );
                }
            }
        }
        assert!(!db.is_parked(&chore.id).unwrap() && !db.is_parked(&p).unwrap());
        // The patch route has the same gate.
        let patch = WorkItemPatch::builder().parked(true).build();
        assert!(
            db.update_work_item_as_actor(&chore.id, patch.clone(), "engine")
                .is_err()
        );
        db.update_work_item_as_actor(&chore.id, patch, LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        assert!(db.is_parked(&chore.id).unwrap());
    }

    #[test]
    fn a_parking_patch_cannot_carry_other_edits() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "Pure patch");
        let mixed = WorkItemPatch::builder().parked(true).name("renamed").build();
        let error = db
            .update_work_item_as_actor(&chore.id, mixed, LAST_STATUS_ACTOR_HUMAN)
            .unwrap_err();
        assert!(format!("{error:#}").contains("own request"), "{error:#}");
        let reason_only = WorkItemPatch::builder().parked_reason("why").build();
        assert!(
            db.update_work_item_as_actor(&chore.id, reason_only, LAST_STATUS_ACTOR_HUMAN)
                .is_err()
        );
        let task = get_task(&db, &chore.id);
        assert!(!task.is_parked(), "a refused patch applies nothing");
        assert_eq!(task.name, "Pure patch");
    }

    #[test]
    fn refuses_what_cannot_be_parked() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "Parent");
        let revision = revision_of(&db, &chore.id, "rev");
        assert!(
            db.park_work_item_as_actor(&revision, true, None, "human")
                .unwrap_err()
                .to_string()
                .contains("follows its parent")
        );
        assert!(db.park_work_item_as_actor(&product, true, None, "human").is_err());

        let done = create_test_chore_manual(&db, &product, "Finished");
        db.update_work_item(&done.id, WorkItemPatch::builder().status("done").build())
            .unwrap();
        assert!(
            db.park_work_item_as_actor(&done.id, true, None, "human")
                .unwrap_err()
                .to_string()
                .contains("nothing to park")
        );
        assert!(db.park_work_item_as_actor("task_missing", true, None, "human").is_err());
    }

    #[test]
    fn project_park_stamps_every_open_child_including_hidden_revisions_and_deferred_rows() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Cascade");
        let plain = project_task(&db, &product, &p, "plain");
        let deferred = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product)
                    .project_id(&p)
                    .name("deferred")
                    .deferred(true)
                    .build(),
            )
            .unwrap()
            .id;
        let active = project_task(&db, &product, &p, "active");
        db.update_work_item(&active, WorkItemPatch::builder().status("active").build())
            .unwrap();
        let revision = revision_of(&db, &plain, "hidden");
        let done = project_task(&db, &product, &p, "done");
        db.update_work_item(&done, WorkItemPatch::builder().status("done").build())
            .unwrap();
        let deleted = project_task(&db, &product, &p, "deleted");
        db.delete_work_item(&deleted).unwrap();
        let direct = project_task(&db, &product, &p, "direct");
        db.park_work_item_as_actor(&direct, true, Some("mine"), "human")
            .unwrap();
        let outside = create_test_chore_manual(&db, &product, "outside").id;

        db.park_work_item_as_actor(&p, true, Some("focus elsewhere"), "boss")
            .unwrap();

        let project_row = project_row(&db, &p);
        assert!(project_row.is_parked());
        assert_eq!(project_row.parked_by.as_deref(), Some("boss"));
        assert_eq!(project_row.parked_reason.as_deref(), Some("focus elsewhere"));
        let via = parked_via_project(&p);
        for id in [&plain, &deferred, &active, &revision] {
            let row = get_task(&db, id);
            assert!(row.is_parked(), "{id} is open and in the project");
            assert_eq!(row.parked_via.as_deref(), Some(via.as_str()), "{id}");
            assert_eq!(row.parked_reason.as_deref(), Some("focus elsewhere"));
            assert_eq!(row.parked_by.as_deref(), Some("boss"));
            let rows = ledger(&db, id);
            assert_eq!(rows.len(), 1, "{id}: {rows:?}");
            assert_eq!(rows[0].3, BASIS_PROJECT_PARK_CASCADE);
            assert_eq!(rows[0].2, "boss");
            assert!(db.is_parked(id).unwrap());
        }
        assert_eq!(get_task(&db, &direct).parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert_eq!(get_task(&db, &direct).parked_reason.as_deref(), Some("mine"));
        for id in [&done, &deleted, &outside] {
            assert_eq!(
                get_task(&db, id).parked_at,
                None,
                "{id} is terminal, deleted or outside"
            );
        }
    }

    #[test]
    fn project_unpark_leaves_directly_parked_children_parked() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Unpark");
        let cascaded = project_task(&db, &product, &p, "cascaded");
        let direct = project_task(&db, &product, &p, "direct");
        park(&db, &direct);
        let other = project(&db, &product, "Other");
        let elsewhere = project_task(&db, &product, &other, "elsewhere");
        park(&db, &p);
        park(&db, &other);

        // A child cannot be un-parked on its own while its project is parked.
        let error = unpark(&db, &cascaded).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("boss project unpark {p}")),
            "{error:#}"
        );

        unpark(&db, &p).unwrap();
        assert!(!project_row(&db, &p).is_parked());
        assert!(!db.is_parked(&cascaded).unwrap());
        assert!(db.is_parked(&direct).unwrap(), "the owner parked it separately");
        assert!(
            db.is_parked(&elsewhere).unwrap(),
            "another project's cascade is untouched"
        );
        let rows = ledger(&db, &cascaded);
        assert_eq!(rows.last().unwrap().3, BASIS_PROJECT_UNPARK_CASCADE);
    }

    #[test]
    fn children_created_in_or_moved_into_a_parked_project_are_parked() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Born parked");
        park(&db, &p);
        let parked_at = project_row(&db, &p).parked_at;

        let born = project_task(&db, &product, &p, "born");
        let row = get_task(&db, &born);
        assert!(row.is_parked(), "a task created in a parked project is born parked");
        assert_eq!(row.parked_via, Some(parked_via_project(&p)));
        assert_eq!(
            row.parked_at, parked_at,
            "parked since the owner decided, not since creation"
        );
        assert_eq!(ledger(&db, &born).len(), 1);

        let hidden = revision_of(&db, &born, "born_rev");
        assert!(get_task(&db, &hidden).is_parked());

        let chore = create_test_chore_manual(&db, &product, "wanderer");
        assert!(!db.is_parked(&chore.id).unwrap());
        db.update_work_item(&chore.id, WorkItemPatch::builder().project_id(&p).build())
            .unwrap();
        assert!(get_task(&db, &chore.id).is_parked(), "moving in joins the park");
        db.update_work_item(&chore.id, WorkItemPatch::builder().project_id("").build())
            .unwrap();
        let left = get_task(&db, &chore.id);
        assert!(
            !left.is_parked(),
            "moving out drops the project's cascade stamp: {left:?}"
        );
        assert!(db.upkeep_eligible(&chore.id).unwrap());
    }

    #[test]
    fn a_revision_is_born_parked_from_a_directly_parked_parent() {
        let (_dir, db, product) = fixture();
        let parent = create_test_chore_manual(&db, &product, "Parent PR");
        park(&db, &parent.id);
        let parent_row = get_task(&db, &parent.id);
        let revision = revision_of(&db, &parent.id, "follows");
        let row = get_task(&db, &revision);
        assert!(row.is_parked(), "section 1.2: born parked with the parent's stamp");
        assert_eq!(row.parked_at, parent_row.parked_at);
        assert_eq!(row.parked_by, parent_row.parked_by);
        assert_eq!(row.parked_reason, parent_row.parked_reason);
        assert_eq!(row.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        let rows = ledger(&db, &revision);
        assert_eq!(
            rows.len(),
            1,
            "a born-parked revision writes a parked_at ledger row: {rows:?}"
        );
        assert!(row.parked_at.is_some());
        let holder = parked_holder(&db.connect().unwrap(), &revision).unwrap().unwrap();
        assert_eq!(
            (holder.holder_id.as_str(), holder.holder_is_project),
            (parent.id.as_str(), false)
        );
        unpark(&db, &parent.id).unwrap();
        let after = get_task(&db, &revision);
        assert!(!after.is_parked());
        assert_eq!(after.parked_at, None);
        assert!(db.upkeep_eligible(&revision).unwrap());
        assert_eq!(ledger(&db, &revision).last().unwrap().3, BASIS_UNPARKED);
    }

    #[test]
    fn parking_a_parent_stamps_existing_revisions_and_cancels_only_their_pending_executions() {
        let (_dir, db, product) = fixture();
        let parent = create_test_chore_manual(&db, &product, "Parent PR");
        let pending_rev = revision_of(&db, &parent.id, "pending");
        let live_rev = revision_of(&db, &parent.id, "live");
        let pending_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(pending_rev.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let live_exec = create_spawned_execution(&db, &live_rev, 4243);
        park(&db, &parent.id);
        let pending_row = get_task(&db, &pending_rev);
        let live_row = get_task(&db, &live_rev);
        assert!(pending_row.is_parked() && live_row.is_parked());
        assert_eq!(pending_row.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert_eq!(live_row.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert_eq!(execution_status(&db, &pending_exec.id), "cancelled");
        assert_eq!(
            execution_status(&db, &live_exec),
            "running",
            "a live revision worker is never killed"
        );
    }

    #[test]
    fn unparking_a_direct_child_of_a_parked_project_is_refused_and_keeps_the_stamp() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Still parked");
        let child = project_task(&db, &product, &p, "direct-child");
        park(&db, &p);
        db.park_work_item_as_actor(&child, true, Some("mine"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        assert_eq!(get_task(&db, &child).parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        let error = unpark(&db, &child).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("boss project unpark {p}")),
            "{error:#}"
        );
        let row = get_task(&db, &child);
        assert!(row.is_parked(), "is_parked and the stamp must agree");
        assert_eq!(row.parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
        assert_eq!(row.parked_reason.as_deref(), Some("mine"));
    }

    #[test]
    fn the_sql_predicate_and_the_holder_agree() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Agreement");
        let child = project_task(&db, &product, &p, "child");
        let revision = revision_of(&db, &child, "rev");
        let loose = create_test_chore_manual(&db, &product, "loose").id;
        let agree = |label: &str| {
            let conn = db.connect().unwrap();
            for id in [&p, &child, &revision, &loose, &product] {
                assert_eq!(
                    is_parked(&conn, id).unwrap(),
                    parked_holder(&conn, id).unwrap().is_some(),
                    "{label}: {id}"
                );
            }
        };
        agree("nothing parked");
        park(&db, &p);
        agree("project parked");
        unpark(&db, &p).unwrap();
        park(&db, &child);
        agree("child parked directly");
        park(&db, &loose);
        agree("loose chore parked");
        assert!(db.is_parked(&revision).unwrap());
        assert!(db.is_parked(&loose).unwrap());
        assert!(!db.is_parked(&p).unwrap());
        assert!(!db.is_parked(&product).unwrap());
    }

    #[test]
    fn moving_a_task_into_a_parked_project_cancels_pending_executions_but_not_live_ones() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Parked target");
        park(&db, &p);
        let parent = create_test_chore_manual(&db, &product, "mover");
        let ready_exec = create_ready_chore_execution(&db, parent.id.clone());
        let pending_rev = revision_of(&db, &parent.id, "pending-rev");
        let rev_exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(pending_rev.clone())
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let live_rev = revision_of(&db, &parent.id, "live-rev");
        let live_exec = create_spawned_execution(&db, &live_rev, 4244);

        db.update_work_item(&parent.id, WorkItemPatch::builder().project_id(&p).build())
            .unwrap();

        assert!(get_task(&db, &parent.id).is_parked());
        assert!(get_task(&db, &pending_rev).is_parked());
        assert_eq!(execution_status(&db, &ready_exec.id), "cancelled");
        assert_eq!(execution_status(&db, &rev_exec.id), "cancelled");
        assert_eq!(
            execution_status(&db, &live_exec),
            "running",
            "a live worker is never killed"
        );
    }

    #[test]
    fn parking_cancels_never_started_executions_and_leaves_live_ones_alone() {
        let (_dir, db, product) = fixture();
        let ready = create_test_chore_manual(&db, &product, "ready");
        let ready_exec = create_ready_chore_execution(&db, ready.id.clone());
        let mut statuses = vec![(ready.id.clone(), ready_exec.id.clone())];
        for (name, status) in [
            ("queued", ExecutionStatus::Queued),
            ("waiting", ExecutionStatus::WaitingDependency),
        ] {
            let task = create_test_chore_manual(&db, &product, name);
            let execution = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(task.id.clone())
                        .kind(ExecutionKind::ChoreImplementation)
                        .status(status)
                        .build(),
                )
                .unwrap();
            statuses.push((task.id, execution.id));
        }
        let live = create_active_chore(&db, &product, "live");
        let running = create_spawned_execution(&db, &live, 4242);
        assert_eq!(execution_status(&db, &running), "running");

        for (task, _) in &statuses {
            park(&db, task);
        }
        park(&db, &live);

        for (task, execution) in &statuses {
            assert_eq!(execution_status(&db, execution), "cancelled", "{task}");
        }
        assert_eq!(
            execution_status(&db, &running),
            "running",
            "a live worker is never killed"
        );
        assert_eq!(get_task(&db, &live).status, TaskStatus::Active);
        assert!(db.is_parked(&live).unwrap());
    }

    #[test]
    fn project_park_cancels_pending_executions_of_the_children_it_stamps() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Pending");
        let a = project_task(&db, &product, &p, "a");
        let execution = create_ready_chore_execution(&db, a.clone());
        park(&db, &p);
        assert_eq!(execution_status(&db, &execution.id), "cancelled");
    }

    #[test]
    fn parked_rows_are_never_minted_an_execution_and_an_explicit_start_is_refused() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore(&db, &product, "auto");
        park(&db, &chore.id);
        let execution_count = |db: &WorkDb, id: &str| -> i64 {
            db.connect()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND status IN ('ready', 'queued')",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(execution_count(&db, &chore.id), 0);
        db.reconcile_product_executions(&product).unwrap();
        assert_eq!(execution_count(&db, &chore.id), 0, "reconcile skips a parked row");

        let error = db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .build(),
            )
            .unwrap_err();
        let Some(ParkingRefusal::UnparkFirst {
            holder_id,
            unpark_command,
            ..
        }) = error.downcast_ref::<ParkingRefusal>()
        else {
            panic!("expected the typed un-park-first refusal, got {error:#}");
        };
        assert_eq!(holder_id, &chore.id);
        assert_eq!(unpark_command, &format!("boss task unpark {}", chore.id));
        assert!(error.to_string().contains("un-park it first"), "{error:#}");
        assert_eq!(execution_count(&db, &chore.id), 0);

        unpark(&db, &chore.id).unwrap();
        db.reconcile_product_executions(&product).unwrap();
        assert_eq!(
            execution_count(&db, &chore.id),
            1,
            "un-parking restores ordinary dispatch"
        );
    }

    #[test]
    fn flush_parked_at_ledger_is_a_noop_when_upgrade_tests_have_dropped_the_sink() {
        let (_dir, db, _product) = fixture();
        let conn = db.connect().unwrap();
        conn.execute_batch("DROP TABLE parking_audit_sink; DROP TABLE parking_audit_context;")
            .unwrap();
        flush_parked_at_ledger(&conn).unwrap();
        crate::task_status_audit::execute(
            &conn,
            "unattributed_write",
            LAST_STATUS_ACTOR_HUMAN,
            "UPDATE tasks SET updated_at = updated_at WHERE 0",
            [],
        )
        .unwrap();
    }

    #[test]
    fn a_child_of_a_parked_project_names_the_project_when_refused() {
        let (_dir, db, product) = fixture();
        let p = project(&db, &product, "Named");
        let child = project_task(&db, &product, &p, "child");
        park(&db, &p);
        let error = db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(child.clone())
                    .build(),
            )
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&format!("because project {p} is parked")), "{message}");
        assert!(message.contains(&format!("boss project unpark {p}")), "{message}");
        assert!(message.contains("not now"), "the owner's reason is echoed: {message}");
    }

    #[test]
    fn the_dependency_cascade_cannot_mint_for_a_parked_row() {
        let (_dir, db, product) = fixture();
        let prerequisite = create_test_chore_manual(&db, &product, "prerequisite");
        let dependent = create_test_chore(&db, &product, "dependent");
        db.add_dependency(boss_protocol::AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prerequisite.id.clone(),
            relation: None,
        })
        .unwrap();
        park(&db, &dependent.id);
        db.update_work_item(&prerequisite.id, WorkItemPatch::builder().status("done").build())
            .unwrap();
        let row = get_task(&db, &dependent.id);
        assert_eq!(row.status, TaskStatus::Todo, "the lifecycle position still advances");
        let live: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND status IN ('ready', 'queued', 'waiting_dependency')",
                [&dependent.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 0, "the cascade mints nothing for a parked row");
    }

    #[test]
    fn stuck_report_buckets_parked_rows_and_their_dependents() {
        let (_dir, db, product) = fixture();
        let prerequisite = create_test_chore_manual(&db, &product, "prerequisite");
        let dependent = create_test_chore_manual(&db, &product, "dependent");
        let transitive = create_test_chore_manual(&db, &product, "transitive");
        for (dependent_id, prerequisite_id) in [(&dependent.id, &prerequisite.id), (&transitive.id, &dependent.id)] {
            db.add_dependency(boss_protocol::AddDependencyInput {
                dependent: dependent_id.clone(),
                prerequisite: prerequisite_id.clone(),
                relation: None,
            })
            .unwrap();
        }
        assert_ne!(stuck_bucket(&db, &dependent.id), Some(StuckBucket::ParkedUpstream));

        park(&db, &prerequisite.id);
        assert_eq!(stuck_bucket(&db, &prerequisite.id), Some(StuckBucket::Parked));
        assert_eq!(stuck_bucket(&db, &dependent.id), Some(StuckBucket::ParkedUpstream));
        assert_eq!(
            stuck_bucket(&db, &transitive.id),
            Some(StuckBucket::ParkedUpstream),
            "a dependent of a dependent of a parked prerequisite is waiting on the owner too"
        );

        let report = db
            .stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs() + 10, &|_| false)
            .unwrap();
        let row = report.rows.iter().find(|r| r.work_item_id == prerequisite.id).unwrap();
        assert_eq!(row.last_ledger_basis, BASIS_PARKED);
        assert!(!row.age_is_lower_bound, "a parked row ages from the owner's stamp");

        unpark(&db, &prerequisite.id).unwrap();
        assert_ne!(stuck_bucket(&db, &prerequisite.id), Some(StuckBucket::Parked));
        assert_ne!(stuck_bucket(&db, &dependent.id), Some(StuckBucket::ParkedUpstream));
    }

    #[test]
    fn a_parked_failure_is_not_an_unconverted_declared_blocker() {
        let (_dir, db, product) = fixture();
        let chore = create_test_chore_manual(&db, &product, "declared");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET dispatch_failed_reason = 'deliberate_park', dispatch_failed_at = '1' WHERE id = ?1",
                [&chore.id],
            )
            .unwrap();
        let now = boss_engine_utils::epoch_time::now_epoch_secs() + 10;
        assert_eq!(
            db.stuck_report(0, now, &|_| false)
                .unwrap()
                .unconverted_declared_blockers,
            1
        );
        park(&db, &chore.id);
        let report = db.stuck_report(0, now, &|_| false).unwrap();
        assert_eq!(report.unconverted_declared_blockers, 0);
        assert_eq!(stuck_bucket(&db, &chore.id), Some(StuckBucket::Parked));
    }
}
