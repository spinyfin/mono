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
/// reply is sent. All probes share one worker and one pending rerun.
pub(super) fn spawn_org_recheck(server_state: Arc<ServerState>, token: String) {
    let queue = server_state.org_recheck.clone();
    queue.request(token, move |token| {
        let server_state = server_state.clone();
        async move {
            let resolved = probe_org_state(&server_state, &token).await;
            server_state.github_auth.update_org_state(&token, resolved);
        }
    });
}

/// The idle transition and request registration share a lock so requests
/// cannot be stranded between the worker's last check and its exit.
#[derive(Default)]
pub(super) struct OrgRecheck {
    state: std::sync::Mutex<RecheckState>,
}

#[derive(Default)]
struct RecheckState {
    running: bool,
    pending: Option<String>,
}

impl OrgRecheck {
    fn request<F, Fut>(self: &Arc<Self>, token: String, probe: F)
    where
        F: Fn(String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        {
            let mut state = self.state.lock().unwrap();
            state.pending = Some(token);
            if state.running {
                return;
            }
            state.running = true;
        }
        let queue = self.clone();
        boss_task_scope::spawn(async move {
            loop {
                let token = {
                    let mut state = queue.state.lock().unwrap();
                    match state.pending.take() {
                        Some(token) => token,
                        None => {
                            state.running = false;
                            return;
                        }
                    }
                };
                probe(token).await;
            }
        });
    }
}

#[cfg(test)]
#[path = "github_auth_tests.rs"]
mod tests;
