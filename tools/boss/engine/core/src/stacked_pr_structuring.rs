//! Layer 4 / T11 — Stacked-PR auto-structuring for predicted conflicts
//! (`future / not a v1 blocker` in
//! `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
//!
//! T10 ([`crate::speculative_conflict`]) predicts whether one in-flight PR
//! branch conflicts with **`main`**. T11 asks the complementary question:
//! do two *in-flight* branches conflict with **each other**? When they do,
//! the design's remedy is to *offer* to convert the would-be conflict into
//! an ordered **stack** — restack the newer PR on top of the older one so
//! the `auto_rebase` machinery keeps them ordered and the second merges
//! cleanly after the first, instead of both racing `main` and colliding.
//!
//! This module ships the **detection + offer** half of that. Piggybacking
//! on the merge poller's periodic sweep it:
//!
//!   1. gathers each in-flight PR's changed-file set (`gh pr view --json
//!      files`),
//!   2. finds same-product pairs whose file sets overlap on *non-mechanical*
//!      files — the cheap, deterministic file-overlap signal Layer 3 already
//!      uses for `merge_order`; lockfile / `BUILD.bazel` / registry overlap
//!      is excluded because those classes are mechanically resolvable
//!      ([`crate::conflict_diagnosis::classify_conflict_class`]) and are a
//!      poor reason to serialise two branches, and
//!   3. emits an ordered [`StackProposal`] as a typed
//!      [`FrontendEvent::StackProposalOffered`] so an operator (or, later, an
//!      auto-accept path) can act on it.
//!
//! The overlap heuristic is deliberately cheap and admits false positives;
//! per the design that is fine — a false positive produces only a harmless,
//! human-vetted *offer*, never a mutation. A precise confirmation (a
//! throwaway pairwise `git merge-tree` / speculative rebase) is a natural
//! future refinement layered on top of this signal.
//!
//! **Deliberately out of scope here** (declared deferred; blocked on the
//! `auto_rebase` machinery, which is designed but not yet shipped — see
//! `auto-rebase-stacked-prs.md` and
//! [`WorkDb::has_active_rebase_attempt_for_pr`], which already degrades
//! gracefully while the `rebase_attempts` table is absent): actually
//! *accepting* an offer — retargeting the dependent PR's base onto the base
//! branch and handing the ongoing rebase to `auto_rebase`. The offer is the
//! seam; the conversion lands with that flow.
//!
//! Gated by the `stacked_pr_auto_structuring` feature flag (default OFF, see
//! [`crate::completion::WorkerCompletionHandler::stacked_pr_auto_structuring_enabled`])
//! and the same per-product `auto_pr_maintenance_enabled` opt-out the rest
//! of the conflict pipeline honours. Rate-limited three ways to bound `gh`
//! churn: a minimum interval between passes ([`MIN_PASS_INTERVAL`]), a
//! per-pass candidate cap ([`MAX_BRANCHES_PER_PASS`]), and a per-pair
//! re-offer interval ([`REOFFER_INTERVAL`]).

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use anyhow::Result;
use boss_protocol::FrontendEvent;

use crate::coordinator::ExecutionPublisher;
use crate::gh_invocation::gh_output;
use crate::merge_poller::{parse_pr_number, sanitize_metric_name_component};
use crate::metrics::Registry;
use crate::work::{PendingMergeCheck, WorkDb};

/// Minimum wall-clock between full stacking passes. The per-pair re-offer
/// interval already prevents spamming the same offer, but this bounds how
/// often the pass fans out `gh pr view` calls at all, independent of how
/// frequently the host merge-poller loop ticks.
const MIN_PASS_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Do not re-offer the same in-flight pair more often than this. An offer
/// is advisory; once surfaced it stays actionable, so re-emitting it every
/// pass would only be noise.
const REOFFER_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Upper bound on how many in-flight PRs one pass fetches changed files for,
/// so a large in-review backlog can't fan out an unbounded burst of `gh`
/// calls in a single pass. The remainder is considered on the next pass
/// (logged, never silently dropped).
const MAX_BRANCHES_PER_PASS: usize = 12;

/// Minimum number of overlapping *non-mechanical* files for a pair to be
/// worth offering a stack for. One genuinely-shared semantic file is enough
/// to make two branches likely to conflict.
const MIN_OVERLAP_FILES: usize = 1;

crate::register_counter!(
    STACK_PROPOSAL_OFFERED,
    "stacked_pr_structuring.offered",
    "In-flight branch pairs offered an ordered-stack restructuring in one sweep."
);

/// Register this module's counter handles. Called from
/// [`crate::metrics::init_all`] at engine startup.
pub fn init(registry: &Registry) {
    registry.register_counter(&STACK_PROPOSAL_OFFERED);
}

/// Increment the per-product offered-proposal counter. Layer 0's hard
/// requirement ("every counter must be scopable per-product") applies here
/// too — mirrors [`crate::speculative_conflict`]'s per-product outcome
/// counter.
fn record_offer_counter(registry: &Registry, product_id: &str) {
    let name = format!(
        "stacked_pr_structuring.{}.offered",
        sanitize_metric_name_component(product_id)
    );
    registry.counter_inc_by_dynamic(
        &name,
        "In-flight branch pairs for this product offered an ordered-stack restructuring.",
        1,
    );
}

/// One in-flight PR branch under consideration, with the set of files it
/// changes. The pure planner ([`plan_stack_proposals`]) consumes a slice of
/// these; the orchestration layer builds them from live `gh` data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightBranch {
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
    /// The PR number, used both to identify the branch and as the age proxy
    /// for stack ordering — PR numbers are monotonic per repo, so the lower
    /// number is the older, likelier-to-merge-first branch (the base).
    pub pr_number: i64,
    /// The branch's changed-file paths, already filtered to *stack-worthy*
    /// (non-mechanical) files. A `BTreeSet` so intersections are
    /// deterministic.
    pub changed_files: BTreeSet<String>,
}

/// An ordered-stack offer for a pair of in-flight branches predicted to
/// conflict. `base` is the lower-numbered (older) PR; `dependent` is the
/// higher-numbered PR that would be restacked on top of it.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct StackProposal {
    pub product_id: String,
    pub base_pr_url: String,
    pub base_pr_number: i64,
    pub base_work_item_id: String,
    pub dependent_pr_url: String,
    pub dependent_pr_number: i64,
    pub dependent_work_item_id: String,
    /// The non-mechanical files both branches touch, sorted for
    /// determinism. This is the "why" the operator sees on the offer.
    pub overlapping_files: Vec<String>,
}

impl StackProposal {
    /// Stable identity for a pair, order-independent: `(base, dependent)`.
    /// Because `base_pr_number < dependent_pr_number` always holds, this is
    /// already the normalised `(min, max)` form.
    fn pair_key(&self) -> (i64, i64) {
        (self.base_pr_number, self.dependent_pr_number)
    }
}

/// Pure core: given the in-flight branches, produce one ordered
/// [`StackProposal`] per same-product pair whose changed-file sets overlap
/// on at least [`MIN_OVERLAP_FILES`] files. Deterministic: each unordered
/// pair yields at most one proposal, `base` is always the lower PR number,
/// and the returned list is sorted by `(base, dependent)`. No I/O — the
/// caller supplies the file sets.
pub fn plan_stack_proposals(branches: &[InFlightBranch]) -> Vec<StackProposal> {
    let mut proposals = Vec::new();
    for i in 0..branches.len() {
        for j in (i + 1)..branches.len() {
            let a = &branches[i];
            let b = &branches[j];
            // Same repo/product only — auto_rebase is same-product by
            // design (`auto-rebase-stacked-prs.md` non-goals). A dedup guard
            // on the PR number also drops a candidate accidentally listed
            // twice.
            if a.product_id != b.product_id || a.pr_number == b.pr_number {
                continue;
            }
            let overlap: Vec<String> = a.changed_files.intersection(&b.changed_files).cloned().collect();
            if overlap.len() < MIN_OVERLAP_FILES {
                continue;
            }
            // Order the stack: the lower (older) PR number is the base; the
            // higher is the dependent restacked on top.
            let (base, dependent) = if a.pr_number < b.pr_number { (a, b) } else { (b, a) };
            let mut overlapping_files = overlap;
            overlapping_files.sort();
            proposals.push(StackProposal {
                product_id: base.product_id.clone(),
                base_pr_url: base.pr_url.clone(),
                base_pr_number: base.pr_number,
                base_work_item_id: base.work_item_id.clone(),
                dependent_pr_url: dependent.pr_url.clone(),
                dependent_pr_number: dependent.pr_number,
                dependent_work_item_id: dependent.work_item_id.clone(),
                overlapping_files,
            });
        }
    }
    proposals.sort_by_key(|p| p.pair_key());
    proposals
}

/// Fetches an in-flight PR's changed-file paths. Abstracted behind a trait
/// so the orchestration layer is unit-testable without shelling out to
/// `gh` — production uses [`GhPrChangedFiles`], tests use a scripted double.
#[async_trait::async_trait]
pub trait PrChangedFilesFetcher: Send + Sync {
    /// Return the repo-relative paths the PR at `pr_url` changes. Errors are
    /// non-fatal to the sweep: the caller logs and skips that candidate.
    async fn changed_files(&self, pr_url: &str) -> Result<Vec<String>>;
}

/// Production [`PrChangedFilesFetcher`]: `gh pr view <url> --json files`.
/// Zero-sized — the PR URL fully identifies the repo, exactly as the merge
/// poller's own `gh pr view` probe relies on.
pub struct GhPrChangedFiles;

#[async_trait::async_trait]
impl PrChangedFilesFetcher for GhPrChangedFiles {
    async fn changed_files(&self, pr_url: &str) -> Result<Vec<String>> {
        let output = gh_output(&["pr", "view", pr_url, "--json", "files"])
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn `gh pr view {pr_url} --json files`: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "`gh pr view {pr_url} --json files` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| anyhow::anyhow!("failed to parse `gh pr view {pr_url} --json files`: {e}"))?;
        let paths = value
            .get("files")
            .and_then(|f| f.as_array())
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Ok(paths)
    }
}

/// In-memory, best-effort rate limiter for the stacking pass. Like
/// [`crate::speculative_conflict`]'s schedule this is purely in-memory: a
/// restart just means the next pass runs immediately and pairs may be
/// re-offered once, never a correctness problem for an advisory,
/// never-mutating path.
#[derive(Default)]
pub struct StackingSchedule {
    last_pass_at: Option<Instant>,
    /// Last time each pair (keyed by `(base_pr, dependent_pr)`) was offered.
    last_offered: HashMap<(i64, i64), Instant>,
}

impl StackingSchedule {
    fn pass_due(&self, now: Instant) -> bool {
        match self.last_pass_at {
            Some(last) => now.duration_since(last) >= MIN_PASS_INTERVAL,
            None => true,
        }
    }

    fn mark_pass(&mut self, now: Instant) {
        self.last_pass_at = Some(now);
    }

    fn offer_due(&self, pair: (i64, i64), now: Instant) -> bool {
        match self.last_offered.get(&pair) {
            Some(&last) => now.duration_since(last) >= REOFFER_INTERVAL,
            None => true,
        }
    }

    fn mark_offered(&mut self, pair: (i64, i64), now: Instant) {
        self.last_offered.insert(pair, now);
    }

    /// Drop offer timestamps older than [`REOFFER_INTERVAL`] so the map does
    /// not grow unbounded across a long-running engine process. Any entry
    /// that old would already permit a re-offer, so dropping it is
    /// behaviour-preserving.
    fn prune(&mut self, now: Instant) {
        self.last_offered
            .retain(|_, &mut last| now.duration_since(last) < REOFFER_INTERVAL);
    }
}

/// Piggyback on the merge poller's sweep: at most once per
/// [`MIN_PASS_INTERVAL`], fetch changed files for up to
/// [`MAX_BRANCHES_PER_PASS`] in-flight candidates, plan ordered-stack
/// proposals for overlapping same-product pairs, and emit each fresh
/// proposal as an advisory [`FrontendEvent::StackProposalOffered`]. Honors
/// the per-product `auto_pr_maintenance_enabled` opt-out; never mutates any
/// PR or task state.
pub async fn run_stacking_pass(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    fetcher: &dyn PrChangedFilesFetcher,
    metrics: &Registry,
    schedule: &mut StackingSchedule,
    candidates: &[PendingMergeCheck],
) {
    let now = Instant::now();
    if !schedule.pass_due(now) {
        return;
    }
    schedule.mark_pass(now);
    schedule.prune(now);

    // Gather in-flight branches with their (stack-worthy) changed files,
    // honoring the per-product opt-out and capping the fan-out per pass.
    let considered = candidates.len().min(MAX_BRANCHES_PER_PASS);
    if candidates.len() > MAX_BRANCHES_PER_PASS {
        tracing::info!(
            considered = MAX_BRANCHES_PER_PASS,
            total = candidates.len(),
            "stacked_pr_structuring: capped candidates this pass; remainder considered next pass",
        );
    }
    let mut branches: Vec<InFlightBranch> = Vec::with_capacity(considered);
    for candidate in candidates.iter().take(MAX_BRANCHES_PER_PASS) {
        // Same unified opt-out the escalation ladder, conflict_watch, and the
        // speculative sweep honour (design Q7): an opted-out product gets no
        // engine-driven PR activity, not even read-only offers.
        match work_db.product_auto_pr_maintenance_enabled(&candidate.product_id) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "stacked_pr_structuring: failed to read auto_pr_maintenance_enabled; treating as enabled",
                );
            }
        }
        let Some(pr_number) = parse_pr_number(&candidate.pr_url).filter(|n| *n > 0) else {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "stacked_pr_structuring: could not parse PR number; skipping",
            );
            continue;
        };
        let files = match fetcher.changed_files(&candidate.pr_url).await {
            Ok(files) => files,
            Err(err) => {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    error = %format!("{err:#}"),
                    "stacked_pr_structuring: changed-file fetch failed; skipping candidate",
                );
                continue;
            }
        };
        let changed_files: BTreeSet<String> = files.into_iter().filter(|p| is_stack_worthy_file(p)).collect();
        if changed_files.is_empty() {
            // Only mechanical/lockfile churn — nothing worth stacking over.
            continue;
        }
        branches.push(InFlightBranch {
            work_item_id: candidate.work_item_id.clone(),
            product_id: candidate.product_id.clone(),
            pr_url: candidate.pr_url.clone(),
            pr_number,
            changed_files,
        });
    }

    let proposals = plan_stack_proposals(&branches);
    if proposals.is_empty() {
        return;
    }
    tracing::debug!(
        proposals = proposals.len(),
        branches = branches.len(),
        "stacked_pr_structuring: sweep produced ordered-stack proposals",
    );

    for proposal in proposals {
        let key = proposal.pair_key();
        if !schedule.offer_due(key, now) {
            continue;
        }
        schedule.mark_offered(key, now);
        STACK_PROPOSAL_OFFERED.inc(metrics);
        record_offer_counter(metrics, &proposal.product_id);
        tracing::info!(
            product_id = %proposal.product_id,
            base_pr = proposal.base_pr_number,
            dependent_pr = proposal.dependent_pr_number,
            overlapping_files = proposal.overlapping_files.len(),
            "stacked_pr_structuring: offering ordered stack for predicted cross-branch conflict",
        );
        publisher
            .publish_frontend_event_on_product(
                &proposal.product_id,
                FrontendEvent::StackProposalOffered {
                    product_id: proposal.product_id.clone(),
                    base_pr_url: proposal.base_pr_url.clone(),
                    base_pr_number: proposal.base_pr_number,
                    dependent_pr_url: proposal.dependent_pr_url.clone(),
                    dependent_pr_number: proposal.dependent_pr_number,
                    overlapping_files: proposal.overlapping_files.clone(),
                },
            )
            .await;
    }
}

/// Whether a changed file counts toward the cross-branch overlap signal. We
/// exclude the mechanically-resolvable classes — lockfiles, `BUILD.bazel` /
/// `.bzl`, and `mod.rs`/`lib.rs`-style registries — because they are touched
/// by a large fraction of PRs and are union/regenerate-mergeable (the design
/// hands them to rung-0 resolvers), so overlap on them alone is a poor reason
/// to serialise two branches into a stack. Reuses the shipped Layer-0
/// classifier so the class list stays in one place.
fn is_stack_worthy_file(path: &str) -> bool {
    !matches!(
        crate::conflict_diagnosis::classify_conflict_class(std::slice::from_ref(&path.to_owned())),
        "lockfile" | "build_file" | "registry"
    )
}

#[cfg(test)]
#[path = "stacked_pr_structuring_tests.rs"]
mod tests;
