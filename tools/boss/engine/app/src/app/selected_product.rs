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

/// Resolve with an app default without letting it hide ambiguity. On a miss,
/// list explicit selectors in other products instead of silently switching.
fn resolve_with_selection(
    db: &boss_work_db::WorkDb,
    id: &str,
    selection: SelectedProductState,
    include_deleted: bool,
) -> anyhow::Result<String> {
    let resolve = |selector: &str| {
        if include_deleted {
            db.resolve_work_item_ref_including_deleted(selector)
        } else {
            db.resolve_work_item_ref(selector)
        }
    };
    let boss_protocol::WorkItemSelector::ShortId(short_id) = boss_protocol::parse_work_item_selector(id) else {
        return resolve(id);
    };
    let SelectedProductState::Selected { product_id, name, .. } = selection else {
        return resolve(id);
    };
    // Ambiguity is a global property, including when none of the candidates
    // belongs to the app's current selection. Preserve the full candidate list.
    let global = match resolve(id) {
        Err(err) if !err.to_string().contains(boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER) => return Err(err),
        result => result,
    };
    match resolve(&format!("{product_id}/{short_id}")) {
        // The selected product is only an implicit default, not permission to
        // pick one of several rows sharing this number.
        Ok(_) => global,
        Err(err) if err.to_string().contains(boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER) => {
            let mut suggestions = Vec::new();
            for product in db.list_products()? {
                if product.id == product_id {
                    continue;
                }
                match resolve(&format!("{}/{short_id}", product.id)) {
                    Ok(_) => suggestions.push(format!("{}/{short_id}", product.slug)),
                    Err(other) if other.to_string().contains(boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER) => {}
                    Err(other) => return Err(other),
                }
            }
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!("; did you mean {}?", suggestions.join(" or "))
            };
            anyhow::bail!(
                "could not resolve id {id}: {} in product {name} (selected in the app){hint}",
                boss_protocol::WORK_ITEM_ID_NOT_FOUND_MARKER
            )
        }
        Err(err) => Err(err),
    }
}

impl ServerState {
    /// Canonical and product-qualified selectors are explicit. Unqualified
    /// short ids must be globally unambiguous, even with an app selection.
    /// A miss in the app's selected product suggests qualified alternatives.
    pub(super) async fn resolve_work_item_id(&self, id: &str) -> anyhow::Result<String> {
        self.resolve_work_item_id_with_db(&self.work_db, id).await
    }

    pub(super) async fn resolve_work_item_id_with_db(
        &self,
        db: &boss_work_db::WorkDb,
        id: &str,
    ) -> anyhow::Result<String> {
        if !matches!(
            boss_protocol::parse_work_item_selector(id),
            boss_protocol::WorkItemSelector::ShortId(_)
        ) {
            return db.resolve_work_item_ref(id);
        }
        resolve_with_selection(db, id, self.selected_product_state_with_db(db).await, false)
    }

    /// Restore and dependency operations use the same scope rules, but include
    /// tombstoned rows so callers can restore or remove their dependency edges.
    pub(super) async fn resolve_work_item_id_including_deleted(&self, id: &str) -> anyhow::Result<String> {
        if !matches!(
            boss_protocol::parse_work_item_selector(id),
            boss_protocol::WorkItemSelector::ShortId(_)
        ) {
            return self.work_db.resolve_work_item_ref_including_deleted(id);
        }
        resolve_with_selection(&self.work_db, id, self.selected_product_state().await, true)
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
        self.selected_product_state_with_db(&self.work_db).await
    }

    /// Same as [`Self::selected_product_state`] but looks the product up on
    /// `db`, so read paths holding the read connection never touch the
    /// writer.
    pub(super) async fn selected_product_state_with_db(&self, db: &boss_work_db::WorkDb) -> SelectedProductState {
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

        match db.get_product(&product_id) {
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

/// The active products: the product selected in the app plus any named by
/// dispatch focus (directly, or through a focused project). Singles out
/// nothing — so the poller treats every product as active — when neither
/// names one: no app connected, nothing selected, focus mode off.
pub(super) struct PollerActiveProducts(pub(super) Weak<ServerState>);

#[async_trait]
impl boss_engine_merge_poller::ActiveProducts for PollerActiveProducts {
    async fn active_product_ids(&self) -> Option<HashSet<String>> {
        let state = self.0.upgrade()?;
        let mut products = HashSet::new();
        if let SelectedProductState::Selected { product_id, .. } = state.selected_product_state().await {
            products.insert(product_id);
        }
        match state.work_db.load_dispatch_focus() {
            Ok(focus) => {
                products.extend(focus.product_ids);
                for project_id in &focus.project_ids {
                    match state.work_db.get_project(project_id) {
                        Ok(project) => {
                            products.insert(project.product_id);
                        }
                        Err(err) => tracing::warn!(
                            ?err,
                            project_id = %project_id,
                            "merge poller: focused project not found; its product is not treated as active",
                        ),
                    }
                }
            }
            // An unreadable focus set singles nothing out; the selection, if
            // any, still counts.
            Err(err) => tracing::warn!(?err, "merge poller: could not read dispatch focus for active products"),
        }
        (!products.is_empty()).then_some(products)
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
