//! `FrontendRequest` handlers for the Trunk org API token
//! (`TrunkSetToken`/`TrunkStatus`). See the design's "Auth: the Trunk org
//! API token" section
//! (`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`).
//!
//! `trunk_status` reports token configuration and, when a token is
//! configured, also runs a live `getQueue` smoke check against the first
//! `trunk_queue`-mechanism product it finds (`trunk_queue_smoke_check`).
//! `queue_check` stays unset — with a `note` explaining why — when there's
//! nothing to probe yet: no `trunk_queue` product configured, or that
//! product's `repo_remote_url` doesn't parse into Trunk repo coordinates.

use super::*;

use crate::protocol::TrunkQueueCheckDto;
use boss_trunk_client::GetQueueRequest;

/// Error from a [`TrunkTokenSource`] operation. Wraps
/// [`boss_trunk_auth::TokenStoreError`]'s message rather than the error
/// itself, so tests can construct a fake failure without reaching into
/// `boss-trunk-auth`'s keychain-backend error internals.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct TrunkTokenSourceError(String);

impl From<boss_trunk_auth::TokenStoreError> for TrunkTokenSourceError {
    fn from(e: boss_trunk_auth::TokenStoreError) -> Self {
        Self(e.to_string())
    }
}

/// Storage abstraction backing `ServerState::trunk_token_store`. The
/// production impl is [`boss_trunk_auth::TrunkTokenStore`]; tests inject a
/// fake so `handle_trunk_set_token`/`handle_trunk_status` can be exercised
/// without touching the real OS keychain.
pub(crate) trait TrunkTokenSource: Send + Sync {
    fn set(&self, token: &str) -> Result<(), TrunkTokenSourceError>;
    fn source(&self) -> Result<Option<boss_trunk_auth::TokenSource>, TrunkTokenSourceError>;
}

impl TrunkTokenSource for boss_trunk_auth::TrunkTokenStore {
    fn set(&self, token: &str) -> Result<(), TrunkTokenSourceError> {
        Ok(boss_trunk_auth::TrunkTokenStore::set(self, token)?)
    }

    fn source(&self) -> Result<Option<boss_trunk_auth::TokenSource>, TrunkTokenSourceError> {
        Ok(boss_trunk_auth::TrunkTokenStore::source(self)?)
    }
}

pub(super) async fn handle_trunk_set_token(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::TrunkSetToken { token } = req else {
        unreachable!()
    };
    let token = token.trim();
    if token.is_empty() {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: "Trunk API token must not be empty".to_owned(),
            },
        );
        return;
    }
    if let Err(e) = server_state.trunk_token_store.set(token) {
        tracing::error!(target: "boss_engine::trunk_auth", error = %e, "failed to persist Trunk API token to keychain");
        send_response(
            &sink,
            &request_id,
            FrontendEvent::WorkError {
                message: format!("failed to store Trunk API token: {e}"),
            },
        );
        return;
    }
    send_response(&sink, &request_id, trunk_status_event(&server_state).await);
}

pub(super) async fn handle_trunk_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::TrunkStatus = req else {
        unreachable!()
    };
    send_response(&sink, &request_id, trunk_status_event(&server_state).await);
}

async fn trunk_status_event(server_state: &ServerState) -> FrontendEvent {
    match server_state.trunk_token_store.source() {
        Ok(Some(source)) => {
            let (queue_check, note) = trunk_queue_smoke_check(server_state).await;
            FrontendEvent::TrunkStatus {
                configured: true,
                source: Some(source.as_str().to_owned()),
                queue_check,
                note,
            }
        }
        Ok(None) => FrontendEvent::TrunkStatus {
            configured: false,
            source: None,
            queue_check: None,
            note: Some("run `boss engine trunk set-token` to configure a Trunk API token".to_owned()),
        },
        Err(e) => {
            tracing::warn!(target: "boss_engine::trunk_auth", error = %e, "failed to read Trunk token source from keychain");
            FrontendEvent::TrunkStatus {
                configured: false,
                source: None,
                queue_check: None,
                note: Some(format!("failed to read Trunk token from keychain: {e}")),
            }
        }
    }
}

/// Live `getQueue` smoke check against the first `trunk_queue`-mechanism
/// product, if one exists. Returns `(None, Some(reason))` when there's
/// nothing to probe yet (no `trunk_queue` product, or its `repo_remote_url`
/// doesn't parse into Trunk repo coordinates); returns
/// `(Some(outcome), None)` once a real `getQueue` call has actually been
/// attempted, `outcome.ok` reflecting whether it succeeded.
async fn trunk_queue_smoke_check(server_state: &ServerState) -> (Option<TrunkQueueCheckDto>, Option<String>) {
    let products = match server_state.work_db.list_products() {
        Ok(products) => products,
        Err(e) => {
            tracing::warn!(target: "boss_engine::trunk_auth", error = %e, "failed to list products for trunk queue smoke check");
            return (None, Some(format!("failed to list products: {e}")));
        }
    };
    let Some(product) = products.into_iter().find(|p| {
        matches!(
            crate::merge_mechanism::MergeMechanism::parse(p.merge_mechanism.as_deref()),
            Ok(crate::merge_mechanism::MergeMechanism::TrunkQueue { .. })
        )
    }) else {
        return (
            None,
            Some("no trunk_queue-mechanism product configured yet to smoke-check".to_owned()),
        );
    };
    let target_branch = match crate::merge_mechanism::MergeMechanism::parse(product.merge_mechanism.as_deref()) {
        Ok(crate::merge_mechanism::MergeMechanism::TrunkQueue { target_branch }) => target_branch,
        _ => unreachable!("product was filtered on MergeMechanism::TrunkQueue above"),
    };
    let Some(repo_slug) = product
        .repo_remote_url
        .as_deref()
        .and_then(crate::pr_url_capture::parse_product_slug)
    else {
        return (
            None,
            Some(format!(
                "product '{}' has merge_mechanism=trunk_queue but no parseable repo_remote_url",
                product.name
            )),
        );
    };
    let Some(repo_ref) = crate::trunk_merge::trunk_repo_ref(&repo_slug) else {
        return (
            None,
            Some(format!(
                "product '{}' repo slug '{repo_slug}' is not a usable owner/name pair",
                product.name
            )),
        );
    };
    let request = GetQueueRequest::new(repo_ref, target_branch.clone());
    match server_state.trunk_client.get_queue(&request).await {
        Ok(queue) => (
            Some(TrunkQueueCheckDto {
                ok: true,
                detail: format!(
                    "{} @ {target_branch}: queue {:?}, {} PR(s) enqueued",
                    product.name,
                    queue.state,
                    queue.enqueued_pull_requests.len()
                ),
            }),
            None,
        ),
        Err(e) => (
            Some(TrunkQueueCheckDto {
                ok: false,
                detail: format!("{} @ {target_branch}: {e}", product.name),
            }),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use boss_trunk_client::{CallConfig, StaticTokenProvider, TrunkClient};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_support::create_test_product_with_repo;

    /// In-memory [`TrunkTokenSource`] fake. `fail_reads` makes `source()`
    /// return a keychain error, exercising the degrade-to-unconfigured path
    /// without touching the real OS keychain.
    struct FakeTrunkTokenSource {
        stored: std::sync::Mutex<Option<String>>,
        fail_reads: bool,
    }

    impl FakeTrunkTokenSource {
        fn empty() -> Self {
            Self {
                stored: std::sync::Mutex::new(None),
                fail_reads: false,
            }
        }

        fn failing() -> Self {
            Self {
                stored: std::sync::Mutex::new(None),
                fail_reads: true,
            }
        }
    }

    impl TrunkTokenSource for FakeTrunkTokenSource {
        fn set(&self, token: &str) -> Result<(), TrunkTokenSourceError> {
            *self.stored.lock().unwrap() = Some(token.to_owned());
            Ok(())
        }

        fn source(&self) -> Result<Option<boss_trunk_auth::TokenSource>, TrunkTokenSourceError> {
            if self.fail_reads {
                return Err(TrunkTokenSourceError("simulated keychain failure".to_owned()));
            }
            Ok(self
                .stored
                .lock()
                .unwrap()
                .as_ref()
                .map(|_| boss_trunk_auth::TokenSource::Keychain))
        }
    }

    fn test_server_state_with_trunk_store(store: Arc<dyn TrunkTokenSource>) -> (Arc<ServerState>, tempfile::TempDir) {
        test_server_state_with_trunk_store_and_client(store, None)
    }

    /// Like [`test_server_state_with_trunk_store`], but also lets the
    /// caller inject a [`boss_trunk_client::TrunkClient`] pointed at a
    /// `wiremock` server — needed to exercise `trunk_queue_smoke_check`'s
    /// live `getQueue` call without reaching the real Trunk API.
    fn test_server_state_with_trunk_store_and_client(
        store: Arc<dyn TrunkTokenSource>,
        trunk_client: Option<boss_trunk_client::TrunkClient>,
    ) -> (Arc<ServerState>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let state =
            ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, Some(store), trunk_client, None)
                .unwrap();
        (state, temp)
    }

    fn dispatch_ctx(server_state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
        Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(server_state.work_db.clone())
            .sink(sink.clone())
            .session_id("s1")
            .request_id("req-1")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build()
    }

    fn make_session_sink() -> Arc<SessionSink> {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        Arc::new(SessionSink::new(shutdown_tx))
    }

    #[tokio::test]
    async fn set_token_replies_with_trunk_status_on_success() {
        let (server_state, _dir) = test_server_state_with_trunk_store(Arc::new(FakeTrunkTokenSource::empty()));
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_set_token(
            ctx,
            FrontendRequest::TrunkSetToken {
                token: "trunk_tok_abc".to_owned(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus { configured, source, .. } => {
                assert!(configured);
                assert_eq!(source.as_deref(), Some("keychain"));
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_token_rejects_empty_token() {
        let (server_state, _dir) = test_server_state_with_trunk_store(Arc::new(FakeTrunkTokenSource::empty()));
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_set_token(
            ctx,
            FrontendRequest::TrunkSetToken {
                token: "   ".to_owned(),
            },
        )
        .await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::WorkError { message } => {
                assert!(message.contains("must not be empty"), "unexpected message: {message}");
            }
            other => panic!("expected WorkError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_reports_unconfigured_when_keychain_read_fails() {
        let (server_state, _dir) = test_server_state_with_trunk_store(Arc::new(FakeTrunkTokenSource::failing()));
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_status(ctx, FrontendRequest::TrunkStatus).await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus {
                configured,
                source,
                note,
                ..
            } => {
                assert!(!configured);
                assert_eq!(source, None);
                assert!(note.is_some(), "expected a note explaining the read failure");
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_reports_unconfigured_when_no_token_stored() {
        let (server_state, _dir) = test_server_state_with_trunk_store(Arc::new(FakeTrunkTokenSource::empty()));
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_status(ctx, FrontendRequest::TrunkStatus).await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus { configured, source, .. } => {
                assert!(!configured);
                assert_eq!(source, None);
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_notes_absence_of_a_trunk_queue_product() {
        let (server_state, _dir) = test_server_state_with_trunk_store(Arc::new(FakeTrunkTokenSource::empty()));
        server_state.trunk_token_store.set("trunk_tok").unwrap();
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_status(ctx, FrontendRequest::TrunkStatus).await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus {
                configured,
                queue_check,
                note,
                ..
            } => {
                assert!(configured);
                assert!(queue_check.is_none());
                assert!(
                    note.as_deref().is_some_and(|n| n.contains("no trunk_queue")),
                    "unexpected note: {note:?}"
                );
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_runs_a_live_queue_check_against_the_first_trunk_queue_product() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "RUNNING",
                "branch": "main",
                "enqueuedPullRequests": [],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let trunk_client = TrunkClient::new(
            Arc::new(StaticTokenProvider::new("test-token")),
            CallConfig::new(Duration::from_secs(5)).with_base_url(server.uri()),
        );
        let (server_state, _dir) =
            test_server_state_with_trunk_store_and_client(Arc::new(FakeTrunkTokenSource::empty()), Some(trunk_client));
        server_state.trunk_token_store.set("trunk_tok").unwrap();
        let product = create_test_product_with_repo(
            &server_state.work_db,
            "flunge",
            Some("git@github.com:brianduff/flunge.git"),
        );
        server_state
            .work_db
            .set_product_merge_mechanism(&product.id, Some("trunk_queue"))
            .unwrap();
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_status(ctx, FrontendRequest::TrunkStatus).await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus {
                configured,
                queue_check,
                note,
                ..
            } => {
                assert!(configured);
                assert!(note.is_none(), "unexpected note alongside a queue_check: {note:?}");
                let check = queue_check.expect("expected a live queue_check outcome");
                assert!(check.ok, "expected ok=true, got detail: {}", check.detail);
                assert!(check.detail.contains("flunge"), "unexpected detail: {}", check.detail);
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_reports_a_failed_queue_check_when_trunk_rejects_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getQueue"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .expect(1)
            .mount(&server)
            .await;
        let trunk_client = TrunkClient::new(
            Arc::new(StaticTokenProvider::new("test-token")),
            CallConfig::new(Duration::from_secs(5)).with_base_url(server.uri()),
        );
        let (server_state, _dir) =
            test_server_state_with_trunk_store_and_client(Arc::new(FakeTrunkTokenSource::empty()), Some(trunk_client));
        server_state.trunk_token_store.set("trunk_tok").unwrap();
        let product = create_test_product_with_repo(
            &server_state.work_db,
            "flunge",
            Some("git@github.com:brianduff/flunge.git"),
        );
        server_state
            .work_db
            .set_product_merge_mechanism(&product.id, Some("trunk_queue"))
            .unwrap();
        let sink = make_session_sink();
        let ctx = dispatch_ctx(&server_state, &sink);

        handle_trunk_status(ctx, FrontendRequest::TrunkStatus).await;

        let envelope = sink.next().await.expect("a response should be enqueued");
        match envelope.payload {
            FrontendEvent::TrunkStatus {
                configured,
                queue_check,
                ..
            } => {
                assert!(configured);
                let check = queue_check.expect("expected a live queue_check outcome");
                assert!(!check.ok, "expected ok=false, got detail: {}", check.detail);
            }
            other => panic!("expected TrunkStatus, got {other:?}"),
        }
    }
}
