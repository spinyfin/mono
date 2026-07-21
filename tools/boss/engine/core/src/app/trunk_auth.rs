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
    if let Err(e) = server_state.trunk_token_store.set(&token) {
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
            note: Some(
                "no trunk_queue-mechanism product configured yet; the live queue smoke check \
                 activates automatically once one exists"
                    .to_owned(),
            ),
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
