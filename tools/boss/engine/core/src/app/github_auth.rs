//! `FrontendRequest` handlers — GitHub OAuth device-flow controls.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_git_hub_auth_start(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthStart = req else {
        unreachable!()
    };
    {
        // Begin (or restart) the device flow. The controller transitions
        // to `RequestingCode` synchronously and drives the rest in a
        // background task; the forwarder pushes each subsequent state on
        // the `github.auth` topic. Reply with the immediate state so the
        // request completes.
        server_state.github_auth.start_flow().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_cancel(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthCancel = req else {
        unreachable!()
    };
    {
        server_state.github_auth.cancel().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_disconnect(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthDisconnect = req else {
        unreachable!()
    };
    {
        // Deletes the keychain token and drops to `Disconnected`.
        server_state.github_auth.disconnect().await;
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: server_state.github_auth.current_state().to_dto(),
            },
        );
    }
}

pub(super) async fn handle_git_hub_auth_status(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GitHubAuthStatus = req else {
        unreachable!()
    };
    {
        let current = server_state.github_auth.current_state();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::GitHubAuthState {
                state: current.to_dto(),
            },
        );
        // "Re-check" (design §7): when connected, re-run the org/SSO
        // probe so an org-approval / SSO banner clears on its own once
        // the owner approves or the user SSO-authorizes — no full
        // re-auth needed. `update_org_state` only notifies on a real
        // change, which the forwarder then pushes on `github.auth`.
        if let GitHubAuthState::Authorized { record, .. } = current {
            spawn_org_recheck(server_state, record.token);
        }
    }
}

/// Run the org/SSO probe for `token` against the work DB's products and the
/// controller's shared HTTP client, returning the aggregate org state.
///
/// The one place the engine wires [`probe_and_record_org_state`] to its
/// `WorkDb`; the forwarder's initial probe and the on-demand rechecks all go
/// through it.
pub(super) async fn probe_org_state(server_state: &ServerState, token: &str) -> OrgAuthState {
    let flow = server_state.github_auth.device_flow();
    let org_sink = WorkDbOrgStateSink::new(server_state.work_db.as_ref());
    probe_and_record_org_state(&org_sink, flow.as_ref(), token).await
}

/// Re-run the org/SSO probe in the background and record the result.
///
/// This is the explicit trigger for re-probing after the forwarder's own
/// once-per-authorization probe: the app's `GitHubAuthStatus` "Re-check", and
/// a product's external-tracker config changing (the probe only covers
/// GitHub-tracked products, so a product newly bound to GitHub — or an org
/// edit — must be re-probed to resolve a state the forwarder left `Unknown`).
/// Detached so the caller's permits and ordering turn are released once its
/// reply is sent; at most one re-check runs at a time (`org_recheck`), and a
/// request that finds one in flight is dropped rather than queued.
pub(super) fn spawn_org_recheck(server_state: Arc<ServerState>, token: String) {
    let Ok(permit) = server_state.org_recheck.clone().try_acquire_owned() else {
        return;
    };
    boss_task_scope::spawn(async move {
        let _permit = permit;
        let resolved = probe_org_state(&server_state, &token).await;
        server_state.github_auth.update_org_state(resolved);
    });
}
