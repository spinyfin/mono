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
