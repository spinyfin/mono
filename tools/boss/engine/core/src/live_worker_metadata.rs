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
pub fn execution_binding(work_db: &WorkDb, execution: &WorkExecution) -> boss_protocol::WorkItemBinding {
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

pub(crate) use boss_engine_completion::driver_transcript::guide_launch_config;

/// Shared identity for both restart registration paths, with the execution
/// stamp taking precedence over the pre-spawn guide configuration.
pub fn adopted_identity(
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
pub fn resolve_with_automation(
    work_db: &WorkDb,
    execution: &WorkExecution,
    host_id: &str,
    has_source_automation: bool,
) -> LiveWorkerMetadata {
    let work_item = owning_work_item(work_db, execution);
    let header = work_item_header(work_db, execution, work_item.as_ref());
    LiveWorkerMetadata {
        review_stage: review_stage(work_db, execution),
        agent_type: Some(
            AgentType::for_execution(&execution.kind, has_source_automation)
                .as_str()
                .to_owned(),
        ),
        project_id: header.project_id,
        project_name: header.project_name,
        host_id: Some(host_id.to_owned()),
        started_at: execution
            .started_epoch()
            .map(boss_engine_utils::iso8601::format_epoch_iso8601),
        product_id: header.product_id,
        product_name: header.product_name,
        work_item_short_id: header.short_id,
        owning_work_item_id: header.owning_work_item_id,
        owning_work_item_title: header.owning_work_item_title,
        pr_url: header.pr_url,
    }
}

/// The work item an execution's pane header describes. A review-guide run
/// binds `work_item_id` to an immutable comparison, not a `tasks` row; the
/// item it explains is the comparison series' root task, which is also the
/// chain root when the PR belongs to a revision chain. `None` when no row
/// resolves (an automation id, an external-PR guide whose series has no
/// owning task, a deleted item).
pub(crate) fn owning_work_item(work_db: &WorkDb, execution: &WorkExecution) -> Option<WorkItem> {
    let owning_id = work_db.owning_work_item_id(&execution.work_item_id);
    work_db.get_work_item(&owning_id).ok()
}

/// Everything the Agents pane header's second line is built from, resolved
/// once so live panes and finished-pane labels read identically.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct WorkItemHeader {
    pub owning_work_item_id: Option<String>,
    pub owning_work_item_title: Option<String>,
    pub product_id: Option<String>,
    pub product_name: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub short_id: Option<i64>,
    pub pr_url: Option<String>,
}

/// Product, project, short id and bound PR URL for the pane header, for the
/// owning `work_item` of `execution` (see [`owning_work_item`]).
///
/// The PR is, in order: for a review-guide run, the comparison's canonical PR
/// (so a manual guide on an external PR still names its PR); the work-item
/// row's `pr_url`; the execution's `pr_url`. Revision tasks are inserted with
/// a NULL `pr_url`; their bound PR lives on [`WorkExecution::pr_url`] (stamped
/// from the chain root at dispatch).
pub(crate) fn work_item_header(
    work_db: &WorkDb,
    execution: &WorkExecution,
    work_item: Option<&WorkItem>,
) -> WorkItemHeader {
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
    let product_name = product_id.as_deref().and_then(|id| match work_db.get_product(id) {
        Ok(product) => product.map(|product| product.name),
        Err(error) => {
            tracing::warn!(product_id = id, %error, "live worker metadata: could not read product name");
            None
        }
    });
    let guide_pr_url = (execution.kind == ExecutionKind::PrReviewGuide)
        .then(|| {
            work_db
                .get_pr_review_guide_comparison_by_id(&execution.work_item_id)
                .ok()
                .flatten()
                .and_then(|capture| non_empty_pr_url(Some(&capture.packet.canonical_pr_url)))
        })
        .flatten();
    let pr_url = guide_pr_url
        .or(task_pr_url)
        .or_else(|| non_empty_pr_url(execution.pr_url.as_deref()));
    let (project_id, project_name) = project_attribution(work_db, work_item);
    WorkItemHeader::builder()
        .maybe_owning_work_item_id(work_item.map(|_| work_db.owning_work_item_id(&execution.work_item_id)))
        .maybe_owning_work_item_title(work_item.map(|item| crate::runner::work_item_name(item).to_owned()))
        .maybe_product_id(product_id)
        .maybe_product_name(product_name)
        .maybe_project_id(project_id)
        .maybe_project_name(project_name)
        .maybe_short_id(short_id)
        .maybe_pr_url(pr_url)
        .build()
}

fn non_empty_pr_url(url: Option<&str>) -> Option<String> {
    url.filter(|url| !url.trim().is_empty()).map(str::to_owned)
}

#[cfg(test)]
#[path = "live_worker_metadata_tests.rs"]
mod tests;
