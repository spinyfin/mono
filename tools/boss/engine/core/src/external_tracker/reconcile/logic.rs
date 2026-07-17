//! Per-product reconcile logic: the core state machine that maps a fetched
//! upstream item set onto Boss work items.
//!
//! [`process_product`] is the entry point the pass runner calls; it fetches
//! upstream items, reconciles each against Boss state via [`reconcile_existing`]
//! and [`import_new`], and drains the deferred upstream API calls (close,
//! set-project-status, add-label) queued during reconcile. See the parent
//! module ([`super`]) for the pass-runner entry points and metric definitions.

use std::collections::{HashMap, HashSet};

use boss_protocol::{CREATED_VIA_EXTERNAL_TRACKER_SYNC, CreateChoreInput};
use tracing::{info, warn};

use crate::external_tracker::{
    CloseReason, ExternalTracker, TrackerContext, TrackerError, UpstreamItem, UpstreamPrAssociation, UpstreamRef,
    UpstreamStatus,
};
use crate::metrics::Registry;
use crate::work::{TaskStatus, WorkDb, content_checksum};

use super::{
    CLOSED, FETCH_FAILED, FETCH_SUCCEEDED, IMPORTED, IN_PROGRESS_SET_FAILED, IN_PROGRESS_SET_SUCCEEDED, PR_ATTACHED,
    PR_MERGE_CLOSE_FAILED, PR_MERGE_CLOSE_SUCCEEDED, PassOutcome, REVERSE_CLOSE_FAILED, REVERSE_CLOSE_SUCCEEDED,
    SKIPPED_CLOSED_AT_FIRST_SIGHT, TITLE_BODY_CONFLICT, TITLE_BODY_SYNCED, TRACKED_LABEL, TRACKED_LABEL_ATTACH_FAILED,
    TRACKED_LABEL_ATTACH_SUCCEEDED, UNBOUND, WorkInvalidationPublisher,
};

/// Which code path queued this close.  Drives metric selection in the close loop.
enum CloseTrigger {
    /// Behavior 5: a linked PR merged upstream.
    PrMerge,
    /// Behavior 3: boss row flipped to `done` without a merged PR (reverse-close).
    ReverseClose,
}

/// Carries intent to call `close_issue` on the upstream tracker after all
/// Boss-side SQL writes are done.
struct CloseCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
    trigger: CloseTrigger,
    /// PR URL to reference in the closing comment on the issue, if known.
    pr_url: Option<String>,
}

/// Carries intent to call `set_project_status` (Behavior 6) after all
/// Boss-side SQL writes are done.
struct InProgressCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
}

/// Carries intent to call `add_label` (Behavior 7 retry) for an already-imported
/// item that is missing the `tracked` label upstream.
struct LabelCandidate {
    work_item_id: String,
    upstream_ref: UpstreamRef,
}

/// Read-only handles and per-product config threaded through the per-item
/// reconcile helpers ([`reconcile_existing`], [`import_new`]). Bundling
/// these keeps the helper signatures under clippy's argument-count
/// threshold and avoids repeating the same handle list at every call site.
#[derive(bon::Builder)]
struct ProductReconcileCtx<'a> {
    work_db: &'a WorkDb,
    tracker: &'a dyn ExternalTracker,
    ctx: &'a TrackerContext,
    product_id: &'a str,
    reverse_close: bool,
    in_progress_column: &'a str,
    metrics: &'a Registry,
    publisher: &'a dyn WorkInvalidationPublisher,
}

/// Mutable accumulators for the deferred upstream API calls a reconcile pass
/// queues (close, in-progress move, label-add). Populated per item, then
/// drained after the Boss-side SQL for the whole product has committed.
#[derive(Default)]
struct ReconcileCandidates {
    close: Vec<CloseCandidate>,
    in_progress: Vec<InProgressCandidate>,
    label: Vec<LabelCandidate>,
}

pub(super) async fn process_product(
    work_db: &WorkDb,
    tracker: &dyn ExternalTracker,
    product_id: &str,
    ctx: &TrackerContext,
    outcome: &mut PassOutcome,
    metrics: &Registry,
    publisher: &dyn WorkInvalidationPublisher,
) {
    let reverse_close = ctx.config["reverse_close"].as_bool().unwrap_or(false);
    let in_progress_column = ctx.config["in_progress_column"]
        .as_str()
        .unwrap_or("In Progress")
        .to_owned();

    // ── 1. Fetch upstream items ───────────────────────────────────────────────
    let upstream_items = match tracker.fetch_items(ctx).await {
        Ok(items) => {
            FETCH_SUCCEEDED.inc(metrics);
            // Clear any stale fetch-failure attention items now that the
            // fetch has succeeded.
            for kind in &[
                "external_tracker_auth_failed",
                "external_tracker_token_revoked",
                "external_tracker_transient_errors",
            ] {
                if let Err(e) = work_db.resolve_external_tracker_attention(product_id, kind) {
                    warn!(product_id, %kind, error = %e, "resolve_external_tracker_attention failed");
                }
            }
            items
        }
        Err(ref e @ TrackerError::TokenRevoked(ref msg)) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e, "fetch_items 401: OAuth token revoked; skipping product this tick");
            let title = format!("GitHub OAuth token revoked for product {product_id}");
            let body = format!(
                "Boss received HTTP 401 from GitHub — the stored OAuth token has been revoked or expired: {msg}\n\n\
                 Please reconnect via Settings → Issue Sync → Connect to authorize a new token."
            );
            if let Err(attn_err) =
                work_db.upsert_external_tracker_attention(product_id, "external_tracker_token_revoked", &title, &body)
            {
                warn!(product_id, error = %attn_err,
                    "upsert_external_tracker_attention (token_revoked) failed");
            }
            return;
        }
        Err(ref e @ TrackerError::Auth(ref msg)) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e, "fetch_items auth failure; skipping product this tick");
            let title = format!("External tracker auth failed for product {product_id}");
            let body = format!(
                "Boss could not authenticate with the external tracker: {msg}\n\n\
                 This may indicate an org approval or SSO authorization is needed. \
                 Check your GitHub org settings, or run `gh auth login` to refresh credentials."
            );
            if let Err(attn_err) =
                work_db.upsert_external_tracker_attention(product_id, "external_tracker_auth_failed", &title, &body)
            {
                warn!(product_id, error = %attn_err,
                    "upsert_external_tracker_attention (auth_failed) failed");
            }
            return;
        }
        Err(ref e @ TrackerError::Transient(ref msg)) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e,
                "fetch_items transient error; skipping product this tick");
            let title = format!("External tracker fetch failing for product {product_id}");
            let body = format!(
                "Boss is unable to reach the external tracker: {msg}\n\n\
                 This is usually a transient network issue. Boss will retry automatically."
            );
            if let Err(attn_err) = work_db.upsert_external_tracker_attention(
                product_id,
                "external_tracker_transient_errors",
                &title,
                &body,
            ) {
                warn!(product_id, error = %attn_err,
                    "upsert_external_tracker_attention (transient_errors) failed");
            }
            return;
        }
        Err(e) => {
            FETCH_FAILED.inc(metrics);
            warn!(product_id, error = %e, "fetch_items failed; skipping product this tick");
            return;
        }
    };

    // Fast lookup: canonical_id → upstream item.
    let upstream_map: HashMap<&str, &UpstreamItem> = upstream_items
        .iter()
        .map(|item| (item.upstream_ref.canonical_id.as_str(), item))
        .collect();

    // ── 2. Load existing bindings ─────────────────────────────────────────────
    // Includes unbound rows (external_ref_unbound_at IS NOT NULL) so the
    // reconciler can automatically re-bind items that reappear upstream.
    let existing = match work_db.list_external_refs_for_product(product_id) {
        Ok(refs) => refs,
        Err(e) => {
            warn!(product_id, error = %e, "list_external_refs_for_product failed");
            return;
        }
    };

    // Canonical-ids already known in Boss (active OR previously unbound).
    let known_canonical_ids: HashSet<&str> = existing.iter().map(|(_, r)| r.canonical_id.as_str()).collect();

    let rctx = ProductReconcileCtx {
        work_db,
        tracker,
        ctx,
        product_id,
        reverse_close,
        in_progress_column: &in_progress_column,
        metrics,
        publisher,
    };
    let mut candidates = ReconcileCandidates::default();

    // ── 3. Reconcile each upstream item ───────────────────────────────────────
    for item in &upstream_items {
        let canonical_id = &item.upstream_ref.canonical_id;

        match work_db.find_by_external_ref(&item.upstream_ref.kind, canonical_id) {
            Ok(Some(task)) => {
                reconcile_existing(&rctx, &task, item, &mut candidates, outcome).await;
            }
            Ok(None) => {
                if known_canonical_ids.contains(canonical_id.as_str()) {
                    // Row exists but is unbound — re-bind and reconcile.
                    if let Some((work_item_id, stored_ref)) =
                        existing.iter().find(|(_, r)| r.canonical_id == *canonical_id)
                    {
                        if let Err(e) = work_db.set_external_ref(
                            work_item_id,
                            &item.upstream_ref.kind,
                            &item.upstream_ref.canonical_id,
                            &item.upstream_ref.raw,
                        ) {
                            warn!(
                                work_item_id,
                                canonical_id, error = %e,
                                "re-bind set_external_ref failed"
                            );
                            continue;
                        }
                        // Now the row is active; reconcile normally.
                        match work_db.find_by_external_ref(&stored_ref.kind, &stored_ref.canonical_id) {
                            Ok(Some(task)) => {
                                reconcile_existing(&rctx, &task, item, &mut candidates, outcome).await;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(work_item_id, error = %e, "find_by_external_ref after re-bind failed");
                            }
                        }
                    }
                } else {
                    import_new(&rctx, item, outcome).await;
                }
            }
            Err(e) => {
                warn!(canonical_id, error = %e, "find_by_external_ref failed");
            }
        }
    }

    // ── 4. Unbind items removed from the upstream project ────────────────────
    for (work_item_id, stored_ref) in &existing {
        if stored_ref.unbound_at.is_some() {
            continue; // Already unbound; skip.
        }
        if !upstream_map.contains_key(stored_ref.canonical_id.as_str()) {
            match work_db.clear_external_ref(work_item_id) {
                Ok(()) => {
                    UNBOUND.inc(metrics);
                    outcome.items_unbound += 1;
                    info!(
                        work_item_id,
                        canonical_id = %stored_ref.canonical_id,
                        "upstream item no longer in project scope; external ref unbound"
                    );
                    publisher
                        .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                        .await;
                    let title = format!("Upstream binding for {} cleared", stored_ref.canonical_id);
                    let body = format!(
                        "`{}` was bound to upstream `{}` which is no longer in the configured \
                         project. The link has been cleared; re-bind manually with \
                         `boss chore link-external` if this was unintended.",
                        work_item_id, stored_ref.canonical_id
                    );
                    if let Err(e) = work_db.upsert_external_tracker_attention(
                        work_item_id,
                        "external_tracker_removed_upstream",
                        &title,
                        &body,
                    ) {
                        warn!(work_item_id, error = %e,
                            "upsert_external_tracker_attention (removed_upstream) failed");
                    }
                }
                Err(e) => {
                    warn!(work_item_id, error = %e, "clear_external_ref failed");
                }
            }
        }
    }

    // ── 5. Issue close calls post-commit (Behavior 5 and Behavior 3) ──────────
    // Cap at 20 per tick to avoid saturating the rate-limit window.
    const CLOSE_BUDGET: usize = 20;
    for candidate in candidates.close.into_iter().take(CLOSE_BUDGET) {
        let is_b3 = matches!(candidate.trigger, CloseTrigger::ReverseClose);
        match tracker
            .close_issue(ctx, &candidate.upstream_ref, CloseReason::Completed)
            .await
        {
            Ok(()) => {
                if is_b3 {
                    REVERSE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.reverse_close_succeeded += 1;
                    info!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        "Behavior 3: upstream issue closed via reverse-close"
                    );
                } else {
                    PR_MERGE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.close_issue_succeeded += 1;
                    info!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        "Behavior 5: upstream issue closed after merged PR"
                    );
                }
                if let Some(ref pr_url) = candidate.pr_url
                    && let Err(e) = tracker
                        .post_closing_pr_comment(ctx, &candidate.upstream_ref, pr_url)
                        .await
                {
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "post_closing_pr_comment failed (non-fatal); PR linkage comment will be missing"
                    );
                }
            }
            Err(TrackerError::NotFound(_)) => {
                // Issue already closed (404). Treat as success.
                if is_b3 {
                    REVERSE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.reverse_close_succeeded += 1;
                } else {
                    PR_MERGE_CLOSE_SUCCEEDED.inc(metrics);
                    outcome.close_issue_succeeded += 1;
                }
            }
            Err(ref e @ TrackerError::PermissionDenied(ref msg)) => {
                if is_b3 {
                    REVERSE_CLOSE_FAILED.inc(metrics);
                    outcome.reverse_close_failed += 1;
                } else {
                    PR_MERGE_CLOSE_FAILED.inc(metrics);
                    outcome.close_issue_failed += 1;
                }
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "close_issue permission denied; credential lacks write scope"
                );
                let title = format!(
                    "Cannot close upstream issue {} — permission denied",
                    candidate.upstream_ref.canonical_id
                );
                let body = format!(
                    "Boss could not close upstream issue `{}`: {msg}\n\n\
                     The credential lacks `issues:write` scope. \
                     Re-run `gh auth login --scopes repo` to grant write permission, \
                     or close the issue manually.",
                    candidate.upstream_ref.canonical_id
                );
                if let Err(e) = work_db.upsert_external_tracker_attention(
                    &candidate.work_item_id,
                    "external_tracker_permission_denied",
                    &title,
                    &body,
                ) {
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        error = %e,
                        "upsert_external_tracker_attention (permission_denied) failed"
                    );
                }
            }
            Err(e) => {
                if is_b3 {
                    REVERSE_CLOSE_FAILED.inc(metrics);
                    outcome.reverse_close_failed += 1;
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "Behavior 3: reverse-close failed (transient); will retry next tick"
                    );
                } else {
                    PR_MERGE_CLOSE_FAILED.inc(metrics);
                    outcome.close_issue_failed += 1;
                    warn!(
                        work_item_id = %candidate.work_item_id,
                        canonical_id = %candidate.upstream_ref.canonical_id,
                        error = %e,
                        "Behavior 5: close_issue failed (transient); will retry next tick"
                    );
                }
            }
        }
    }

    // ── 6. Set project status to "In progress" (Behavior 6) ──────────────────
    // Fires when a Boss task entered the active (Doing) state and the upstream
    // item's project column is not already at the configured in-progress value.
    // Cap at 20 per tick to match the close-candidates budget.
    const IN_PROGRESS_BUDGET: usize = 20;
    for candidate in candidates.in_progress.into_iter().take(IN_PROGRESS_BUDGET) {
        match tracker.set_project_status(ctx, &candidate.upstream_ref).await {
            Ok(()) => {
                IN_PROGRESS_SET_SUCCEEDED.inc(metrics);
                outcome.in_progress_set_succeeded += 1;
                info!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    "Behavior 6: project status set to In progress"
                );
            }
            Err(e) => {
                IN_PROGRESS_SET_FAILED.inc(metrics);
                outcome.in_progress_set_failed += 1;
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "Behavior 6: set_project_status failed (transient); will retry next tick"
                );
            }
        }
    }

    // ── 7. Retroactively attach `tracked` label (Behavior 7 retry) ───────────
    // Items whose initial label-add failed (e.g. due to the T630 string-not-array
    // bug) are re-attempted on every reconcile pass until the label is confirmed
    // present in the upstream fetch. Cap at 20 to match other budgets.
    const LABEL_BUDGET: usize = 20;
    for candidate in candidates.label.into_iter().take(LABEL_BUDGET) {
        match tracker.add_label(ctx, &candidate.upstream_ref, TRACKED_LABEL).await {
            Ok(()) => {
                TRACKED_LABEL_ATTACH_SUCCEEDED.inc(metrics);
                outcome.tracked_label_attach_succeeded += 1;
                info!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    "Behavior 7: tracked label attached (reconcile retry)"
                );
            }
            Err(e) => {
                TRACKED_LABEL_ATTACH_FAILED.inc(metrics);
                outcome.tracked_label_attach_failed += 1;
                warn!(
                    work_item_id = %candidate.work_item_id,
                    canonical_id = %candidate.upstream_ref.canonical_id,
                    error = %e,
                    "Behavior 7: add_label failed on reconcile retry; will retry next tick"
                );
            }
        }
    }
}

// ── Per-item helpers ──────────────────────────────────────────────────────────

/// Reconcile an existing Boss work item against the current upstream state.
///
/// - **Behavior 4** (PR attach): if `pr_url` is null and upstream has PR
///   associations, write the best URL.
/// - **Behavior 2** (close-mirror): if upstream is `Closed` and boss is not
///   `done`, flip the boss row.
/// - **Behavior 5** (PR-merge close): if upstream is `Open` and either a
///   merged PR is present in the associations, or the boss row is already
///   `done` with a `pr_url` (retry path), queue a `close_issue` call.
/// - **Behavior 3** (reverse-close, opt-in): if upstream is `Open`, boss is
///   `done`, no merged PR drove the transition, and `reverse_close=true` in
///   the product config, queue a `close_issue` call.
/// - **Behavior 7** (tracked-label retry): if the `tracked` label is absent
///   upstream, queue an `add_label` call so the label converges even when
///   the initial import-time attach failed.
/// - Always bumps `external_ref_synced_at`.
async fn reconcile_existing(
    rctx: &ProductReconcileCtx<'_>,
    task: &boss_protocol::Task,
    upstream: &UpstreamItem,
    candidates: &mut ReconcileCandidates,
    outcome: &mut PassOutcome,
) {
    let &ProductReconcileCtx {
        work_db,
        reverse_close,
        in_progress_column,
        metrics,
        product_id,
        publisher,
        ..
    } = rctx;
    let ReconcileCandidates {
        close: close_candidates,
        in_progress: in_progress_candidates,
        label: label_candidates,
    } = candidates;

    let work_item_id = &task.id;

    // Behavior 4: attach a PR URL if the boss row currently has none.
    if task.pr_url.as_deref().unwrap_or("").is_empty()
        && let Some(best_pr) = pick_best_pr(&upstream.pr_associations)
    {
        match work_db.reconciler_attach_pr_url(work_item_id, &best_pr.pr_url) {
            Ok(true) => {
                PR_ATTACHED.inc(metrics);
                outcome.pr_attached += 1;
                info!(work_item_id, pr_url = %best_pr.pr_url, "Behavior 4: pr_url attached");
                publisher
                    .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                    .await;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(work_item_id, error = %e, "reconciler_attach_pr_url failed");
            }
        }
    }

    match &upstream.status {
        UpstreamStatus::Closed { .. } => {
            // Behavior 2: close-mirror — upstream is done, boss must follow.
            if task.status != TaskStatus::Done && task.status != TaskStatus::Archived {
                match work_db.reconciler_close_work_item(work_item_id) {
                    Ok(true) => {
                        CLOSED.inc(metrics);
                        outcome.items_closed += 1;
                        info!(work_item_id, "Behavior 2: close-mirror — upstream Closed → boss done");
                        publisher
                            .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                            .await;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(work_item_id, error = %e, "reconciler_close_work_item failed (Behavior 2)");
                    }
                }
            }
        }
        UpstreamStatus::Open => {
            // Behavior 5: close-on-merge.
            let has_merged_pr = upstream.pr_associations.iter().any(|p| p.merged);
            let boss_is_done = task.status == TaskStatus::Done || task.status == TaskStatus::Archived;
            let boss_has_pr = !task.pr_url.as_deref().unwrap_or("").is_empty();

            if has_merged_pr && !boss_is_done {
                // Merged PR detected upstream but boss row not yet done → flip it.
                match work_db.reconciler_close_work_item(work_item_id) {
                    Ok(true) => {
                        outcome.items_closed += 1;
                        info!(work_item_id, "Behavior 5: merged PR detected → boss row → done");
                        publisher
                            .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                            .await;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(work_item_id, error = %e, "reconciler_close_work_item failed (Behavior 5)");
                    }
                }
            }

            // Queue close_issue for Behavior 5 if:
            //   (a) merged PR detected in upstream associations, OR
            //   (b) boss is already done with a pr_url (retry from prior failed close)
            if has_merged_pr || (boss_is_done && boss_has_pr) {
                let pr_url = if has_merged_pr {
                    pick_best_pr(&upstream.pr_associations).map(|p| p.pr_url.clone())
                } else {
                    task.pr_url.clone()
                };
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::PrMerge,
                    pr_url,
                });
            } else if reverse_close && boss_is_done {
                // Behavior 3: boss done without a merged PR driving the
                // transition.  Only queue if B5 didn't already claim it
                // (guarded by the `else` branch above).
                close_candidates.push(CloseCandidate {
                    work_item_id: work_item_id.clone(),
                    upstream_ref: upstream.upstream_ref.clone(),
                    trigger: CloseTrigger::ReverseClose,
                    pr_url: task.pr_url.clone(),
                });
            }

            // Behavior 6: mirror boss→active to the upstream project column.
            // Queue only when the task is active (Doing) and the upstream
            // project status is not already the target column; this prevents
            // a regression if the user has manually advanced the item to a
            // later column while the task is still in progress.
            if task.status == TaskStatus::Active {
                let already_at_target = upstream.project_status.as_deref() == Some(in_progress_column);
                if !already_at_target {
                    in_progress_candidates.push(InProgressCandidate {
                        work_item_id: work_item_id.clone(),
                        upstream_ref: upstream.upstream_ref.clone(),
                    });
                }
            }
        }
    }

    // Behavior 7 (retry): if the upstream item doesn't carry the `tracked`
    // label, queue a label-add so the label converges on the next pass.
    // This catches items whose initial import-time add_label failed.
    if !upstream.labels.iter().any(|l| l == TRACKED_LABEL) {
        label_candidates.push(LabelCandidate {
            work_item_id: work_item_id.clone(),
            upstream_ref: upstream.upstream_ref.clone(),
        });
    }

    // Behavior 8: upstream title/body drift — compare SHA-256 checksums of the
    // current upstream content against the stored baseline and auto-sync or
    // flag conflicts. Checksums avoid storing full content while preserving all
    // three distinctions: upstream-only changed, both changed, nothing changed.
    //
    // Policy:
    //   • Only upstream changed → auto-sync (title and description).
    //   • Both sides changed → warn and emit a metric; operator must reconcile.
    //   • No baseline (pre-migration import) → establish baseline silently.
    //   • Only boss changed → operator edit; leave it alone.
    match work_db.reconciler_get_content_checksums(work_item_id) {
        Err(e) => {
            warn!(work_item_id, error = %e, "reconciler_get_content_checksums failed (Behavior 8)");
        }
        Ok(None) => {
            // Pre-migration item: no baseline yet. Record checksums of the
            // current upstream and boss content without auto-syncing (we can't
            // tell if the boss side has been edited since import).
            if let Err(e) = work_db.reconciler_set_content_checksums_baseline(
                work_item_id,
                &upstream.title,
                &upstream.body,
                &task.name,
                &task.description,
            ) {
                warn!(
                    work_item_id,
                    error = %e,
                    "reconciler_set_content_checksums_baseline failed (Behavior 8)"
                );
            }
        }
        Ok(Some((stored_upstream_checksum, stored_boss_checksum))) => {
            let current_upstream_checksum = content_checksum(&upstream.title, &upstream.body);
            let upstream_changed = current_upstream_checksum != stored_upstream_checksum;

            if upstream_changed {
                // Check whether the boss side has diverged from the last-synced baseline.
                let current_boss_checksum = content_checksum(&task.name, &task.description);
                let boss_changed = current_boss_checksum != stored_boss_checksum;

                if !boss_changed {
                    // Only the upstream changed → auto-sync name and description.
                    let new_name = upstream.title.clone();
                    let new_desc = format!("> Imported from {}\n\n{}", upstream.upstream_url, upstream.body);
                    match work_db.reconciler_update_name_and_description(
                        work_item_id,
                        &new_name,
                        &new_desc,
                        &upstream.title,
                        &upstream.body,
                    ) {
                        Ok(true) => {
                            TITLE_BODY_SYNCED.inc(metrics);
                            outcome.title_body_synced += 1;
                            info!(
                                work_item_id,
                                canonical_id = %upstream.upstream_ref.canonical_id,
                                "Behavior 8: upstream title/body changed → boss row auto-synced"
                            );
                            publisher
                                .publish_work_item_invalidated(product_id, work_item_id, "chore_updated")
                                .await;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!(
                                work_item_id,
                                error = %e,
                                "reconciler_update_name_and_description failed (Behavior 8)"
                            );
                        }
                    }
                } else {
                    // Both sides changed → flag for operator attention, preserve boss edits.
                    TITLE_BODY_CONFLICT.inc(metrics);
                    outcome.title_body_conflict += 1;
                    warn!(
                        work_item_id,
                        canonical_id = %upstream.upstream_ref.canonical_id,
                        upstream_title = %upstream.title,
                        boss_name = %task.name,
                        "Behavior 8: upstream title/body drift detected but boss side was also \
                         edited — skipping auto-sync; operator must reconcile manually"
                    );
                }
            }
        }
    }

    // Bump synced_at every successful reconcile.
    if let Err(e) = work_db.touch_external_ref_synced_at(work_item_id) {
        warn!(work_item_id, error = %e, "touch_external_ref_synced_at failed");
    }
}

/// Import an upstream item that has no Boss mirror yet.
///
/// Skip if the item is already `Closed` at first sight (bootstrap rule from
/// Design Q7: turning on a binding must not flood Boss with historic closed
/// issues).
async fn import_new(rctx: &ProductReconcileCtx<'_>, upstream: &UpstreamItem, outcome: &mut PassOutcome) {
    let &ProductReconcileCtx {
        work_db,
        tracker,
        ctx,
        product_id,
        metrics,
        publisher,
        ..
    } = rctx;

    // Bootstrap rule: skip items that are already closed.
    if matches!(upstream.status, UpstreamStatus::Closed { .. }) {
        SKIPPED_CLOSED_AT_FIRST_SIGHT.inc(metrics);
        info!(
            canonical_id = %upstream.upstream_ref.canonical_id,
            "skipping already-closed upstream item at first import (bootstrap rule)"
        );
        return;
    }

    let description = format!("> Imported from {}\n\n{}", upstream.upstream_url, upstream.body);

    let input = CreateChoreInput::builder()
        .product_id(product_id)
        .name(upstream.title.clone())
        .description(description)
        .autostart(false)
        .created_via(CREATED_VIA_EXTERNAL_TRACKER_SYNC)
        .force_duplicate(true)
        .build();

    // Use the atomic import method so the chore row and its external_ref
    // binding are committed together. A plain create_chore + set_external_ref
    // pair leaves a crash window where the chore exists but has no ref,
    // making it invisible to the reconciler and breaking reverse_close.
    // upstream.title and upstream.body seed the Behavior 8 drift baseline.
    let chore = match work_db.import_chore_with_external_ref(
        input,
        &upstream.upstream_ref.kind,
        &upstream.upstream_ref.canonical_id,
        &upstream.upstream_ref.raw,
        &upstream.title,
        &upstream.body,
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                canonical_id = %upstream.upstream_ref.canonical_id,
                error = %e,
                "import_chore_with_external_ref failed; skipping upstream item"
            );
            return;
        }
    };

    // Attach a PR URL if one is already associated upstream.
    if let Some(pr) = pick_best_pr(&upstream.pr_associations)
        && let Err(e) = work_db.reconciler_attach_pr_url(&chore.id, &pr.pr_url)
    {
        warn!(work_item_id = %chore.id, error = %e, "reconciler_attach_pr_url failed after import");
    }

    publisher
        .publish_work_item_invalidated(product_id, &chore.id, "chore_created")
        .await;

    IMPORTED.inc(metrics);
    outcome.items_imported += 1;
    info!(
        work_item_id = %chore.id,
        canonical_id = %upstream.upstream_ref.canonical_id,
        "imported new upstream item as Boss chore"
    );

    // Behavior 7: attach the `tracked` label so humans browsing the upstream
    // tracker can see which issues Boss is mirroring. Skip the API call when
    // the label is already present; failures are logged but never block import.
    if upstream.labels.iter().any(|l| l == TRACKED_LABEL) {
        return;
    }
    match tracker.add_label(ctx, &upstream.upstream_ref, TRACKED_LABEL).await {
        Ok(()) => {
            TRACKED_LABEL_ATTACH_SUCCEEDED.inc(metrics);
            outcome.tracked_label_attach_succeeded += 1;
            info!(
                work_item_id = %chore.id,
                canonical_id = %upstream.upstream_ref.canonical_id,
                "Behavior 7: tracked label attached to upstream item"
            );
        }
        Err(e) => {
            TRACKED_LABEL_ATTACH_FAILED.inc(metrics);
            outcome.tracked_label_attach_failed += 1;
            warn!(
                work_item_id = %chore.id,
                canonical_id = %upstream.upstream_ref.canonical_id,
                error = %e,
                "Behavior 7: add_label failed; import continues, will retry on next sync of this item only if re-imported"
            );
        }
    }
}

/// Pick the best PR to use as the `pr_url`: prefer merged (highest `merged_at`),
/// then fall back to any unmerged PR association.
pub(super) fn pick_best_pr(associations: &[UpstreamPrAssociation]) -> Option<&UpstreamPrAssociation> {
    let merged = associations
        .iter()
        .filter(|p| p.merged)
        .max_by_key(|p| (p.merged_at.unwrap_or(0), p.pr_url.as_str()));
    if merged.is_some() {
        return merged;
    }
    associations.iter().max_by_key(|p| p.pr_url.as_str())
}
