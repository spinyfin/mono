//! Shared retrying GitHub REST GET used by PR lookup paths.
//!
//! Timeouts, connection errors, HTTP 5xx and 429 are retried with jittered
//! exponential backoff (honouring `Retry-After` on 429, capped). Other 4xx
//! responses fail on the first attempt. The per-request timeout is unchanged;
//! retries are bounded so a dead GitHub still fails in a predictable time.

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tracing::warn;

use crate::http_retry::is_retryable_http_status;

/// Three attempts: the original plus two retries. Each request still uses the
/// 30s GitHub client timeout; backoff is 1s then 2s (jittered, capped at 8s,
/// including `Retry-After`). Worst case stays under [`GithubRetryPolicy::production`]'s
/// two-minute budget: 3 × 30s + two 8s delays = 106s.
pub const GITHUB_RETRY_MAX_ATTEMPTS: u32 = 3;
const GITHUB_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const GITHUB_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const GITHUB_RETRY_BUDGET: Duration = Duration::from_secs(120);

/// Retry / budget settings for one GitHub REST GET.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GithubRetryPolicy {
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles on each later attempt, then
    /// jittered, then capped at [`Self::max_delay`].
    pub base_delay: Duration,
    /// Cap on a single backoff sleep, including a server-supplied `Retry-After`.
    pub max_delay: Duration,
    /// Inclusive bound on request time plus sleeps. No retry is started that
    /// would outlast it; a request already in flight is still bounded by the
    /// client timeout.
    pub budget: Duration,
    /// When true, apply +/- 25% jitter to exponential backoff (not to
    /// `Retry-After`). Tests turn this off so timings stay deterministic.
    pub jitter: bool,
}

impl GithubRetryPolicy {
    pub fn production() -> Self {
        Self {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: GITHUB_RETRY_BASE_DELAY,
            max_delay: GITHUB_RETRY_MAX_DELAY,
            budget: GITHUB_RETRY_BUDGET,
            jitter: true,
        }
    }

    /// Short, unjittered delays so tests exercise the retry loop without
    /// waiting on production backoff.
    pub fn for_tests() -> Self {
        Self {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            budget: Duration::from_secs(2),
            jitter: false,
        }
    }
}

/// Classified result of a GitHub GET after retries have been applied.
#[derive(Debug)]
pub(crate) enum GithubGetOutcome {
    Success(Vec<u8>),
    Timeout { attempts: u32 },
    Transport { error: reqwest::Error },
    Body { error: reqwest::Error },
    Http { status: StatusCode },
}

#[derive(Clone, Copy)]
enum RetryableKind {
    Timeout,
    Transport,
    Body,
    Http,
}

struct AttemptFailure {
    outcome: GithubGetOutcome,
    retryable: Option<RetryableKind>,
    cause: String,
    retry_after: Option<Duration>,
}

/// GET `url` with GitHub REST headers, retrying transient failures.
pub(crate) async fn github_get(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    operation: &'static str,
    request_timeout: Duration,
    retry: GithubRetryPolicy,
) -> GithubGetOutcome {
    let started = Instant::now();
    let max_attempts = retry.max_attempts.max(1);
    let mut last: Option<GithubGetOutcome> = None;

    for attempt in 1..=max_attempts {
        let remaining = retry.budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let timeout = request_timeout.min(remaining).max(Duration::from_millis(1));
        let failure = match send_once(client, url, token, timeout, attempt).await {
            Ok(bytes) => return GithubGetOutcome::Success(bytes),
            Err(failure) => failure,
        };
        last = Some(failure.outcome);
        let Some(kind) = failure.retryable else {
            break;
        };
        if attempt == max_attempts {
            break;
        }
        let delay = retry_sleep(&retry, attempt, failure.retry_after);
        let remaining_after = retry.budget.saturating_sub(started.elapsed());
        if delay >= remaining_after {
            break;
        }
        warn!(
            attempt,
            max_attempts,
            operation,
            cause = %failure.cause,
            kind = retryable_kind_name(kind),
            delay_ms = delay.as_millis() as u64,
            "retrying GitHub API call"
        );
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    last.unwrap_or(GithubGetOutcome::Timeout { attempts: 1 })
}

async fn send_once(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    timeout: Duration,
    attempt: u32,
) -> Result<Vec<u8>, AttemptFailure> {
    let mut request = client
        .get(url)
        .timeout(timeout)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "checkleft-cli");
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        request = request.bearer_auth(token);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Err(timeout_failure(attempt, "timed out"));
        }
        Err(error) => {
            return Err(transport_failure(error));
        }
    };

    let status = response.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(response.headers()).filter(|_| status == StatusCode::TOO_MANY_REQUESTS);
        let retryable = is_retryable_http_status(status).then_some(RetryableKind::Http);
        return Err(AttemptFailure {
            outcome: GithubGetOutcome::Http { status },
            retryable,
            cause: format!("HTTP {status}"),
            retry_after,
        });
    }

    match response.bytes().await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(error) if error.is_timeout() => Err(timeout_failure(attempt, "timed out reading the response body")),
        Err(error) => Err(body_failure(error)),
    }
}

fn timeout_failure(attempt: u32, cause: &str) -> AttemptFailure {
    AttemptFailure {
        outcome: GithubGetOutcome::Timeout { attempts: attempt },
        retryable: Some(RetryableKind::Timeout),
        cause: cause.to_owned(),
        retry_after: None,
    }
}

fn transport_failure(error: reqwest::Error) -> AttemptFailure {
    let retryable = is_retryable_transport(&error).then_some(RetryableKind::Transport);
    AttemptFailure {
        cause: format!("transport error: {error}"),
        outcome: GithubGetOutcome::Transport { error },
        retryable,
        retry_after: None,
    }
}

fn body_failure(error: reqwest::Error) -> AttemptFailure {
    let retryable = is_retryable_transport(&error).then_some(RetryableKind::Body);
    AttemptFailure {
        cause: format!("response body error: {error}"),
        outcome: GithubGetOutcome::Body { error },
        retryable,
        retry_after: None,
    }
}

fn is_retryable_transport(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retryable_kind_name(kind: RetryableKind) -> &'static str {
    match kind {
        RetryableKind::Timeout => "timeout",
        RetryableKind::Transport => "transport",
        RetryableKind::Body => "body",
        RetryableKind::Http => "http",
    }
}

fn retry_sleep(policy: &GithubRetryPolicy, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let delay = match retry_after {
        Some(retry_after) => retry_after.min(policy.max_delay),
        None => {
            let exponential = policy
                .base_delay
                .saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)));
            let capped = exponential.min(policy.max_delay);
            if policy.jitter { jitter(capped) } else { capped }
        }
    };
    delay.min(policy.max_delay)
}

/// +/- 25% jitter. A zero delay stays zero.
fn jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let nanos = delay.as_nanos();
    let span = nanos / 4;
    let width = span.saturating_mul(2).saturating_add(1);
    let sample = entropy_nanos() % width;
    Duration::from_nanos(nanos.saturating_sub(span).saturating_add(sample) as u64)
}

fn entropy_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u128::from(duration.subsec_nanos()))
        .unwrap_or(0)
}

/// Integer-seconds `Retry-After` only. An HTTP-date value falls through to
/// exponential backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tracing_subscriber::fmt::MakeWriter;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{GITHUB_RETRY_MAX_ATTEMPTS, GithubGetOutcome, GithubRetryPolicy, github_get, parse_retry_after};

    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;

        fn make_writer(&'a self) -> Self::Writer {
            LogBuf(Arc::clone(&self.0))
        }
    }

    fn install_warn_capture() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(LogBuf(Arc::clone(&buf)))
            .finish();
        (tracing::subscriber::set_default(subscriber), buf)
    }

    fn captured(buf: &Mutex<Vec<u8>>) -> String {
        String::from_utf8_lossy(&buf.lock().expect("log buffer")).into_owned()
    }

    fn retry() -> GithubRetryPolicy {
        GithubRetryPolicy::for_tests()
    }

    fn url(server: &MockServer) -> String {
        format!("{}/repos/o/r/pulls", server.uri())
    }

    async fn get(server: &MockServer, timeout: Duration) -> GithubGetOutcome {
        let client = reqwest::Client::builder().timeout(timeout).build().expect("client");
        github_get(
            &client,
            &url(server),
            Some("token"),
            "looking up the open PR for a branch",
            timeout,
            retry(),
        )
        .await
    }

    #[test]
    fn parse_retry_after_reads_integer_seconds_and_ignores_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().expect("header"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[tokio::test]
    async fn timeout_then_success_retries_once_and_logs_warn() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(80)))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&server)
            .await;

        let (_guard, buf) = install_warn_capture();
        let outcome = get(&server, Duration::from_millis(10)).await;
        match outcome {
            GithubGetOutcome::Success(bytes) => assert_eq!(bytes, b"[]"),
            other => panic!("expected success after retry, got {other:?}"),
        }
        let logs = captured(&buf);
        assert!(logs.contains("retrying GitHub API call"), "{logs}");
        assert!(logs.contains("attempt=1"), "{logs}");
        assert!(logs.contains("timed out"), "{logs}");
        let retry_count = logs.matches("retrying GitHub API call").count();
        assert_eq!(retry_count, 1, "exactly one retry should be logged: {logs}");
    }

    #[tokio::test]
    async fn http_502_then_success_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"number":7}]"#))
            .expect(1)
            .mount(&server)
            .await;

        match get(&server, Duration::from_secs(1)).await {
            GithubGetOutcome::Success(bytes) => {
                assert_eq!(bytes, br#"[{"number":7}]"#);
            }
            other => panic!("expected success after 502, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn persistent_timeouts_fail_after_max_attempts_within_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(80)))
            .expect(u64::from(GITHUB_RETRY_MAX_ATTEMPTS))
            .mount(&server)
            .await;

        let started = Instant::now();
        let outcome = get(&server, Duration::from_millis(10)).await;
        let elapsed = started.elapsed();
        match outcome {
            GithubGetOutcome::Timeout { attempts } => {
                assert_eq!(attempts, GITHUB_RETRY_MAX_ATTEMPTS);
            }
            other => panic!("expected timeout after retries, got {other:?}"),
        }
        assert!(
            elapsed < retry().budget,
            "persistent timeouts must fail inside the retry budget, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn http_404_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        match get(&server, Duration::from_secs(1)).await {
            GithubGetOutcome::Http { status } => {
                assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
            }
            other => panic!("expected HTTP 404 without retry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_429_honours_retry_after_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&server)
            .await;

        match get(&server, Duration::from_secs(1)).await {
            GithubGetOutcome::Success(bytes) => assert_eq!(bytes, b"[]"),
            other => panic!("expected success after 429, got {other:?}"),
        }
    }
}
