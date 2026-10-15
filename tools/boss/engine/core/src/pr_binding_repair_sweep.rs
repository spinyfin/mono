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
//! Each candidate is then confirmed against GitHub, and the execution's own
//! branch ([`crate::completion::expected_branch_name`]) is the only selector:
//!
//! 1. Fetch the bound PR's `headRefName`. If it identifies ANY primary
//!    implementation execution of the row's work item (a resumed execution
//!    pushes to its predecessor's branch) the binding is right; only a foreign doc pointer (if any) is
//!    refreshed from that PR.
//! 2. Otherwise run the branch-keyed detector on the execution's branch. A PR
//!    there (open or merged) replaces the wrong binding on the task and the
//!    execution, the doc pointer is re-derived from it, the row moves to
//!    `in_review`, and — for an open PR — a review is requested, exactly as
//!    `bossctl review start` would.
//! 3. No PR on the branch: the wrong binding is cleared and the row is
//!    parked `blocked:pr_binding_unresolved` with a detail naming the branch
//!    and the rejected PR. It never keeps pointing at a PR it did not open.
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
    /// Rows skipped this pass because GitHub could not be consulted.
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

    // Step 1: is the bound PR actually on this execution's branch?
    let bound_is_own = if binding_suspect || doc_suspect {
        let slug = parse_repo_slug(&candidate.repo_remote_url)
            .map_err(|err| Inconclusive::GitHub(format!("cannot parse repo slug: {err:#}")))?;
        match pr_number_from_url(&candidate.bound_pr_url) {
            Some(pr_number) => {
                let head_ref = verifier.fetch_pr_head_ref(&slug, pr_number).await.map_err(|err| {
                    Inconclusive::GitHub(format!("fetching head ref of {}: {err:#}", candidate.bound_pr_url))
                })?;
                head_identifies_any_branch(&head_ref, &candidate.own_branches)
            }
            None => {
                tracing::warn!(
                    task_id = %candidate.task_id,
                    bound_pr_url = %candidate.bound_pr_url,
                    "pr-binding-repair sweep: bound PR URL has no parseable number; treating as not this execution's PR",
                );
                false
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
                PrStatus::Closed { url } => format!("the PR on that branch, {url}, is closed without merging"),
                _ => "no PR has that branch as its head".to_owned(),
            };
            let detail = format!(
                "This row was bound to {bound}, a PR its own execution (`{exec}`) did not open — its head is not \
                 `{expected_branch}`. No PR could be bound from that branch either: {what}. The engine cleared the \
                 wrong binding rather than leave the row pointing at a PR it did not open. If the worker's branch \
                 was pushed, open a PR from `{expected_branch}` and re-bind with `boss task update --pr-url`; \
                 otherwise re-run the task.",
                bound = candidate.bound_pr_url,
                exec = candidate.execution_id,
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
mod tests {
    use std::sync::Mutex as StdMutex;

    use boss_protocol::{
        CreateChoreInput, CreateExecutionInput, ExecutionKind, ExecutionStatus, FinishExecutionRunInput, TaskKind,
        TaskStatus, WorkItem,
    };

    use super::*;
    use crate::test_support::{create_test_product_with_repo, open_db};
    use crate::work::{FakePrStateChecker, PrOpenState};

    const REPO: &str = "git@github.com:brianduff/flunge.git";
    const ORIGIN: &str = "https://github.com/brianduff/flunge/pull/1757";
    const OWN: &str = "https://github.com/brianduff/flunge/pull/1764";

    /// A follow-up for origin #1757 whose completed implementation
    /// execution was (wrongly) bound to #1757 — the incident row.
    fn mis_bound_followup(db: &WorkDb, bound_pr_url: &str) -> (String, String, String) {
        let product = create_test_product_with_repo(db, "flunge", Some(REPO));
        let task = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Address post-merge review findings")
                    .maybe_description(Some(format!("Found in post-merge review of {ORIGIN}.")))
                    .autostart(false)
                    .maybe_kind_override(Some(TaskKind::Followup))
                    .maybe_origin_pr_number(Some(1757))
                    .build(),
            )
            .unwrap();
        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(task.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url(REPO)
                    .build(),
            )
            .unwrap();
        let (exec, run) = db
            .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&exec.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::Completed)
                .run_status("completed")
                .build(),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_url = ?2, cube_lease_id = NULL, workspace_path = NULL WHERE id = ?1",
            rusqlite::params![exec.id, bound_pr_url],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET pr_url = ?2, status = 'todo', doc_branch = 'boss/exec_18dad2723a80ed10_23',
                 doc_path = 'docs/designs/origin.md' WHERE id = ?1",
            rusqlite::params![task.id, bound_pr_url],
        )
        .unwrap();
        (product.id, task.id, exec.id)
    }

    struct MapVerifier(HashMap<u64, String>);

    #[async_trait]
    impl BranchVerifier for MapVerifier {
        async fn fetch_pr_head_ref(&self, _slug: &str, pr_number: u64) -> anyhow::Result<String> {
            self.0
                .get(&pr_number)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no head ref configured for PR {pr_number}"))
        }
        async fn fetch_pr_head_oid(&self, _: &str, _: u64) -> anyhow::Result<String> {
            Ok("oid".into())
        }
        async fn fetch_pr_head_oid_fresh(&self, _: &str, _: u64) -> anyhow::Result<String> {
            Ok("oid".into())
        }
        async fn fetch_diff_line_count(&self, _: &str, _: &str, _: &str) -> anyhow::Result<u64> {
            Ok(1)
        }
        async fn fetch_pr_base_ref(&self, _: &str, _: u64) -> anyhow::Result<String> {
            Ok("main".into())
        }
        async fn fetch_diff_signature(&self, _: &str, _: &str, _: &str) -> anyhow::Result<String> {
            Ok("sig".into())
        }
        async fn fetch_pr_title_and_body(&self, _: &str, _: u64) -> anyhow::Result<(String, String)> {
            Ok((String::new(), String::new()))
        }
    }

    struct FixedDetector(PrStatus, StdMutex<Vec<String>>);

    #[async_trait]
    impl PrDetector for FixedDetector {
        async fn detect_pr(&self, _repo: &str, expected_branch: &str) -> anyhow::Result<PrStatus> {
            self.1.lock().unwrap().push(expected_branch.to_owned());
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct RecordingDocRefresher(StdMutex<Vec<String>>);

    #[async_trait]
    impl DocPointerRefresher for RecordingDocRefresher {
        async fn scan(&self, _: &str, pr_url: &str) -> Option<crate::design_detector::PrScanResult> {
            self.0.lock().unwrap().push(pr_url.to_owned());
            Some(crate::design_detector::PrScanResult {
                doc_path: None,
                head_ref_name: None,
                base_ref_name: None,
            })
        }
    }

    fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
        match db.get_work_item(id).unwrap() {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            other => panic!("expected task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn followup_bound_to_its_origin_pr_is_rebound_from_its_branch_and_reviewed() {
        let (_dir, db) = open_db();
        let (_product, task_id, exec_id) = mis_bound_followup(&db, ORIGIN);
        let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
        let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
        let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
        let docs = RecordingDocRefresher::default();
        let mut attempts = HashMap::new();

        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &docs,
            &mut attempts,
        )
        .await;

        assert_eq!(outcome.rebound, 1, "{outcome:?}");
        assert_eq!(detector.1.lock().unwrap().as_slice(), [own_branch.as_str()]);
        let t = task(&db, &task_id);
        assert_eq!(t.pr_url.as_deref(), Some(OWN));
        assert_eq!(t.status, TaskStatus::InReview);
        assert!(t.description.contains("re-bound pr_url"));
        assert_eq!(db.get_execution(&exec_id).unwrap().pr_url.as_deref(), Some(OWN));
        assert_eq!(
            docs.0.lock().unwrap().as_slice(),
            [OWN],
            "doc pointer re-derived from the real PR"
        );
        assert!(
            db.list_executions(Some(&task_id))
                .unwrap()
                .iter()
                .any(|e| e.kind == ExecutionKind::PrReview && !e.status.is_terminal()),
            "a review of the re-bound PR must be requested",
        );
        // Idempotent: the row is no longer a candidate.
        assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_pr_on_the_execution_branch_parks_the_row_visibly() {
        let (_dir, db) = open_db();
        let (_product, task_id, exec_id) = mis_bound_followup(&db, ORIGIN);
        let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let mut attempts = HashMap::new();

        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;

        assert_eq!(outcome.unresolved, 1, "{outcome:?}");
        let t = task(&db, &task_id);
        assert_eq!(t.pr_url, None, "the wrong binding must not survive");
        assert_eq!(t.status, TaskStatus::Blocked);
        assert_eq!(
            t.blocked_reason.as_deref(),
            Some(crate::work::BLOCKED_REASON_PR_BINDING_UNRESOLVED)
        );
        let detail = t.blocked_detail.unwrap_or_default();
        assert!(detail.contains(ORIGIN) && detail.contains(&expected_branch_name(&exec_id, &Default::default(), None)));
        assert_eq!(db.get_execution(&exec_id).unwrap().pr_url, None);
        assert!(
            db.list_attention_items_for_work_item(&task_id)
                .unwrap()
                .iter()
                .any(|a| a.kind == ATTENTION_KIND_PR_BINDING_REPAIR)
        );
    }

    #[tokio::test]
    async fn correctly_bound_row_with_a_foreign_doc_branch_only_gets_its_doc_pointer_refreshed() {
        let (_dir, db) = open_db();
        // Already backfilled by hand to the right PR, but the doc pointer
        // still sits on the origin PR's branch — the two hand-repaired rows.
        let (_product, task_id, exec_id) = mis_bound_followup(&db, OWN);
        let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
        let verifier = MapVerifier(HashMap::from([(1764, own_branch.clone())]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let docs = RecordingDocRefresher::default();
        let mut attempts = HashMap::new();

        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &docs,
            &mut attempts,
        )
        .await;

        assert_eq!(outcome.doc_repaired, 1, "{outcome:?}");
        assert!(
            detector.1.lock().unwrap().is_empty(),
            "a correct binding is never re-detected"
        );
        assert_eq!(docs.0.lock().unwrap().as_slice(), [OWN]);
        let t = task(&db, &task_id);
        assert_eq!(t.pr_url.as_deref(), Some(OWN));
        assert_eq!(t.status, TaskStatus::Todo, "a correct binding leaves status alone");
        assert_eq!(
            db.task_doc_path(&task_id).unwrap(),
            None,
            "the doc path copied off the origin PR must be gone"
        );
        assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rebind_clears_the_foreign_doc_path_even_on_main() {
        let (_dir, db) = open_db();
        let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET doc_branch = 'main' WHERE id = ?1", [&task_id])
            .unwrap();
        let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
        let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
        let mut attempts = HashMap::new();
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;
        assert_eq!(outcome.rebound, 1, "{outcome:?}");
        assert_eq!(db.task_doc_path(&task_id).unwrap(), None);
    }

    #[tokio::test]
    async fn live_review_allows_only_the_doc_repair() {
        let (_dir, db) = open_db();
        let (_product, task_id, exec_id) = mis_bound_followup(&db, OWN);
        let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
        db.request_pr_review(&task_id, &FakePrStateChecker::always(PrOpenState::Open))
            .unwrap();
        let verifier = MapVerifier(HashMap::from([(1764, own_branch)]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let mut attempts = HashMap::new();
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;
        assert_eq!(outcome.doc_repaired, 1, "{outcome:?}");
        assert_eq!(db.task_doc_path(&task_id).unwrap(), None);
    }

    #[tokio::test]
    async fn github_failures_are_retried_then_escalated_without_touching_the_row() {
        let (_dir, db) = open_db();
        let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
        let verifier = MapVerifier(HashMap::new()); // every head-ref fetch errors
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let mut attempts = HashMap::new();
        for pass in 1..=MAX_ATTEMPTS_PER_TASK + 1 {
            let outcome = run_one_pass(
                &db,
                &detector,
                &verifier,
                &FakePrStateChecker::always(PrOpenState::Open),
                &RecordingDocRefresher::default(),
                &mut attempts,
            )
            .await;
            let expected_inconclusive = usize::from(pass <= MAX_ATTEMPTS_PER_TASK);
            assert_eq!(outcome.inconclusive, expected_inconclusive, "pass {pass}: {outcome:?}");
        }
        let t = task(&db, &task_id);
        assert_eq!(
            t.pr_url.as_deref(),
            Some(ORIGIN),
            "an inconclusive pass never rewrites the row"
        );
        assert!(
            db.list_attention_items_for_work_item(&task_id)
                .unwrap()
                .iter()
                .any(|a| a.kind == ATTENTION_KIND_PR_BINDING_REPAIR)
        );
    }
    /// Second primary execution on `task_id` (a resume / bounce-back), newer
    /// than the first and never pushed.
    fn add_resumed_execution(db: &WorkDb, task_id: &str) -> String {
        let exec = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(task_id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Completed)
                    .repo_remote_url(REPO)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = '9999-01-01T00:00:00Z' WHERE id = ?1",
                [&exec.id],
            )
            .unwrap();
        exec.id
    }

    #[tokio::test]
    async fn resumed_row_bound_to_a_predecessor_executions_pr_is_left_untouched() {
        let (_dir, db) = open_db();
        let (_product, task_id, exec_a) = mis_bound_followup(&db, OWN);
        let branch_a = expected_branch_name(&exec_a, &Default::default(), None);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET doc_branch = ?2 WHERE id = ?1", [&task_id, &branch_a])
            .unwrap();
        let exec_b = add_resumed_execution(&db, &task_id);
        assert_ne!(exec_a, exec_b);
        let verifier = MapVerifier(HashMap::from([(1764, branch_a)]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let docs = RecordingDocRefresher::default();
        let mut attempts = HashMap::new();

        assert!(
            db.list_pr_binding_repair_candidates().unwrap().is_empty(),
            "a doc pointer on the predecessor execution's branch is the row's own"
        );
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &docs,
            &mut attempts,
        )
        .await;

        assert_eq!(outcome, PrBindingRepairOutcome::default());
        let t = task(&db, &task_id);
        assert_eq!(t.pr_url.as_deref(), Some(OWN));
        assert_eq!(t.status, TaskStatus::Todo);
        assert!(detector.1.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resumed_row_whose_origin_binding_is_suspect_but_pr_is_a_predecessors_is_confirmed() {
        let (_dir, db) = open_db();
        // Bound to the origin PR number but its head is the predecessor's branch.
        let (_product, task_id, exec_a) = mis_bound_followup(&db, ORIGIN);
        add_resumed_execution(&db, &task_id);
        let verifier = MapVerifier(HashMap::from([(
            1757,
            expected_branch_name(&exec_a, &Default::default(), None),
        )]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let mut attempts = HashMap::new();
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;
        assert_eq!(outcome.confirmed_ok + outcome.doc_repaired, 1, "{outcome:?}");
        assert_eq!(task(&db, &task_id).pr_url.as_deref(), Some(ORIGIN));
    }

    #[tokio::test]
    async fn engine_closed_followup_bound_to_its_origin_pr_is_rebound_and_reopened() {
        let (_dir, db) = open_db();
        let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'done', last_status_actor = 'engine', completed_at = 'now' WHERE id = ?1",
                [&task_id],
            )
            .unwrap();
        let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
        let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
        let mut attempts = HashMap::new();
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;
        assert_eq!(outcome.rebound, 1, "{outcome:?}");
        let t = task(&db, &task_id);
        assert_eq!(t.pr_url.as_deref(), Some(OWN));
        assert_eq!(t.status, TaskStatus::InReview);
    }

    #[tokio::test]
    async fn human_closed_row_bound_to_its_origin_pr_is_not_a_candidate() {
        let (_dir, db) = open_db();
        let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'done', last_status_actor = 'human' WHERE id = ?1",
                [&task_id],
            )
            .unwrap();
        assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
    }

    struct RetryDocScanner(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl DocPointerRefresher for RetryDocScanner {
        async fn scan(&self, _: &str, _: &str) -> Option<crate::design_detector::PrScanResult> {
            self.0
                .swap(true, std::sync::atomic::Ordering::SeqCst)
                .then(|| crate::design_detector::PrScanResult {
                    doc_path: Some("docs/designs/own.md".into()),
                    head_ref_name: Some("main".into()),
                    base_ref_name: Some("main".into()),
                })
        }
    }

    #[tokio::test]
    async fn failed_doc_scan_preserves_signal_and_pointer_until_retry_succeeds() {
        for bound in [OWN, ORIGIN] {
            let (_dir, db) = open_db();
            let (_, task_id, exec_id) = mis_bound_followup(&db, bound);
            let verifier = MapVerifier(HashMap::from([
                (1764, expected_branch_name(&exec_id, &Default::default(), None)),
                (1757, "boss/exec_foreign".into()),
            ]));
            let detector = FixedDetector(PrStatus::Fresh { url: OWN.into() }, StdMutex::default());
            let docs = RetryDocScanner(std::sync::atomic::AtomicBool::new(false));
            let mut attempts = HashMap::new();
            let checker = FakePrStateChecker::always(PrOpenState::Open);
            let first = run_one_pass(&db, &detector, &verifier, &checker, &docs, &mut attempts).await;
            assert_eq!(first.inconclusive, 1);
            assert_eq!(first.doc_repaired + first.rebound, 0);
            assert_eq!(task(&db, &task_id).pr_url.as_deref(), Some(bound));
            assert_eq!(
                db.task_doc_path(&task_id).unwrap().as_deref(),
                Some("docs/designs/origin.md")
            );
            assert_eq!(db.list_pr_binding_repair_candidates().unwrap().len(), 1);
            let second = run_one_pass(&db, &detector, &verifier, &checker, &docs, &mut attempts).await;
            assert_eq!(second.inconclusive, 0);
            assert_eq!(second.doc_repaired + second.rebound, 1);
            assert_eq!(
                db.task_doc_path(&task_id).unwrap().as_deref(),
                Some("docs/designs/own.md")
            );
            assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
        }
    }
}
