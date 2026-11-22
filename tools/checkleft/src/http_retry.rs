//! Shared retrying HTTP client helpers.
//!
//! One attempt loop, budget, backoff, jitter, `Retry-After` parsing, and
//! typed failure classification. [`get_bytes_with_retry`] is the generic GET
//! used by config and Node downloads; GitHub REST calls are a thin adapter
//! on the same loop.

use std::collections::hash_map::RandomState;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use reqwest::StatusCode;
use tracing::info;

/// Retry policy for [`get_bytes_with_retry`].
pub struct GetRetryPolicy<'a> {
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles on each later attempt.
    pub base_delay: Duration,
    /// Cap on one backoff sleep, including a server-supplied `Retry-After`.
    /// Production callers use [`GET_MAX_RETRY_DELAY`].
    pub max_delay: Duration,
    /// When set, a 404 is treated as transient (e.g. a just-published file)
    /// and retried with this base delay. When unset, a 404 fails at once.
    pub not_found_base_delay: Option<Duration>,
    /// When set, returns the time left in an overall budget. Each request is
    /// bounded by it, and no retry is started that would outlast it.
    pub remaining: Option<&'a dyn Fn() -> Duration>,
}

/// Cap on one backoff sleep for the generic GET, including a server-supplied
/// `Retry-After`, so a hostile or misconfigured server cannot stall a fetch
/// for hours when no overall budget is set.
pub(crate) const GET_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Settings for [`retry_loop`]. GitHub REST and generic GET both map into this.
pub(crate) struct RetrySettings<'a> {
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Cap on a single backoff sleep, including a server-supplied `Retry-After`.
    pub max_delay: Duration,
    pub jitter: bool,
    pub not_found_base_delay: Option<Duration>,
    pub remaining: Option<&'a dyn Fn() -> Duration>,
    /// Per-attempt request timeout, further clamped by [`Self::remaining`].
    pub request_timeout: Option<Duration>,
}

impl<'a> RetrySettings<'a> {
    pub(crate) fn from_get(policy: &'a GetRetryPolicy<'a>) -> Self {
        Self {
            max_attempts: policy.max_attempts,
            base_delay: policy.base_delay,
            max_delay: policy.max_delay,
            jitter: false,
            not_found_base_delay: policy.not_found_base_delay,
            remaining: policy.remaining,
            request_timeout: None,
        }
    }
}

pub fn is_retryable_http_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Classified failure of one HTTP attempt, before the retry loop fills in the
/// attempt count.
#[derive(Debug)]
pub(crate) struct AttemptFailure {
    pub kind: FailureKind,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    pub cause: String,
}

#[derive(Debug)]
pub(crate) enum FailureKind {
    Timeout,
    Transport(reqwest::Error),
    Body(reqwest::Error),
    Http { status: StatusCode, body: Vec<u8> },
}

impl AttemptFailure {
    fn timed_out(&self) -> bool {
        matches!(self.kind, FailureKind::Timeout)
    }

    fn not_found(&self) -> bool {
        matches!(self.kind, FailureKind::Http { status, .. } if status == StatusCode::NOT_FOUND)
    }
}

/// Final classified result after retries have been applied.
#[derive(Debug)]
pub(crate) enum SendOutcome {
    Success(Vec<u8>),
    Timeout {
        attempts: u32,
    },
    Transport {
        error: reqwest::Error,
        attempts: u32,
    },
    Body {
        error: reqwest::Error,
        attempts: u32,
    },
    Http {
        status: StatusCode,
        body: Vec<u8>,
        attempts: u32,
    },
}

pub(crate) struct RetryNotice<'a> {
    pub attempt: u32,
    pub max_attempts: u32,
    pub cause: &'a str,
    pub delay: Duration,
}

pub(crate) fn failure_from_send(error: reqwest::Error) -> AttemptFailure {
    if error.is_timeout() {
        return AttemptFailure {
            kind: FailureKind::Timeout,
            retryable: true,
            retry_after: None,
            cause: "timed out".to_owned(),
        };
    }
    // Resets and early closes on pooled connections are not `is_connect()`
    // but are transient; only clearly permanent request errors are terminal.
    let retryable = !(error.is_builder() || error.is_redirect());
    AttemptFailure {
        cause: format!("transport error: {error}"),
        kind: FailureKind::Transport(error),
        retryable,
        retry_after: None,
    }
}

pub(crate) fn failure_from_body(error: reqwest::Error) -> AttemptFailure {
    if error.is_timeout() {
        return AttemptFailure {
            kind: FailureKind::Timeout,
            retryable: true,
            retry_after: None,
            cause: "timed out reading the response body".to_owned(),
        };
    }
    let retryable = error.is_body() || error.is_connect();
    AttemptFailure {
        cause: format!("response body error: {error}"),
        kind: FailureKind::Body(error),
        retryable,
        retry_after: None,
    }
}

pub(crate) fn failure_from_http(
    status: StatusCode,
    body: Vec<u8>,
    retry_after: Option<Duration>,
    retry_not_found: bool,
) -> AttemptFailure {
    let retryable = is_retryable_http_status(status) || (status == StatusCode::NOT_FOUND && retry_not_found);
    let retry_after = retry_after.filter(|_| is_retryable_http_status(status));
    AttemptFailure {
        cause: format!("HTTP {status}"),
        kind: FailureKind::Http { status, body },
        retryable,
        retry_after,
    }
}

/// Integer-seconds or IMF-fixdate `Retry-After`. A past HTTP-date is a zero
/// delay (retry immediately). An unparseable value is ignored.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = parse_imf_fixdate(value)?;
    Some(when.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO))
}

/// +/- 25% jitter from OS-seeded `RandomState`. A zero delay stays zero.
pub(crate) fn jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let unit = (RandomState::new().build_hasher().finish() as f64) / (u64::MAX as f64);
    delay.mul_f64(0.75 + 0.5 * unit)
}

pub(crate) fn retry_delay(settings: &RetrySettings<'_>, attempt: u32, failure: &AttemptFailure) -> Duration {
    if let Some(retry_after) = failure.retry_after {
        return retry_after.min(settings.max_delay);
    }
    let base = if failure.not_found() {
        settings.not_found_base_delay.unwrap_or(settings.base_delay)
    } else {
        settings.base_delay
    };
    let exponential = base.saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)));
    let capped = exponential.min(settings.max_delay);
    if settings.jitter { jitter(capped) } else { capped }
}

/// Run `send_once` up to `max_attempts`, honouring budget, `Retry-After`, and
/// jittered exponential backoff. If no attempt succeeded and at least one
/// timed out, the outcome is [`SendOutcome::Timeout`] even when a later
/// attempt failed for a different retryable reason.
pub(crate) async fn retry_loop<F, Fut>(
    settings: &RetrySettings<'_>,
    send_once: F,
    on_retry: impl FnMut(RetryNotice<'_>),
) -> SendOutcome
where
    F: FnMut(u32, Duration) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, AttemptFailure>>,
{
    retry_loop_detailed(settings, send_once, on_retry).await.0
}

/// Like [`retry_loop`], also reporting whether the loop stopped early because
/// the overall budget ran out (rather than a non-retryable failure or the
/// attempt limit).
pub(crate) async fn retry_loop_detailed<F, Fut>(
    settings: &RetrySettings<'_>,
    mut send_once: F,
    mut on_retry: impl FnMut(RetryNotice<'_>),
) -> (SendOutcome, bool)
where
    F: FnMut(u32, Duration) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, AttemptFailure>>,
{
    let max_attempts = settings.max_attempts.max(1);
    let mut last: Option<AttemptFailure> = None;
    let mut saw_timeout = false;
    let mut attempts = 0;
    let mut budget_stopped = false;

    for attempt in 1..=max_attempts {
        attempts = attempt;
        if remaining_budget(settings).is_some_and(|remaining| remaining.is_zero() && attempt > 1) {
            budget_stopped = true;
            break;
        }
        let timeout = attempt_timeout(settings);
        let failure = match send_once(attempt, timeout).await {
            Ok(bytes) => return (SendOutcome::Success(bytes), false),
            Err(failure) => failure,
        };
        if failure.timed_out() {
            saw_timeout = true;
        }
        if !failure.retryable || attempt == max_attempts {
            last = Some(failure);
            break;
        }
        let delay = retry_delay(settings, attempt, &failure);
        if remaining_budget(settings).is_some_and(|remaining| remaining <= delay) {
            last = Some(failure);
            budget_stopped = true;
            break;
        }
        let cause = failure.cause.clone();
        on_retry(RetryNotice {
            attempt,
            max_attempts,
            cause: &cause,
            delay,
        });
        last = Some(failure);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    (finish(last, attempts, saw_timeout), budget_stopped)
}

fn remaining_budget(settings: &RetrySettings<'_>) -> Option<Duration> {
    settings.remaining.map(|remaining| remaining())
}

fn attempt_timeout(settings: &RetrySettings<'_>) -> Duration {
    let remaining = remaining_budget(settings);
    match (settings.request_timeout, remaining) {
        (Some(timeout), Some(remaining)) => timeout.min(remaining).max(Duration::from_millis(1)),
        (Some(timeout), None) => timeout.max(Duration::from_millis(1)),
        (None, Some(remaining)) => remaining.max(Duration::from_millis(1)),
        (None, None) => Duration::from_secs(u64::MAX / 4),
    }
}

fn finish(last: Option<AttemptFailure>, attempts: u32, saw_timeout: bool) -> SendOutcome {
    let attempts = attempts.max(1);
    if saw_timeout && last.as_ref().is_none_or(|failure| failure.retryable) {
        return SendOutcome::Timeout { attempts };
    }
    match last {
        None => SendOutcome::Timeout { attempts },
        Some(AttemptFailure {
            kind: FailureKind::Timeout,
            ..
        }) => SendOutcome::Timeout { attempts },
        Some(AttemptFailure {
            kind: FailureKind::Transport(error),
            ..
        }) => SendOutcome::Transport { error, attempts },
        Some(AttemptFailure {
            kind: FailureKind::Body(error),
            ..
        }) => SendOutcome::Body { error, attempts },
        Some(AttemptFailure {
            kind: FailureKind::Http { status, body },
            ..
        }) => SendOutcome::Http { status, body, attempts },
    }
}

/// GETs `url`, retrying transient failures (connect/timeout/body errors, 429
/// and 5xx, and 404 when the policy says so). `what` names the resource in
/// error messages.
pub async fn get_bytes_with_retry(
    client: &reqwest::Client,
    url: &reqwest::Url,
    what: &str,
    policy: &GetRetryPolicy<'_>,
) -> Result<Vec<u8>> {
    let settings = RetrySettings::from_get(policy);
    let retry_not_found = policy.not_found_base_delay.is_some();
    let (outcome, budget_stopped) = retry_loop_detailed(
        &settings,
        |attempt, timeout| {
            let mut request = client.get(url.clone());
            if policy.remaining.is_some() || settings.request_timeout.is_some() {
                request = request.timeout(timeout);
            }
            async move {
                info!(url = %url, attempt, max_attempts = policy.max_attempts, "fetching {what}");
                send_get_once(request, what, retry_not_found).await
            }
        },
        |_| {},
    )
    .await;
    match outcome {
        SendOutcome::Success(bytes) => Ok(bytes),
        SendOutcome::Timeout { attempts } => Err(get_retry_error(
            format!("failed to retrieve {what}: timed out"),
            attempts,
            policy.max_attempts,
            budget_stopped,
        )),
        SendOutcome::Transport { error, attempts } => Err(get_retry_error(
            format!("failed to retrieve {what}: {error}"),
            attempts,
            policy.max_attempts,
            budget_stopped,
        )),
        SendOutcome::Body { error, attempts } => Err(get_retry_error(
            format!("failed to read {what} response body: {error}"),
            attempts,
            policy.max_attempts,
            budget_stopped,
        )),
        SendOutcome::Http { status, attempts, .. } => {
            let message = format!("{what} returned {status}");
            if !is_retryable_http_status(status)
                && !(status == StatusCode::NOT_FOUND && policy.not_found_base_delay.is_some())
            {
                bail!("{message}");
            }
            Err(get_retry_error(message, attempts, policy.max_attempts, budget_stopped))
        }
    }
}

fn get_retry_error(message: String, attempts: u32, max_attempts: u32, deadline_exhausted: bool) -> anyhow::Error {
    if deadline_exhausted {
        anyhow::anyhow!("{message} after {attempts} of {max_attempts} attempts (deadline exhausted)")
    } else {
        anyhow::anyhow!("{message} after {attempts} attempts")
    }
}

async fn send_get_once(
    request: reqwest::RequestBuilder,
    what: &str,
    retry_not_found: bool,
) -> Result<Vec<u8>, AttemptFailure> {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return Err(failure_from_send(error)),
    };
    let retry_after = parse_retry_after(response.headers());
    let status = response.status();
    if status == StatusCode::NOT_FOUND && retry_not_found {
        let _ = response.bytes().await;
        let mut failure = failure_from_http(status, Vec::new(), retry_after, true);
        failure.cause = format!("{what} returned 404 Not Found");
        return Err(failure);
    }
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default().to_vec();
        return Err(failure_from_http(status, body, retry_after, retry_not_found));
    }
    match response.bytes().await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(error) => Err(failure_from_body(error)),
    }
}

/// IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`) to `SystemTime`.
fn parse_imf_fixdate(value: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let rest = value.split_once(", ")?.1;
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = u32::try_from(MONTHS.iter().position(|&name| name == month_name)? + 1).ok()?;
    let year: i32 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let hour: u32 = hms.next()?.parse().ok()?;
    let minute: u32 = hms.next()?.parse().ok()?;
    let second: u32 = hms.next()?.parse().ok()?;
    if parts.next()? != "GMT" {
        return None;
    }
    let unix = days_from_civil(year, month, day)?
        .checked_mul(86400)?
        .checked_add(i64::from(hour) * 3600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    let unix = u64::try_from(unix).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(unix))
}

/// Howard Hinnant's civil-from-days, returning days since Unix epoch.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let year = if month <= 2 { year.checked_sub(1)? } else { year };
    let era = if year >= 0 { year } else { year.checked_sub(399)? }.div_euclid(400);
    let yoe = u32::try_from(year - era * 400).ok()?;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(i64::from(era) * 146097 + i64::from(doe) - 719468)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        GetRetryPolicy, RetrySettings, get_bytes_with_retry, jitter, parse_imf_fixdate, parse_retry_after, retry_delay,
    };

    fn url(server: &MockServer) -> reqwest::Url {
        format!("{}/file", server.uri()).parse().expect("url")
    }

    fn policy<'a>(not_found_base_delay: Option<Duration>) -> GetRetryPolicy<'a> {
        GetRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(5),
            max_delay: super::GET_MAX_RETRY_DELAY,
            not_found_base_delay,
            remaining: None,
        }
    }

    fn client() -> reqwest::Client {
        crate::vcs::ensure_rustls_provider();
        reqwest::Client::new()
    }

    #[test]
    fn parse_imf_fixdate_known_instant() {
        let parsed = parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT").expect("date");
        assert_eq!(
            parsed.duration_since(UNIX_EPOCH).expect("epoch").as_secs(),
            1_445_412_480
        );
    }

    #[test]
    fn parse_retry_after_reads_integer_seconds_and_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().expect("header"),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2099 07:28:00 GMT".parse().expect("header"),
        );
        let delay = parse_retry_after(&headers).expect("future date");
        assert!(
            delay > Duration::from_secs(60),
            "future HTTP-date should be far away, got {delay:?}"
        );
    }

    #[test]
    fn retry_after_above_cap_is_clamped() {
        let settings = RetrySettings {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            jitter: false,
            not_found_base_delay: None,
            remaining: None,
            request_timeout: None,
        };
        let failure = super::AttemptFailure {
            kind: super::FailureKind::Http {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                body: Vec::new(),
            },
            retryable: true,
            retry_after: Some(Duration::from_secs(3600)),
            cause: "HTTP 503".to_owned(),
        };
        assert_eq!(retry_delay(&settings, 1, &failure), Duration::from_secs(8));
    }

    #[test]
    fn jitter_stays_within_plus_minus_25_percent() {
        let base = Duration::from_millis(100);
        for _ in 0..64 {
            let jittered = jitter(base);
            assert!(
                jittered >= Duration::from_millis(75) && jittered <= Duration::from_millis(125),
                "{jittered:?}"
            );
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test]
    async fn timeout_then_retryable_http_reports_timeout_after_all_attempts() {
        use std::cell::Cell;

        let n = Cell::new(0);
        let settings = RetrySettings {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter: false,
            not_found_base_delay: None,
            remaining: None,
            request_timeout: Some(Duration::from_secs(1)),
        };
        let outcome = super::retry_loop(
            &settings,
            |_attempt, _timeout| {
                n.set(n.get() + 1);
                let kind = n.get();
                async move {
                    if kind == 1 {
                        Err(super::AttemptFailure {
                            kind: super::FailureKind::Timeout,
                            retryable: true,
                            retry_after: None,
                            cause: "timed out".to_owned(),
                        })
                    } else {
                        Err(super::failure_from_http(
                            reqwest::StatusCode::BAD_GATEWAY,
                            Vec::new(),
                            None,
                            false,
                        ))
                    }
                }
            },
            |_| {},
        )
        .await;
        assert!(
            matches!(outcome, super::SendOutcome::Timeout { attempts: 3 }),
            "mixed timeout then 502 must stay a timeout, got {outcome:?}"
        );
        assert_eq!(n.get(), 3);
    }

    #[tokio::test]
    async fn not_found_fails_at_once_without_not_found_delay() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let error = get_bytes_with_retry(&client(), &url(&server), "thing", &policy(None))
            .await
            .expect_err("404 should fail");
        assert!(error.to_string().contains("404"), "{error}");
    }

    #[tokio::test]
    async fn remaining_budget_cuts_retries_short() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        // The first retry delay (60 s) exceeds the budget (5 s), so the loop
        // stops after one attempt without depending on request timing.
        let remaining = || Duration::from_secs(5);
        let policy = GetRetryPolicy {
            base_delay: Duration::from_secs(60),
            remaining: Some(&remaining),
            ..policy(None)
        };
        let error = get_bytes_with_retry(&client(), &url(&server), "thing", &policy)
            .await
            .expect_err("503 should fail");
        let message = error.to_string();
        assert!(message.contains("503"), "{message}");
        assert!(
            message.contains("after 1 of 3 attempts (deadline exhausted)"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn retries_transient_failure_then_returns_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let bytes = get_bytes_with_retry(&client(), &url(&server), "thing", &policy(None))
            .await
            .expect("should succeed");
        assert_eq!(bytes, b"payload");
    }

    #[test]
    fn generic_get_caps_retry_after_without_budget() {
        let policy = policy(None);
        let settings = RetrySettings::from_get(&policy);
        let failure = super::failure_from_http(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Vec::new(),
            Some(Duration::from_secs(3600)),
            false,
        );
        assert_eq!(
            retry_delay(&settings, 1, &failure),
            super::GET_MAX_RETRY_DELAY,
            "an uncapped Retry-After must not stall a budgetless GET"
        );
    }

    #[tokio::test]
    async fn closed_connection_without_response_is_retried() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            loop {
                let (mut socket, _) = listener.accept().expect("accept");
                accepted += 1;
                if accepted == 1 {
                    drop(socket);
                    continue;
                }
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf);
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .expect("write");
                return accepted;
            }
        });
        let url: reqwest::Url = format!("http://{addr}/file").parse().expect("url");
        let bytes = get_bytes_with_retry(&client(), &url, "thing", &policy(None))
            .await
            .expect("reset connection should be retried");
        assert_eq!(bytes, b"ok");
        assert_eq!(server.join().expect("server"), 2);
    }

    #[tokio::test]
    async fn budgetless_get_retries_after_capped_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "3600"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let policy = GetRetryPolicy {
            max_delay: Duration::from_millis(40),
            remaining: None,
            ..policy(None)
        };
        let started = Instant::now();
        let bytes = get_bytes_with_retry(&client(), &url(&server), "thing", &policy)
            .await
            .expect("capped Retry-After should retry");
        let elapsed = started.elapsed();
        assert_eq!(bytes, b"payload");
        assert!(
            elapsed >= Duration::from_millis(25),
            "budgetless GET must sleep the capped Retry-After, elapsed={elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "3600s Retry-After must be capped, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn non_retryable_attempt_does_not_set_budget_stopped() {
        let remaining = || Duration::from_secs(60);
        let settings = RetrySettings {
            max_attempts: 3,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            jitter: false,
            not_found_base_delay: None,
            remaining: Some(&remaining),
            request_timeout: None,
        };
        let mut attempts = 0;
        let (outcome, budget_stopped) = super::retry_loop_detailed(
            &settings,
            |_attempt, _timeout| {
                attempts += 1;
                async {
                    Err(super::AttemptFailure {
                        kind: super::FailureKind::Http {
                            status: reqwest::StatusCode::BAD_REQUEST,
                            body: Vec::new(),
                        },
                        retryable: false,
                        retry_after: None,
                        cause: "HTTP 400".to_owned(),
                    })
                }
            },
            |_| {},
        )
        .await;
        assert_eq!(attempts, 1);
        assert!(
            !budget_stopped,
            "a non-retryable failure with budget remaining must not be budget-stopped"
        );
        match outcome {
            super::SendOutcome::Http {
                status, attempts: 1, ..
            } => {
                assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            }
            other => panic!("expected HTTP 400 after one attempt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_retryable_error_under_budget_is_not_labelled_deadline() {
        let remaining = || Duration::from_secs(60);
        let url: reqwest::Url = "ftp://127.0.0.1/file".parse().expect("url");
        let policy = GetRetryPolicy {
            remaining: Some(&remaining),
            ..policy(None)
        };
        let error = get_bytes_with_retry(&client(), &url, "thing", &policy)
            .await
            .expect_err("unsupported scheme should fail without retry");
        let message = error.to_string();
        assert!(
            !message.contains("deadline exhausted"),
            "non-retryable send error with budget remaining must not be labelled deadline exhausted: {message}"
        );
        assert!(message.contains("after 1 attempts"), "{message}");
    }

    #[tokio::test]
    async fn http_503_honours_retry_after_within_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "3600"))
            .expect(1)
            .mount(&server)
            .await;

        let remaining = || Duration::from_millis(80);
        let policy = GetRetryPolicy {
            base_delay: Duration::from_millis(5),
            remaining: Some(&remaining),
            ..policy(None)
        };
        // Retry-After 3600s is far past the 80ms remaining budget, so the loop
        // must stop after one attempt rather than sleeping for an hour.
        let started = Instant::now();
        let error = get_bytes_with_retry(&client(), &url(&server), "thing", &policy)
            .await
            .expect_err("budget should cut the 3600s Retry-After");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(error.to_string().contains("503"), "{error}");
    }
}
