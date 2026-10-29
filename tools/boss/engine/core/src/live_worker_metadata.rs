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
fn project_attribution(work_db: &WorkDb, work_item: Option<&WorkItem>) -> (Option<String>, Option<String>) {
    let project_id = match work_item {
        Some(WorkItem::Project(project)) => return (Some(project.id.clone()), Some(project.name.clone())),
        Some(WorkItem::Task(task) | WorkItem::Chore(task)) => task.project_id.clone(),
        Some(WorkItem::Product(_)) | None => None,
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
    let work_item = work_db.get_work_item(&execution.work_item_id).ok();
    let (project_id, project_name) = project_attribution(work_db, work_item.as_ref());
    let (product_id, work_item_short_id, pr_url) = work_item_header_fields(work_item.as_ref(), execution);
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
        product_id,
        work_item_short_id,
        pr_url,
    }
}

/// Product id, short id, and bound PR URL for the Agents pane header.
/// `None` triple (aside from a possible execution-level PR URL) when the
/// execution is not pointed at a work-item row (automation id,
/// review-guide comparison, missing row).
///
/// Prefers the work-item row's `pr_url`. Revision tasks are inserted with
/// that column NULL; their bound PR lives on [`WorkExecution::pr_url`]
/// (stamped from the chain root at dispatch). Fall back to a non-empty
/// execution URL so revision workers show the PR they are updating.
fn work_item_header_fields(
    work_item: Option<&WorkItem>,
    execution: &WorkExecution,
) -> (Option<String>, Option<i64>, Option<String>) {
    let (product_id, short_id, task_pr_url) = match work_item {
        Some(WorkItem::Task(task) | WorkItem::Chore(task)) => (
            Some(task.product_id.clone()),
            task.short_id,
            non_empty_pr_url(task.pr_url.as_deref()),
        ),
        Some(WorkItem::Project(project)) => (Some(project.product_id.clone()), project.short_id, None),
        Some(WorkItem::Product(product)) => (Some(product.id.clone()), None, None),
        None => (None, None, None),
    };
    let pr_url = task_pr_url.or_else(|| non_empty_pr_url(execution.pr_url.as_deref()));
    (product_id, short_id, pr_url)
}

fn non_empty_pr_url(url: Option<&str>) -> Option<String> {
    url.filter(|url| !url.trim().is_empty()).map(str::to_owned)
}

#[cfg(test)]
#[path = "live_worker_metadata_tests.rs"]
mod tests;
