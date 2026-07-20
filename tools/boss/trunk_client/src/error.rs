//! Structured error taxonomy. Callers (the merge verb, the queue poller)
//! branch on these variants, so they stay coarse and stable rather than
//! exposing every HTTP status Trunk might return.

/// A failure talking to the Trunk queue API.
#[derive(Debug, thiserror::Error)]
pub enum TrunkError {
    /// No token available, or Trunk rejected it (401/403). Callers should
    /// surface this as a loud "token missing/expired" condition — never
    /// retry it automatically.
    #[error("trunk authentication failed: {0}")]
    Auth(String),
    /// The PR isn't in the queue (404 from `getSubmittedPullRequest` and
    /// friends).
    #[error("not found in trunk queue: {0}")]
    NotFound(String),
    /// Trunk's API is unavailable: exhausted retries against a 5xx, a 429
    /// rate limit, or any other unclassified non-2xx response.
    #[error("trunk queue unavailable: {0}")]
    QueueUnavailable(String),
    /// The HTTP client failed before/while getting a response (timeout,
    /// TLS, DNS, connection reset), or a 2xx response body didn't decode
    /// into the expected shape.
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_include_the_detail() {
        assert_eq!(
            TrunkError::Auth("no token configured".to_owned()).to_string(),
            "trunk authentication failed: no token configured"
        );
        assert_eq!(
            TrunkError::NotFound("pr not queued".to_owned()).to_string(),
            "not found in trunk queue: pr not queued"
        );
        assert_eq!(
            TrunkError::QueueUnavailable("trunk returned 503: overloaded".to_owned()).to_string(),
            "trunk queue unavailable: trunk returned 503: overloaded"
        );
        assert_eq!(
            TrunkError::Transport("connection reset".to_owned()).to_string(),
            "transport error: connection reset"
        );
    }
}
