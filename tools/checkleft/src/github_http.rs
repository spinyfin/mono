//! GitHub REST adapter over the shared HTTP retry loop.
//!
//! Timeouts, connection errors, HTTP 5xx and 429 are retried with jittered
//! exponential backoff (honouring `Retry-After` on 429 and retryable 5xx,
//! capped). Other 4xx responses fail on the first attempt. The per-request
//! timeout is unchanged; retries are bounded so a dead GitHub still fails in
//! a predictable time.
//!
//! GET, POST, and PATCH share this path. POST/PATCH are retried on the same
//! transient classes: a 5xx means GitHub did not accept the write, and a
//! connect/timeout with no response leaves the call unconfirmed. A timeout
//! after GitHub actually applied a write can still produce a duplicate
//! check-run or SARIF upload; that is accepted so annotation posting is not
//! one-shot on a blip.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use tracing::warn;

use crate::http_retry::{
    RetrySettings, SendOutcome, failure_from_body, failure_from_http, failure_from_send, parse_retry_after, retry_loop,
};

/// Three attempts: the original plus two retries. Each request still uses the
/// 30s GitHub client timeout; backoff is 1s then 2s (jittered, capped at 8s,
/// including `Retry-After`). Worst case stays under [`GithubRetryPolicy::production`]'s
/// two-minute budget: 3 × 30s + two 8s delays = 106s.
pub const GITHUB_RETRY_MAX_ATTEMPTS: u32 = 3;
const GITHUB_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const GITHUB_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const GITHUB_RETRY_BUDGET: Duration = Duration::from_secs(120);

/// Per-attempt timeout for GitHub REST calls.
pub(crate) const GITHUB_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry / budget settings for one GitHub REST call.
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
    #[cfg(any(test, feature = "test-support"))]
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

/// Classified result of a GitHub REST call after retries have been applied.
#[derive(Debug)]
pub(crate) enum GithubGetOutcome {
    Success(Vec<u8>),
    Timeout { attempts: u32 },
    Transport { error: reqwest::Error },
    Body { error: reqwest::Error },
    Http { status: StatusCode, body: Vec<u8> },
}

pub(crate) struct GithubCall<'a> {
    pub method: Method,
    pub url: &'a str,
    pub token: Option<&'a str>,
    pub operation: &'static str,
    pub timeout: Duration,
    pub retry: GithubRetryPolicy,
    pub extra_headers: &'a [(&'a str, &'a str)],
    pub body: Option<Vec<u8>>,
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
    github_request(
        client,
        GithubCall {
            method: Method::GET,
            url,
            token,
            operation,
            timeout: request_timeout,
            retry,
            extra_headers: &[],
            body: None,
        },
    )
    .await
}

/// POST/PATCH/GET with GitHub REST headers, retrying transient failures.
pub(crate) async fn github_request(client: &reqwest::Client, call: GithubCall<'_>) -> GithubGetOutcome {
    let started = Instant::now();
    let remaining = || call.retry.budget.saturating_sub(started.elapsed());
    let settings = RetrySettings {
        max_attempts: call.retry.max_attempts,
        base_delay: call.retry.base_delay,
        max_delay: call.retry.max_delay,
        jitter: call.retry.jitter,
        not_found_base_delay: None,
        remaining: Some(&remaining),
        request_timeout: Some(call.timeout),
    };
    let outcome = retry_loop(
        &settings,
        |_attempt, timeout| {
            send_once(
                client,
                call.method.clone(),
                call.url,
                call.token,
                timeout,
                call.extra_headers,
                call.body.clone(),
            )
        },
        |notice| {
            warn!(
                attempt = notice.attempt,
                max_attempts = notice.max_attempts,
                operation = call.operation,
                cause = %notice.cause,
                delay_ms = notice.delay.as_millis() as u64,
                "retrying GitHub API call"
            );
        },
    )
    .await;
    match outcome {
        SendOutcome::Success(bytes) => GithubGetOutcome::Success(bytes),
        SendOutcome::Timeout { attempts } => GithubGetOutcome::Timeout { attempts },
        SendOutcome::Transport { error, .. } => GithubGetOutcome::Transport { error },
        SendOutcome::Body { error, .. } => GithubGetOutcome::Body { error },
        SendOutcome::Http { status, body, .. } => GithubGetOutcome::Http { status, body },
    }
}

async fn send_once(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    token: Option<&str>,
    timeout: Duration,
    extra_headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, crate::http_retry::AttemptFailure> {
    let mut request = client
        .request(method, url)
        .timeout(timeout)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "checkleft-cli");
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.body(body);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return Err(failure_from_send(error)),
    };

    let retry_after = parse_retry_after(response.headers());
    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default().to_vec();
        return Err(failure_from_http(status, body, retry_after, false));
    }

    match response.bytes().await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(error) => Err(failure_from_body(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tracing_subscriber::fmt::MakeWriter;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{GITHUB_RETRY_MAX_ATTEMPTS, GithubGetOutcome, GithubRetryPolicy, github_get};
    use crate::http_retry::parse_retry_after;

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

    async fn get_with(server: &MockServer, timeout: Duration, retry: GithubRetryPolicy) -> GithubGetOutcome {
        let client = reqwest::Client::builder().timeout(timeout).build().expect("client");
        github_get(
            &client,
            &url(server),
            Some("token"),
            "looking up the open PR for a branch",
            timeout,
            retry,
        )
        .await
    }

    async fn get(server: &MockServer, timeout: Duration) -> GithubGetOutcome {
        get_with(server, timeout, retry()).await
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
            GithubGetOutcome::Http { status, .. } => {
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

    #[tokio::test]
    async fn http_503_honours_nonzero_retry_after_capped_by_max_delay() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "3600"))
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

        let policy = GithubRetryPolicy {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(40),
            budget: Duration::from_secs(2),
            jitter: false,
        };
        let started = Instant::now();
        match get_with(&server, Duration::from_secs(1), policy).await {
            GithubGetOutcome::Success(bytes) => assert_eq!(bytes, b"[]"),
            other => panic!("expected success after capped 503 Retry-After, got {other:?}"),
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(25),
            "503 Retry-After must sleep a nonzero capped delay, elapsed={elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "3600s Retry-After must be capped, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn http_503_honours_http_date_retry_after_within_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "Wed, 21 Oct 2099 07:28:00 GMT"))
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

        let policy = GithubRetryPolicy {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(40),
            budget: Duration::from_secs(2),
            jitter: false,
        };
        let started = Instant::now();
        match get_with(&server, Duration::from_secs(1), policy).await {
            GithubGetOutcome::Success(bytes) => assert_eq!(bytes, b"[]"),
            other => panic!("expected success after HTTP-date Retry-After, got {other:?}"),
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(25),
            "HTTP-date Retry-After must sleep the capped delay, elapsed={elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "HTTP-date Retry-After must be capped, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn tiny_budget_stops_after_first_retryable_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let policy = GithubRetryPolicy {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            budget: Duration::from_millis(5),
            jitter: false,
        };
        match get_with(&server, Duration::from_secs(1), policy).await {
            GithubGetOutcome::Http { status, .. } => {
                assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
            }
            other => panic!("expected HTTP 503 after budget exhaustion, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_then_502_stays_a_timeout() {
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
            .respond_with(ResponseTemplate::new(502))
            .expect(1)
            .mount(&server)
            .await;

        let policy = GithubRetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            budget: Duration::from_secs(2),
            jitter: false,
        };
        match get_with(&server, Duration::from_millis(10), policy).await {
            GithubGetOutcome::Timeout { attempts } => {
                assert_eq!(attempts, 2);
            }
            other => panic!("timeout then 502 must stay a timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_is_retried_then_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/repos/o/r/pulls");
        let policy = GithubRetryPolicy {
            max_attempts: GITHUB_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(15),
            max_delay: Duration::from_millis(20),
            budget: Duration::from_secs(2),
            jitter: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let started = Instant::now();
        match github_get(
            &client,
            &url,
            Some("token"),
            "looking up the open PR for a branch",
            Duration::from_secs(1),
            policy,
        )
        .await
        {
            GithubGetOutcome::Transport { .. } => {}
            other => panic!("connection refused must be transport after retries, got {other:?}"),
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(25),
            "connection-refused must sleep between the three attempts, elapsed={elapsed:?}"
        );
    }
}
