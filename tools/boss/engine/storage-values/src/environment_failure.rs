//! Host-environment failures: the machine cannot reach the model API.
//!
//! Distinct from a transient *service* error (overloaded, rate-limited, 5xx)
//! and from a failure of the item the agent was looking at. A laptop that
//! sleeps or drops DNS produces these strings; they say nothing about the
//! work item, so progression retries them on the launch backoff instead of
//! counting them toward the infrastructure-failure cap.
//!
//! Drivers that expose a structured class still render a text body onto the
//! execution (`last_error` / `work_runs.error_text`). This module is the one
//! classifier for that body: Node errnos (`ENOTFOUND`, `ECONNREFUSED`, …) and
//! the equivalent prose every driver has been observed to print. `Transient`
//! from a driver is broader (it includes 5xx and rate limits) and is not
//! used here.

/// Substrings matched against a lowercased error body. Keep these specific
/// to "cannot reach the network / DNS / API host". Do not add `api_error`,
/// `timeout`, `overloaded`, `rate_limit`, or HTTP 5xx — those remain item
/// infrastructure failures.
const MARKERS: &[&str] = &[
    "enotfound",
    "getaddrinfo",
    "econnrefused",
    "connection refused",
    "connectionrefused",
    "enetunreach",
    "network is unreachable",
    "ehostunreach",
    "no route to host",
    "name resolution",
    "can't reach the api",
    "cannot reach the api",
    "cant reach the api",
    "unable to connect",
];

/// True when `text` is a host-unreachable / DNS failure rather than a
/// failure of the work item.
pub fn is_environment_unreachable(text: &str) -> bool {
    let haystack = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| haystack.contains(marker))
}

/// SQL boolean over a text expression (already a column or `COALESCE(...)`)
/// that is true when that expression matches [`is_environment_unreachable`].
/// Generated from the same marker list the Rust classifier uses.
pub fn environment_unreachable_sql(expr: &str) -> String {
    MARKERS
        .iter()
        .map(|marker| {
            let escaped = marker.replace('\'', "''");
            format!("LOWER({expr}) LIKE '%{escaped}%'")
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_driver_unreachable_strings_match() {
        for text in [
            "api_error (auto-resume attempt 1/3): API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)",
            "API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)",
            "getaddrinfo ENOTFOUND api.anthropic.com",
            "getaddrinfo ENOTFOUND api.x.ai",
            "connect ECONNREFUSED 127.0.0.1:443",
            "Error: connect ECONNREFUSED",
            "API Error: Unable to connect to API (ConnectionRefused)",
            "Unable to connect to API (ConnectionRefused)",
            "Error: network is unreachable",
            "connect ENETUNREACH",
            "No route to host",
            "Temporary failure in name resolution",
        ] {
            assert!(
                is_environment_unreachable(text),
                "expected environment-unreachable for: {text}"
            );
        }
    }

    #[test]
    fn genuine_item_failures_do_not_match() {
        for text in [
            "the pane died",
            "worker lost",
            "progression-run-timeout: elapsed 1800s (limit 1800s)",
            "api_error (auto-resume attempt 3/3): API Error: overloaded_error",
            "Error code: 503 - service unavailable",
            "rate_limit_error: rate limit exceeded",
            "authentication_error: invalid x-api-key",
            "no disposition",
            "driver error",
            "refusing to complete codex spawn: no driver hook or session event arrived within 120s",
        ] {
            assert!(
                !is_environment_unreachable(text),
                "did not expect environment-unreachable for: {text}"
            );
        }
    }

    #[test]
    fn sql_fragment_uses_the_same_markers() {
        let sql = environment_unreachable_sql("err");
        assert!(sql.contains("enotfound"));
        assert!(sql.contains("can''t reach the api"), "{sql}");
        assert!(!sql.contains("overloaded"));
        assert!(!sql.contains("timeout"));
    }
}
