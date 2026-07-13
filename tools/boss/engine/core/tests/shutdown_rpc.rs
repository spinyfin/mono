//! Integration tests for the token-authenticated `Shutdown` RPC.
//!
//! Issue #705: `SIGTERM` had no way to distinguish "the macOS app is
//! auto-restarting me" from "a worker test accidentally targeted
//! `/tmp/boss-engine.pid`". The token gate fixes that by making the
//! everyday shutdown path require a credential that lives at a path
//! the bazel sandbox already denies access to.
//!
//! These tests exercise both halves on a real engine bound to a
//! temp socket:
//!   - a valid token is accepted, the engine sends
//!     `ShutdownAccepted`, and the accept loop exits.
//!   - a wrong token is rejected with `ShutdownRejected { reason:
//!     "token_mismatch" }` and the engine keeps running.

use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{FrontendEvent, FrontendRequest};

mod common;
use common::{TestEngine, TestEngineOptions};

#[tokio::test]
async fn shutdown_with_correct_token_is_accepted() -> Result<()> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        with_control_token: true,
    })
    .await?;

    // Token file must exist and contain the canonical schema.
    let parsed = engine
        .read_token()
        .map_err(|e| anyhow!("failed to read token file: {e}"))?;
    assert_eq!(parsed.token.len(), 64, "token should be 64 hex chars");
    assert_eq!(parsed.socket_path, engine.socket_path.display().to_string());

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::Shutdown {
            token: parsed.token.clone(),
        })
        .await?;
    match response {
        FrontendEvent::ShutdownAccepted => {}
        other => return Err(anyhow!("expected ShutdownAccepted, got {other:?}")),
    }

    // The engine should now exit its accept loop within the
    // shutdown_workers grace window (5s) + the 50ms response-defer.
    // Probe by trying to reconnect — connection refused means the
    // socket closed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut socket_closed = false;
    while std::time::Instant::now() < deadline {
        if !boss_client::engine_socket_reachable(engine.socket_str()).await {
            socket_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        socket_closed,
        "engine should have closed its socket after ShutdownAccepted"
    );

    Ok(())
}

#[tokio::test]
async fn shutdown_with_wrong_token_is_rejected() -> Result<()> {
    let engine = TestEngine::spawn_with(TestEngineOptions {
        on_disk_db: true,
        with_control_token: true,
    })
    .await?;

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::Shutdown {
            token: "not-the-real-token".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::ShutdownRejected { reason } => {
            assert_eq!(reason, "token_mismatch");
        }
        other => return Err(anyhow!("expected ShutdownRejected, got {other:?}")),
    }

    // The engine must still be alive — a second request should
    // succeed.
    let v = client.send_request(&FrontendRequest::GetEngineVersion).await?;
    match v {
        FrontendEvent::EngineVersionResult { .. } => Ok(()),
        other => Err(anyhow!(
            "engine should still respond after a rejected shutdown; got {other:?}"
        )),
    }
}
