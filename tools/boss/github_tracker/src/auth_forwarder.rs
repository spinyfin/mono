//! The GitHub OAuth auth-state forwarder loop.
//!
//! Watches a [`GitHubAuthController`]'s state channel and does two jobs, each
//! on an explicit trigger rather than on every wake-up:
//!
//! - **Broadcast** the display-safe [`GitHubAuthStateDto`] — only when it
//!   differs from the last one broadcast.
//! - **Probe** the org/SSO state — once per authorization, when the state is
//!   `Authorized { org_state: Unknown }` for a token this loop has not yet
//!   probed. A probe that resolves back to `Unknown` (no GitHub-tracked
//!   product, `list_products` failing, every org probe erroring) is a
//!   *result*, not a reason to probe again: the loop parks on the watch
//!   channel until something outside it changes the state. Re-probing after
//!   that is the job of the explicit triggers — a fresh authorization
//!   (authorize, `restore_from_store`, after a disconnect), the on-demand
//!   recheck behind `GitHubAuthStatus`, or a product's tracker config
//!   changing — never a timer and never this loop's own write.
//!
//! The transport (broadcast) and the probe itself are injected as closures so
//! the loop stays free of engine types and can be driven deterministically.

use std::future::Future;

use boss_protocol::{GitHubAuthStateDto, OrgAuthState};

use crate::github_oauth::{GitHubAuthController, GitHubAuthState};

/// Run the forwarder until the controller's sender is dropped (the engine is
/// shutting down).
///
/// `rx` should be a receiver obtained *after* any boot-time
/// `restore_from_store`, so the first iteration sees the restored state.
/// `probe` is handed the stored token and returns the resolved org state;
/// `broadcast` is handed each DTO that differs from the previous one.
pub async fn run_auth_forwarder<P, PF, B, BF>(
    controller: &GitHubAuthController,
    mut rx: tokio::sync::watch::Receiver<GitHubAuthState>,
    probe: P,
    broadcast: B,
) where
    P: Fn(String) -> PF,
    PF: Future<Output = OrgAuthState>,
    B: Fn(GitHubAuthStateDto) -> BF,
    BF: Future<Output = ()>,
{
    let mut last_broadcast: Option<GitHubAuthStateDto> = None;
    // The token this loop last started a probe for. Cleared whenever the
    // state leaves `Authorized`, so a disconnect followed by a fresh
    // authorization (even of the same token) probes again.
    let mut probed_token: Option<String> = None;

    loop {
        let state = rx.borrow_and_update().clone();

        let dto = state.to_dto();
        if last_broadcast.as_ref() != Some(&dto) {
            broadcast(dto.clone()).await;
            last_broadcast = Some(dto);
        }

        match &state {
            GitHubAuthState::Authorized {
                record,
                org_state: OrgAuthState::Unknown,
            } if probed_token.as_deref() != Some(record.token.as_str()) => {
                probed_token = Some(record.token.clone());
                let resolved = probe(record.token.clone()).await;
                controller.update_org_state(resolved);
            }
            GitHubAuthState::Authorized { .. } => {}
            _ => probed_token = None,
        }

        if rx.changed().await.is_err() {
            // Sender dropped — the engine is shutting down.
            break;
        }
    }
}

#[cfg(test)]
#[path = "auth_forwarder_tests.rs"]
mod tests;
