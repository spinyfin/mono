//! Narrow task / execution reads for the engine's periodic sweeps.
//!
//! Reads return only the columns each sweep consumes, batched per pass,
//! with the same row visibility as `get_work_item`.

use super::*;

/// The `get_work_item` visibility predicate for `tasks` rows: soft-deleted
/// rows stay hidden unless they are archived, the inspectable terminal
/// state. Kept as a shared constant so every narrow read here agrees with
/// the full projection on which rows exist.
pub const VISIBLE_TASK_PREDICATE: &str = "(deleted_at IS NULL OR status = 'archived')";

/// What the dispatch scheduler needs to name a chain sibling in a
/// wait-reason label: the `T<n>` label, the task name, and the
/// chain root's `pr_url`. See `chain_serialized_wait_reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWaitLabel {
    pub short_id: Option<i64>,
    pub name: String,
    pub pr_url: Option<String>,
}

impl TaskWaitLabel {
    /// The same wait-reason label `Task::short_label` renders.
    pub fn short_label(&self) -> String {
        boss_protocol::short_id_label(self.short_id).unwrap_or_else(|| "a task".to_owned())
    }
}

/// The three columns the pane-death sweep's terminal-convergence scan
/// consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExecutionRef {
    pub id: String,
    pub work_item_id: String,
    pub status: ExecutionStatus,
}

/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 32766 on modern builds
/// but 999 on older ones; chunking well below the smaller limit keeps a
/// batched `IN (...)` read valid everywhere without caring which the host
/// links against.
const IN_CHUNK: usize = 500;

fn placeholders(n: usize) -> String {
    (1..=n).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",")
}

/// Run `select_sql` — a `SELECT` whose `WHERE` clause ends in
/// `id IN ({placeholders})` — once per chunk of `ids`, mapping each row with
/// `map` and folding the results into one map keyed by the row's id.
fn batched_by_id<T>(
    conn: &Connection,
    ids: &[String],
    select_sql: impl Fn(&str) -> String,
    map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<(String, T)>,
) -> Result<HashMap<String, T>> {
    let mut out = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(IN_CHUNK) {
        let sql = select_sql(&placeholders(chunk.len()));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), &map)?;
        for row in rows {
            let (id, value) = row?;
            out.insert(id, value);
        }
    }
    Ok(out)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// `effort_level` for every visible task/chore in `ids`, in one query
    /// per 500 ids. Ids with no visible row, or whose effort column is
    /// `NULL` / empty (never classified), are absent from the result. A
    /// corrupt row is logged and skipped so other slots retain enforcement.
    pub fn task_effort_levels(&self, ids: &[String]) -> Result<HashMap<String, EffortLevel>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.connect()?;
        let raw: HashMap<String, String> = batched_by_id(
            &conn,
            ids,
            |placeholders| {
                format!(
                    "SELECT id, effort_level FROM tasks
                     WHERE {VISIBLE_TASK_PREDICATE}
                       AND effort_level IS NOT NULL AND effort_level != ''
                       AND id IN ({placeholders})"
                )
            },
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(raw
            .into_iter()
            .filter_map(|(id, effort)| match effort.parse::<EffortLevel>() {
                Ok(level) => Some((id, level)),
                Err(error) => {
                    tracing::warn!(task_id = %id, %error, "corrupt effort_level; skipping this task's envelope");
                    None
                }
            })
            .collect())
    }

    /// Single-id form of [`Self::task_effort_levels`] for the timer-driven
    /// envelope recheck, which evaluates exactly one slot. `None` when the
    /// row is not visible or has never been classified.
    pub fn task_effort_level(&self, id: &str) -> Result<Option<EffortLevel>> {
        Ok(self
            .task_effort_levels(std::slice::from_ref(&id.to_owned()))?
            .remove(id))
    }

    /// Whether each visible task/chore in `ids` is in a terminal status
    /// ([`TaskStatus::is_terminal`]), in one query per 500 ids. Ids with no
    /// visible row are absent from the result; the caller decides whether
    /// that means "gone" (see [`Self::work_item_row_missing`]) or "unknown".
    ///
    /// Supplies the terminal-work sweep with one status per live slot.
    pub fn task_closed_states(&self, ids: &[String]) -> Result<HashMap<String, bool>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.connect()?;
        let raw: HashMap<String, String> = batched_by_id(
            &conn,
            ids,
            |placeholders| {
                format!(
                    "SELECT id, status FROM tasks
                     WHERE {VISIBLE_TASK_PREDICATE} AND id IN ({placeholders})"
                )
            },
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        raw.into_iter()
            .map(|(id, status)| {
                let status = status
                    .parse::<TaskStatus>()
                    .map_err(|err| anyhow::anyhow!("task {id} has a corrupt status column: {err}"))?;
                Ok((id, status.is_terminal()))
            })
            .collect()
    }

    /// Single-id form of [`Self::task_closed_states`]: `Err` (`unknown
    /// task: <id>`) when no visible row exists, matching the error
    /// [`Self::get_work_item`] returned on the same input so callers that
    /// follow up with [`Self::work_item_row_missing`] keep working.
    pub fn task_closed_state(&self, id: &str) -> Result<bool> {
        self.task_closed_states(std::slice::from_ref(&id.to_owned()))?
            .remove(id)
            .with_context(|| format!("unknown task: {id}"))
    }

    /// `pr_url` of one visible task/chore (`None` when the column is
    /// `NULL`). `Err` (`unknown task: <id>`) when no visible row exists,
    /// matching [`Self::get_work_item`] on the same input.
    ///
    /// Used by PR-review recovery, which needs only the URL to ask GitHub
    /// whether the PR is still open.
    pub fn task_pr_url(&self, id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let sql = format!("SELECT pr_url FROM tasks WHERE {VISIBLE_TASK_PREDICATE} AND id = ?1");
        conn.query_row(&sql, params![id], |row| row.get::<_, Option<String>>(0))
            .optional()?
            .with_context(|| format!("unknown task: {id}"))
    }

    /// The label fields for one visible task/chore (`None` when no visible
    /// row exists). Used by the dispatch scheduler to word a
    /// chain-serialized wait reason: the `T<n>` label, the name, and the
    /// chain root's `pr_url`.
    pub fn task_wait_label(&self, id: &str) -> Result<Option<TaskWaitLabel>> {
        let conn = self.connect()?;
        let sql = format!("SELECT short_id, name, pr_url FROM tasks WHERE {VISIBLE_TASK_PREDICATE} AND id = ?1");
        Ok(conn
            .query_row(&sql, params![id], |row| {
                Ok(TaskWaitLabel {
                    short_id: row.get(0)?,
                    name: row.get(1)?,
                    pr_url: row.get(2)?,
                })
            })
            .optional()?)
    }

    /// Terminal executions whose LATEST `work_runs` row ran on this host with
    /// a recorded shell pid within `max_age_secs` — the pane-death sweep's
    /// restart-robust candidate set for a worker that outlived the state the
    /// engine recorded for it. Returns only the id / work item / status
    /// triple the sweep consumes.
    ///
    /// The latest-run, local-host, and `COALESCE(finished_at, created_at)`
    /// predicates deliberately match
    /// [`Self::latest_local_shell_pid_for_execution_within`]. The caller
    /// performs that bounded probe before acting; applying the same bound
    /// here keeps old terminal history out of every sweep pass without
    /// weakening the destructive path's own safety check.
    pub fn list_recent_terminal_execution_refs_with_local_shell_pid(
        &self,
        max_age_secs: i64,
        now_epoch_secs: i64,
    ) -> Result<Vec<TerminalExecutionRef>> {
        let conn = self.connect()?;
        let cutoff = now_epoch_secs - max_age_secs;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.work_item_id, e.status
             FROM work_executions e
             WHERE e.status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND EXISTS (
                   SELECT 1 FROM work_runs r
                   WHERE r.id = (
                       SELECT latest.id FROM work_runs latest
                       WHERE latest.execution_id = e.id
                       ORDER BY latest.created_at DESC, latest.id DESC
                       LIMIT 1
                   )
                     AND r.host_id = 'local'
                     AND r.shell_pid IS NOT NULL
                     AND r.shell_pid > 0
                     AND CAST(COALESCE(r.finished_at, r.created_at) AS INTEGER) >= ?1
               )
             ORDER BY e.created_at ASC, e.id ASC",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            let status: String = row.get(2)?;
            Ok(TerminalExecutionRef {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                status: status.parse().map_err(|e: String| {
                    rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, e.into())
                })?,
            })
        })?;
        collect_rows(rows)
    }
}

#[cfg(test)]
#[path = "sweep_reads_tests.rs"]
mod tests;
