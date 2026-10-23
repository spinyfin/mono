use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use super::*;
use boss_protocol::DispatchWaitBlocker;

#[derive(Default)]
pub(super) struct DispatchWait {
    pub reason: Option<String>,
    pub not_before: Option<String>,
    pub blocker: Option<DispatchWaitBlocker>,
    /// The projection consulted another work item's state (dependency gating
    /// or a chain-hold blocker), which the item's own execution rows do not
    /// version. The board runtime cache must not retain such a result.
    pub reads_other_items: bool,
}

/// Wait reason for a project task held back by the ordinal chain.
pub(super) const WAIT_ORDINAL_CHAIN: &str = "ordinal_chain";
/// Wait reason for a pending execution the scheduler has not stamped yet.
pub(super) const WAIT_AWAITING_SCHEDULER: &str = "awaiting_scheduler";
/// Wait reason for a `waiting_dependency` execution whose gates have all
/// cleared: the next readiness reconcile promotes it to `ready`.
pub(super) const WAIT_AWAITING_RECONCILE: &str = "awaiting_reconcile";

/// Project tasks in ordinal-chain order, grouped by project, with the first
/// task that still accepts an execution. `reconcile_product_executions` runs
/// project tasks in `(ordinal, created_at, id)` order and only that first
/// task is `ready`, so every later one waits behind it. Built once from the
/// tasks `get_work_tree` already holds, so resolving the gate for a row is a
/// map lookup rather than a sibling query per row.
#[derive(Default)]
pub(super) struct OrdinalChains {
    projects: HashMap<String, ProjectChain>,
}

#[derive(Default)]
struct ProjectChain {
    order: Vec<String>,
    first_accepting: Option<String>,
}

impl OrdinalChains {
    pub(super) fn from_tasks<'a>(tasks: impl IntoIterator<Item = &'a Task>) -> Self {
        let mut by_project: HashMap<&str, Vec<&Task>> = HashMap::new();
        for task in tasks {
            if let Some(project_id) = task.project_id.as_deref()
                && matches!(task.kind, TaskKind::ProjectTask | TaskKind::Design)
            {
                by_project.entry(project_id).or_default().push(task);
            }
        }
        let projects = by_project
            .into_iter()
            .map(|(project_id, mut members)| {
                members.sort_by(|a, b| {
                    (a.ordinal.unwrap_or(i64::MAX), &a.created_at, &a.id).cmp(&(
                        b.ordinal.unwrap_or(i64::MAX),
                        &b.created_at,
                        &b.id,
                    ))
                });
                let first_accepting = members
                    .iter()
                    .find(|task| task_accepts_execution(task))
                    .map(|task| task.id.clone());
                let order = members.into_iter().map(|task| task.id.clone()).collect();
                (project_id.to_owned(), ProjectChain { order, first_accepting })
            })
            .collect();
        Self { projects }
    }

    /// One project's chain, read from the database. Used only by the
    /// single-item lookup, which has no pre-loaded task list; every
    /// statement it runs is counted.
    fn load_project(conn: &Connection, project_id: &str, queries: &mut u64) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT id FROM tasks
             WHERE project_id = ?1 AND deleted_at IS NULL AND kind IN ('project_task', 'design')",
        )?;
        *queries += 1;
        let ids = stmt
            .query_map([project_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            *queries += 1;
            tasks.extend(query_task(conn, &id)?);
        }
        Ok(Self::from_tasks(&tasks))
    }

    /// The earlier task `task_id` waits behind, if any.
    fn blocker_id(&self, project_id: &str, task_id: &str) -> Option<&str> {
        let chain = self.projects.get(project_id)?;
        let first = chain.first_accepting.as_deref()?;
        for id in &chain.order {
            if id == task_id {
                return None;
            }
            if id == first {
                return Some(first);
            }
        }
        None
    }
}

/// Project only pending execution state; never leak an old hold onto a live run.
/// Every pending execution comes back with an engine-stated reason: a
/// dependency, the ordinal chain, a scheduled time, the scheduler's recorded
/// reason, `awaiting_scheduler` for a runnable row the scheduler has not
/// stamped yet, or `awaiting_reconcile` for a `waiting_dependency` row whose
/// gates have all cleared. A `waiting_dependency` row never takes a scheduler
/// stamp: the scheduler does not drain that status, so a stamp left over
/// from an earlier `ready` period would outlive the gate that now holds the
/// row. `queries` counts every database lookup this makes (a lookup can run
/// more than one statement). `chains` is the pre-built ordinal chain, or
/// `None` to read the row's project from the database.
pub(super) fn for_execution(
    conn: &Connection,
    execution: Option<&WorkExecution>,
    task: Option<&Task>,
    chains: Option<&OrdinalChains>,
    queries: &mut u64,
    now: i64,
) -> Result<DispatchWait> {
    let Some(execution) = execution else {
        return Ok(DispatchWait::default());
    };
    let mut wait = DispatchWait::default();
    match execution.status {
        ExecutionStatus::WaitingDependency => {
            wait.reads_other_items = true;
            waiting_dependency(conn, execution, task, chains, queries, &mut wait)?;
            return Ok(wait);
        }
        ExecutionStatus::Queued | ExecutionStatus::Ready => {}
        _ => return Ok(wait),
    }
    if let Some(epoch) = execution
        .dispatch_not_before
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|epoch| *epoch > now)
    {
        wait.reason = Some("not_before".into());
        wait.not_before = Some(epoch.to_string());
    } else if let Some(reason) = execution
        .dispatch_wait_reason
        .as_ref()
        .filter(|reason| !reason.trim().is_empty())
    {
        wait.reason = Some(reason.clone());
        *queries += 1;
        let blocker_id: Option<String> = conn.query_row(
            "SELECT dispatch_wait_blocker_id FROM work_executions WHERE id = ?1",
            [&execution.id],
            |row| row.get(0),
        )?;
        if let Some(id) = blocker_id {
            wait.reads_other_items = true;
            // A persisted chain hold is only current while its named writer/reviewer is live.
            // Do not carry its prose or link through the gap before the next scheduler pass.
            *queries += 1;
            if blocker_is_live(conn, &id)? {
                wait.blocker = blocker_for(conn, &id, queries)?;
            } else {
                wait.reason = None;
            }
        }
    }
    if wait.reason.is_none() {
        wait.reason = Some(WAIT_AWAITING_SCHEDULER.into());
    }
    Ok(wait)
}

/// Explain a `waiting_dependency` execution from the gates that currently
/// hold it: declared prerequisites (revision-aware, including same-PR
/// sequencing; a missing prerequisite row still gates), then the ordinal
/// chain. With neither, the row waits only for the next readiness reconcile.
fn waiting_dependency(
    conn: &Connection,
    execution: &WorkExecution,
    task: Option<&Task>,
    chains: Option<&OrdinalChains>,
    queries: &mut u64,
    wait: &mut DispatchWait,
) -> Result<()> {
    *queries += 1;
    if let Some(id) = deps::gating_prereqs_for(conn, &execution.work_item_id)?.first() {
        wait.reason = Some("waiting_dependency".into());
        wait.blocker = blocker_for(conn, id, queries)?;
        return Ok(());
    }
    let loaded;
    let task = match task {
        Some(task) => Some(task),
        None => {
            *queries += 1;
            loaded = query_task(conn, &execution.work_item_id)?;
            loaded.as_ref()
        }
    };
    if let Some(task) = task
        && let Some(project_id) = task.project_id.as_deref()
        && matches!(task.kind, TaskKind::ProjectTask | TaskKind::Design)
    {
        let loaded_chains;
        let chains = match chains {
            Some(chains) => chains,
            None => {
                loaded_chains = OrdinalChains::load_project(conn, project_id, queries)?;
                &loaded_chains
            }
        };
        if let Some(blocker) = chains.blocker_id(project_id, &task.id) {
            wait.reason = Some(WAIT_ORDINAL_CHAIN.into());
            wait.blocker = blocker_for(conn, blocker, queries)?;
            return Ok(());
        }
    }
    if task.is_some_and(|task| !task_accepts_execution(task)) {
        // The reconciler only promotes rows that accept an execution, so this
        // shape will not repair itself on the next pass.
        warn_unexplained_once(execution);
        return Ok(());
    }
    wait.reason = Some(WAIT_AWAITING_RECONCILE.into());
    Ok(())
}

/// Log a `waiting_dependency` row the reconciler will not promote, once per
/// execution: every work-tree fetch re-derives the wait, so an unthrottled
/// warning would repeat for as long as the row exists.
fn warn_unexplained_once(execution: &WorkExecution) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    const WARNED_CAP: usize = 4096;
    let mut warned = WARNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if warned.len() >= WARNED_CAP {
        warned.clear();
    }
    if warned.insert(execution.id.clone()) {
        tracing::warn!(
            work_item_id = %execution.work_item_id,
            execution_id = %execution.id,
            "queued wait: waiting_dependency execution has no gating prerequisite and is not behind \
             an earlier project task, but its work item no longer accepts an execution, so no \
             reconcile will promote it",
        );
    }
}

/// Whether a recorded chain-hold blocker still holds the chain: running, waiting on a human,
/// or claimed (in-flight, cube setup not yet committed to `running`). The scheduler holds on
/// in-flight siblings too, so `query_live_execution_for_work_item` alone would drop that window.
fn blocker_is_live(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions
         WHERE work_item_id = ?1 AND status IN ('claimed', 'running', 'waiting_human'))",
        [work_item_id],
        |row| row.get(0),
    )?)
}

fn blocker_for(conn: &Connection, id: &str, queries: &mut u64) -> Result<Option<DispatchWaitBlocker>> {
    *queries += 1;
    if let Some(task) = query_task(conn, id)? {
        return Ok(Some(DispatchWaitBlocker {
            work_item_id: task.id,
            product_id: task.product_id,
            short_id: task.short_id,
            kind: "task".into(),
        }));
    }
    *queries += 1;
    Ok(query_project(conn, id)?.map(|project| DispatchWaitBlocker {
        work_item_id: project.id,
        product_id: project.product_id,
        short_id: project.short_id,
        kind: "project".into(),
    }))
}
