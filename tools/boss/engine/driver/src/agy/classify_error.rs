//! Antigravity/Google error vocabulary. Eligibility-service failure was
//! captured in the permission study's incidental observations. The CLI carries
//! QUOTA_EXHAUSTED and "You have exhausted your quota on this model."
//! (agy 1.5.9). Google RPC status spellings are accepted alongside prose.
//! These are error strings, not arbitrary pane history or successful output.

use crate::WorkerErrorClass;

pub(super) fn classify_agy_error(text: &str) -> WorkerErrorClass {
    let lower = text.to_ascii_lowercase();
    // Keep status-code boundaries: JSON punctuation and line wraps are OK,
    // but 5000, an identifier, or a path containing 500 must not imply HTTP 500.
    let has_status = |code: &str| {
        lower
            .split(|c: char| {
                c.is_ascii_whitespace() || matches!(c, ':' | ',' | '"' | '[' | ']' | '{' | '}' | '(' | ')')
            })
            .any(|word| word == code)
    };
    let has = |markers: &[&str]| markers.iter().any(|marker| lower.contains(marker));

    // Quota takes precedence over retryable wrappers (including 429/500).
    // RESOURCE_EXHAUSTED can also mean a short rate limit, but absent a
    // distinct signal it must not repeatedly dispatch into exhausted quota.
    if has(&[
        "quota_exhausted",
        "quota exhausted",
        "quota_exceeded",
        "quota exceeded",
        "quota has been exceeded",
        "quota has been exhausted",
        "exhausted your quota",
        "subscription limit for this account is used up",
        "resource_exhausted",
        "resourceexhausted",
    ]) {
        return WorkerErrorClass::QuotaExhausted;
    }
    if has(&["eligibility check failed"]) && has_status("500") {
        return WorkerErrorClass::EligibilityServiceFailure;
    }
    if has(&[
        "unauthenticated",
        "authentication required",
        "authentication failed",
        "invalid_grant",
        "permission_denied",
        "permissiondenied",
        "invalid_argument",
        "invalidargument",
        "invalid api key",
        "api key not valid",
        "not eligible",
        "prompt is too long",
        "exceeds the maximum number of tokens",
    ]) || ["400", "401", "403"].iter().any(|code| has_status(code))
    {
        return WorkerErrorClass::Permanent;
    }
    if has(&[
        "too many requests",
        "rate limit",
        "rate_limit",
        "internal server error",
        "service unavailable",
        "deadline_exceeded",
        "deadlineexceeded",
        "deadline exceeded",
        "connection reset",
        "econnreset",
        "connection refused",
        "econnrefused",
        "unexpected eof",
        "timed out",
    ]) || ["429", "500", "502", "503", "504"].iter().any(|code| has_status(code))
    {
        return WorkerErrorClass::Transient;
    }
    WorkerErrorClass::Indeterminate
}

#[cfg(test)]
mod tests {
    use crate::{AgentDriver, WorkerErrorClass, agy::AgyDriver};

    #[test]
    fn quota_is_distinct_and_wins_over_retryable_wrappers() {
        for error in [
            "You have exhausted your quota on this model.",
            "Quota exhausted",
            "The subscription limit for this account is used up; it resets in 5h",
            "QUOTA_EXHAUSTED: retry after reset",
            "HTTP 429: RESOURCE_EXHAUSTED",
            "rpc error: code = ResourceExhausted desc = Quota exceeded",
            "Your image generation quota has been exceeded. Please try again later.",
            "Eligibility check failed: 500 Internal Server Error: QUOTA_EXHAUSTED",
        ] {
            assert_eq!(
                AgyDriver::default().classify_error(error),
                WorkerErrorClass::QuotaExhausted,
                "{error}"
            );
        }
    }

    #[test]
    fn eligibility_service_failure_is_distinct_from_denied_entitlement() {
        let driver = AgyDriver::default();
        for error in [
            "Eligibility check failed: failed to fetch user info: 500 Internal Server Error",
            "ELIGIBILITY CHECK FAILED: failed to fetch user info:\n500",
        ] {
            assert_eq!(
                driver.classify_error(error),
                WorkerErrorClass::EligibilityServiceFailure
            );
        }
        assert_eq!(
            driver.classify_error("Eligibility check failed: 403 not eligible"),
            WorkerErrorClass::Permanent
        );
        assert_eq!(
            driver.classify_error("Eligibility check failed"),
            WorkerErrorClass::Indeterminate
        );
    }

    #[test]
    fn google_auth_and_request_failures_are_permanent() {
        for error in [
            "UNAUTHENTICATED: invalid credentials",
            "Authentication required. Please visit the URL to log in:",
            "invalid_grant: Token has been expired or revoked",
            "PERMISSION_DENIED: account not eligible",
            "rpc error: code = InvalidArgument",
            "HTTP 401 Unauthorized",
            "API key not valid. Please pass a valid API key.",
            "input token count exceeds the maximum number of tokens allowed",
            "authentication failed after request timed out",
        ] {
            assert_eq!(
                AgyDriver::default().classify_error(error),
                WorkerErrorClass::Permanent,
                "{error}"
            );
        }
    }

    #[test]
    fn transient_and_unknown_errors_keep_bounded_recovery() {
        let driver = AgyDriver::default();
        for error in [
            "HTTP 429 Too Many Requests",
            "503 Service Unavailable",
            "500",
            r#"{"code":503,"status":"UNAVAILABLE"}"#,
            "model returned an error (code 429)",
            "rpc error: code = DeadlineExceeded",
            "connection reset by peer",
            "request timed out",
        ] {
            assert_eq!(driver.classify_error(error), WorkerErrorClass::Transient, "{error}");
        }
        for error in [
            "",
            "unknown vendor error",
            "quota available",
            "5000",
            "file /tmp/500",
            "Eligibility check failed: code 1500",
        ] {
            assert_eq!(driver.classify_error(error), WorkerErrorClass::Indeterminate, "{error}");
        }
    }
}
