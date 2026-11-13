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
                %err,
                "get_worker_context failed to read",
            );
            send_rejection(&sink, &request_id, err.as_proposal_error("failed to build context"));
        }
    }
}

/// A worker-attributed read failed. Transient storage errors retry; missing
/// or inconsistent rows are a durable denial — a read has no side effect to
/// preserve, so answering with a data-free rejection is always safe.
#[derive(Debug)]
pub(super) enum AttributedReadError {
    Denied(String),
    Transient(anyhow::Error),
}

impl AttributedReadError {
    pub(super) fn from_storage(err: anyhow::Error) -> Self {
        if err.chain().any(|cause| cause.is::<rusqlite::Error>()) {
            Self::Transient(err)
        } else {
            Self::Denied(err.to_string())
        }
    }

    pub(super) fn as_proposal_error(&self, prefix: &str) -> ProposalSubmissionError {
        match self {
            Self::Denied(message) => {
                ProposalSubmissionError::new(ProposalErrorCode::NotAccepted, format!("{prefix}: {message}"))
            }
            Self::Transient(err) => {
                ProposalSubmissionError::new(ProposalErrorCode::Internal, format!("{prefix}: {err}"))
            }
        }
    }
}

impl std::fmt::Display for AttributedReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(message) => f.write_str(message),
            Self::Transient(err) => write!(f, "{err:#}"),
        }
    }
}

fn storage<T>(result: Result<T>) -> std::result::Result<T, AttributedReadError> {
    result.map_err(AttributedReadError::from_storage)
}

/// The caller's own task/chore row. Both `WorkItem::Task` and
/// `WorkItem::Chore` are plain [`Task`] rows — an execution never resolves
/// to a bare product or project, so those variants are treated as an
/// internal-consistency failure rather than given their own bundle shape.
///
/// `pub(super)`: reused by `app::pr_status`, which is attributed and scoped
/// identically (own task lookup off the caller's attributed work item).
pub(super) fn own_task(work_db: &WorkDb, work_item_id: &str) -> std::result::Result<Task, AttributedReadError> {
    match work_db.get_work_item(work_item_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => Ok(task),
        Ok(_) => Err(AttributedReadError::Denied(format!(
            "execution's work item {work_item_id} is not a task"
        ))),
        Err(err) => Err(AttributedReadError::from_storage(err)),
    }
}

fn dependencies_for(work_db: &WorkDb, work_item_id: &str) -> Result<WorkItemDependencyDetail> {
    work_db.list_dependencies_detailed(ListDependenciesInput {
        work_item: work_item_id.to_owned(),
        direction: Some(DependencyDirection::Both),
    })
}

pub(super) fn build_bundle(
    work_db: &WorkDb,
    work_item_id: &str,
) -> std::result::Result<WorkerContextBundle, AttributedReadError> {
    let task = own_task(work_db, work_item_id)?;
    let product = match work_db.get_product(&task.product_id) {
        Ok(Some(product)) => product,
        Ok(None) => {
            return Err(AttributedReadError::Denied(format!(
                "unknown product: {}",
                task.product_id
            )));
        }
        Err(err) => return Err(AttributedReadError::from_storage(err)),
    };
    let project = match task.project_id.as_deref() {
        Some(id) => Some(storage(work_db.get_project(id))?),
        None => None,
    };

    let sibling_tasks = match &project {
        Some(project) => work_db
            .list_tasks(&task.product_id, Some(&project.id), None, false)
            .map_err(AttributedReadError::from_storage)?
            .into_iter()
            .filter(|sibling| sibling.id != task.id)
            .map(|sibling| {
                let dependencies = storage(dependencies_for(work_db, &sibling.id))?;
                Ok(WorkerContextSiblingTask {
                    task: sibling,
                    dependencies,
                })
            })
            .collect::<std::result::Result<Vec<_>, AttributedReadError>>()?,
        None => Vec::new(),
    };

    let own_dependencies = storage(dependencies_for(work_db, &task.id))?;

    let attention_groups: Vec<AttentionGroup> =
        storage(work_db.list_attention_groups(&task.product_id, None, Some(&task.id), None, None))?;

    let proposals = storage(work_db.list_worker_proposals_for_work_item(work_item_id, None, None))?;

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
