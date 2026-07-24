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
    /// TLS, DNS, connection reset). Transient — worth retrying.
    #[error("transport error: {0}")]
    Transport(String),
    /// A 2xx response body didn't decode into the expected shape. Never
    /// transient — the same request would just decode the same way again —
    /// so callers must not retry this the way they would [`Self::Transport`].
    #[error("failed to decode trunk response: {0}")]
    Decode(String),
    /// A 2xx response whose body isn't JSON at all — as opposed to
    /// [`Self::Decode`], where the body parses as JSON but doesn't match
    /// the expected shape. This is the case that used to surface as an
    /// opaque `Decode("expected value at line 1 column 1")` with no way to
    /// tell an HTML/plaintext page apart from a genuine schema drift.
    /// Carries the status, content type, and a truncated body snippet so
    /// the operator has something to compare against Trunk's actual
    /// response instead of a bare serde parse error. Never retried, for
    /// the same reason as `Decode`.
    #[error(
        "trunk returned a non-JSON response where JSON was expected ({0}); this is a real 2xx response, not an \
         auth/token problem — run `boss engine trunk status` and compare against what Trunk actually sent"
    )]
    NonJsonResponse(String),
    /// Trunk answered with a redirect (3xx) instead of a direct response.
    /// The HTTP client no longer follows redirects (see
    /// `boss_http_retry::http_client`), so a redirect now surfaces here
    /// explicitly instead of being silently followed to whatever page it
    /// points at and having that page's response mistaken for Trunk's.
    /// Never retried: a redirect on what should be a direct API POST is a
    /// transport-level anomaly worth surfacing loudly, not a transient blip.
    #[error(
        "trunk redirected the request instead of answering directly ({0}); run `boss engine trunk status` to \
         confirm the queue integration is otherwise healthy"
    )]
    Redirected(String),
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
        assert_eq!(
            TrunkError::Decode("unexpected eof".to_owned()).to_string(),
            "failed to decode trunk response: unexpected eof"
        );
        let non_json = TrunkError::NonJsonResponse("status 200, content-type \"text/html\": <html>...".to_owned());
        assert!(non_json.to_string().starts_with(
            "trunk returned a non-JSON response where JSON was expected (status 200, content-type \"text/html\""
        ));
        assert!(non_json.to_string().contains("boss engine trunk status"));

        let redirected = TrunkError::Redirected("status 302: <html>...".to_owned());
        assert!(
            redirected
                .to_string()
                .starts_with("trunk redirected the request instead of answering directly (status 302")
        );
        assert!(redirected.to_string().contains("boss engine trunk status"));
    }
}
