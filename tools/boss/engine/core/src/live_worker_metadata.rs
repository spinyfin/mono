//! Resolves the membership and ordering metadata stamped on a
//! [`LiveWorkerState`](boss_protocol::LiveWorkerState): badge type, project
//! attribution, host, and execution start time.
//!
//! Every registration path (local spawn, tmux boot adoption, re-adoption,
//! remote registration) goes through [`resolve`], so a worker carries the same
//! metadata whether it was just spawned or recovered after an engine restart —
//! the values derive from durable rows, never from in-memory spawn state.

use boss_protocol::{AgentType, ExecutionKind, LiveWorkerMetadata, WorkItem};

use crate::work::{WorkDb, WorkExecution};

/// Keep the durable binding even for execution owners (such as review-guide
/// comparisons and comments) that are not ordinary work-item variants.
pub(crate) fn execution_binding(work_db: &WorkDb, execution: &WorkExecution) -> boss_protocol::WorkItemBinding {
    let work_item_name = work_db
        .get_work_item(&execution.work_item_id)
        .map(|item| crate::runner::work_item_name(&item).to_owned())
        .unwrap_or_else(|_| execution.work_item_id.clone());
    boss_protocol::WorkItemBinding {
        work_item_id: execution.work_item_id.clone(),
        work_item_name,
        execution_id: execution.id.clone(),
    }
}

/// Guide launch configuration is persisted before the pane starts, unlike
/// the execution launch stamp, which is written after spawn returns.
pub(crate) fn guide_launch_config(work_db: &WorkDb, execution: &WorkExecution) -> Option<(String, String)> {
    if execution.kind != ExecutionKind::PrReviewGuide {
        return None;
    }
    match work_db.pr_review_guide_attempt_for_execution(&execution.id) {
        Ok(Some(attempt)) => Some((attempt.driver?, attempt.model?)),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(execution_id = %execution.id, %error, "could not read guide launch configuration");
            None
        }
    }
}

/// Shared identity for both restart registration paths, with the execution
/// stamp taking precedence over the pre-spawn guide configuration.
pub(crate) fn adopted_identity(
    work_db: &WorkDb,
    execution: &WorkExecution,
    driver: Option<&dyn crate::driver::AgentDriver>,
) -> (String, bool) {
    let model = execution
        .model
        .clone()
        .or_else(|| guide_launch_config(work_db, execution).map(|(_, model)| model))
        .unwrap_or_else(|| {
            driver
                .map(|driver| driver.descriptor().label.to_owned())
                .unwrap_or_else(|| "Unknown driver".to_owned())
        });
    let awaiting_input_capable = driver.is_some_and(|driver| {
        driver
            .capabilities()
            .provides(crate::driver::Capability::AwaitingInputSignal)
    });
    (model, awaiting_input_capable)
}

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

/// Stage of an AI review for a `pr_review` execution, from the durable batch
/// member role the engine recorded when it launched the run. `None` for any
/// other kind and for a legacy reviewer that has no batch member row.
fn review_stage(work_db: &WorkDb, execution: &WorkExecution) -> Option<String> {
    if execution.kind != ExecutionKind::PrReview {
        return None;
    }
    match work_db.review_batch_member_for_execution(&execution.id) {
        Ok(member) => member.map(|member| member.role.stage().as_str().to_owned()),
        Err(error) => {
            tracing::warn!(execution_id = %execution.id, %error, "live worker metadata: could not read review batch member");
            None
        }
    }
}

/// Build the metadata for `execution` running on `host_id` (`"local"` or a
/// registered remote host id).
///
/// Looks up whether the work item has a source automation. Registration paths
/// that already computed that flag for the pool label should call
/// [`resolve_with_automation`] instead of paying for a second query.
#[cfg(test)]
pub(crate) fn resolve(work_db: &WorkDb, execution: &WorkExecution, host_id: &str) -> LiveWorkerMetadata {
    let has_source_automation = matches!(
        work_db.source_automation_id_for_work_item(&execution.work_item_id),
        Ok(Some(_))
    );
    resolve_with_automation(work_db, execution, host_id, has_source_automation)
}

/// Like [`resolve`], but takes the caller's already-computed
/// `has_source_automation` (the same flag fed to `attributed_pool_label`), so
/// the badge type and the stamped pool derive from one lookup.
pub(crate) fn resolve_with_automation(
    work_db: &WorkDb,
    execution: &WorkExecution,
    host_id: &str,
    has_source_automation: bool,
) -> LiveWorkerMetadata {
    let (project_id, project_name) = project_attribution(work_db, &execution.work_item_id);
    LiveWorkerMetadata {
        review_stage: review_stage(work_db, execution),
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
