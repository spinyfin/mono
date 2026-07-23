//! Types for `boss context` — the worker-tier "one call, one round trip"
//! sanitized read bundle.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary".

use serde::{Deserialize, Serialize};

use crate::{AttentionGroup, Product, Project, Task, WorkItemDependencyDetail, WorkerProposal};

/// One other task in the caller's project, with its own dependency edges
/// already joined in so the worker sees the project's dependency graph
/// without a follow-up round trip per sibling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerContextSiblingTask {
    pub task: Task,
    pub dependencies: WorkItemDependencyDetail,
}

/// The sanitized one-call bundle `boss context` returns: the caller's own
/// task/project/product, its project's sibling tasks (each with status, PR
/// URL, and dependency edges), the edges touching the caller's own task,
/// open attention groups on its work item, and its own work item's
/// proposals across executions with their dispositions.
///
/// Resolved entirely from the caller's verified identity (socket peer →
/// attributed execution → work item) — there is no work-item argument to
/// widen or misdirect. See [`crate::FrontendRequest::GetWorkerContext`].
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerContextBundle {
    /// The caller's own task or chore row. `WorkItem::Task` and
    /// `WorkItem::Chore` are both plain [`Task`] rows, so this doubles as
    /// "its chore's current state" per the design when the caller's work
    /// item is a chore.
    pub task: Task,
    /// `None` when [`Self::task`] is a chore — chores have no parent
    /// project.
    pub project: Option<Project>,
    pub product: Product,
    /// Other tasks in the same project, excluding the caller's own task.
    /// Always empty when [`Self::project`] is `None`.
    pub sibling_tasks: Vec<WorkerContextSiblingTask>,
    /// Dependency edges touching the caller's own task.
    pub own_dependencies: WorkItemDependencyDetail,
    /// Open (`open` + `partially_answered`) attention groups on the
    /// caller's own work item.
    pub attention_groups: Vec<AttentionGroup>,
    /// Every proposal filed against the caller's own work item, across all
    /// its executions, with dispositions — same scope as
    /// [`crate::FrontendRequest::ListProposals`].
    pub proposals: Vec<WorkerProposal>,
}
