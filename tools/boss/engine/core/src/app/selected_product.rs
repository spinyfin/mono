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
    /// ISO-8601 UTC timestamp at which the report arrived, formatted
    /// once here so the wire type never has to.
    reported_at: String,
}

/// Rewrite a product-scoped resolution failure into one that names the
/// product, but only for the not-found case — every other error (an
/// ambiguity error naming candidates, a DB failure) is propagated
/// unchanged so it is never misread as "the row does not exist".
fn rewrite_not_found(err: anyhow::Error, id: &str, product_name: &str) -> anyhow::Error {
    if err.to_string().contains(boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER) {
        anyhow::anyhow!(
            "could not resolve id {id}: {} in product {product_name}",
            boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER
        )
    } else {
        err
    }
}

impl ServerState {
    /// Resolve a caller-supplied work-item selector at the engine boundary.
    /// Canonical ids and explicitly product-scoped short ids retain their
    /// existing semantics. A bare short id is scoped to the product currently
    /// selected in the connected app, because short ids are only unique within
    /// a product — but only when a selection is actually available. When the
    /// app is disconnected or has not reported a selection, this falls back
    /// to the pre-existing global resolution (`work_db.resolve_work_item_ref`),
    /// which still never guesses: an id unique across products resolves, and
    /// an ambiguous one is a hard error naming every candidate. This keeps
    /// `boss task show T<n>` working with the app closed, matching the
    /// documented contract on `resolve_short_id_item`
    /// (tools/boss/cli/src/data.rs) that globally-unique short ids resolve
    /// without `--product`.
    pub(super) async fn resolve_work_item_id(&self, id: &str) -> anyhow::Result<String> {
        let selector = boss_protocol::parse_work_item_selector(id);
        let boss_protocol::WorkItemSelector::ShortId(short_id) = selector else {
            return self.work_db.resolve_work_item_ref(id);
        };

        let SelectedProductState::Selected { product_id, name, .. } = self.selected_product_state().await else {
            return self.work_db.resolve_work_item_ref(id);
        };
        self.work_db
            .resolve_work_item_ref(&format!("{product_id}/{short_id}"))
            .map_err(|err| rewrite_not_found(err, id, &name))
    }

    /// Resolve a restore selector without excluding a tombstoned row. The
    /// restore database operation owns the final lookup because it is the one
    /// engine path that intentionally includes deleted work items. Mirrors
    /// [`Self::resolve_work_item_id`]'s fallback: with no selected product,
    /// resolution runs globally across products (ambiguity still a hard
    /// error) rather than refusing outright.
    pub(super) async fn resolve_work_item_id_for_restore(&self, id: &str) -> anyhow::Result<String> {
        let boss_protocol::WorkItemSelector::ShortId(short_id) = boss_protocol::parse_work_item_selector(id) else {
            return Ok(id.to_owned());
        };
        let (selector, product_name) = match self.selected_product_state().await {
            SelectedProductState::Selected { product_id, name, .. } => (format!("{product_id}/{short_id}"), Some(name)),
            _ => (id.to_owned(), None),
        };
        self.work_db
            .resolve_work_item_ref_for_restore(&selector)?
            .ok_or_else(|| match &product_name {
                Some(name) => anyhow::anyhow!(
                    "could not resolve id {id}: {} in product {name}",
                    boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER
                ),
                None => anyhow::anyhow!(
                    "could not resolve id {id}: {}",
                    boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER
                ),
            })
    }

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
            reported_at: boss_engine_utils::iso8601::format_epoch_iso8601(
                boss_engine_utils::epoch_time::now_epoch_secs(),
            ),
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
