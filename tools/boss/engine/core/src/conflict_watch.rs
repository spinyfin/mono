//! Detection-trigger pipeline for merge-conflict handling on
//! `in_review` PRs (`tools/boss/docs/designs/merge-conflict-handling-in-review.md`).
//!
//! Two entry points, both invoked from `merge_poller::sweep_one`:
//!
//!   - [`on_conflict_detected`] — fired when the probe reports a PR
//!     in [`OpenPrMergeability::Conflict`]. Flips the parent
//!     `tasks` row from `in_review` to `blocked: merge_conflict`
//!     unless the auto-rebase flow already owns the slot (design
//!     Q7) or the WHERE-guard misses (human moved the row).
//!
//!   - [`on_resolved`] — fired when the probe reports a previously
//!     conflicting PR back in [`OpenPrMergeability::Clean`]. Flips
//!     the parent back to `in_review`, flips the engine-owned
//!     `conflict_resolutions` row to `succeeded`, and releases the
//!     worker's cube lease (design Q5). The WHERE guard ensures we
//!     only undo engine-owned transitions; a human who manually
//!     reclassified the row stays in charge.
//!
//! Both transitions are idempotent: a second call for the same
//! `(work_item, pr_url)` finds the row already in the target state
//! and updates zero rows, so re-firing on every sweep is harmless.
//!
//! Worker spawn lives in Phase 3 (`runner.rs`); this module reads the
//! attempt row written by that path to drive the retire side.
//!
//! [`OpenPrMergeability`]: crate::merge_poller::OpenPrMergeability

#[cfg(test)]
use boss_protocol::TaskKind;
use boss_protocol::{CREATED_VIA_MERGE_CONFLICT_PREFIX, CreateRevisionInput, EffortLevel, FrontendEvent};

use crate::blocking_signal::{self, SignalKind};
use crate::conflict_ladder;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::merge_poller::{PrLifecycleProbe, parse_pr_number, pr_labels_opt_out};
#[cfg(test)]
use crate::work::TaskStatus;
use crate::work::{ConflictResolutionInsertInput, PendingMergeCheck, PrStateChecker, WorkDb};

/// Decide whether the unified `auto_pr_maintenance_enabled` opt-out
/// (per-product flag or per-PR label) suppresses this conflict-watch
/// transition. Returns `true` to gate the path off, logging at debug
/// for traceability. DB-read errors fall through to "not opted out"
/// so a transient lookup failure doesn't silently drop a real signal —
/// the per-PR label is the second line of defence in that case.
///
/// Phase 6 #18 / design Q7: both gates fire on either the conflict
/// flip or the retire path; "opted out" means leave the row alone.
fn auto_pr_maintenance_disabled(work_db: &WorkDb, candidate: &PendingMergeCheck, labels: &[String]) -> bool {
    if pr_labels_opt_out(labels) {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "conflict_watch: PR labelled with opt-out; skipping",
        );
        return true;
    }
    match work_db.product_auto_pr_maintenance_enabled(&candidate.product_id) {
        Ok(true) => false,
        Ok(false) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: product opted out of auto_pr_maintenance; skipping",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                ?err,
                "conflict_watch: failed to read auto_pr_maintenance_enabled; treating as enabled",
            );
            false
        }
    }
}

/// Conflict-detection entry point — creates a `conflict_resolutions`
/// attempt and dispatches an engine-triggered revision fix vehicle when
/// the probe reports `OpenPrMergeability::Conflict`.
///
/// **Parent-state model (post-revision-unification):**
/// While an active conflict-resolution revision is in flight, the
/// parent stays in `in_review` (Review column) — exactly as a normal
/// revision leaves its parent. The parent flips to
/// `blocked: merge_conflict` only when there is no tractable fix
/// vehicle: the churn cap was exceeded, or `create_revision` failed
/// (parent PR no longer revisable). That is the genuine "needs a
/// human" terminal.
///
/// Implementation note: we still call `mark_chore_blocked_merge_conflict`
/// as the upfront WHERE guard (it enforces `status='in_review'` to
/// protect against human-moved rows), then immediately clear it back to
/// `in_review` and upsert the `task_blocked_signals` row whenever a
/// revision is successfully spawned. The brief intermediate `blocked`
/// state is invisible to the sweep — the entire detect → spawn → unblock
/// sequence runs within a single call.
///
/// Returns `true` when the parent task status changed (in either
/// direction) or a fresh attempt row was created; `false` for purely
/// idempotent repeat probes and human-owned rows.
///
/// **Escalation ladder (T4):** when `cube_client` is `Some`, a fresh conflict
/// first attempts the engine-direct mechanical rebase (rung 1) before spawning
/// a worker — see [`crate::conflict_ladder`]. A clean rebase resolves and
/// pushes the PR with no agent; the harness retires the attempt and this
/// returns early. `None` (the default at call sites, and whenever the
/// `conflict_ladder_mechanical_rebase` flag is off) preserves the pre-ladder
/// worker-only path exactly.
pub async fn on_conflict_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    // Phase 6 #18 / Q7: the unified opt-out gates the entire flow.
    // Check it first so opted-out products never even probe the
    // rebase-attempt table or touch the parent row.
    if auto_pr_maintenance_disabled(work_db, candidate, &probe.labels) {
        return false;
    }
    // Q7: when `auto-rebase-stacked-prs` is already chasing this PR,
    // step aside. Auto-rebase escalation owns the slot until it
    // hits a terminal status; the next conflict-watch sweep will
    // re-evaluate once that resolves.
    match work_db.has_active_rebase_attempt_for_pr(&candidate.pr_url) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: rebase attempt active; deferring conflict flip",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to check rebase attempt; deferring",
            );
            return false;
        }
    }
    // Pre-flight: when an active revision fix vehicle already exists for this
    // work item, the detection flow is essentially a no-op for an `in_review`
    // parent (signal already armed, revision already in Doing). Skip the
    // upfront flip+unblock cycle to avoid redundant state changes on every
    // sweep.  The blocked-parent reconciliation (T791/T898) is handled below
    // via the re-arm path; we fall through there when `rearm` says blocked.
    match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
        Ok(Some(ref active_crz)) if active_crz.revision_task_id.is_some() => {
            match work_db.rearm_blocked_merge_conflict_signal(&candidate.work_item_id) {
                Ok(true) => {
                    // Parent is blocked with an active revision in flight — fall
                    // through to the reconciliation path in the re-arm branch,
                    // which applies the same `supersede_if_stale` check before
                    // deciding whether to reconcile or supersede (T4/dead-revision
                    // fix): a dead/stale attempt must not survive a blind reconcile
                    // just because the parent happened to be `blocked` this pass.
                }
                Ok(false) | Err(_) => {
                    // Parent is in_review (or human-moved). Before treating this
                    // as an idempotent no-op, check whether the active crz is
                    // stale (see `supersede_if_stale`) and, if so, abandon it and
                    // fall through to spawn a fresh resolution against the
                    // current head.
                    if supersede_if_stale(work_db, candidate, probe, active_crz) {
                        // Fall through: mark_chore_blocked → insert_conflict_resolution
                        // creates a new row → spawn fresh revision.
                    } else {
                        // Same head SHA and revision is still live — idempotent
                        // no-op: re-arm the signal so maybe_clear_blocked fires
                        // when the PR becomes clean, then return false.
                        let _ = work_db.record_merge_conflict_in_flight(&candidate.work_item_id, &active_crz.id);
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            attempt_id = %active_crz.id,
                            "conflict_watch: active revision in flight; idempotent probe no-op",
                        );
                        return false;
                    }
                }
            }
        }
        _ => {}
    }

    // T2381/PR#1861 fix: before attempting the normal `in_review` → `blocked`
    // flip below, check whether this row is already stuck in a FOREIGN
    // watcher's bucket with the live probe now reporting CONFLICTING. A row
    // another watcher flipped to `blocked: <reason>` and never returned to
    // `in_review` (e.g. the ci_watch merge-queue-rebounce gap fixed above)
    // is invisible to `mark_chore_blocked_merge_conflict`'s `status='in_review'`
    // WHERE guard — every subsequent sweep would silently no-op forever
    // without this takeover. Design §Q2's priority order (dependency >
    // review_feedback > merge_conflict > ci_failure_exhausted > ci_failure)
    // and §Q1 ("conflict pre-empts CI") both say merge_conflict outranks a
    // `ci_failure` / `ci_failure_exhausted` block, so only those two reasons
    // are eligible for takeover; a higher-priority foreign reason
    // (dependency, review_feedback, parent_pr_closed, …) is left alone but
    // logged at `info` so this class of cross-watcher orphaning is visible
    // in the trace even when we correctly decline to act on it.
    match work_db.task_blocked_reason(&candidate.work_item_id) {
        Ok(Some(reason))
            if (reason == "ci_failure" || reason == "ci_failure_exhausted")
                && !work_db
                    .active_blocked_signals(&candidate.work_item_id)
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.reason == "merge_conflict") =>
        {
            // Guarded on there being no already-active `merge_conflict`
            // side-table signal: when both signals are legitimately active at
            // once (the polymorphic multi-signal model — see
            // `polymorphic_clear_each_signal_independent_when_both_active`),
            // conflict detection is already independently tracked and must
            // not collapse it into a scalar takeover; leave that case to the
            // normal re-arm path below. The genuine T2381/PR#1861 orphan has
            // no `merge_conflict` signal at all — only ci_watch ever touched
            // this row — so the takeover applies cleanly there.
            if !take_over_foreign_ci_block(work_db, candidate, &reason) {
                return false;
            }
            // Row is now `blocked: merge_conflict`; fall through into the
            // normal flip attempt below, which will hit the WHERE-guard-miss
            // re-arm path and correctly recognise the row as already ours.
        }
        Ok(Some(reason)) if reason != "merge_conflict" => {
            // Either a genuinely higher-priority foreign reason (dependency,
            // review_feedback, parent_pr_closed, …), or a ci_failure/
            // ci_failure_exhausted row that already has its own active
            // `merge_conflict` signal (the polymorphic multi-signal case
            // filtered out of the arm above) — either way this watcher
            // doesn't own the scalar reason this sweep.
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                owning_reason = %reason,
                "conflict_watch: row parked in another watcher's bucket; not taking over this sweep",
            );
            return false;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_watch: failed to read blocked_reason for foreign-bucket check; continuing",
            );
        }
    }

    // Trunk merge-queue coordination (design §"Coordination with
    // conflict_watch / ci_watch"): a conflict detected while a Trunk merge
    // intent is still live in the queue is real — Trunk will fail it too —
    // so the conflict resolver takes over the slot. Best-effort and
    // idempotent (a no-op for a non-`trunk_queue` product, or an intent
    // that's already evicted/superseded/awaiting resubmit).
    //
    // Placed here — after the auto-rebase-active and foreign-bucket-owned
    // early returns above, not before them — so the sentinel is only ever
    // set on a path where conflict_watch is actually about to take
    // ownership of the slot (attempt the `in_review` → `blocked` flip
    // below). Marking it on the auto-rebase or foreign-bucket-owned paths
    // (where this function returns without creating a `conflict_resolutions`
    // row or flipping the parent) would strand the intent in the sentinel
    // forever: `on_resolved` only clears it once `conflict_watch` itself
    // observes the PR mergeable again, which never happens for a slot it
    // never took over.
    crate::trunk_merge::mark_trunk_intent_superseded_by_conflict(work_db, &candidate.work_item_id);

    // Try to flip the parent from `in_review` → `blocked: merge_conflict`.
    // The WHERE guard (`status = 'in_review'`) is load-bearing: it protects
    // rows a human moved away from `in_review` (return false, leave alone).
    // If the guard misses because the task is already `blocked: merge_conflict`,
    // we fall into the stale-crz re-arm path below.
    //
    // IMPORTANT (post-revision-unification): if `maybe_spawn_conflict_revision`
    // succeeds, we immediately clear this flip back to `in_review` and upsert
    // the signal row — the parent stays in Review while the fix is in flight.
    // The flip is only kept when there is NO active fix vehicle (churn cap,
    // create_revision failure).
    let task_flipped_to_blocked = match work_db
        .mark_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(_task)) => true,
        Ok(None) => {
            // WHERE guard missed. Two sub-cases:
            // (a) Human moved the row — leave it alone.
            // (b) Task IS blocked:merge_conflict — check for an active revision
            //     fix vehicle and reconcile if found (post-revision-unification
            //     catch-up for rows that were blocked before this model shipped),
            //     or dispatch a fresh attempt for the stale-base scenario.
            let is_blocked = match work_db.rearm_blocked_merge_conflict_signal(&candidate.work_item_id) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "conflict_watch: failed to check/rearm blocked signal; skipping",
                    );
                    return false;
                }
            };
            if !is_blocked {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "conflict_watch: WHERE guard missed; row not blocked:merge_conflict (manually moved); skipping",
                );
                return false;
            }
            // Task IS blocked:merge_conflict; signal re-armed.
            //
            // Check for an active (pending/running) crz.
            //   - Active crz with revision_task_id: the fix vehicle is in
            //     flight but the parent is erroneously blocked (pre-model-
            //     change rows like T791/T898). Reconcile by clearing the block
            //     so the parent returns to Review.
            //   - Active crz without revision_task_id: old-style bespoke
            //     execution still running — leave blocked, no new dispatch.
            //   - No active crz: check latest terminal status for stale-base
            //     re-arm vs churn-guard terminal.
            match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
                Ok(Some(active_crz)) => {
                    if active_crz.revision_task_id.is_some() {
                        // Before blindly reconciling, check whether this attempt
                        // is stale/dead (`supersede_if_stale`) — e.g. the linked
                        // revision died in an engine restart. Reconciling a dead
                        // attempt back to `in_review` would strand the parent
                        // with no live fix vehicle until some *later* pass
                        // happened to observe it via the `in_review` branch
                        // above; superseding here instead means a dead revision
                        // is caught on the very first pass that finds it,
                        // regardless of which state the parent is in.
                        if supersede_if_stale(work_db, candidate, probe, &active_crz) {
                            // Fall through (do not reconcile-and-return): the
                            // shared flip+insert logic below re-affirms the
                            // (already blocked) parent, inserts a fresh attempt
                            // at the now-freed UNIQUE key, and spawns a
                            // replacement revision — which unblocks the parent
                            // back to in_review once it spawns.
                        } else {
                            // Active revision fix vehicle, but parent is blocked.
                            // This is the reconciliation path for rows that were
                            // blocked before the revision-unification model shipped.
                            // Flip parent back to in_review; the revision card in
                            // Doing is the user-visible "something is happening."
                            tracing::info!(
                                work_item_id = %candidate.work_item_id,
                                pr_url = %candidate.pr_url,
                                attempt_id = %active_crz.id,
                                revision_task_id = %active_crz.revision_task_id.as_deref().unwrap_or(""),
                                "conflict_watch: active revision in flight but parent blocked; reconciling to in_review",
                            );
                            let reconciled = match work_db
                                .clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
                            {
                                Ok(Some(_)) => true,
                                Ok(None) => false,
                                Err(err) => {
                                    tracing::warn!(
                                        work_item_id = %candidate.work_item_id,
                                        ?err,
                                        "conflict_watch: failed to reconcile block during re-arm",
                                    );
                                    false
                                }
                            };
                            if reconciled {
                                if let Err(err) =
                                    work_db.record_merge_conflict_in_flight(&candidate.work_item_id, &active_crz.id)
                                {
                                    tracing::warn!(
                                        work_item_id = %candidate.work_item_id,
                                        ?err,
                                        "conflict_watch: failed to record in-flight signal during reconcile",
                                    );
                                }
                                publisher
                                    .publish_work_item_changed(
                                        &candidate.product_id,
                                        &candidate.work_item_id,
                                        "conflict_revision_in_flight",
                                    )
                                    .await;
                            }
                            publisher
                                .publish_frontend_event_on_product(
                                    &candidate.product_id,
                                    FrontendEvent::ConflictResolutionStarted {
                                        product_id: candidate.product_id.clone(),
                                        work_item_id: candidate.work_item_id.clone(),
                                        attempt_id: active_crz.id.clone(),
                                        pr_url: candidate.pr_url.clone(),
                                    },
                                )
                                .await;
                            tracing::info!(
                                work_item_id = %candidate.work_item_id,
                                reconciled,
                                "conflict_watch: re-arm reconciliation complete",
                            );
                            return reconciled;
                        }
                    } else {
                        // Old-style crz (no revision), still in flight.
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            "conflict_watch: blocked signal re-armed; active crz still in flight; no new dispatch",
                        );
                        return false;
                    }
                }
                Ok(None) => {
                    // No active crz. Check the most recent crz to decide
                    // whether to re-arm.
                    let latest = match work_db.latest_conflict_resolution_for_work_item(&candidate.work_item_id) {
                        Ok(latest) => latest,
                        Err(err) => {
                            tracing::warn!(
                                work_item_id = %candidate.work_item_id,
                                ?err,
                                "conflict_watch: failed to read latest crz during re-arm; skipping dispatch",
                            );
                            return false;
                        }
                    };
                    // No crz at all → a fresh block, not a stale-base scenario;
                    // the insert path handles it (treated as "pending").
                    let latest_status = latest.as_ref().map(|c| c.status.as_str()).unwrap_or("pending");
                    match latest_status {
                        "succeeded" => {
                            // Previous attempt succeeded but the PR is CONFLICTING
                            // again. Fall through to the insert path, which
                            // creates a fresh row keyed on (base, head).
                            tracing::info!(
                                work_item_id = %candidate.work_item_id,
                                pr_url = %candidate.pr_url,
                                base_ref_oid = ?probe.base_ref_oid,
                                head_ref_oid = ?probe.head_ref_oid,
                                "conflict_watch: stale-base re-arm: succeeded crz but PR still CONFLICTING; attempting fresh dispatch",
                            );
                            // Wedge fix (mono#1398/#1764): when the succeeded
                            // attempt's UNIQUE key still equals the current
                            // probe's (base + head unchanged since it "succeeded"
                            // — the resolution never advanced the head, yet the
                            // PR is CONFLICTING again), the fall-through INSERT
                            // below would collide and no fresh attempt could ever
                            // land, so `on_conflict_detected` would re-detect this
                            // exact state every ~6s forever. Invalidate the stale
                            // succeeded row (freeing its UNIQUE slot) so exactly
                            // one churn-guarded fresh attempt proceeds; once the
                            // churn guard trips, the parent rests `blocked` for
                            // human attention instead of hot-looping.
                            //
                            // Only when the INSERT would genuinely collide: both
                            // key columns are non-NULL (NULL is distinct in the
                            // UNIQUE index, so a NULL base/head never collides)
                            // and equal to the succeeded row's. When the head has
                            // advanced (the healthy re-arm — see
                            // `rearm_dispatches_fresh_attempt_when_succeeded_crz_has_stale_frozen_base`),
                            // the keys differ and we leave the succeeded row alone.
                            if let Some(succeeded) = latest.as_ref() {
                                let would_collide = probe.base_ref_oid.is_some()
                                    && probe.head_ref_oid.is_some()
                                    && probe.base_ref_oid.as_deref() == succeeded.base_sha_at_trigger.as_deref()
                                    && probe.head_ref_oid.as_deref() == succeeded.head_sha_before.as_deref();
                                if would_collide {
                                    tracing::warn!(
                                        work_item_id = %candidate.work_item_id,
                                        pr_url = %candidate.pr_url,
                                        attempt_id = %succeeded.id,
                                        base_ref_oid = ?probe.base_ref_oid,
                                        head_ref_oid = ?probe.head_ref_oid,
                                        "conflict_watch: succeeded crz's UNIQUE key still matches the CONFLICTING probe (head never advanced); \
                                         invalidating the stale success to free the slot for one churn-guarded fresh attempt",
                                    );
                                    if let Err(err) = work_db.invalidate_stale_succeeded_conflict_resolution(
                                        &succeeded.id,
                                        "stale_success_still_conflicting",
                                    ) {
                                        tracing::warn!(
                                            work_item_id = %candidate.work_item_id,
                                            attempt_id = %succeeded.id,
                                            ?err,
                                            "conflict_watch: failed to invalidate stale succeeded crz; fresh insert may still collide",
                                        );
                                    }
                                }
                            }
                        }
                        "pending" => {
                            // No previous crz (or brand-new pending one) — fall
                            // through to the insert path; it handles idempotency
                            // via the UNIQUE key guard.
                        }
                        other => {
                            // failed / abandoned — churn guard or human owns retry.
                            tracing::debug!(
                                work_item_id = %candidate.work_item_id,
                                terminal_status = other,
                                "conflict_watch: blocked signal re-armed; latest crz terminal ({other}); churn guard owns retry",
                            );
                            return false;
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "conflict_watch: failed to check active crz during re-arm; skipping dispatch",
                    );
                    return false;
                }
            }
            // task was already blocked (re-arm path), didn't flip here.
            false
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to flip row to blocked: merge_conflict",
            );
            return false;
        }
    };

    // T2381/PR#1861 fix: design §Q1 says conflict pre-empts CI — but only
    // the CI side of that rule was enforced (`on_ci_failure_detected` defers
    // to an active `conflict_resolutions` attempt). Whenever conflict_watch
    // is genuinely proceeding to own this PR (fresh flip, foreign-bucket
    // takeover, or a stale-base re-arm), supersede any active
    // `ci_remediations` attempt too — conflict and CI remediation are never
    // supposed to be concurrently active for the same work item. Without
    // this, a stale attempt from e.g. a merge-queue-rebounce fix would sit
    // `pending` forever (no retire path ever marks it `succeeded` once
    // conflict_watch has taken the PR over), stranding a phantom "ci
    // failing" badge alongside the correct conflict block.
    supersede_stale_ci_remediation(work_db, candidate);

    // Insert the `conflict_resolutions` attempt row. The UNIQUE key is
    // `(work_item_id, base_sha_at_trigger, head_sha_before)`, so a second
    // sweep for the same base+head returns `Ok(None)` — idempotent and
    // safe to call on every conflict-detected event. NOTE:
    // `base_sha_at_trigger` mirrors GitHub's PR `baseRefOid`, which is
    // fixed at PR-open time and does NOT track `main` advancing under an
    // in-review PR — it is not "the current main SHA," and re-arms can't
    // rely on it changing. `head_sha_before` is what actually varies
    // across re-arms: a genuine resolution attempt pushes a new commit,
    // so a re-arm past a stale `succeeded` row sees a different head and
    // gets a fresh key here, while a true repeat probe (nothing has
    // changed since the last attempt) still collides and dedupes. The
    // churn guard pre-abandons the 4th attempt inside a rolling 1h window.
    let attempt = match work_db.insert_conflict_resolution(ConflictResolutionInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number: parse_pr_number(&candidate.pr_url).unwrap_or(0),
        head_branch: probe.head_ref_name.as_deref().unwrap_or("").to_owned(),
        base_branch: probe.base_ref_name.as_deref().unwrap_or("").to_owned(),
        base_sha_at_trigger: probe.base_ref_oid.clone(),
        head_sha_before: probe.head_ref_oid.clone(),
    }) {
        Ok(Some(a)) => Some(a),
        Ok(None) => {
            // UNIQUE collision — a row for this (base, head) key already
            // exists. This used to be silent, which made the preceding
            // "dispatching fresh attempt" log actively misleading (it
            // logged intent to dispatch, then this no-op quietly ate it).
            // Log the colliding row so the fall-through is diagnosable,
            // then fall back to a lookup so the started-event still fires
            // if that row happens to be active.
            let colliding = work_db
                .latest_conflict_resolution_for_work_item(&candidate.work_item_id)
                .ok()
                .flatten();
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                base_sha_at_trigger = ?probe.base_ref_oid,
                head_sha_before = ?probe.head_ref_oid,
                colliding_attempt_id = ?colliding.as_ref().map(|c| c.id.as_str()),
                colliding_status = ?colliding.as_ref().map(|c| c.status.as_str()),
                "conflict_watch: insert_conflict_resolution UNIQUE collision; no fresh attempt created for this key",
            );
            work_db
                .active_conflict_resolution_for_work_item(&candidate.work_item_id)
                .unwrap_or(None)
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to insert conflict_resolution attempt; continuing without execution request",
            );
            None
        }
    };

    // Phase 3 cutover / post-revision-unification parent-state model:
    //
    // For a genuinely-new live attempt, create an engine-triggered revision.
    // If the revision spawns successfully (or an existing revision is already
    // in flight via a UNIQUE-collision path):
    //   - Clear the task back to `in_review` (undoing the upfront flip).
    //   - Upsert the `task_blocked_signals` row so `maybe_clear_blocked`
    //     dispatches `on_resolved` when the PR later becomes mergeable.
    //   - Parent stays in Review column while the fix is in Doing.
    // If the revision fails (create_revision gate refused) or the churn cap
    // pre-abandoned the attempt:
    //   - Keep the `blocked: merge_conflict` flip (no revision vehicle means
    //     the parent must surface in the Blocked column for human attention).
    // The "clear the upfront flip back to in_review + record the in-flight
    // signal" sequence is the #1007 parent-state model, now written once in
    // [`crate::blocking_signal`] and shared with the CI-failure path.
    let mut task_unblocked_for_revision = false;

    if let Some(ref a) = attempt {
        if a.status == "pending" && a.revision_task_id.is_none() {
            // Escalation ladder (T4/T6): before spawning a full worker, try
            // the engine-direct mechanical rungs. On a clean rebase (rung 1)
            // the conflict is resolved and pushed with no agent; the harness
            // retires this attempt and clears the parent back to Review, so we
            // return without ever spawning a worker. `cube_client` is `Some`
            // only when the mechanical-rebase flag is enabled (gated at the
            // sweep call site); `None` preserves the pre-ladder worker path.
            //
            // A `FellThrough` residue that is bounded (`rung2_eligible`) means
            // the residual conflict is a small, focused fix rather than a
            // large/architectural one — the spawn below uses rung 2's small-
            // agent profile instead of the default full-worker one.
            let mut use_small_agent_profile = false;
            let mut mechanical_rungs_unavailable = false;
            if let Some(cube) = cube_client {
                match conflict_ladder::try_mechanical_rungs(work_db, publisher, cube, candidate, a).await {
                    conflict_ladder::LadderOutcome::Retired => {
                        tracing::info!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            attempt_id = %a.id,
                            "conflict_watch: conflict auto-resolved by engine-direct rebase (rung 1); no worker spawned",
                        );
                        return true;
                    }
                    conflict_ladder::LadderOutcome::HaltedForSignoff => {
                        // T9/T2562: a mechanical rung pushed a resolution the
                        // deletion tripwire rejected. The task is already
                        // `blocked: deletion_signoff` pending operator
                        // sign-off — do NOT spawn a worker (no automatic
                        // remediation for a flagged deletion).
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            attempt_id = %a.id,
                            "conflict_watch: mechanical rung's resolution rejected by the deletion tripwire; \
                             halted for operator sign-off, no worker spawned",
                        );
                        return true;
                    }
                    conflict_ladder::LadderOutcome::FellThrough {
                        residual_conflict_files,
                    } => {
                        use_small_agent_profile = conflict_ladder::rung2_eligible(residual_conflict_files);
                    }
                    conflict_ladder::LadderOutcome::MechanicalRungsUnavailable => {
                        // Rung 1's workspace lease failed even after a
                        // retry — an infra hiccup, not evidence of a
                        // large/semantic conflict. Spawning the most
                        // expensive rung (full worker) on that signal
                        // would be strictly worse than trying again
                        // shortly, so this tick spawns nothing at all:
                        // the attempt stays `pending` with no
                        // `revision_task_id`, and the next
                        // `conflict_watch` tick re-enters the ladder
                        // from scratch.
                        mechanical_rungs_unavailable = true;
                        tracing::info!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            attempt_id = %a.id,
                            "conflict_watch: mechanical rungs unavailable this attempt (rung-1 lease); \
                             no worker spawned, ladder will retry on the next tick",
                        );
                    }
                }
            } else {
                // No cube client → the mechanical-rebase flag is off, so the
                // escalation ladder (rungs 0/1, including the deterministic
                // lockfile resolver) is not attempted this dispatch at all.
                // This used to be a debug-only line, invisible in the
                // production trace and therefore unfalsifiable from the
                // outside (spinyfin/mono#2032, chore T2680): promote to INFO
                // with the canonical routing-verdict line so "why no rung
                // activity for this PR?" is always answerable.
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %a.id,
                    "conflict_watch: mechanical rebase ladder disabled (no cube client); spawning worker directly (rung 3)",
                );
                conflict_ladder::log_routing_verdict(
                    &candidate.work_item_id,
                    parse_pr_number(&candidate.pr_url).map(|n| n as u64),
                    &[],
                    "skip",
                    "conflict_ladder_mechanical_rebase feature flag is off; ladder (rungs 0/1) never attempted",
                );
            }
            // Fresh attempt — try to spawn a revision, unless the ladder
            // itself was unavailable this tick (rung-1 lease failure): in
            // that case spawn nothing and leave the attempt `pending` for
            // the next tick's retry (see the match arm above).
            if !mechanical_rungs_unavailable {
                let spawned = maybe_spawn_conflict_revision(
                    work_db,
                    publisher,
                    pr_checker,
                    candidate,
                    probe,
                    a,
                    use_small_agent_profile,
                )
                .await;
                if spawned {
                    task_unblocked_for_revision =
                        blocking_signal::unblock_for_revision(work_db, SignalKind::MergeConflict, candidate, &a.id);
                }
                // If !spawned: attempt abandoned (revision_create_failed). Parent
                // stays `blocked: merge_conflict`.
            }
        } else if a.revision_task_id.is_some() && task_flipped_to_blocked {
            // UNIQUE collision: existing revision in flight (repeat probe at
            // same base sha). The upfront flip to blocked was premature — clear
            // it back so the parent stays in Review while the fix continues.
            task_unblocked_for_revision =
                blocking_signal::unblock_for_revision(work_db, SignalKind::MergeConflict, candidate, &a.id);
        }
        // a.status == "abandoned" (churn guard) with no revision_task_id:
        // parent stays blocked — this is the human-attention terminal.
    }

    // Publish parent state-change event.
    // - Flipped to blocked (churn cap, create_revision failure, UNIQUE-collision
    //   with no active revision): "blocked_merge_conflict"
    // - Fix vehicle spawned (parent is now/stays `in_review` with revision
    //   in Doing): "conflict_revision_in_flight"
    // - Pure no-op (idempotent UNIQUE collision with existing revision): no event
    if task_unblocked_for_revision {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "conflict_revision_in_flight",
            )
            .await;
    } else if task_flipped_to_blocked {
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "blocked_merge_conflict")
            .await;
    }

    if let Some(ref a) = attempt {
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::ConflictResolutionStarted {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    attempt_id: a.id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        base_ref_oid = ?probe.base_ref_oid,
        attempt_id = ?attempt.as_ref().map(|a| a.id.as_str()),
        attempt_status = ?attempt.as_ref().map(|a| a.status.as_str()),
        task_flipped_to_blocked,
        task_unblocked_for_revision,
        raw_mergeable = %probe.raw_mergeable,
        raw_merge_state_status = %probe.raw_merge_state_status,
        "conflict_watch: PR conflicts with base; conflict detection ran",
    );
    task_flipped_to_blocked || task_unblocked_for_revision
}

/// Is `active_crz` (an in-flight attempt with a linked revision) stale?
/// A crz is stale when either:
///   (a) the probe head SHA has moved since the crz was created — the
///       revision pushed a commit but didn't resolve the conflict, then its
///       exec was abandoned by the orphan sweep (not NudgeBreakerParked, so
///       finalize_conflict_resolution_attempt never ran), leaving the crz
///       `pending` with `revision_task_id` set against an old head; or
///   (b) the linked revision task is dead — either it reached a terminal
///       status (in_review/done/cancelled) without the crz ever being
///       finalised (same orphan-sweep abandonment scenario, head SHA
///       unchanged), or its execution died outright (e.g. an engine
///       restart) while the task itself never progressed past a live
///       status.
/// Mirrors ci_watch's stale-head supersede logic.
///
/// When stale, abandons the row (nullifying `base_sha_at_trigger` when the
/// base hasn't also moved, so the UNIQUE key is freed for a fresh insert)
/// and returns `true` — the caller falls through to re-detect instead of
/// treating the probe as an idempotent no-op or a plain reconcile.
///
/// Called from **both** places `on_conflict_detected` discovers an active
/// crz with a linked revision — the `in_review` idempotency check and the
/// `blocked` reconciliation path — so a dead/stale attempt is superseded on
/// the first detection pass that observes it, regardless of which state the
/// parent happens to be in on that pass. Previously only the `in_review`
/// branch ran this check; the `blocked` branch just reconciled blindly,
/// which let a revision killed by an engine restart survive until some
/// later pass happened to find the parent back in `in_review`.
fn supersede_if_stale(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    active_crz: &crate::work::ConflictResolution,
) -> bool {
    let current_head_sha = probe.head_ref_oid.as_deref();
    let head_sha_stale = match current_head_sha {
        Some(current) => active_crz
            .head_sha_before
            .as_deref()
            .map(|s| s != current)
            .unwrap_or(false),
        None => false, // can't compare — conservative: don't supersede
    };
    let revision_dead = active_crz
        .revision_task_id
        .as_deref()
        .map(|rid| !work_db.is_conflict_resolution_revision_live(rid).unwrap_or(true))
        .unwrap_or(false);
    if !(head_sha_stale || revision_dead) {
        return false;
    }
    // base_sha_changed is true when the PR's base (main) has also advanced
    // since this crz was created — the realistic path for crz rows that sat
    // pending for hours (T1764). In that case the stale row's UNIQUE key
    // (work_item_id, base_sha_at_trigger) differs from the fresh row's, so a
    // plain abandon is safe. When base SHA is unchanged (same-base head-move,
    // terminal-revision, or dead-revision-with-no-head-move), we must also
    // nullify base_sha_at_trigger on the abandoned row so the INSERT below can
    // create a fresh row at the same key and the churn guard can count this
    // supersede, matching ci_watch's abandon path.
    let base_sha_changed = head_sha_stale && probe.base_ref_oid.as_deref() != active_crz.base_sha_at_trigger.as_deref();
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        attempt_id = %active_crz.id,
        stale_sha = ?active_crz.head_sha_before,
        current_sha = ?current_head_sha,
        revision_task_id = ?active_crz.revision_task_id,
        head_sha_stale,
        revision_dead,
        base_sha_changed,
        "conflict_watch: active crz is stale; superseding and re-detecting",
    );
    if base_sha_changed {
        // Base SHA advanced: plain abandon suffices; the INSERT below uses a
        // new (work_item_id, base_sha) key and will not collide.
        if let Err(err) = work_db.mark_conflict_resolution_abandoned(&active_crz.id, "superseded_stale_head") {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                attempt_id = %active_crz.id,
                ?err,
                "conflict_watch: failed to abandon stale crz (base changed); falling through anyway",
            );
        }
    } else if let Err(err) = work_db.abandon_conflict_resolution_for_supersede(&active_crz.id, "superseded_stale_head")
    {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            attempt_id = %active_crz.id,
            ?err,
            "conflict_watch: failed to abandon stale crz (same base); falling through anyway",
        );
    }
    true
}

/// Abandon any active `ci_remediations` attempt for this work item and
/// clear its `ci_failure` in-flight signal — best-effort, silent no-op when
/// none exists. Design §Q1 ("conflict pre-empts CI") means conflict and CI
/// remediation are never supposed to be concurrently active for the same
/// work item; `on_ci_failure_detected` already enforces the CI side of that
/// (it defers to an active `conflict_resolutions` attempt), so this is the
/// symmetric enforcement from the conflict side. Called unconditionally by
/// [`on_conflict_detected`] right before it commits to owning the PR.
fn supersede_stale_ci_remediation(work_db: &WorkDb, candidate: &PendingMergeCheck) {
    let Ok(Some(stale)) = work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) else {
        return;
    };
    if let Err(err) = work_db.mark_ci_remediation_abandoned(&stale.id, "superseded_by_conflict") {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            attempt_id = %stale.id,
            ?err,
            "conflict_watch: failed to abandon stale ci_remediation superseded by conflict",
        );
    }
    if let Err(err) = work_db.clear_ci_failure_signal_only(&candidate.work_item_id) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "conflict_watch: failed to clear stale ci_failure signal superseded by conflict",
        );
    }
}

/// Foreign-bucket takeover (T2381/PR#1861 fix): re-bucket a row that is
/// currently `blocked: ci_failure` (or `ci_failure_exhausted`) into
/// `blocked: merge_conflict` because the live probe now reports CONFLICTING.
///
/// Also supersedes any active `ci_remediations` attempt for the row —
/// design §Q1 says conflict pre-empts CI, so a CI-fix attempt still "in
/// flight" against a PR now known to conflict is stale. Left alone it would
/// strand a `pending` row forever: no retire path ever marks it
/// `succeeded` for the merge-queue-rebounce case (`on_ci_resolved`
/// deliberately declines to retire a `merge_queue_rebounce` attempt from a
/// clean head-branch probe, and the execution-stop retire path requires
/// per-check names a rebounce attempt never records), which would leave a
/// permanent phantom "ci failing" badge alongside the correct conflict
/// block.
///
/// Returns `true` on a successful re-bucket (WHERE-guard hit); `false` on a
/// guard miss (raced with a concurrent clear) or DB error, both logged. The
/// caller should treat `false` the same as any other "not ours to touch
/// this sweep" outcome.
fn take_over_foreign_ci_block(work_db: &WorkDb, candidate: &PendingMergeCheck, from_reason: &str) -> bool {
    // The caller (`on_conflict_detected`) also unconditionally supersedes
    // any active `ci_remediations` attempt right before it inserts a fresh
    // `conflict_resolutions` row, covering every path that reaches that
    // point (including this takeover, once it falls through). Called here
    // too so the row's ownership signals are consistent immediately after a
    // successful takeover even if a caller inspects them before that point.
    supersede_stale_ci_remediation(work_db, candidate);
    match work_db.retarget_blocked_ci_failure_to_merge_conflict(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(_)) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                from_reason,
                "conflict_watch: row parked in ci_watch's bucket but live probe reports CONFLICTING; \
                 re-bucketing into blocked: merge_conflict",
            );
            true
        }
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: foreign-bucket takeover WHERE guard missed (raced); skipping",
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to re-bucket foreign ci_failure block",
            );
            false
        }
    }
}

/// Create the engine-triggered revision that delivers the conflict fix and
/// stamp the trigger-ledger row's `revision_task_id` back-pointer (design
/// Q1/Q2/Q5).
///
/// `attempt` is the just-inserted, live (`pending`) `conflict_resolutions`
/// row. On success the reconcile loop picks up the new `kind=revision` task
/// and dispatches a `revision_implementation` execution into the chain
/// root's warm workspace. On failure — almost always the create-time gate
/// (`assert_parent_revisable`, R4) refusing a parent whose PR has since
/// merged/closed, occasionally a transient `gh` probe error — the ledger
/// row is marked `abandoned` so it never strands as a `pending` attempt
/// with no fix vehicle (which the dormant backfill/rescue paths would
/// otherwise try to dispatch). The parent stays `blocked: merge_conflict`;
/// the poller's merged/closed handling reconciles it on a later sweep.
///
/// Returns `true` when the revision was successfully created and
/// `revision_task_id` was stamped; `false` on any failure (attempt
/// abandoned). The caller uses this to decide whether to flip the parent
/// back to `in_review` or leave it `blocked: merge_conflict`.
///
/// `use_small_agent_profile` selects rung 2 (T6) — a bounded residue rung 1
/// left behind gets `effort_level = trivial` (the escalation ladder's small,
/// cheap, focused-agent profile) instead of the default effort resolution.
/// `trivial` — not `small` — is deliberate: `default_revision_effort_level`
/// already resolves an un-overridden task/chore-rooted revision (rung 3's
/// fallback) to `small`, so pinning rung 2 to `small` would dispatch
/// byte-identical model/effort knobs to rung 3 for the dominant case,
/// defeating the "cheap, bounded-scope agent" goal. `trivial` still floors
/// to the Sonnet model (never Haiku — issue #746) but resolves to
/// `claude --effort low` instead of rung 3's `medium`, so rung 2 is a real,
/// observably cheaper dispatch rather than a telemetry-only distinction.
/// The attempt is stamped `resolved_by_rung = 2` up front so a later
/// default-to-rung-3 stamp is a no-op. `false` preserves today's behaviour
/// (the full-worker rung 3 path) unchanged.
async fn maybe_spawn_conflict_revision(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    attempt: &crate::work::ConflictResolution,
    use_small_agent_profile: bool,
) -> bool {
    let base_branch = probe
        .base_ref_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("main");
    // Short, one-line card title (design Q3 / R5): generated from the base
    // branch, never the diagnosis body. The long worker directive
    // (diagnosis tables, step-by-step rebase recipe) is injected at
    // dispatch by `compose_revision_directive`, keyed off `created_via`
    // (Phase 2). It is already scoped to "resolve only these conflicted
    // hunks" (`compose_conflict_resolution_fragment`), so rung 2 reuses it
    // unchanged — the small-agent profile only changes the effort/model
    // knob, not the prompt.
    let description = format!("Resolve merge conflict against {base_branch}");
    let created_via = format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{}", attempt.id);

    let revision = match work_db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(candidate.work_item_id.clone())
            .description(description)
            .created_via(created_via)
            .maybe_effort_level(use_small_agent_profile.then_some(EffortLevel::Trivial))
            .build(),
        pr_checker,
    ) {
        Ok(rev) => rev,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                attempt_id = %attempt.id,
                error = %format!("{err:#}"),
                "conflict_watch: create_revision failed (parent likely no longer revisable); abandoning attempt",
            );
            if let Err(abandon_err) = work_db.mark_conflict_resolution_abandoned(&attempt.id, "revision_create_failed")
            {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?abandon_err,
                    "conflict_watch: failed to abandon attempt after create_revision failure",
                );
            }
            return false;
        }
    };

    // Stamp the reverse link. This is the idempotency latch (repeat probes
    // at the same base sha find it set and skip) and the marker that tells
    // the dormant backfill/rescue paths to leave this row alone.
    match work_db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                "conflict_watch: attempt row vanished before revision_task_id could be stamped",
            );
        }
        Err(err) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                ?err,
                "conflict_watch: failed to stamp revision_task_id; revision will still run",
            );
        }
    }

    // Rung 2 (T6): stamp the rung before the revision has actually run, so
    // `mark_conflict_resolution_succeeded`'s default-to-rung-3 `COALESCE`
    // preserves this instead of overwriting it later.
    if use_small_agent_profile
        && let Err(err) =
            work_db.stamp_conflict_resolution_rung(&attempt.id, conflict_ladder::RUNG_SMALL_RESOLUTION_AGENT)
    {
        tracing::warn!(
            attempt_id = %attempt.id,
            ?err,
            "conflict_watch: failed to stamp rung-2 small-agent profile on attempt",
        );
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = %attempt.id,
        use_small_agent_profile,
        revision_task_id = %revision.id,
        "conflict_watch: spawned engine-triggered revision for merge conflict",
    );

    // Nudge the scheduler so the reconcile loop dispatches the revision's
    // `revision_implementation` execution promptly rather than waiting for
    // the next opportunistic kick.
    publisher.kick_scheduler();
    true
}

/// Symmetric resolution path: retire the active `conflict_resolutions`
/// attempt when the probe says the PR is mergeable again. Returns `true`
/// on any transition (task or attempt row updated).
///
/// **Post-revision-unification:** the parent task may be in either
/// `blocked: merge_conflict` (no-fix-vehicle terminal) OR `in_review`
/// (revision was in flight). Both cases are handled:
///
/// - `blocked: merge_conflict` → flip to `in_review`, retire attempt,
///   publish `merge_conflict_resolved` work-item event (classic path).
/// - `in_review` (parent never left Review) → skip the task flip, clear
///   the `merge_conflict` signal from `task_blocked_signals`, retire
///   attempt, publish `ConflictResolutionSucceeded` typed event. No
///   `merge_conflict_resolved` work-item event (the parent didn't
///   change status).
///
/// The WHERE guard on the strict clear path still protects rows a human
/// moved elsewhere — only engine-owned `blocked: merge_conflict` rows
/// or rows with an active `merge_conflict` signal are touched.
pub async fn on_resolved(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    candidate: &PendingMergeCheck,
    labels: &[String],
    raw_mergeable: &str,
    raw_merge_state_status: &str,
) -> bool {
    // Phase 6 #18 / Q7: opt-out is symmetric — an opted-out product's
    // retire path is also a no-op so the engine doesn't undo a manual
    // intervention on a row it has stopped auto-managing.
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }
    // Look up the engine-owned attempt row first. If one exists, drive
    // the strict (attempt-id-guarded) retire path; otherwise fall back
    // to the relaxed pr_url-only WHERE clause so this module still
    // closes the loop when Phase 3 wiring hasn't shipped yet.
    let attempt = match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to look up active conflict_resolutions row; falling back to relaxed retire",
            );
            None
        }
    };

    let task_result = if let Some(attempt) = attempt.as_ref() {
        work_db.clear_chore_blocked_merge_conflict_for_attempt(&candidate.work_item_id, &candidate.pr_url, &attempt.id)
    } else {
        work_db.clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    };

    let task_transitioned = match task_result {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to clear blocked: merge_conflict",
            );
            return false;
        }
    };

    // The attempt row's update is independent of the parent flip. The
    // design (Q5) requires both to happen even if one of them has
    // already been moved by a concurrent path (manual override, on-Stop
    // completion, etc.).
    //
    // For a `pending` attempt we mark succeeded when:
    //   (a) The parent was blocked and we just cleared it (`task_transitioned`).
    //   (b) The parent was `in_review` (revision fix vehicle — the WHERE guard
    //       on the task clear missed, but the attempt itself should retire).
    //       We detect this via `revision_task_id` being set: a pending attempt
    //       with a revision always corresponds to the new-model in-flight path.
    //   (c) The attempt was already `running` (worker was active).
    let mut attempt_transitioned = false;
    if let Some(attempt) = attempt.as_ref() {
        let parent_in_review_with_revision =
            !task_transitioned && attempt.status == "pending" && attempt.revision_task_id.is_some();
        let should_succeed = attempt.status == "running" || task_transitioned || parent_in_review_with_revision;
        if should_succeed {
            match work_db.mark_conflict_resolution_succeeded(&attempt.id, None) {
                Ok(Some(succeeded)) => {
                    attempt_transitioned = true;
                    // When the parent was `in_review` (never blocked), clear the
                    // `merge_conflict` signal so `maybe_clear_blocked` does not
                    // re-trigger on the next probe.
                    if parent_in_review_with_revision
                        && let Err(err) = work_db.clear_merge_conflict_signal_only(&candidate.work_item_id)
                    {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "conflict_watch: failed to clear in-flight signal after retire",
                        );
                    }
                    // Release the cube workspace lease the attempt owned.
                    // Idempotent on the cube side — the lease may already
                    // have been released by the worker's on-Stop completion
                    // path, in which case cube returns a benign error that
                    // we log at debug.
                    if let (Some(client), Some(lease_id)) = (cube_client, succeeded.cube_lease_id.as_deref())
                        && let Err(err) = client.release_workspace(lease_id).await
                    {
                        tracing::debug!(
                            attempt_id = %succeeded.id,
                            lease_id,
                            ?err,
                            "conflict_watch: lease release on retire failed (likely already released)",
                        );
                    }
                    // Close the revision task this attempt spawned so a
                    // retired attempt never leaves a stale
                    // todo/active/blocked row behind — see
                    // `WorkDb::close_resolved_conflict_revision`'s doc for
                    // why this can't just wait for the eventual
                    // parent-PR-merge sweep to clean it up.
                    if let Some(revision_task_id) = succeeded.revision_task_id.as_deref() {
                        match work_db.close_resolved_conflict_revision(revision_task_id) {
                            Ok(Some(_)) => {
                                tracing::info!(
                                    work_item_id = %candidate.work_item_id,
                                    attempt_id = %succeeded.id,
                                    revision_task_id,
                                    "conflict_watch: closed the revision task the resolved attempt spawned",
                                );
                            }
                            Ok(None) => {
                                // Already terminal/in_review, or a worker is
                                // still live driving it — nothing to do.
                            }
                            Err(err) => {
                                tracing::warn!(
                                    work_item_id = %candidate.work_item_id,
                                    attempt_id = %succeeded.id,
                                    revision_task_id,
                                    ?err,
                                    "conflict_watch: failed to close revision task on retire",
                                );
                            }
                        }
                    }
                    publisher
                        .publish_frontend_event_on_product(
                            &candidate.product_id,
                            FrontendEvent::ConflictResolutionSucceeded {
                                product_id: candidate.product_id.clone(),
                                work_item_id: candidate.work_item_id.clone(),
                                attempt_id: succeeded.id.clone(),
                                pr_url: candidate.pr_url.clone(),
                            },
                        )
                        .await;
                }
                Ok(None) => {
                    tracing::debug!(
                        attempt_id = %attempt.id,
                        work_item_id = %candidate.work_item_id,
                        "conflict_watch: attempt row already terminal; skipping succeeded UPDATE",
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        ?err,
                        "conflict_watch: failed to mark conflict_resolution succeeded",
                    );
                }
            }
        }
    }

    if !task_transitioned && !attempt_transitioned {
        return false;
    }
    // Publish a work-item status-change event only when the parent actually
    // transitioned (blocked → in_review). When the parent was already
    // `in_review` the status didn't change, so no broadcast is needed;
    // `ConflictResolutionSucceeded` (above) handles the activity-feed entry.
    if task_transitioned {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "merge_conflict_resolved",
            )
            .await;
    }
    // GitHub reporting the PR mergeable again is a trustworthy signal here
    // (unlike a bare CI-clean probe for a queue-side CI failure) — mergeable
    // genuinely reflects the rebase the conflict-resolution revision just
    // pushed. If a Trunk merge intent was superseded by THIS conflict, it's
    // now clear to resubmit. Scoped to only that sub-state: an eviction
    // episode (`last_trunk_state` `failed`/`pending_failure`) can be live on
    // the same work item at once (see `on_conflict_detected`'s takeover of a
    // `blocked: ci_failure` row) and must not be advanced by an unrelated
    // conflict resolving — only `ci_watch::on_ci_resolved` owns that fix.
    crate::trunk_merge::mark_trunk_intent_awaiting_resubmit(
        work_db,
        &candidate.work_item_id,
        &[crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT],
    );
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = ?attempt.as_ref().map(|a| a.id.as_str()),
        task_transitioned,
        attempt_transitioned,
        raw_mergeable,
        raw_merge_state_status,
        "conflict_watch: PR mergeable again; retire path ran",
    );
    true
}

#[cfg(test)]
#[path = "conflict_watch_tests/mod.rs"]
mod tests;
