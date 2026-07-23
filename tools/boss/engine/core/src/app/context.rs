//! `FrontendRequest::GetWorkerContext` handler — `boss context`, the
//! worker-tier "one call, one round trip" sanitized read bundle.
//!
//! Attribution is identical to [`super::proposals`]: the caller's own
//! execution and work item are derived from the socket peer, never from a
//! caller-supplied argument, and a caller that cannot be attributed is
//! refused the same way `SubmitProposal`/`ListProposals` are —
//! [`proposals::attribute_caller`] and [`proposals::send_rejection`] are
//! reused rather than re-implemented, so the two verbs cannot drift on what
//! "attribution failed" means.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary".

use super::proposals::{attribute_caller, send_rejection};
use super::*;

use boss_protocol::{
    AttentionGroup, DependencyDirection, ListDependenciesInput, ProposalErrorCode, ProposalSubmissionError,
    WorkItemDependencyDetail, WorkerContextBundle, WorkerContextSiblingTask,
};

pub(super) async fn handle_get_worker_context(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::GetWorkerContext { run_id } = req else {
        unreachable!()
    };

    let caller = match attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "get_worker_context rejected: attribution failed",
            );
            return send_rejection(&sink, &request_id, error);
        }
    };

    match build_bundle(&work_db, &caller.work_item_id) {
        Ok(bundle) => send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkerContextResult {
                bundle: Box::new(bundle),
            },
        ),
        Err(err) => {
            tracing::error!(
                work_item_id = %caller.work_item_id,
                ?err,
                "get_worker_context failed to read",
            );
            send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("failed to build context: {err}")),
            );
        }
    }
}

/// The caller's own task/chore row. Both `WorkItem::Task` and
/// `WorkItem::Chore` are plain [`Task`] rows — an execution never resolves
/// to a bare product or project, so those variants are treated as an
/// internal-consistency failure rather than given their own bundle shape.
fn own_task(work_db: &WorkDb, work_item_id: &str) -> Result<Task> {
    match work_db.get_work_item(work_item_id)? {
        WorkItem::Task(task) | WorkItem::Chore(task) => Ok(task),
        item @ (WorkItem::Product(_) | WorkItem::Project(_)) => {
            bail!("execution's work item {work_item_id} resolved to a non-task work item: {item:?}")
        }
    }
}

fn dependencies_for(work_db: &WorkDb, work_item_id: &str) -> Result<WorkItemDependencyDetail> {
    work_db.list_dependencies_detailed(ListDependenciesInput {
        work_item: work_item_id.to_owned(),
        direction: Some(DependencyDirection::Both),
    })
}

fn build_bundle(work_db: &WorkDb, work_item_id: &str) -> Result<WorkerContextBundle> {
    let task = own_task(work_db, work_item_id)?;
    let product = work_db
        .get_product(&task.product_id)?
        .with_context(|| format!("unknown product: {}", task.product_id))?;
    let project = task
        .project_id
        .as_deref()
        .map(|id| work_db.get_project(id))
        .transpose()?;

    let sibling_tasks = match &project {
        Some(project) => work_db
            .list_tasks(&task.product_id, Some(&project.id), None, false)?
            .into_iter()
            .filter(|sibling| sibling.id != task.id)
            .map(|sibling| {
                let dependencies = dependencies_for(work_db, &sibling.id)?;
                Ok(WorkerContextSiblingTask {
                    task: sibling,
                    dependencies,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let own_dependencies = dependencies_for(work_db, &task.id)?;

    let attention_groups: Vec<AttentionGroup> =
        work_db.list_attention_groups(&task.product_id, None, Some(&task.id), None, None)?;

    let proposals = work_db.list_worker_proposals_for_work_item(work_item_id, None, None)?;

    Ok(WorkerContextBundle {
        task,
        project,
        product,
        sibling_tasks,
        own_dependencies,
        attention_groups,
        proposals,
    })
}
