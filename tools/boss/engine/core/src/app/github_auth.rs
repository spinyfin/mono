//! `FrontendRequest` handlers — GitHub OAuth device-flow controls.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

/// Spawn the GitHub OAuth auth-state forwarder.
///
/// At boot it restores any keychain-persisted token (so the status surface
/// reflects a prior connection across engine restarts), then hands the
/// controller's state channel to [`run_auth_forwarder`], which:
/// - pushes the display-safe [`GitHubAuthStateDto`] on the `github.auth` topic
///   whenever it differs from the last one pushed, so subscribed frontends
///   re-render, and
/// - runs the org/SSO probe ([`probe_org_state`]) once per authorization that
///   lands as `Authorized { org_state: Unknown }`.
///
/// A probe that resolves to `Unknown` (no GitHub-tracked product, transient
/// failure) is not retried by this loop: re-probing is the job of the
/// on-demand recheck (`GitHubAuthStatus`, and a product's tracker config
/// changing), both via [`spawn_org_recheck`]. A newly connected session reads
/// the current state with its connect-time `GitHubAuthStatus`.
pub(super) fn spawn_github_auth_forwarder(server_state: Arc<ServerState>) -> tokio::task::JoinHandle<()> {
    boss_task_scope::spawn(async move {
        let controller = server_state.github_auth.clone();
        // Restore a persisted token (if any) before subscribing, so the first
        // loop iteration sees the restored `Authorized { Unknown }` state and
        // runs the org probe.
        controller.restore_from_store();

        let rx = controller.subscribe();
        let probe_state = server_state.clone();
        let broadcast_state = server_state.clone();
        run_auth_forwarder(
            &controller,
            rx,
            |token| {
                let server_state = probe_state.clone();
                async move { probe_org_state(&server_state, &token).await }
            },
            |dto| {
                let server_state = broadcast_state.clone();
                async move { server_state.broadcast_github_auth_state(dto).await }
            },
        )
        .await;
    })
}

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
