//! HTTP transport: the process-wide client, retry/backoff policy, and the
//! six queue endpoints `TrunkClient` exposes.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::TrunkError;
use crate::models::{
    GetQueueRequest, ListPullRequestsRequest, ListPullRequestsResponse, SubmitPullRequestRequest, TrunkPrLookup,
    TrunkPullRequest, TrunkQueue,
};
use crate::secret::TrunkTokenProvider;

/// Base URL for Trunk's queue API. All six endpoints are POST under this
/// prefix.
pub const TRUNK_API_BASE_URL: &str = "https://api.trunk.io/v1";

// ── Retry policy + per-call config ────────────────────────────────────────────

/// How many attempts to make and how long to back off between them. Backoff
/// is exponential (`base_backoff * 2^(attempt-1)`, capped at `max_backoff`)
/// with +/-25% jitter, applied only to retryable failures (5xx, transport
/// errors, and 429 without a usable `Retry-After`).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, `>= 1`. `1` means no retry.
    pub max_attempts: u32,
    /// Backoff before the first retry; doubles for each subsequent retry.
    pub base_backoff: Duration,
    /// Upper bound on any single backoff, so a long attempt count can't
    /// balloon into an unbounded wait.
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// One attempt, no retry.
    pub const NONE: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    };

    pub const fn new(max_attempts: u32, base_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_attempts,
            base_backoff,
            max_backoff,
        }
    }
}

impl Default for RetryPolicy {
    /// Four attempts, 250ms base doubling up to a 30s cap — enough to ride
    /// out a Trunk blip without a poller sweep stalling for minutes.
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 4,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Per-call transport configuration: the wall-clock timeout (applied per
/// attempt), the [`RetryPolicy`], and a base URL tests override to point at
/// a mock server.
#[derive(Debug, Clone)]
pub struct CallConfig {
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub base_url: String,
}

impl CallConfig {
    /// A config with the given per-attempt timeout, the default retry
    /// policy, and [`TRUNK_API_BASE_URL`].
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            retry: RetryPolicy::default(),
            base_url: TRUNK_API_BASE_URL.to_owned(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

impl Default for CallConfig {
    fn default() -> Self {
        Self::new(Duration::from_secs(15))
    }
}

// ── The single HTTP client ────────────────────────────────────────────────────

/// The process-wide reqwest client shared by every Trunk call. Carries no
/// default timeout — each request applies its own via [`CallConfig::timeout`].
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`, so a default
        // crypto provider must be installed before the first TLS handshake.
        // `install_default` errors if one is already set; that's fine, we
        // ignore it (this crate may share a process with `claude_client`,
        // which installs the same provider).
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

// ── Raw transport failure (pre error-taxonomy classification) ────────────────

/// A failure from one round trip, before it's classified into a
/// [`TrunkError`]. Kept separate so the retry loop can decide retryability
/// and honor `Retry-After` without losing that detail once the final
/// [`TrunkError`] is constructed.
#[derive(Debug)]
enum RawFailure {
    Api {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    Transport(String),
    /// A 2xx response whose body didn't decode. Never retried: the same
    /// request would just decode the same way again.
    Decode(String),
}

impl RawFailure {
    fn is_retryable(&self) -> bool {
        match self {
            RawFailure::Api { status, .. } => *status == 429 || (500..=599).contains(status),
            RawFailure::Transport(_) => true,
            RawFailure::Decode(_) => false,
        }
    }

    fn into_trunk_error(self) -> TrunkError {
        match self {
            RawFailure::Api { status, body, .. } if status == 401 || status == 403 => {
                TrunkError::Auth(format!("trunk returned {status}: {body}"))
            }
            RawFailure::Api { status: 404, body, .. } => TrunkError::NotFound(body),
            // 429/5xx after retries are exhausted, and any other
            // unclassified non-2xx: the queue API isn't behaving.
            RawFailure::Api { status, body, .. } => {
                TrunkError::QueueUnavailable(format!("trunk returned {status}: {body}"))
            }
            RawFailure::Transport(msg) => TrunkError::Transport(msg),
            RawFailure::Decode(msg) => TrunkError::Transport(msg),
        }
    }
}

/// Backoff before the `attempt + 1`th try (1-based `attempt`), jittered
/// +/-25% so many concurrent callers don't retry in lockstep.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX).max(1);
    let capped = policy.base_backoff.saturating_mul(factor).min(policy.max_backoff);
    jitter(capped)
}

fn jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let factor = 0.75 + fastrand::f64() * 0.5;
    Duration::from_millis((delay.as_millis() as f64 * factor).round() as u64)
}

/// The delay before retrying `err`: `Retry-After` on a 429 when present,
/// otherwise the jittered exponential backoff.
fn retry_delay(err: &RawFailure, policy: &RetryPolicy, attempt: u32) -> Duration {
    if let RawFailure::Api {
        status: 429,
        retry_after: Some(retry_after),
        ..
    } = err
    {
        return *retry_after;
    }
    backoff_delay(policy, attempt)
}

/// Parse a `Retry-After` header value as whole seconds. Trunk's API doesn't
/// document the HTTP-date form, so only the integer-seconds form is
/// supported; an unparseable value falls back to backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// One round trip: POST `body` to `url` and return the decoded JSON, or a
/// [`RawFailure`] classifying what went wrong.
async fn call_once<T: Serialize + ?Sized>(
    token: &str,
    url: &str,
    body: &T,
    timeout: Duration,
) -> Result<Value, RawFailure> {
    let response = http_client()
        .post(url)
        .header("x-api-token", token)
        .header("content-type", "application/json")
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(|err| RawFailure::Transport(err.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let retry_after = if status.as_u16() == 429 {
            parse_retry_after(response.headers())
        } else {
            None
        };
        let body_text = response.text().await.unwrap_or_default();
        return Err(RawFailure::Api {
            status: status.as_u16(),
            body: body_text,
            retry_after,
        });
    }

    response
        .json::<Value>()
        .await
        .map_err(|err| RawFailure::Decode(err.to_string()))
}

/// Send `body` to `url`, retrying transient failures per `config.retry`.
async fn send_with_retry<T: Serialize + ?Sized>(
    token_provider: &dyn TrunkTokenProvider,
    url: &str,
    body: &T,
    config: &CallConfig,
) -> Result<Value, TrunkError> {
    let token = token_provider.token()?;
    let attempts = config.retry.max_attempts.max(1);
    for attempt in 1..=attempts {
        match call_once(token.expose_secret(), url, body, config.timeout).await {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && attempt < attempts => {
                let delay = retry_delay(&err, &config.retry, attempt);
                tracing::warn!(
                    attempt,
                    max_attempts = attempts,
                    backoff_ms = delay.as_millis() as u64,
                    err = ?err,
                    "trunk_client: transient failure; retrying",
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
            Err(err) => return Err(err.into_trunk_error()),
        }
    }
    unreachable!("retry loop always returns on the final attempt")
}

// ── The client ────────────────────────────────────────────────────────────────

/// A client for Trunk's merge-queue REST API. Cheap to clone (the token
/// provider is `Arc`'d and the underlying `reqwest::Client` is a shared
/// process-wide singleton).
#[derive(Clone)]
pub struct TrunkClient {
    token_provider: Arc<dyn TrunkTokenProvider>,
    config: CallConfig,
}

impl TrunkClient {
    pub fn new(token_provider: Arc<dyn TrunkTokenProvider>, config: CallConfig) -> Self {
        Self { token_provider, config }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.config.base_url.trim_end_matches('/'))
    }

    async fn call<Req, Resp>(&self, path: &str, request: &Req) -> Result<Resp, TrunkError>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let url = self.url(path);
        let value = send_with_retry(self.token_provider.as_ref(), &url, request, &self.config).await?;
        serde_json::from_value(value)
            .map_err(|err| TrunkError::Transport(format!("failed to decode {path} response: {err}")))
    }

    /// `POST /v1/submitPullRequest` — enqueue a PR. Success is a bare `{}`.
    pub async fn submit_pull_request(&self, request: &SubmitPullRequestRequest) -> Result<(), TrunkError> {
        let _: Value = self.call("submitPullRequest", request).await?;
        Ok(())
    }

    /// `POST /v1/getSubmittedPullRequest` — the queue's view of one PR.
    pub async fn get_submitted_pull_request(&self, request: &TrunkPrLookup) -> Result<TrunkPullRequest, TrunkError> {
        self.call("getSubmittedPullRequest", request).await
    }

    /// `POST /v1/listPullRequests` — paged, filterable PR history; the
    /// reconciliation backstop.
    pub async fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
    ) -> Result<ListPullRequestsResponse, TrunkError> {
        self.call("listPullRequests", request).await
    }

    /// `POST /v1/getQueue` — queue state plus every enqueued PR in one call.
    pub async fn get_queue(&self, request: &GetQueueRequest) -> Result<TrunkQueue, TrunkError> {
        self.call("getQueue", request).await
    }

    /// `POST /v1/cancelPullRequest` — dequeue a PR.
    pub async fn cancel_pull_request(&self, request: &TrunkPrLookup) -> Result<(), TrunkError> {
        let _: Value = self.call("cancelPullRequest", request).await?;
        Ok(())
    }

    /// `POST /v1/restartTestsOnPullRequest` — re-test a still-live entry.
    pub async fn restart_tests_on_pull_request(&self, request: &TrunkPrLookup) -> Result<(), TrunkError> {
        let _: Value = self.call("restartTestsOnPullRequest", request).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TrunkPrRef, TrunkRepoRef};
    use crate::secret::StaticTokenProvider;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn lookup() -> TrunkPrLookup {
        TrunkPrLookup::new(
            TrunkRepoRef::new("github.com", "brianduff", "flunge"),
            TrunkPrRef::new(978),
            "main",
        )
    }

    fn client(base_url: String, retry: RetryPolicy) -> TrunkClient {
        TrunkClient::new(
            Arc::new(StaticTokenProvider::new("test-token")),
            CallConfig::new(Duration::from_secs(5))
                .with_retry(retry)
                .with_base_url(base_url),
        )
    }

    // ── retry classification ──────────────────────────────────────────────

    #[test]
    fn retryable_classification() {
        assert!(
            RawFailure::Api {
                status: 429,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            RawFailure::Api {
                status: 500,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            RawFailure::Api {
                status: 503,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(RawFailure::Transport("connection reset".into()).is_retryable());
        assert!(
            !RawFailure::Api {
                status: 401,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            !RawFailure::Api {
                status: 403,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            !RawFailure::Api {
                status: 404,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            !RawFailure::Api {
                status: 400,
                body: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(!RawFailure::Decode("bad json".into()).is_retryable());
    }

    #[test]
    fn classifies_into_the_right_trunk_error_variant() {
        let auth = RawFailure::Api {
            status: 401,
            body: "nope".into(),
            retry_after: None,
        }
        .into_trunk_error();
        assert!(matches!(auth, TrunkError::Auth(_)), "got {auth:?}");

        let not_found = RawFailure::Api {
            status: 404,
            body: "pr not queued".into(),
            retry_after: None,
        }
        .into_trunk_error();
        assert!(matches!(not_found, TrunkError::NotFound(msg) if msg == "pr not queued"));

        let unavailable = RawFailure::Api {
            status: 503,
            body: "down".into(),
            retry_after: None,
        }
        .into_trunk_error();
        assert!(
            matches!(unavailable, TrunkError::QueueUnavailable(_)),
            "got {unavailable:?}"
        );

        let rate_limited = RawFailure::Api {
            status: 429,
            body: "slow down".into(),
            retry_after: None,
        }
        .into_trunk_error();
        assert!(
            matches!(rate_limited, TrunkError::QueueUnavailable(_)),
            "got {rate_limited:?}"
        );

        let transport = RawFailure::Transport("dns failure".into()).into_trunk_error();
        assert!(matches!(transport, TrunkError::Transport(_)), "got {transport:?}");

        let decode = RawFailure::Decode("unexpected eof".into()).into_trunk_error();
        assert!(matches!(decode, TrunkError::Transport(_)), "got {decode:?}");
    }

    #[test]
    fn backoff_is_exponential_and_capped_within_jitter_bounds() {
        let policy = RetryPolicy::new(6, Duration::from_millis(100), Duration::from_millis(1000));
        // attempt 1: base 100ms, jittered +/-25% -> [75, 125]ms
        let d1 = backoff_delay(&policy, 1).as_millis();
        assert!((75..=125).contains(&d1), "attempt 1 delay {d1}ms out of range");
        // attempt 2: base 200ms -> [150, 250]ms
        let d2 = backoff_delay(&policy, 2).as_millis();
        assert!((150..=250).contains(&d2), "attempt 2 delay {d2}ms out of range");
        // attempt 5 would be 1600ms uncapped; max_backoff caps it at 1000ms -> [750, 1250]ms
        let d5 = backoff_delay(&policy, 5).as_millis();
        assert!((750..=1250).contains(&d5), "attempt 5 delay {d5}ms out of range");
    }

    #[test]
    fn zero_delay_is_never_jittered_into_a_nonzero_wait() {
        let policy = RetryPolicy::NONE;
        assert_eq!(backoff_delay(&policy, 1), Duration::ZERO);
    }

    #[test]
    fn retry_delay_prefers_retry_after_on_429() {
        let policy = RetryPolicy::new(3, Duration::from_millis(100), Duration::from_secs(30));
        let err = RawFailure::Api {
            status: 429,
            body: String::new(),
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(retry_delay(&err, &policy, 1), Duration::from_secs(7));
    }

    #[test]
    fn retry_delay_falls_back_to_backoff_without_retry_after() {
        let policy = RetryPolicy::new(3, Duration::from_millis(100), Duration::from_secs(30));
        let err = RawFailure::Api {
            status: 429,
            body: String::new(),
            retry_after: None,
        };
        let delay = retry_delay(&err, &policy, 1).as_millis();
        assert!((75..=125).contains(&delay), "delay {delay}ms out of range");
    }

    // ── end-to-end transport ───────────────────────────────────────────────

    #[tokio::test]
    async fn submit_pull_request_sends_the_auth_header_and_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .and(header("x-api-token", "test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let request = SubmitPullRequestRequest::builder()
            .repo(TrunkRepoRef::new("github.com", "brianduff", "flunge"))
            .pr(TrunkPrRef::new(978))
            .target_branch("main")
            .build();
        client(server.uri(), RetryPolicy::NONE)
            .submit_pull_request(&request)
            .await
            .expect("success");
    }

    #[tokio::test]
    async fn get_queue_parses_the_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "RUNNING",
                "branch": "main",
                "enqueuedPullRequests": [],
            })))
            .mount(&server)
            .await;

        let request = GetQueueRequest::new(TrunkRepoRef::new("github.com", "brianduff", "flunge"), "main");
        let queue = client(server.uri(), RetryPolicy::NONE)
            .get_queue(&request)
            .await
            .expect("success");
        assert_eq!(queue.state, crate::models::TrunkQueueState::Running);
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getSubmittedPullRequest"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/getSubmittedPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "entry_1",
                "state": "pending",
                "prNumber": 1,
            })))
            .mount(&server)
            .await;

        let pr = client(
            server.uri(),
            RetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(10)),
        )
        .get_submitted_pull_request(&lookup())
        .await
        .expect("retry then success");
        assert_eq!(pr.id, "entry_1");
    }

    #[tokio::test]
    async fn retries_429_honoring_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_string("slow down"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "RUNNING",
                "branch": "main",
                "enqueuedPullRequests": [],
            })))
            .mount(&server)
            .await;

        let request = GetQueueRequest::new(TrunkRepoRef::new("github.com", "brianduff", "flunge"), "main");
        let queue = client(
            server.uri(),
            RetryPolicy::new(2, Duration::from_secs(30), Duration::from_secs(30)),
        )
        .get_queue(&request)
        .await
        .expect("429 retried honoring retry-after=0, not the 30s base backoff");
        assert_eq!(queue.state, crate::models::TrunkQueueState::Running);
    }

    #[tokio::test]
    async fn does_not_retry_401_and_surfaces_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getSubmittedPullRequest"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/getSubmittedPullRequest"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": "x", "state": "pending", "prNumber": 1 })),
            )
            .mount(&server)
            .await;

        let err = client(
            server.uri(),
            RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(10)),
        )
        .get_submitted_pull_request(&lookup())
        .await
        .unwrap_err();
        assert!(matches!(err, TrunkError::Auth(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn surfaces_not_found_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getSubmittedPullRequest"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no such entry"))
            .mount(&server)
            .await;

        let err = client(server.uri(), RetryPolicy::NONE)
            .get_submitted_pull_request(&lookup())
            .await
            .unwrap_err();
        assert!(matches!(err, TrunkError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn transport_error_on_unreachable_endpoint() {
        let client = client("http://127.0.0.1:1".to_owned(), RetryPolicy::NONE);
        let err = client.get_submitted_pull_request(&lookup()).await.unwrap_err();
        assert!(matches!(err, TrunkError::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn exhausted_retries_on_persistent_5xx_surface_queue_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(503).set_body_string("still down"))
            .mount(&server)
            .await;

        let request = GetQueueRequest::new(TrunkRepoRef::new("github.com", "brianduff", "flunge"), "main");
        let err = client(
            server.uri(),
            RetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(10)),
        )
        .get_queue(&request)
        .await
        .unwrap_err();
        assert!(matches!(err, TrunkError::QueueUnavailable(_)), "got {err:?}");
    }
}
