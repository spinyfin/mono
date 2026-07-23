//! `FrontendRequest` handlers for the Trunk org API token
//! (`TrunkSetToken`/`TrunkStatus`). See the design's "Auth: the Trunk org
//! API token" section
//! (`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`).
//!
//! The live `getQueue` smoke check against a `trunk_queue`-mechanism
//! product is deferred: `products.merge_mechanism` (design item 2) has not
//! landed yet, so there is no product to probe against. `trunk_status`
//! reports token configuration honestly and leaves `queue_check` unset
//! with a `note` explaining why; once that field exists, wiring in the
//! live probe here is the only change needed.

use super::*;

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
    send_response(&sink, &request_id, trunk_status_event(&server_state));
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
    send_response(&sink, &request_id, trunk_status_event(&server_state));
}

fn trunk_status_event(server_state: &ServerState) -> FrontendEvent {
    match server_state.trunk_token_store.source() {
        Ok(Some(source)) => FrontendEvent::TrunkStatus {
            configured: true,
            source: Some(source.as_str().to_owned()),
            queue_check: None,
            note: Some("live queue smoke check not yet available".to_owned()),
        },
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(temp.path().to_path_buf())
                .db_path(temp.path().join("state.db"))
                .build(),
            None,
        ));
        let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, Some(store)).unwrap();
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
}
