//! Resolves the membership and ordering metadata stamped on a
//! [`LiveWorkerState`](boss_protocol::LiveWorkerState): badge type, project
//! attribution, host, and execution start time.
//!
//! Every registration path (local spawn, tmux boot adoption, re-adoption,
//! remote registration) goes through [`resolve`], so a worker carries the same
//! metadata whether it was just spawned or recovered after an engine restart —
//! the values derive from durable rows, never from in-memory spawn state.

use boss_protocol::{AgentType, LiveWorkerMetadata, WorkItem};

use crate::work::{WorkDb, WorkExecution};

/// Project attribution for a dispatched work item, in precedence order:
///
/// 1. The work item *is* a project (project-level work): that project.
/// 2. A task or chore with a `project_id`: that project. Revisions inherit
///    their chain root's project at the database level, so no chain walk is
///    needed here.
/// 3. Anything else — an unfiled task, a product, or an id that is not a work
///    item at all (an automation, a comment, a review-guide comparison):
///    unfiled, `None`. Never guessed from the worker pool.
///
/// The name is `None` when a project is attributed but its row cannot be read.
fn project_attribution(work_db: &WorkDb, work_item_id: &str) -> (Option<String>, Option<String>) {
    let project_id = match work_db.get_work_item(work_item_id) {
        Ok(WorkItem::Project(project)) => return (Some(project.id), Some(project.name)),
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => task.project_id,
        Ok(WorkItem::Product(_)) | Err(_) => None,
    };
    let Some(project_id) = project_id else {
        return (None, None);
    };
    match work_db.get_project(&project_id) {
        Ok(project) => (Some(project_id), Some(project.name)),
        Err(error) => {
            tracing::warn!(project_id, %error, "live worker metadata: could not read attributed project name");
            (Some(project_id), None)
        }
    }
}

/// Build the metadata for `execution` running on `host_id` (`"local"` or a
/// registered remote host id).
pub(crate) fn resolve(work_db: &WorkDb, execution: &WorkExecution, host_id: &str) -> LiveWorkerMetadata {
    let has_source_automation = matches!(
        work_db.source_automation_id_for_work_item(&execution.work_item_id),
        Ok(Some(_))
    );
    let (project_id, project_name) = project_attribution(work_db, &execution.work_item_id);
    LiveWorkerMetadata {
        agent_type: Some(
            AgentType::for_execution(&execution.kind, has_source_automation)
                .as_str()
                .to_owned(),
        ),
        project_id,
        project_name,
        host_id: Some(host_id.to_owned()),
        started_at: execution
            .started_epoch()
            .map(boss_engine_utils::iso8601::format_epoch_iso8601),
    }
}

#[cfg(test)]
#[path = "live_worker_metadata_tests.rs"]
mod tests;
