//! Shared retrying HTTP GET with exponential backoff.
//!
//! Callers supply policy (attempt count, delays, whether a 404 is transient,
//! an optional overall time budget) and handle the returned bytes themselves.
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::StatusCode;
use tracing::info;

/// Retry policy for [`get_bytes_with_retry`].
pub struct GetRetryPolicy<'a> {
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles on each later attempt.
    pub base_delay: Duration,
    /// When set, a 404 is treated as transient (e.g. a just-published file)
    /// and retried with this base delay. When unset, a 404 fails at once.
    pub not_found_base_delay: Option<Duration>,
    /// When set, returns the time left in an overall budget. Each request is
    /// bounded by it, and no retry is started that would outlast it.
    pub remaining: Option<&'a dyn Fn() -> Duration>,
}

pub fn is_retryable_http_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_delay(policy: &GetRetryPolicy<'_>, attempt: u32, status: StatusCode) -> Duration {
    let base = match policy.not_found_base_delay {
        Some(not_found) if status == StatusCode::NOT_FOUND => not_found,
        _ => policy.base_delay,
    };
    base.saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)))
}

/// GETs `url`, retrying transient failures (network errors, body read errors,
/// 429 and 5xx, and 404 when the policy says so). `what` names the resource
/// in error messages.
pub async fn get_bytes_with_retry(
    client: &reqwest::Client,
    url: &reqwest::Url,
    what: &str,
    policy: &GetRetryPolicy<'_>,
) -> Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 1..=policy.max_attempts {
        info!(url = %url, attempt, max_attempts = policy.max_attempts, "fetching {what}");
        let mut request = client.get(url.clone());
        if let Some(remaining) = policy.remaining {
            request = request.timeout(remaining().max(Duration::from_millis(1)));
        }
        // The status used to pick the next delay.
        let mut delay_status = StatusCode::REQUEST_TIMEOUT;
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                delay_status = status;
                if status == StatusCode::NOT_FOUND && policy.not_found_base_delay.is_some() {
                    last_error = Some(format!("{what} returned 404 Not Found"));
                } else if !status.is_success() {
                    let message = format!("{what} returned {status}");
                    if !is_retryable_http_status(status) {
                        bail!("{message}");
                    }
                    last_error = Some(message);
                } else {
                    match response.bytes().await {
                        Ok(bytes) => return Ok(bytes.to_vec()),
                        Err(error) => {
                            last_error = Some(format!("failed to read {what} response body: {error}"));
                        }
                    }
                }
            }
            Err(error) => last_error = Some(format!("failed to retrieve {what}: {error}")),
        }
        if attempt == policy.max_attempts {
            break;
        }
        let delay = retry_delay(policy, attempt, delay_status);
        if policy.remaining.is_some_and(|remaining| remaining() <= delay) {
            break;
        }
        tokio::time::sleep(delay).await;
    }
    let message = last_error.unwrap_or_else(|| format!("failed to retrieve {what}"));
    bail!("{message} after {} attempts", policy.max_attempts)
}
