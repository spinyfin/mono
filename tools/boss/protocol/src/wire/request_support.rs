//! Request-side list predicates, preservation result types, and serde
//! defaults for [`super::FrontendRequest`]. Re-exported by the wire module.

use serde::{Deserialize, Serialize};

/// Optional task-list predicates evaluated before transferring rows to clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskListQuery {
    /// Canonical task id or decimal short id, scoped by the request's product.
    pub selector: Option<String>,
    pub match_term: Option<String>,
    #[serde(default)]
    pub kinds: Vec<crate::TaskKind>,
    pub created_since: Option<i64>,
}

pub(super) fn default_pool_size() -> i64 {
    1
}

/// Serde default for [`super::FrontendRequest::ProbeRun`]'s `interrupt` field.
///
/// `true`, so a payload written before the field existed gets the
/// interrupting behaviour rather than the old wait-for-a-boundary one. That
/// direction is deliberate: the boundary-only path is the one that silently
/// lost probes, so an unspecified request must not opt into it by accident.
pub(super) fn default_probe_interrupt() -> bool {
    true
}

/// One task preserved (not deleted) by [`super::FrontendRequest::UnpopulateProject`]
/// because it already had an execution — i.e. it was released and
/// dispatched, so undoing the populate would destroy in-flight work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpopulatePreservedTask {
    pub id: String,
    pub name: String,
}
