//! HTTP transport: the process-wide client, retry/backoff policy, and the
//! six queue endpoints `TrunkClient` exposes.

use std::sync::Arc;
use std::time::Duration;

pub use boss_http_retry::RetryPolicy;
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
//
// [`RetryPolicy`] itself — the max-attempts/backoff/cap shape — lives in
// [`boss_http_retry`], shared with `claude_client`. Retries apply only to
// retryable failures (5xx, transport errors, and 429 without a usable
// `Retry-After`); backoff is exponential with +/-25% jitter, applied by
// [`backoff_delay`] below.

/// Four attempts, 250ms base doubling up to a 30s cap — enough to ride out a
/// Trunk blip without a poller sweep stalling for minutes.
fn default_retry_policy() -> RetryPolicy {
    RetryPolicy::new(4, Duration::from_millis(250), Duration::from_secs(30))
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
            retry: default_retry_policy(),
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
            // The client no longer follows redirects (see
            // `boss_http_retry::http_client`), so a 3xx now arrives here
            // instead of being silently followed to whatever page it
            // points at and having that unrelated page's response mistaken
            // for Trunk's own.
            RawFailure::Api { status, body, .. } if (300..400).contains(&status) => {
                TrunkError::Redirected(format!("status {status}: {body}"))
            }
            // 429/5xx after retries are exhausted, and any other
            // unclassified non-2xx: the queue API isn't behaving.
            RawFailure::Api { status, body, .. } => {
                TrunkError::QueueUnavailable(format!("trunk returned {status}: {body}"))
            }
            RawFailure::Transport(msg) => TrunkError::Transport(msg),
            // Never retryable — a schema mismatch would just decode the same
            // way again — so keep it out of `Transport`, which callers
            // reasonably treat as a transient blip worth retrying.
            RawFailure::Decode(msg) => TrunkError::Decode(msg),
        }
    }
}

/// Backoff before the `attempt + 1`th try (1-based `attempt`), jittered
/// +/-25% so many concurrent callers don't retry in lockstep.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    boss_http_retry::jitter(boss_http_retry::backoff_delay(policy, attempt))
}

/// The delay before retrying `err`: `Retry-After` on a 429 when present
/// (clamped to `policy.max_backoff` so a large server-supplied value can't
/// park the caller — e.g. the queue poller — far past the policy's own
/// bound), otherwise the jittered exponential backoff.
fn retry_delay(err: &RawFailure, policy: &RetryPolicy, attempt: u32) -> Duration {
    if let RawFailure::Api {
        status: 429,
        retry_after: Some(retry_after),
        ..
    } = err
    {
        return (*retry_after).min(policy.max_backoff);
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

/// A 2xx response, not yet decoded. Kept as raw text (rather than decoded
/// eagerly inside [`call_once`]) so the two downstream consumers can apply
/// different rules: [`TrunkClient::call`] needs the body to be Trunk's
/// documented JSON; [`TrunkClient::call_unit`] doesn't look at the body at
/// all, so it never has a reason to fail on one.
struct RawResponse {
    status: u16,
    content_type: String,
    body_text: String,
}

/// First `max_chars` characters of `body`, with a trailing ellipsis if
/// truncated. Keeps a spoofed HTML/error page useful as a diagnostic
/// without dumping the whole thing into an error message or log line.
fn snippet(body: &str, max_chars: usize) -> String {
    let mut truncated: String = body.chars().take(max_chars).collect();
    if body.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

/// One round trip: POST `body` to `url` and return the raw 2xx response, or
/// a [`RawFailure`] classifying what went wrong.
async fn call_once<T: Serialize + ?Sized>(
    token: &str,
    url: &str,
    body: &T,
    timeout: Duration,
) -> Result<RawResponse, RawFailure> {
    let response = boss_http_retry::http_client()
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

    // Read the Content-Type before consuming `response` with `.text()` —
    // it's the one signal available on a 2xx that the body might be a
    // spoofed HTML/plaintext page rather than Trunk's real JSON response.
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_owned();
    let body_text = response
        .text()
        .await
        .map_err(|err| RawFailure::Decode(err.to_string()))?;
    Ok(RawResponse {
        status: status.as_u16(),
        content_type,
        body_text,
    })
}

/// Send `body` to `url`, retrying transient failures per `config.retry`.
async fn send_with_retry<T: Serialize + ?Sized>(
    token_provider: &dyn TrunkTokenProvider,
    url: &str,
    body: &T,
    config: &CallConfig,
) -> Result<RawResponse, TrunkError> {
    let token = token_provider.token()?;
    let attempts = config.retry.max_attempts.max(1);
    for attempt in 1..=attempts {
        match call_once(token.expose_secret(), url, body, config.timeout).await {
            Ok(raw) => return Ok(raw),
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

/// Decode a raw 2xx body as JSON, for endpoints whose caller needs the
/// parsed value. An empty body decodes to `Value::Null` — Trunk's success
/// response for a couple of these endpoints has been observed as a 2xx
/// with an empty body rather than the documented bare `{}`. A non-empty
/// body that isn't JSON at all is [`TrunkError::NonJsonResponse`], carrying
/// enough of the response (status, content type, a truncated snippet) to
/// diagnose — distinct from a body that *is* JSON but the wrong shape,
/// which stays a plain [`TrunkError::Decode`].
fn decode_json(path: &str, raw: &RawResponse) -> Result<Value, TrunkError> {
    if raw.body_text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&raw.body_text).map_err(|err| {
        TrunkError::NonJsonResponse(format!(
            "{path}: status {status}, content-type \"{content_type}\": {body} ({err})",
            status = raw.status,
            content_type = raw.content_type,
            body = snippet(&raw.body_text, 200),
        ))
    })
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
        let raw = send_with_retry(self.token_provider.as_ref(), &url, request, &self.config).await?;
        let value = decode_json(path, &raw)?;
        serde_json::from_value(value)
            .map_err(|err| TrunkError::Decode(format!("failed to decode {path} response: {err}")))
    }

    /// Send `request` to `path` and treat any 2xx as success without
    /// looking at the body — used by the three fire-and-forget verbs
    /// (`submitPullRequest`, `cancelPullRequest`,
    /// `restartTestsOnPullRequest`) whose caller only cares whether Trunk
    /// accepted the request, never the payload it answered with.
    ///
    /// This matters beyond convenience: a real, authenticated submit
    /// (flunge#1102) was rejected client-side — its standing merge-queue
    /// intent rolled back, and the Merging-lane state never written —
    /// because the 2xx response body didn't parse as JSON, even though
    /// Trunk had already accepted the PR and went on to merge it. The
    /// token and enrollment were fine; only the response body was
    /// unexpected. Requiring these three endpoints' bodies to parse as
    /// JSON creates a false-negative surface with no corresponding safety
    /// benefit, since the parsed value is discarded either way — whatever
    /// shape a genuinely-successful response takes, this now recognizes it
    /// without needing to match it. A redirect masquerading as a 2xx is
    /// still caught: the client no longer follows redirects (see
    /// `boss_http_retry::http_client`), so one surfaces as an explicit
    /// [`TrunkError::Redirected`] via the normal non-2xx path instead of
    /// being silently accepted here.
    async fn call_unit<Req>(&self, path: &str, request: &Req) -> Result<(), TrunkError>
    where
        Req: Serialize + ?Sized,
    {
        let url = self.url(path);
        send_with_retry(self.token_provider.as_ref(), &url, request, &self.config).await?;
        Ok(())
    }

    /// `POST /v1/submitPullRequest` — enqueue a PR. Success is any 2xx.
    pub async fn submit_pull_request(&self, request: &SubmitPullRequestRequest) -> Result<(), TrunkError> {
        self.call_unit("submitPullRequest", request).await
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
        self.call_unit("cancelPullRequest", request).await
    }

    /// `POST /v1/restartTestsOnPullRequest` — re-test a still-live entry.
    pub async fn restart_tests_on_pull_request(&self, request: &TrunkPrLookup) -> Result<(), TrunkError> {
        self.call_unit("restartTestsOnPullRequest", request).await
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
        assert!(matches!(decode, TrunkError::Decode(_)), "got {decode:?}");
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
    fn retry_delay_clamps_retry_after_to_max_backoff() {
        let policy = RetryPolicy::new(3, Duration::from_millis(100), Duration::from_secs(30));
        let err = RawFailure::Api {
            status: 429,
            body: String::new(),
            // A server-supplied Retry-After far beyond the policy's own cap
            // must not park the caller (e.g. the queue poller) for an hour.
            retry_after: Some(Duration::from_secs(3600)),
        };
        assert_eq!(retry_delay(&err, &policy, 1), Duration::from_secs(30));
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
                "state": "running",
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
                "state": "running",
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

    #[tokio::test]
    async fn list_pull_requests_posts_to_the_right_path_and_decodes_a_paged_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/listPullRequests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pullRequests": [{ "id": "entry_1", "state": "pending", "prNumber": 1 }],
                "nextCursor": "cursor-abc",
            })))
            .mount(&server)
            .await;

        let request = ListPullRequestsRequest::builder()
            .repo(TrunkRepoRef::new("github.com", "brianduff", "flunge"))
            .target_branch("main")
            .build();
        let response = client(server.uri(), RetryPolicy::NONE)
            .list_pull_requests(&request)
            .await
            .expect("success");
        assert_eq!(response.pull_requests.len(), 1);
        assert_eq!(response.pull_requests[0].id, "entry_1");
        assert_eq!(response.next_cursor.as_deref(), Some("cursor-abc"));
    }

    #[tokio::test]
    async fn cancel_pull_request_posts_to_the_right_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cancelPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        client(server.uri(), RetryPolicy::NONE)
            .cancel_pull_request(&lookup())
            .await
            .expect("success");
    }

    #[tokio::test]
    async fn restart_tests_on_pull_request_posts_to_the_right_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/restartTestsOnPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        client(server.uri(), RetryPolicy::NONE)
            .restart_tests_on_pull_request(&lookup())
            .await
            .expect("success");
    }

    #[tokio::test]
    async fn submit_pull_request_succeeds_on_a_2xx_with_an_empty_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200))
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
            .expect("a 2xx with an empty body is success, not a decode/transport error");
    }

    #[tokio::test]
    async fn submit_pull_request_succeeds_on_a_2xx_with_a_non_json_body() {
        // Regression test for flunge#1102: Trunk answered `submitPullRequest`
        // with a 2xx whose body wasn't JSON, and the PR was still queued and
        // merged. The old behavior rejected this client-side (rolling back
        // the merge intent and leaving the Merging lane empty) even though
        // Trunk had accepted the request.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submitPullRequest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"<html>not json</html>".to_vec(), "text/html"))
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
            .expect("a 2xx is success regardless of body shape for a fire-and-forget verb");
    }

    #[tokio::test]
    async fn get_queue_surfaces_a_non_json_2xx_body_as_a_diagnosable_error() {
        // Endpoints whose caller needs the parsed value must NOT get the
        // same free pass as the fire-and-forget verbs: a non-JSON body here
        // is still an error, but a distinct, diagnosable one instead of a
        // bare serde parse error.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"<html>login required</html>".to_vec(), "text/html"))
            .mount(&server)
            .await;

        let request = GetQueueRequest::new(TrunkRepoRef::new("github.com", "brianduff", "flunge"), "main");
        let err = client(server.uri(), RetryPolicy::NONE)
            .get_queue(&request)
            .await
            .unwrap_err();
        assert!(matches!(err, TrunkError::NonJsonResponse(_)), "got {err:?}");
        let message = err.to_string();
        assert!(message.contains("status 200"), "message missing status: {message}");
        assert!(message.contains("text/html"), "message missing content-type: {message}");
        assert!(
            message.contains("login required"),
            "message missing body snippet: {message}"
        );
        assert!(
            message.contains("boss engine trunk status"),
            "message missing remediation hint: {message}"
        );
    }

    #[tokio::test]
    async fn a_3xx_response_is_not_followed_and_surfaces_as_redirected() {
        // The shared client no longer follows redirects (see
        // `boss_http_retry::http_client`) — a redirect landing on an
        // unrelated page that then answers 200 is one way a non-JSON 2xx
        // could arrive. Asserting the 3xx itself is what confirms the
        // client can't be fooled that way anymore.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "https://example.invalid/login")
                    .set_body_string("redirecting…"),
            )
            .mount(&server)
            .await;

        let request = GetQueueRequest::new(TrunkRepoRef::new("github.com", "brianduff", "flunge"), "main");
        let err = client(server.uri(), RetryPolicy::NONE)
            .get_queue(&request)
            .await
            .unwrap_err();
        assert!(matches!(err, TrunkError::Redirected(_)), "got {err:?}");
        assert!(err.to_string().contains("status 302"), "got {err}");
    }

    #[test]
    fn url_trims_a_trailing_slash_on_the_base_url() {
        let client = client("http://host/".to_owned(), RetryPolicy::NONE);
        assert_eq!(client.url("getQueue"), "http://host/getQueue");
    }
}
