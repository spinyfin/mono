//! Periodic repair of work items bound to a PR their own execution branch
//! did not open.
//!
//! ## The incident this closes (2026-10)
//!
//! Two post-merge review follow-ups were bound to the wrong PR: the flunge
//! follow-up for #1757 (execution `exec_18dbae33dac91b10_b6`) opened
//! brianduff/flunge#1764 but was bound to #1757 — its own, already-merged
//! origin PR — and the mono follow-up for #3143 (execution
//! `exec_18de35dc0c3e12e0_311`) opened spinyfin/mono#3153 but was bound to
//! mono#3074, another task's merged PR. In both cases the hook-stream
//! capture took the first PR URL in the publish command's output (the body's
//! origin citation), and the `run_done` delivery path bound it without the
//! branch check. Both reviewer passes failed instantly ("PR N … is MERGED —
//! cannot position on a non-open PR"), the rows sat in the backlog showing a
//! merged PR, the real PRs were never reviewed, and doc-link detection
//! copied the wrong PR's design-doc pointer onto the rows. A third row (a
//! flunge follow-up for origin #1661, 2026-09-16) has the same shape.
//!
//! The capture and delivery paths are fixed at the root
//! (`pr_url_capture`, `completion::run_done_declaration`). This sweep
//! repairs the rows those paths already produced — and any the same shape
//! produces again — by mechanism, so nobody has to notice a merged PR on a
//! backlog card and backfill `pr_url` by hand.
//!
//! ## Design
//!
//! Candidates are selected from the DB alone
//! ([`WorkDb::list_pr_binding_repair_candidates`]): a row whose `pr_url` is
//! its own origin PR, whose `pr_url` another work item also holds, or whose
//! doc pointer sits on an engine-shaped branch that is not its execution's.
//! Those are structural signals, not text heuristics — nothing here reads a
//! description or PR body.
//!
//! Each candidate is confirmed against GitHub using the work item's primary
//! execution branches as the ownership selector:
//!
//! 1. Fetch the bound PR's `headRefName`. If it identifies ANY primary
//!    implementation execution of the row's work item (a resumed execution
//!    pushes to its predecessor's branch) the binding is right; only a foreign
//!    doc pointer (if any) is refreshed from that PR.
//!    Nonmatching hand-named heads or missing execution branches are
//!    inconclusive: leave the row and its binding untouched.
//! 2. For a foreign execution branch, run the branch-keyed detector. A PR
//!    there (open or merged) replaces the wrong binding on the task and the
//!    execution, the doc pointer is re-derived from it, the row moves to
//!    `in_review`, and — for an open PR — a review is requested, exactly as
//!    `bossctl review start` would.
//! 3. No PR on any primary execution branch: the wrong binding is cleared and
//!    the row is parked `blocked:pr_binding_unresolved` with a detail naming
//!    the searched branches and rejected PR. It never keeps a foreign binding.
//!
//! A PR already bound to a different work item is never adopted (step 2
//! checks [`WorkDb::other_work_item_bound_to_pr`]); that case parks the row
//! like step 3.
//!
//! GitHub errors leave the row untouched for the next pass. Per-task attempt
//! counts (in memory, reset on restart) bound the retries: after
//! [`MAX_ATTEMPTS_PER_TASK`] consecutive inconclusive passes an attention
//! item is filed and the row is left alone until the engine restarts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::completion::{
    BranchVerifier, CommandBranchVerifier, CommandPrDetector, PrDetector, PrStatus, expected_branch_name,
    head_identifies_any_branch, parse_repo_slug,
};
use crate::coordinator::ExecutionCoordinator;
use crate::sweep_loop::{SweepOutcome, spawn_sweep_loop};
use crate::work::{GhPrStateChecker, PrBindingRepairCandidate, PrBindingSuspicion, PrStateChecker, WorkDb};
use boss_github::pr_url::pr_number_from_url;

/// How often the sweep runs. Slow: the candidate set is tiny and every
/// candidate costs GitHub calls; the first pass fires on boot.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Consecutive inconclusive passes (GitHub errors) for one task before the
/// sweep files an attention item and stops retrying until restart.
pub const MAX_ATTEMPTS_PER_TASK: u32 = 3;

/// `work_attention_items.kind` filed when a row's binding could not be
/// repaired automatically (no PR on its branch, a foreign PR, or repeated
/// GitHub failures).
pub const ATTENTION_KIND_PR_BINDING_REPAIR: &str = "pr_binding_repair";

/// Scans without mutating so a failed GitHub read preserves the repair signal.
/// A successful scan may contain no unambiguous doc path.
#[async_trait]
pub(crate) trait DocPointerRefresher: Send + Sync {
    async fn scan(&self, task_id: &str, pr_url: &str) -> Option<crate::design_detector::PrScanResult>;
}

/// Production [`DocPointerRefresher`]: the same detector the completion
/// path runs when a PR is first bound.
struct DesignDetectorDocPointerRefresher;

#[async_trait]
impl DocPointerRefresher for DesignDetectorDocPointerRefresher {
    async fn scan(&self, task_id: &str, pr_url: &str) -> Option<crate::design_detector::PrScanResult> {
        crate::design_detector::scan_pr_for_task_doc(task_id, pr_url).await
    }
}

/// Counts from one pass; logged at `info` when non-zero.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PrBindingRepairOutcome {
    /// Rows re-bound to the PR on their execution's branch.
    pub rebound: usize,
    /// Rows whose binding was right and only the doc pointer was refreshed.
    pub doc_repaired: usize,
    /// Rows parked `blocked:pr_binding_unresolved`.
    pub unresolved: usize,
    /// Rows whose suspect binding turned out to be correct (no doc issue).
    pub confirmed_ok: usize,
    /// Rows skipped because ownership was inconclusive or GitHub was unavailable.
    pub inconclusive: usize,
}

impl SweepOutcome for PrBindingRepairOutcome {
    fn has_activity(&self) -> bool {
        self.rebound > 0 || self.doc_repaired > 0 || self.unresolved > 0 || self.inconclusive > 0
    }

    fn log(&self) {
        tracing::info!(
            rebound = self.rebound,
            doc_repaired = self.doc_repaired,
            unresolved = self.unresolved,
            confirmed_ok = self.confirmed_ok,
            inconclusive = self.inconclusive,
            "pr-binding-repair sweep: pass complete",
        );
    }
}

/// Spawn the periodic sweep. Fires immediately on spawn, then every
/// `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let detector: Arc<dyn PrDetector> = Arc::new(CommandPrDetector::new());
    let verifier: Arc<dyn BranchVerifier> = Arc::new(CommandBranchVerifier::new());
    let doc_refresher: Arc<dyn DocPointerRefresher> = Arc::new(DesignDetectorDocPointerRefresher);
    let attempts: Arc<AsyncMutex<HashMap<String, u32>>> = Arc::new(AsyncMutex::new(HashMap::new()));
    spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let detector = Arc::clone(&detector);
        let verifier = Arc::clone(&verifier);
        let doc_refresher = Arc::clone(&doc_refresher);
        let attempts = Arc::clone(&attempts);
        async move {
            let mut attempts = attempts.lock().await;
            let outcome = run_one_pass(
                work_db.as_ref(),
                detector.as_ref(),
                verifier.as_ref(),
                &GhPrStateChecker,
                doc_refresher.as_ref(),
                &mut attempts,
            )
            .await;
            if outcome.rebound > 0 {
                coordinator.kick();
            }
            outcome
        }
    })
}

/// Why a candidate could not be repaired this pass.
enum Inconclusive {
    GitHub(String),
}

/// Run one repair pass. Returns what happened; callers may log it.
pub(crate) async fn run_one_pass(
    work_db: &WorkDb,
    detector: &dyn PrDetector,
    verifier: &dyn BranchVerifier,
    pr_checker: &dyn PrStateChecker,
    doc_refresher: &dyn DocPointerRefresher,
    attempts: &mut HashMap<String, u32>,
) -> PrBindingRepairOutcome {
    let mut outcome = PrBindingRepairOutcome::default();
    let candidates = match work_db.list_pr_binding_repair_candidates() {
        Ok(candidates) => candidates,
        Err(err) => {
            tracing::warn!(
                ?err,
                "pr-binding-repair sweep: failed to list candidates; skipping pass"
            );
            return outcome;
        }
    };
    attempts.retain(|task_id, _| candidates.iter().any(|c| &c.task_id == task_id));

    for candidate in &candidates {
        let prior_attempts = attempts.get(&candidate.task_id).copied().unwrap_or(0);
        if prior_attempts >= MAX_ATTEMPTS_PER_TASK {
            continue;
        }
        match repair_candidate(
            work_db,
            detector,
            verifier,
            pr_checker,
            doc_refresher,
            candidate,
            &mut outcome,
        )
        .await
        {
            Ok(()) => {
                attempts.remove(&candidate.task_id);
            }
            Err(Inconclusive::GitHub(message)) => {
                outcome.inconclusive += 1;
                let count = prior_attempts + 1;
                attempts.insert(candidate.task_id.clone(), count);
                tracing::warn!(
                    task_id = %candidate.task_id,
                    execution_id = %candidate.execution_id,
                    bound_pr_url = %candidate.bound_pr_url,
                    attempt = count,
                    %message,
                    "pr-binding-repair sweep: GitHub could not be consulted; will retry next pass",
                );
                if count >= MAX_ATTEMPTS_PER_TASK {
                    file_attention(
                        work_db,
                        candidate,
                        &format!(
                            "The engine could not confirm which PR this row should be bound to: GitHub kept \
                             failing ({message}). Retries stop until the engine restarts. Check the PR on branch \
                             `{}` by hand and re-bind with `boss task update --pr-url`.",
                            expected_branch_for(candidate)
                        ),
                    );
                }
            }
        }
    }
    outcome
}

fn expected_branch_for(candidate: &PrBindingRepairCandidate) -> String {
    expected_branch_name(
        &candidate.execution_id,
        &candidate.branch_naming,
        candidate.worker_branch_prefix.as_deref(),
    )
}

async fn repair_candidate(
    work_db: &WorkDb,
    detector: &dyn PrDetector,
    verifier: &dyn BranchVerifier,
    pr_checker: &dyn PrStateChecker,
    doc_refresher: &dyn DocPointerRefresher,
    candidate: &PrBindingRepairCandidate,
    outcome: &mut PrBindingRepairOutcome,
) -> Result<(), Inconclusive> {
    let expected_branch = expected_branch_for(candidate);
    let reasons: Vec<&str> = candidate.reasons.iter().map(|r| r.as_str()).collect();
    let binding_suspect = candidate.reasons.iter().any(|r| {
        matches!(
            r,
            PrBindingSuspicion::BoundToOriginPr | PrBindingSuspicion::SharedWithOtherWorkItem
        )
    });
    let doc_suspect = candidate.reasons.contains(&PrBindingSuspicion::DocBranchForeign);

    if candidate.own_branches.is_empty() {
        outcome.inconclusive += 1;
        return Ok(());
    }

    // Step 1: is the bound PR actually on this execution's branch?
    let bound_is_own = if binding_suspect || doc_suspect {
        let slug = parse_repo_slug(&candidate.repo_remote_url)
            .map_err(|err| Inconclusive::GitHub(format!("cannot parse repo slug: {err:#}")))?;
        match pr_number_from_url(&candidate.bound_pr_url) {
            Some(pr_number) => {
                let head_ref = verifier.fetch_pr_head_ref(&slug, pr_number).await.map_err(|err| {
                    Inconclusive::GitHub(format!("fetching head ref of {}: {err:#}", candidate.bound_pr_url))
                })?;
                let is_own = head_identifies_any_branch(&head_ref, &candidate.own_branches);
                // Legacy hand-named heads cannot establish foreign ownership.
                // Use the doc-pointer check's suffix convention, including
                // configured prefixes. Sharing a PR alone proves nothing.
                if !is_own && !crate::completion::branch_work_item_suffix(&head_ref).starts_with("exec_") {
                    outcome.inconclusive += 1;
                    return Ok(());
                }
                is_own
            }
            None => {
                tracing::warn!(
                    task_id = %candidate.task_id,
                    bound_pr_url = %candidate.bound_pr_url,
                    "pr-binding-repair sweep: bound PR URL has no parseable number; ownership is inconclusive",
                );
                outcome.inconclusive += 1;
                return Ok(());
            }
        }
    } else {
        true
    };

    if candidate.has_live_review && !bound_is_own {
        // A live review owns the row; rebinding or parking waits for it.
        return Ok(());
    }

    if bound_is_own {
        if doc_suspect {
            let scan = doc_refresher
                .scan(&candidate.task_id, &candidate.bound_pr_url)
                .await
                .ok_or_else(|| Inconclusive::GitHub("doc scan failed".into()))?;
            clear_foreign_doc_pointer(work_db, candidate);
            crate::design_detector::apply_task_doc_scan(
                work_db,
                &candidate.task_id,
                &candidate.product_id,
                &candidate.bound_pr_url,
                scan,
            );
            outcome.doc_repaired += 1;
            tracing::info!(
                task_id = %candidate.task_id,
                pr_url = %candidate.bound_pr_url,
                ?reasons,
                "pr-binding-repair sweep: binding confirmed on the execution's branch; doc pointer re-derived from it",
            );
        } else {
            outcome.confirmed_ok += 1;
            tracing::info!(
                task_id = %candidate.task_id,
                pr_url = %candidate.bound_pr_url,
                ?reasons,
                "pr-binding-repair sweep: suspect binding confirmed correct — the bound PR is on the execution's branch",
            );
        }
        let _ = work_db.resolve_external_tracker_attention(&candidate.task_id, ATTENTION_KIND_PR_BINDING_REPAIR);
        return Ok(());
    }

    // Step 2: the bound PR is not this execution's. Resolve by branch.
    // Newest primary execution's branch first; an earlier execution's branch
    // is where a resumed row's PR lives.
    let mut status = PrStatus::None;
    let mut branches: Vec<String> = vec![expected_branch.clone()];
    branches.extend(
        candidate
            .own_branches
            .iter()
            .filter(|b| **b != expected_branch)
            .cloned(),
    );
    for branch in &branches {
        status = detector
            .detect_pr(&candidate.repo_remote_url, branch)
            .await
            .map_err(|err| Inconclusive::GitHub(format!("branch-keyed detection for `{branch}`: {err:#}")))?;
        if !matches!(status, PrStatus::None) {
            break;
        }
    }
    let (own_pr_url, merged) = match status {
        PrStatus::Fresh { url } | PrStatus::EmptyDiff { url } | PrStatus::Stale { url, .. } => (url, false),
        PrStatus::Merged { url } => (url, true),
        PrStatus::None | PrStatus::Closed { .. } => {
            let what = match status {
                PrStatus::Closed { url } => format!("the detected PR, {url}, is closed without merging"),
                _ => "no PR has any searched branch as its head".to_owned(),
            };
            let detail = format!(
                "This row was bound to {bound}, a PR whose head does not identify a primary execution of this \
                 work item. Searched branches: {searched}. No PR could be bound from those branches: {what}. \
                 The engine cleared the wrong binding. If the worker's branch was pushed, open a PR from \
                 `{expected_branch}` and re-bind with `boss task update --pr-url`; otherwise re-run the task.",
                bound = candidate.bound_pr_url,
                searched = branches.join(", "),
            );
            return park_unresolved(work_db, candidate, &detail, outcome);
        }
    };
    if let Ok(Some(other)) = work_db.other_work_item_bound_to_pr(&own_pr_url, &candidate.task_id) {
        let detail = format!(
            "This row was bound to {bound}, a PR its own execution (`{exec}`) did not open. The PR on its branch \
             `{expected_branch}`, {own_pr_url}, is already bound to work item `{other}`, so the engine refused to \
             adopt it and cleared the wrong binding. Decide which work item owns {own_pr_url} and re-bind by hand.",
            bound = candidate.bound_pr_url,
            exec = candidate.execution_id,
        );
        return park_unresolved(work_db, candidate, &detail, outcome);
    }
    let scan = doc_refresher
        .scan(&candidate.task_id, &own_pr_url)
        .await
        .ok_or_else(|| Inconclusive::GitHub("doc scan failed".into()))?;
    match work_db.rebind_task_pr_from_execution_branch(
        &candidate.task_id,
        &candidate.execution_id,
        &candidate.bound_pr_url,
        &own_pr_url,
    ) {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                task_id = %candidate.task_id,
                "pr-binding-repair sweep: row changed under the sweep (human fix or concurrent pass); leaving it",
            );
            return Ok(());
        }
        Err(err) => {
            tracing::warn!(task_id = %candidate.task_id, ?err, "pr-binding-repair sweep: re-bind write failed");
            return Ok(());
        }
    }
    outcome.rebound += 1;
    tracing::info!(
        task_id = %candidate.task_id,
        execution_id = %candidate.execution_id,
        old_pr_url = %candidate.bound_pr_url,
        new_pr_url = %own_pr_url,
        merged,
        ?reasons,
        "pr-binding-repair sweep: re-bound row to the PR on its execution's own branch",
    );
    if let Err(err) = crate::reconcile_audit::append_reconcile_audit(
        work_db,
        &candidate.task_id,
        boss_engine_utils::epoch_time::now_epoch_secs(),
        &format!(
            "re-bound pr_url from {} to {own_pr_url}: the former is not on this row's execution branch \
             `{expected_branch}`; the latter is",
            candidate.bound_pr_url
        ),
        None,
    ) {
        tracing::warn!(task_id = %candidate.task_id, ?err, "pr-binding-repair sweep: audit line append failed (non-fatal)");
    }
    clear_foreign_doc_pointer(work_db, candidate);
    crate::design_detector::apply_task_doc_scan(work_db, &candidate.task_id, &candidate.product_id, &own_pr_url, scan);
    let _ = work_db.resolve_external_tracker_attention(&candidate.task_id, ATTENTION_KIND_PR_BINDING_REPAIR);
    if merged {
        // The merge poller owns the `in_review` → `done` transition for a
        // merged PR; requesting a review of a merged PR would only fail.
        return Ok(());
    }
    match work_db.request_pr_review(&candidate.task_id, pr_checker) {
        Ok(review) => tracing::info!(
            task_id = %candidate.task_id,
            review_execution_id = %review.id,
            pr_url = %own_pr_url,
            "pr-binding-repair sweep: requested review of the re-bound PR",
        ),
        Err(err) => tracing::warn!(
            task_id = %candidate.task_id,
            pr_url = %own_pr_url,
            ?err,
            "pr-binding-repair sweep: re-bound the PR but could not request its review; start one with \
             `bossctl review start --pr`",
        ),
    }
    Ok(())
}

/// Drop a doc pointer copied off the wrong PR so the detector can re-derive
/// it from the right one (it never overwrites an existing `doc_path`). When
/// the right PR carries no doc the pointer stays empty.
fn clear_foreign_doc_pointer(work_db: &WorkDb, candidate: &PrBindingRepairCandidate) {
    if let Err(err) = work_db.clear_task_doc_pointer(&candidate.task_id) {
        tracing::warn!(task_id = %candidate.task_id, ?err, "pr-binding-repair sweep: failed to clear foreign doc pointer");
    }
}

fn park_unresolved(
    work_db: &WorkDb,
    candidate: &PrBindingRepairCandidate,
    detail: &str,
    outcome: &mut PrBindingRepairOutcome,
) -> Result<(), Inconclusive> {
    match work_db.mark_task_pr_binding_unresolved(
        &candidate.task_id,
        &candidate.execution_id,
        &candidate.bound_pr_url,
        detail,
    ) {
        Ok(true) => {
            outcome.unresolved += 1;
            tracing::warn!(
                task_id = %candidate.task_id,
                execution_id = %candidate.execution_id,
                bound_pr_url = %candidate.bound_pr_url,
                "pr-binding-repair sweep: cleared a binding to a PR this row did not open; no PR on its own branch — parked visibly",
            );
            file_attention(work_db, candidate, detail);
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(task_id = %candidate.task_id, ?err, "pr-binding-repair sweep: park write failed");
        }
    }
    Ok(())
}

fn file_attention(work_db: &WorkDb, candidate: &PrBindingRepairCandidate, body: &str) {
    if let Err(err) = work_db.upsert_external_tracker_attention(
        &candidate.task_id,
        ATTENTION_KIND_PR_BINDING_REPAIR,
        "Bound to a PR this work item did not open",
        body,
    ) {
        tracing::warn!(task_id = %candidate.task_id, ?err, "pr-binding-repair sweep: failed to file attention item");
    }
}

#[cfg(test)]
#[path = "pr_binding_repair_sweep_tests.rs"]
mod tests;
