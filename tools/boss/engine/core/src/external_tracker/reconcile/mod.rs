//! Reconciler core: `run_one_pass`, `spawn_loop`, and per-product processing.
//!
//! Implements Design Question 5 ("The Reconciler Loop") from
//! `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`.
//!
//! Behavior 5 (close-on-merge wiring per Design Question 8) is included:
//! after Boss-side SQL is committed, the reconciler issues `close_issue`
//! calls for each merged-PR-linked work item whose upstream is still `Open`.
//! Transient failures are logged and retried on the next tick; the retry
//! intent is derived from SQL state (`status='done'`, `pr_url IS NOT NULL`,
//! upstream still Open) so it survives engine crashes without a separate
//! persistence layer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

use super::credentials::{TrackerCredentialError, TrackerCredentialResolver};
use super::{TrackerContext, TrackerCredential, TrackerRegistry};
use crate::metrics::Registry;
use crate::work::WorkDb;

mod logic;

// ── Work-invalidation publisher ───────────────────────────────────────────────

/// Sink for work-invalidation broadcasts emitted by the reconciler.
///
/// Implemented by `ServerState` in production so live UI clients see
/// reconciler-driven mutations (import, close-mirror, PR-attach, unbind)
/// without waiting for a restart or product re-open.
/// `NoopWorkInvalidationPublisher` is used in tests and CLI single-pass paths.
#[async_trait]
pub trait WorkInvalidationPublisher: Send + Sync {
    async fn publish_work_item_invalidated(&self, product_id: &str, work_item_id: &str, reason: &str);
}

/// No-op implementation; used in tests and CLI paths where live UI
/// broadcast is not needed.
#[derive(Default)]
pub struct NoopWorkInvalidationPublisher;

#[async_trait]
impl WorkInvalidationPublisher for NoopWorkInvalidationPublisher {
    async fn publish_work_item_invalidated(&self, _: &str, _: &str, _: &str) {}
}

// ── Metrics ───────────────────────────────────────────────────────────────────

crate::register_counter!(
    FETCH_SUCCEEDED,
    "external_tracker.fetch_succeeded",
    "Upstream fetch calls that completed without error.",
);
crate::register_counter!(
    FETCH_FAILED,
    "external_tracker.fetch_failed",
    "Upstream fetch calls that errored; reconcile is skipped for that product.",
);
crate::register_counter!(
    IMPORTED,
    "external_tracker.imported",
    "New upstream items imported as Boss chores.",
);
crate::register_counter!(
    CLOSED,
    "external_tracker.closed",
    "Boss rows flipped to done because the upstream observed Closed (Behavior 2).",
);
crate::register_counter!(
    PR_ATTACHED,
    "external_tracker.pr_attached",
    "Boss rows that received a pr_url from an upstream PR association (Behavior 4).",
);
crate::register_counter!(
    PR_MERGE_CLOSE_SUCCEEDED,
    "external_tracker.pr_merge_close_succeeded",
    "close_issue calls that succeeded after a linked PR merged (Behavior 5).",
);
crate::register_counter!(
    PR_MERGE_CLOSE_FAILED,
    "external_tracker.pr_merge_close_failed",
    "close_issue calls that failed (transient or permission) after a linked PR merged (Behavior 5).",
);
crate::register_counter!(
    REVERSE_CLOSE_SUCCEEDED,
    "external_tracker.reverse_close_succeeded",
    "close_issue calls that succeeded from the reverse-close path (Behavior 3).",
);
crate::register_counter!(
    REVERSE_CLOSE_FAILED,
    "external_tracker.reverse_close_failed",
    "close_issue calls that failed from the reverse-close path (Behavior 3).",
);
crate::register_counter!(
    UNBOUND,
    "external_tracker.unbound",
    "Work items whose external ref was cleared because the upstream item left project scope.",
);
crate::register_counter!(
    SKIPPED_CLOSED_AT_FIRST_SIGHT,
    "external_tracker.skipped_closed_at_first_sight",
    "Upstream items already Closed at first import; skipped per the bootstrap rule.",
);
crate::register_counter!(
    SKIP_NO_CREDENTIAL,
    "external_tracker.skip_no_credential",
    "Products skipped because credential resolution failed.",
);
crate::register_counter!(
    IN_PROGRESS_SET_SUCCEEDED,
    "external_tracker.in_progress_set_succeeded",
    "set_project_status calls that succeeded when a task moved to active (Behavior 6).",
);
crate::register_counter!(
    IN_PROGRESS_SET_FAILED,
    "external_tracker.in_progress_set_failed",
    "set_project_status calls that failed when a task moved to active (Behavior 6).",
);
crate::register_counter!(
    TRACKED_LABEL_ATTACH_SUCCEEDED,
    "external_tracker.tracked_label_attach_succeeded",
    "add_label calls that succeeded when a fresh upstream item was imported.",
);
crate::register_counter!(
    TRACKED_LABEL_ATTACH_FAILED,
    "external_tracker.tracked_label_attach_failed",
    "add_label calls that failed when a fresh upstream item was imported.",
);
crate::register_counter!(
    TITLE_BODY_SYNCED,
    "external_tracker.title_body_synced",
    "Boss work items whose name/description were auto-synced from upstream because only the upstream side changed (Behavior 8).",
);
crate::register_counter!(
    TITLE_BODY_CONFLICT,
    "external_tracker.title_body_conflict",
    "Upstream title/body drift skipped because the Boss side was also edited since import — operator must reconcile (Behavior 8).",
);

/// Label that the reconciler attaches to upstream items it has imported,
/// so users browsing the upstream tracker can see which issues Boss mirrors.
const TRACKED_LABEL: &str = "tracked";

/// Register all reconciler metrics with the engine's registry.
/// Must be called from `crate::metrics_init::init_all`.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&FETCH_SUCCEEDED);
    registry.register_counter(&FETCH_FAILED);
    registry.register_counter(&IMPORTED);
    registry.register_counter(&CLOSED);
    registry.register_counter(&PR_ATTACHED);
    registry.register_counter(&PR_MERGE_CLOSE_SUCCEEDED);
    registry.register_counter(&PR_MERGE_CLOSE_FAILED);
    registry.register_counter(&REVERSE_CLOSE_SUCCEEDED);
    registry.register_counter(&REVERSE_CLOSE_FAILED);
    registry.register_counter(&UNBOUND);
    registry.register_counter(&SKIPPED_CLOSED_AT_FIRST_SIGHT);
    registry.register_counter(&SKIP_NO_CREDENTIAL);
    registry.register_counter(&IN_PROGRESS_SET_SUCCEEDED);
    registry.register_counter(&IN_PROGRESS_SET_FAILED);
    registry.register_counter(&TRACKED_LABEL_ATTACH_SUCCEEDED);
    registry.register_counter(&TRACKED_LABEL_ATTACH_FAILED);
    registry.register_counter(&TITLE_BODY_SYNCED);
    registry.register_counter(&TITLE_BODY_CONFLICT);
}

// ── Outcome ───────────────────────────────────────────────────────────────────

/// Per-pass aggregate outcome.  Returned by [`run_one_pass`] for the caller
/// (spawn loop, CLI verb) to emit into logs / metrics.
#[derive(Debug, Default, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct PassOutcome {
    pub products_processed: usize,
    pub products_skipped: usize,
    pub items_imported: usize,
    pub items_closed: usize,
    pub pr_attached: usize,
    /// Behavior 5: close_issue calls that succeeded after a linked PR merged.
    pub close_issue_succeeded: usize,
    /// Behavior 5: close_issue calls that failed after a linked PR merged.
    pub close_issue_failed: usize,
    pub items_unbound: usize,
    /// Behavior 3: close_issue calls that succeeded via reverse-close.
    pub reverse_close_succeeded: usize,
    /// Behavior 3: close_issue calls that failed via reverse-close.
    pub reverse_close_failed: usize,
    /// Behavior 6: set_project_status calls that succeeded when a task moved to active.
    pub in_progress_set_succeeded: usize,
    /// Behavior 6: set_project_status calls that failed when a task moved to active.
    pub in_progress_set_failed: usize,
    /// Behavior 7: tracked-label add_label calls that succeeded on import.
    pub tracked_label_attach_succeeded: usize,
    /// Behavior 7: tracked-label add_label calls that failed on import.
    pub tracked_label_attach_failed: usize,
    /// Behavior 8: items whose name/description were auto-synced from upstream.
    pub title_body_synced: usize,
    /// Behavior 8: items where upstream drift was skipped because boss side was also edited.
    pub title_body_conflict: usize,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run one full reconcile pass across every product that has an external
/// tracker binding (`external_tracker_kind IS NOT NULL`).
///
/// Per-product processing is sequential within a pass (intentional: avoids
/// parallel `JoinSet` complexity for the v1 scale of ~10 products).
/// Individual product failures are logged and counted without aborting the
/// pass for other products.
pub async fn run_one_pass(
    work_db: &WorkDb,
    registry: &TrackerRegistry,
    metrics: &Registry,
    publisher: &dyn WorkInvalidationPublisher,
    credential_resolver: &dyn TrackerCredentialResolver,
) -> PassOutcome {
    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "list_products failed; skipping external tracker pass");
            return PassOutcome::default();
        }
    };

    let mut outcome = PassOutcome::default();
    for product in products {
        let (kind, config) = match (product.external_tracker_kind, product.external_tracker_config) {
            (Some(k), Some(c)) => (k, c),
            _ => continue,
        };

        let tracker = match registry.get(&kind) {
            Ok(t) => t,
            Err(e) => {
                warn!(product_id = %product.id, %kind, error = %e,
                    "no tracker registered for kind; skipping product");
                outcome.products_skipped += 1;
                continue;
            }
        };

        let credential = match credential_resolver.resolve(&kind, &config).await {
            Ok(c) => c,
            Err(TrackerCredentialError::AuthFailed { host, detail }) => {
                SKIP_NO_CREDENTIAL.inc(metrics);
                warn!(
                    product_id = %product.id,
                    %kind,
                    %host,
                    %detail,
                    "credential resolution failed; skipping product this tick"
                );
                outcome.products_skipped += 1;
                continue;
            }
            Err(TrackerCredentialError::UnsupportedKind(_)) => TrackerCredential::ambient(),
        };

        let ctx = TrackerContext {
            product_id: product.id.clone(),
            config,
            credential,
        };

        logic::process_product(work_db, &*tracker, &product.id, &ctx, &mut outcome, metrics, publisher).await;
        outcome.products_processed += 1;
    }

    outcome
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
///
/// Fires immediately on spawn (mirrors `dep_unblock_sweep::spawn_loop`
/// and `merge_poller::spawn_loop`) so any stale upstream state is caught
/// at engine startup without waiting for the first interval to elapse.
///
/// Errors per product are logged and counted but never propagate — a
/// transient network blip must not crash the engine.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    registry: Arc<TrackerRegistry>,
    interval: Duration,
    metrics: Arc<Registry>,
    publisher: Arc<dyn WorkInvalidationPublisher>,
    credential_resolver: Arc<dyn TrackerCredentialResolver>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                registry.as_ref(),
                metrics.as_ref(),
                publisher.as_ref(),
                credential_resolver.as_ref(),
            )
            .await;
            if outcome.products_processed > 0
                || outcome.products_skipped > 0
                || outcome.items_imported > 0
                || outcome.items_closed > 0
                || outcome.pr_attached > 0
                || outcome.close_issue_succeeded > 0
                || outcome.close_issue_failed > 0
                || outcome.items_unbound > 0
                || outcome.in_progress_set_succeeded > 0
                || outcome.in_progress_set_failed > 0
                || outcome.tracked_label_attach_succeeded > 0
                || outcome.tracked_label_attach_failed > 0
                || outcome.title_body_synced > 0
                || outcome.title_body_conflict > 0
            {
                tracing::info!(
                    products_processed = outcome.products_processed,
                    products_skipped = outcome.products_skipped,
                    items_imported = outcome.items_imported,
                    items_closed = outcome.items_closed,
                    pr_attached = outcome.pr_attached,
                    close_issue_succeeded = outcome.close_issue_succeeded,
                    close_issue_failed = outcome.close_issue_failed,
                    items_unbound = outcome.items_unbound,
                    in_progress_set_succeeded = outcome.in_progress_set_succeeded,
                    in_progress_set_failed = outcome.in_progress_set_failed,
                    tracked_label_attach_succeeded = outcome.tracked_label_attach_succeeded,
                    tracked_label_attach_failed = outcome.tracked_label_attach_failed,
                    title_body_synced = outcome.title_body_synced,
                    title_body_conflict = outcome.title_body_conflict,
                    "external tracker reconciler: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single reconcile pass for one named product.
///
/// Used by the `boss product sync-external-tracker` CLI verb. Returns the
/// pass outcome for the caller to log; returns `None` if the product has no
/// external tracker binding or is not found.
pub async fn run_one_pass_for_product(
    work_db: &WorkDb,
    registry: &TrackerRegistry,
    metrics: &Registry,
    product_id: &str,
    publisher: &dyn WorkInvalidationPublisher,
    credential_resolver: &dyn TrackerCredentialResolver,
) -> Option<PassOutcome> {
    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "list_products failed");
            return None;
        }
    };

    let product = products.into_iter().find(|p| p.id == product_id)?;

    let (kind, config) = match (product.external_tracker_kind, product.external_tracker_config) {
        (Some(k), Some(c)) => (k, c),
        _ => return None,
    };

    let tracker = match registry.get(&kind) {
        Ok(t) => t,
        Err(e) => {
            warn!(product_id, %kind, error = %e, "no tracker registered for kind");
            return None;
        }
    };

    let credential = match credential_resolver.resolve(&kind, &config).await {
        Ok(c) => c,
        Err(TrackerCredentialError::AuthFailed { host, detail }) => {
            SKIP_NO_CREDENTIAL.inc(metrics);
            warn!(product_id, %kind, %host, %detail, "credential resolution failed; skipping product");
            return None;
        }
        Err(TrackerCredentialError::UnsupportedKind(_)) => TrackerCredential::ambient(),
    };

    let ctx = TrackerContext {
        product_id: product_id.to_owned(),
        config,
        credential,
    };

    let mut outcome = PassOutcome::default();
    logic::process_product(work_db, &*tracker, product_id, &ctx, &mut outcome, metrics, publisher).await;
    outcome.products_processed += 1;
    Some(outcome)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
