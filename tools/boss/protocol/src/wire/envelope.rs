//! Frontend wire envelopes (`FrontendRequestEnvelope`, `FrontendEventEnvelope`)
//! and request-helper DTOs (`TaskListQuery`, `UnpopulatePreservedTask`),
//! re-exported from `wire`.

use serde::{Deserialize, Serialize};

use super::{FrontendEvent, FrontendRequest};

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

/// One task preserved (not deleted) by [`FrontendRequest::UnpopulateProject`]
/// because it already had an execution — i.e. it was released and
/// dispatched, so undoing the populate would destroy in-flight work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpopulatePreservedTask {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRequestEnvelope {
    pub request_id: String,
    pub payload: FrontendRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub payload: FrontendEvent,
}

impl FrontendEventEnvelope {
    pub fn response(request_id: impl Into<String>, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: None,
            payload,
        }
    }

    pub fn push(payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: None,
            payload,
        }
    }

    pub fn response_with_revision(request_id: impl Into<String>, revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: Some(revision),
            payload,
        }
    }

    pub fn push_with_revision(revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: Some(revision),
            payload,
        }
    }
}
