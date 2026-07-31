//! The Boss UI's current product-chooser selection, with the engine as
//! its system of record.
//!
//! # Why this exists
//!
//! Short ids (`T<n>`) are scoped per product, and most `boss` read verbs
//! require `--product`. Nothing used to tell a coordinator session which
//! product the operator was actually looking at, so it guessed — and a
//! wrong guess does not fail: it resolves to a real row, with a real
//! status and a real PR, in the wrong product. That is worse than a miss.
//!
//! # Where the selection lives
//!
//! It starts as app-local view state (`ChatViewModel.selectedWorkProductID`,
//! persisted only to `UserDefaults`). The app reports every change — and
//! its current value on reconnect — via
//! [`FrontendRequest::ReportSelectedProduct`]; this module holds that
//! report and answers [`FrontendRequest::GetSelectedProduct`] from it.
//! The app stays a thin reporter: it sends an id, and the engine does all
//! the resolution.
//!
//! # Why it is in-memory and session-scoped
//!
//! A selection describes a *running* UI. Persisting it would let a dead
//! app's last selection answer a query made hours later, recreating the
//! confident-but-wrong resolution this exists to eliminate. So the report
//! is tagged with the app session that made it, and any registration
//! change drops it (see [`ServerState::clear_selected_product`], called
//! from `app_session`). Losing it on an engine restart is fine: the app
//! reports again as soon as it reconnects.

use super::*;
use boss_protocol::SelectedProductState;

/// One report of the app's product chooser, tagged with the session that
/// sent it so a report cannot outlive its app.
#[derive(Debug, Clone)]
pub(super) struct SelectedProductReport {
    /// Session id of the app that reported this. Compared against the
    /// currently-registered app session before the report is trusted.
    session_id: String,
    /// The reported product id. `None` means the app explicitly reported
    /// "nothing is selected" — distinct from never having reported.
    product_id: Option<String>,
    /// Epoch seconds at which the report arrived.
    reported_at: i64,
}

/// Epoch seconds, saturating at 0 for a clock set before 1970.
fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ServerState {
    /// Record the app's current chooser selection. Returns `false` — and
    /// changes nothing — when `session_id` is not the registered app
    /// session, which is what keeps this a *report* of UI state rather
    /// than a way for any connected client to drive the chooser.
    pub(super) async fn record_selected_product(&self, session_id: &str, product_id: Option<String>) -> bool {
        let is_app = matches!(
            self.app_session.lock().await.as_ref(),
            Some(handle) if handle.session_id == session_id
        );
        if !is_app {
            return false;
        }
        *self.selected_product.lock().unwrap() = Some(SelectedProductReport {
            session_id: session_id.to_owned(),
            product_id,
            reported_at: now_epoch_seconds(),
        });
        true
    }

    /// Forget any recorded selection. Called whenever the app session
    /// registration changes (a new app registers, or the current one
    /// disconnects) so a stale selection can never answer a query.
    pub(super) fn clear_selected_product(&self) {
        *self.selected_product.lock().unwrap() = None;
    }

    /// Resolve the current selection into the state a caller can act on.
    ///
    /// Every unavailable case is reported as itself. There is deliberately
    /// no "fall back to the only product" branch: a caller that cannot
    /// tell "unknown" from "product X" will report facts about the wrong
    /// work item, which is exactly the failure this verb was built for.
    pub(super) async fn selected_product_state(&self) -> SelectedProductState {
        let app_session_id = self
            .app_session
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.session_id.clone());
        let Some(app_session_id) = app_session_id else {
            return SelectedProductState::AppNotConnected;
        };

        let report = self.selected_product.lock().unwrap().clone();
        // A report from a prior app session is not evidence about the
        // session that is connected now, even though `clear_selected_product`
        // should already have dropped it.
        let Some(report) = report.filter(|r| r.session_id == app_session_id) else {
            return SelectedProductState::NoSelection;
        };
        let Some(product_id) = report.product_id else {
            return SelectedProductState::NoSelection;
        };

        match self.work_db.get_product(&product_id) {
            Ok(Some(product)) => SelectedProductState::Selected {
                product_id: product.id,
                name: product.name,
                slug: product.slug,
                reported_at: report.reported_at,
            },
            // A lookup failure is not a selection: reporting the id as
            // unresolvable is honest, and the DB error is logged rather
            // than swallowed into a plausible-looking answer.
            Ok(None) => SelectedProductState::ProductUnknown { product_id },
            Err(err) => {
                tracing::warn!(
                    ?err,
                    product_id = %product_id,
                    "selected_product: product lookup failed; reporting the id as unresolvable",
                );
                SelectedProductState::ProductUnknown { product_id }
            }
        }
    }
}

pub(super) async fn handle_get_selected_product(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::GetSelectedProduct = req else {
        unreachable!()
    };
    // Same tier as `reveal_work_item`: a read of the app's UI state,
    // asked by the coordinator pane or the app shell.
    if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
        tracing::warn!(
            peer_pid = ?peer_pid,
            "get_selected_product rejected: caller not in app/Boss subtree",
        );
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "get_selected_product requires app or Boss authority".to_owned(),
            },
        );
        return;
    }
    let state = server_state.selected_product_state().await;
    send_response(&sink, &request_id, FrontendEvent::SelectedProductResult { state });
}

pub(super) async fn handle_report_selected_product(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ReportSelectedProduct { product_id } = req else {
        unreachable!()
    };
    let accepted = server_state
        .record_selected_product(&session_id, product_id.clone())
        .await;
    if !accepted {
        tracing::warn!(
            session_id = %session_id,
            "report_selected_product ignored: not the registered app session",
        );
    } else {
        tracing::debug!(
            session_id = %session_id,
            product_id = ?product_id,
            "report_selected_product: app chooser selection recorded",
        );
    }
    send_response(&sink, &request_id, FrontendEvent::SelectedProductReported { accepted });
}
