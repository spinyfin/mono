//! Worker completion detection.
//!
//! `PaneSpawnRunner` returns `WaitingHuman` immediately after spawning
//! the worker pane, so the run row is recorded as `completed` before
//! the worker has actually done any work. The execution sits in
//! `waiting_human` with the cube lease retained, and the linked
//! task/chore stays in `active` (kanban "Doing"). Without something
//! else driving the lifecycle, completed work just sits in Doing
//! forever — that is the bug this module exists to close.
//!
//! ## Detection
//!
//! The primary signal is the in-memory PR-URL staging cache populated
//! by the `PostToolUse` hook for `gh pr create` / `gh pr view` /
//! `gh pr edit`: when the worker's hook stream carries a PR URL we
//! finalize the work item against it without touching git or
//! GitHub at all.
//!
//! The cold-path fallback (incident 001, AI #6) handles the case where
//! staging is empty (engine restart, hook miss, etc.) by querying
//! `gh pr list --head <branch>` for the PR whose head matches the
//! engine-supplied per-execution branch name. The branch name is
//! derived deterministically from `execution_id` (see
//! [`expected_branch_name`]) and is injected into the worker prompt,
//! so workers push to the name the engine gave them — sibling workers
//! in other cube workspaces have different execution IDs and therefore
//! cannot collide.
//!
//! The branch-keyed query replaces the previous SHA-keyed
//! `jj_candidate_commit_shas` + `gh api commits/{sha}/pulls` recipe,
//! which was structurally unsafe under cube's shared
//! `.jj/repo/store/git`: bookmarks pushed by ANY concurrent worker
//! were visible from EVERY workspace's `jj log`, so the detector
//! routinely matched a sibling's bookmark and bound the wrong PR.
//! See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` for
//! the full incident write-up.
//!
//! Merges that happen *after* the worker exited are detected by a
//! periodic poller wired in `app.rs`, which calls
//! [`WorkDb::mark_chore_pr_merged`] for any chore in `in_review`
//! whose `pr_url` is now in a merged GitHub state.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use boss_protocol::{
    AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_PRODUCED_TASK, AUTOMATION_OUTCOME_SKIPPED, Attention,
    AttentionGroup, BranchNaming, CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_MERGE_CONFLICT_PREFIX,
    CREATED_VIA_PR_REVIEW_PREFIX, CreateRevisionInput, ExecutionKind, ExecutionStatus, FrontendEvent, ProposalKind,
    TaskKind,
};

use crate::attentions_detector;
use crate::automation_triage::{TriageDecision, parse_triage_decision};
use crate::build_wait::detect_build_wait_signal;
use crate::build_wait_tracker::{BuildWaitDecision, BuildWaitTracker, DEFAULT_BUILD_WAIT_HORIZON_SECS};
use crate::conflict_stop_gate::{self, ConflictClearance};
use crate::coordinator::{CubeClient, ExecutionPublisher, PreemptOutcome};
use crate::design_detector;
use crate::merge_poller::{
    MergeProbe, NoopMergeProbe, OpenPrCiStatus, OpenPrMergeability, PrLifecycleState, update_pr_poll_state,
};
use crate::metrics::Registry;
use crate::nudge_breaker::{DEFAULT_MAX_UNPRODUCTIVE_NUDGES, NudgeBreaker, NudgeDecision};
use crate::work::{CreateAttentionItemInput, PendingMergeCheck, WorkDb, WorkItem, WorkerPrCompletionTarget};
#[cfg(test)]
use crate::work::{FinishExecutionRunInput, TaskStatus};
use crate::worker_escalation::{self, WorkerSignal, WorkerSignalKind};
use boss_engine_gh_invocation::{gh_compare_jq, run_gh};
use boss_github::pr_url::pr_number_from_url;

// The inherent `impl WorkerCompletionHandler` is split across the submodules
// below to keep each file under the 3000-line size limit. Each contributes
// methods to the same type; the handler struct, shared types, traits, and free
// helpers stay in this parent module and reach the submodules via `use super`.
mod execution_started;
mod finalize_passes;
mod handler_build;
mod metadata_gate;
mod no_op;
mod nudge;
mod pr_transition;
mod recheck;
mod release;
mod remediation;
mod stop;
mod worker_signals;

// Phase-3 counter handles for the PR URL capture paths. The primary path
// fires when the PostToolUse staging cache already holds the URL; the
// reconstruction path fires when the cold-path `detect_pr` fallback is
// invoked instead.
crate::register_counter!(
    PR_URL_CAPTURE_PRIMARY_HIT,
    "pr_url_capture.primary_path.hit",
    "on_stop / recheck_for_pr found a staged PR URL and skipped the detector.",
);
crate::register_counter!(
    PR_URL_CAPTURE_RECONSTRUCTION_HIT,
    "pr_url_capture.reconstruction_path.hit",
    "detect_pr cold-path fallback was invoked (staging cache empty).",
);
crate::register_counter!(
    PR_URL_CAPTURE_RECONSTRUCTION_FAILED,
    "pr_url_capture.reconstruction_path.failed",
    "detect_pr cold-path fallback returned Err (network / date-format class).",
);
crate::register_counter!(
    PR_RECHECK_STAGED_BRANCH_MISMATCH,
    "pr_url_capture.recheck_staged.branch_mismatch",
    "staged URL's PR branch did not match execution's expected branch; URL was dropped.",
);

// Worker-proposal seam: fallback-hit counters for
// `detect_and_file_worker_signals`'s legacy marker parsers, incremented only
// when `worker_signal_proposals_seam` is on and no `worker_proposals` row
// covered the signal — the exit criterion for eventually deleting each
// parser (design §"Failure semantics: degrade loudly").
//
// Caveat: remote SSH-host workers always render the legacy marker-only
// prompt text, because `SshHostAdapter` holds no `FeatureFlagsStore` and so
// hardcodes the prompt-side seam flag to `false` regardless of the engine's
// (local) read-path flag state (see `host_adapter.rs`'s `compose_worker_spawn`
// call). Every remote worker's marker therefore counts here even when the
// read path is proposals-first, indistinguishable from a local worker simply
// ignoring the `boss propose` directive. Read this counter against local
// executions only until the remote path also reads feature flags — a nonzero
// count that includes remote executions does not mean the fallback is not
// yet quiet.
crate::register_counter!(
    WORKER_SIGNAL_FALLBACK_HIT_EFFORT_ESCALATION,
    "worker_proposals.fallback_hit.effort_escalation",
    "detect_and_file_worker_signals fell back to the legacy [effort-escalation] marker parser \
     because no worker_proposals row existed for the execution (worker_signal_proposals_seam on).",
);
crate::register_counter!(
    WORKER_SIGNAL_FALLBACK_HIT_BLOCKED,
    "worker_proposals.fallback_hit.blocked",
    "detect_and_file_worker_signals fell back to the legacy [blocked] marker parser because no \
     worker_proposals row existed for the execution (worker_signal_proposals_seam on).",
);

/// Register all PR-URL-capture counter handles with `registry`. Called from
/// [`crate::metrics_init::init_all`] at engine startup so duplicate-name panics
/// surface at boot rather than at the first counter increment.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&PR_URL_CAPTURE_PRIMARY_HIT);
    registry.register_counter(&PR_URL_CAPTURE_RECONSTRUCTION_HIT);
    registry.register_counter(&PR_URL_CAPTURE_RECONSTRUCTION_FAILED);
    registry.register_counter(&PR_RECHECK_STAGED_BRANCH_MISMATCH);
    registry.register_counter(&WORKER_SIGNAL_FALLBACK_HIT_EFFORT_ESCALATION);
    registry.register_counter(&WORKER_SIGNAL_FALLBACK_HIT_BLOCKED);
}

/// Catch-all `failure_reason` stamped on a `conflict_resolutions` row
/// when the bound worker exits without pushing and without otherwise
/// classifying the failure via `boss engine conflicts mark-failed`
/// (design Q5 / Phase 4 #11). The activity-feed surface renders it
/// loudly so the user knows the engine gave up rather than churning.
pub const CONFLICT_NO_PUSH_REASON: &str = "no_push_no_stop_condition";

/// Catch-all `failure_reason` stamped on a `ci_remediations` row when
/// the bound worker exits without pushing and without otherwise
/// classifying the outcome via `boss engine ci mark-failed`
/// (design §Phase 10 #33). Mirrors [`CONFLICT_NO_PUSH_REASON`]; the
/// name diverges to make audits unambiguous about which flow the
/// catch-all fired in.
pub const CI_NO_PUSH_REASON: &str = "no_push_no_classification";

/// Result of a [`WorkerPaneReleaser::release_pane`] call. Tells the
/// caller whether a live worker slot was actually found and reaped — the
/// signal that gates the cube-lease release in [`WorkerCompletionHandler::force_release`].
///
/// The distinction exists to close the T981 mid-spawn-cancel collision:
/// a worker whose pid has not yet materialized has no mapped slot, so the
/// pane release is a no-op (`NoLiveWorker`). Freeing its cube lease at
/// that point would hand a still-to-be-occupied workspace back to cube,
/// which then re-leases it to another execution — two live processes,
/// one working tree. The lease must stay held until the occupant is
/// genuinely gone; the in-flight run reaps + releases once its spawn
/// settles (see `PaneSpawnRunner::run_execution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneReleaseOutcome {
    /// A mapped worker slot was found: its pane was torn down and its OS
    /// process tree signalled (SIGTERM, escalating to SIGKILL). The
    /// workspace is no longer occupied, so the caller may free the lease.
    Reaped,
    /// No slot was mapped for the run. Either the worker already released
    /// (idempotent second call) or — the case this distinction exists for
    /// — it is still mid-spawn with no pid yet, so nothing could be
    /// reaped. The caller MUST NOT free the cube lease on this outcome.
    NoLiveWorker,
}

/// Asks the registered app session to tear down the libghostty pane
/// hosting `run_id`. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
/// The completion handler calls this after a successful cube lease
/// release on PR detection so the Workers grid pane disappears.
///
/// Returns [`PaneReleaseOutcome`] so the caller can decide whether it is
/// safe to release the cube lease (only when a live worker was reaped).
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome;
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs). Reports `Reaped` so the
/// lease-release path is unchanged for setups without a pane subsystem.
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

/// What GitHub reports about a PR associated with the worker's
/// local commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatus {
    /// No PR is associated with any of the worker's local commits.
    None,
    /// PR exists and at least one of the worker's local commit shas
    /// matches the PR's head — nothing local is unpushed.
    Fresh { url: String },
    /// PR exists, but the worker's local commits are ahead of the
    /// PR's pushed head sha. Treat as "no PR yet" for completion
    /// purposes; the worker is probed to push.
    Stale { url: String, reason: String },
    /// PR exists and head_match, but `changed_files == 0` — the worker
    /// pushed a commit with no file changes. Do not advance to
    /// `in_review`; probe the worker to make real edits or close the PR.
    EmptyDiff { url: String },
    /// PR exists and is already merged. Move the work item straight
    /// to `done`.
    Merged { url: String },
    /// PR exists but was closed without merging. The work item
    /// should not advance — surface like "no PR" so the worker can
    /// decide whether to reopen / open a new one.
    Closed { url: String },
}

impl PrStatus {
    /// PR url, regardless of state.
    pub fn url(&self) -> Option<&str> {
        match self {
            PrStatus::None => None,
            PrStatus::Fresh { url }
            | PrStatus::Stale { url, .. }
            | PrStatus::EmptyDiff { url }
            | PrStatus::Merged { url }
            | PrStatus::Closed { url } => Some(url),
        }
    }
}

/// Default worker branch-name prefix when the product's branch-naming
/// strategy is [`BranchNaming::BossExecPrefix`]. Preserves the
/// historical `boss/exec_<id>` shape so existing setups are unchanged.
pub const DEFAULT_WORKER_BRANCH_PREFIX: &str = "boss/";

/// Engine-supplied branch name a worker must push to when opening
/// the PR for an execution. The exact shape depends on the execution's
/// [`BranchNaming`] strategy (snapshotted from the product's
/// `editorial_rules.branch_naming` at spawn time) and on the execution's
/// frozen `worker_branch_prefix` (snapshotted from the product's
/// `Product::worker_branch_prefix` column):
///
/// - [`BranchNaming::BossExecPrefix`] (default): `<prefix><execution_id>`,
///   where `<prefix>` is `worker_branch_prefix` when the product set one
///   (e.g. `bduff/` → `bduff/exec_<id>`) and `boss/` otherwise. This is the
///   knob exposed by `boss product … --worker-branch-prefix`; the execution
///   id is kept verbatim so the branch is unique per execution by
///   construction.
/// - [`BranchNaming::OpaqueHash`]: `boss/<sha256(execution_id)[..8]>` —
///   omits the execution id from the branch name while remaining unique
///   within a repo (32 bits of hash space).
/// - [`BranchNaming::CustomPrefix`]: `<prefix>/<sha256(execution_id)[..8]>` —
///   user-supplied prefix instead of `boss/`, same opaque hash suffix.
///
/// `worker_branch_prefix` only affects the default `BossExecPrefix`
/// strategy: a non-default `branch_naming` is the richer, explicitly
/// configured editorial rule and takes precedence over the plain prefix
/// column. The two knobs also differ in slash convention —
/// `worker_branch_prefix` already carries its trailing `/` (it is
/// concatenated verbatim), whereas `CustomPrefix { prefix }` inserts a `/`.
///
/// In every strategy the branch name is derived deterministically from
/// `execution_id` (and the frozen prefix) so the detector can reconstruct
/// it from `state.db` alone — no local jj reads, no shared-store
/// contamination.
///
/// See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for
/// the uniqueness rationale. Cross-repo hash collisions (R6) are not
/// collisions: the `gh pr list --head` query is always scoped to the
/// product's `repo_remote_url`.
pub fn expected_branch_name(
    execution_id: &str,
    branch_naming: &BranchNaming,
    worker_branch_prefix: Option<&str>,
) -> String {
    match branch_naming {
        BranchNaming::BossExecPrefix => {
            let prefix = worker_branch_prefix.unwrap_or(DEFAULT_WORKER_BRANCH_PREFIX);
            format!("{prefix}{execution_id}")
        }
        BranchNaming::OpaqueHash => {
            let hash = opaque_hash(execution_id);
            format!("boss/{hash}")
        }
        BranchNaming::CustomPrefix { prefix } => {
            let hash = opaque_hash(execution_id);
            format!("{prefix}/{hash}")
        }
    }
}

/// First 8 hex characters of the SHA-256 digest of `execution_id`.
/// Used by [`BranchNaming::OpaqueHash`] and [`BranchNaming::CustomPrefix`]
/// to build a short, unique-by-construction branch suffix that does not
/// leak the literal execution id into the branch name.
fn opaque_hash(execution_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(execution_id.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The work-item-identifying suffix of a branch name: everything after
/// the final `/` (or the whole string when there is no `/`).
///
/// Every engine-supplied branch name has the shape
/// `<prefix>/<work-item-suffix>` (see [`expected_branch_name`]), where the
/// suffix is what uniquely binds the branch to one execution — the
/// `exec_<id>` under [`BranchNaming::BossExecPrefix`], or the opaque hash
/// under [`BranchNaming::OpaqueHash`] / [`BranchNaming::CustomPrefix`].
/// The prefix (`boss/`, `bduff/`, …) is cosmetic and product-configurable
/// (`worker_branch_prefix`), so PR↔work-item association keys on the
/// suffix, never on the prefix.
pub(crate) fn branch_work_item_suffix(branch: &str) -> &str {
    branch.rsplit('/').next().unwrap_or(branch)
}

/// Whether two branch names identify the same work item, **ignoring their
/// prefixes**. A worker that honours a product's `worker_branch_prefix`
/// (e.g. `bduff/`) opens its PR on `bduff/<suffix>` while the engine
/// reconstructs `boss/<suffix>` as the expected branch; those must
/// associate so the worker is not forced to abandon a compliant PR and
/// recreate it under `boss/` (see issue #1145).
///
/// The work-item suffix is unique per execution within a repo (it is the
/// execution id or a hash of it), so matching on the suffix alone is just
/// as safe against cross-execution mis-binding as the exact-branch match
/// it replaces — a sibling worker's branch cannot share this execution's
/// suffix. An empty suffix never matches (defensive: a malformed `…/`
/// branch must not collide with another).
pub(crate) fn branches_identify_same_work_item(a: &str, b: &str) -> bool {
    let suffix_a = branch_work_item_suffix(a);
    !suffix_a.is_empty() && suffix_a == branch_work_item_suffix(b)
}

/// Probes GitHub for the PR opened against an engine-supplied branch
/// name and reports whether the PR is open / merged / closed / absent.
///
/// `repo_remote_url` is the product's `git@github.com:owner/repo.git`
/// (or `https://...`) URL — the detector parses it into an
/// `owner/repo` slug used to scope the `gh pr list` query.
/// `expected_branch` is the engine-supplied head branch (see
/// [`expected_branch_name`]).
#[async_trait]
pub trait PrDetector: Send + Sync {
    /// Returns the PR status for `expected_branch` in `repo_remote_url`.
    /// Implementations must treat "no PR with this head" as
    /// `Ok(PrStatus::None)` to keep the caller's idle-vs-completed
    /// logic clean. Errors are reserved for tool failures (`gh` auth
    /// broken, network blips, etc.).
    async fn detect_pr(&self, repo_remote_url: &str, expected_branch: &str) -> Result<PrStatus>;
}

/// `PrDetector` that shells out to `gh pr list --head <branch>`. The
/// branch name is engine-supplied and execution-unique
/// (see [`expected_branch_name`]), so GitHub returns at most one PR
/// per query — there is no cross-execution overlap to exploit.
///
/// Replaces the pre-incident-001 SHA-keyed recipe
/// (`jj_candidate_commit_shas` + `gh api commits/{sha}/pulls`), which
/// was structurally unsafe under cube's shared `.jj/repo/store/git`:
/// any concurrent worker's bookmark passed the revset's
/// `committer_date(after:…)` gate and the detector misattributed PRs.
#[derive(Debug, Default)]
pub struct CommandPrDetector;

impl CommandPrDetector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PrDetector for CommandPrDetector {
    async fn detect_pr(&self, repo_remote_url: &str, expected_branch: &str) -> Result<PrStatus> {
        let repo_slug = parse_repo_slug(repo_remote_url)
            .with_context(|| format!("failed to parse repo slug from `{repo_remote_url}`"))?;
        let api_pr = match query_pr_for_branch(&repo_slug, expected_branch).await? {
            Some(pr) => pr,
            None => {
                // Prefix-agnostic fallback (issue #1145): the worker may
                // have honoured a product `worker_branch_prefix` and
                // pushed its PR to `<prefix>/<suffix>` (e.g. `bduff/…`)
                // rather than the engine-reconstructed `boss/<suffix>`.
                // The exact `--head` query above misses that PR, so
                // re-query by the work-item suffix (unique per execution
                // within the repo) and accept any prefix.
                let suffix = branch_work_item_suffix(expected_branch);
                match query_pr_by_branch_suffix(&repo_slug, suffix).await? {
                    Some(pr) => {
                        tracing::info!(
                            repo = %repo_slug,
                            expected_branch = %expected_branch,
                            pr_url = %pr.url,
                            "pr_detect: no PR on the exact expected branch, but found one whose work-item suffix matches under a different prefix; associating (prefix-agnostic match)",
                        );
                        pr
                    }
                    None => {
                        tracing::debug!(
                            repo = %repo_slug,
                            branch = %expected_branch,
                            "pr_detect: no PR found for expected branch (exact or suffix match); returning None",
                        );
                        return Ok(PrStatus::None);
                    }
                }
            }
        };
        let status = classify_pr(api_pr);
        // EmptyDiff is tentative: GitHub computes diff stats
        // asynchronously, so a freshly-pushed branch can report all
        // three stat fields as 0 before the computation finishes. Run
        // a secondary check against the full PR endpoint before
        // surfacing EmptyDiff — a false positive here would loop the
        // worker pane with bogus "your diff is empty" directives on
        // every Stop event.
        if let PrStatus::EmptyDiff { ref url } = status {
            tracing::debug!(
                pr_url = %url,
                repo = %repo_slug,
                "all diff stats zero on initial check; verifying via PR endpoint",
            );
            match verify_pr_diff_nonempty(&repo_slug, url).await {
                Ok(true) => {
                    tracing::debug!(
                        pr_url = %url,
                        "secondary check confirms non-empty diff; classifying as Fresh",
                    );
                    return Ok(PrStatus::Fresh { url: url.clone() });
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(
                        pr_url = %url,
                        ?err,
                        "secondary diff-stat check failed; surfacing as detector failure",
                    );
                    return Err(err);
                }
            }
        }
        Ok(status)
    }
}

/// Fetches the `headRefName` of a PR by number, used as the Layer-2
/// defence-in-depth check before a staged PR URL drives the in_review
/// transition. Decoupled from `PrDetector` so tests can stub the two
/// concerns independently.
#[async_trait]
pub trait BranchVerifier: Send + Sync {
    /// Returns the `headRefName` for PR `pr_number` in `repo_slug`, or
    /// an error on network / API failure.
    async fn fetch_pr_head_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String>;

    /// Returns the `headRefOid` (commit SHA of the PR's head ref) for
    /// PR `pr_number` in `repo_slug`. Used by the Stop-boundary
    /// SHA-delta gate to decide whether a resume run actually moved
    /// the chore's bound PR before falling through to the
    /// `PROBE_NO_PR` nudge.
    async fn fetch_pr_head_oid(&self, repo_slug: &str, pr_number: u64) -> Result<String>;

    /// Returns the total number of changed lines (additions + deletions)
    /// between `base` and `head` in `repo_slug`. Used by the no-op /
    /// trivial-diff skip gate (P992 design §8) to detect pure rebases and
    /// trivially-small pushes that don't warrant a fresh reviewer pass.
    async fn fetch_diff_line_count(&self, repo_slug: &str, base: &str, head: &str) -> Result<u64>;

    /// Returns the description/body of PR `pr_number` in `repo_slug`.
    /// Used by the metadata-only CI-fix finalize gate (issue #1252) to
    /// detect an operator-visible PR-metadata delta: a CI-fix revision
    /// that repairs a PR-description validator via `gh pr edit --body`
    /// makes no commit, so the head SHA never moves — the body diff is
    /// the only evidence the worker contributed. An empty body is a
    /// valid value (not an error), unlike the head-ref fetches.
    async fn fetch_pr_body(&self, repo_slug: &str, pr_number: u64) -> Result<String>;
}

/// `BranchVerifier` that shells out to `gh pr view`.
#[derive(Debug, Default)]
pub struct CommandBranchVerifier;

impl CommandBranchVerifier {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BranchVerifier for CommandBranchVerifier {
    async fn fetch_pr_head_ref(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        git_utils::gh_cli::fetch_pr_head_ref(repo_slug, pr_number).await
    }

    async fn fetch_pr_head_oid(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        git_utils::gh_cli::fetch_pr_head_oid(repo_slug, pr_number).await
    }

    async fn fetch_diff_line_count(&self, repo_slug: &str, base: &str, head: &str) -> Result<u64> {
        fetch_diff_line_count_cmd(repo_slug, base, head).await
    }

    async fn fetch_pr_body(&self, repo_slug: &str, pr_number: u64) -> Result<String> {
        fetch_pr_body_cmd(repo_slug, pr_number).await
    }
}

/// Shell out to `gh api repos/<repo_slug>/compare/<base>...<head>` and return
/// the total number of changed lines (additions + deletions) across all files
/// in the comparison. Returns `0` when the diff is empty (pure rebase with no
/// file-content changes). Used by the no-op skip gate (P992 design §8).
async fn fetch_diff_line_count_cmd(repo_slug: &str, base: &str, head: &str) -> Result<u64> {
    let trimmed = gh_compare_jq(
        repo_slug,
        base,
        head,
        "(.files // []) | map(.additions + .deletions) | add // 0",
    )
    .await?;
    let endpoint = format!("repos/{repo_slug}/compare/{base}...{head}");
    let total: u64 = trimmed
        .parse()
        .with_context(|| format!("unexpected output from `gh api {endpoint}`: {trimmed:?}"))?;
    Ok(total)
}

/// Shell out to `gh pr view <pr_number> -R <repo_slug> --json body` and
/// return the PR description. An empty body is a valid result (returned
/// as the empty string) — a PR can legitimately have no description, and
/// the metadata-fix gate needs to distinguish "" (snapshotted empty)
/// from a failed fetch.
async fn fetch_pr_body_cmd(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    run_gh(
        &[
            "pr", "view", &pr_str, "-R", repo_slug, "--json", "body", "--jq", ".body",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug} --json body"),
    )
    .await
}

/// Single PR row returned from `gh pr list --head <branch> --json …`.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
struct ApiPr {
    url: String,
    state: String,
    merged_at: Option<String>,
    /// Number of files changed in the PR.
    /// May be 0 when GitHub hasn't finished computing diff stats yet (race
    /// condition on a freshly-pushed branch); check `additions`/`deletions`
    /// before treating zero as "genuinely empty".
    changed_files: i64,
    /// Lines added in the PR.  0 means absent or not-yet-computed.
    additions: i64,
    /// Lines deleted in the PR.  0 means absent or not-yet-computed.
    deletions: i64,
}

/// Parse the first six tab-separated fields emitted by the shared
/// `gh pr list … --json url,state,mergedAt,changedFiles,additions,deletions
/// --jq … @tsv` query (in that exact order) into an [`ApiPr`].
///
/// Returns `None` when the URL field is empty — the `select(.)` /
/// row-absent case — matching the original `url.is_empty()` guard at both
/// call sites. `mergedAt` of empty or `"null"` (case-insensitively) maps to
/// `None`; the three numeric fields fall back to `0` when missing or
/// unparseable.
///
/// Any trailing fields beyond the first six (e.g. the `headRefName` column
/// in the suffix-scan query) are ignored, so callers that need them must
/// parse them separately from the same line.
fn parse_api_pr_tsv(line: &str) -> Option<ApiPr> {
    let mut parts = line.split('\t');
    let url = parts.next().unwrap_or("").trim().to_owned();
    let state = parts.next().unwrap_or("").trim().to_owned();
    let merged_at_raw = parts.next().unwrap_or("").trim();
    let changed_files_raw = parts.next().unwrap_or("0").trim();
    let additions_raw = parts.next().unwrap_or("0").trim();
    let deletions_raw = parts.next().unwrap_or("0").trim();
    if url.is_empty() {
        return None;
    }
    let merged_at = if merged_at_raw.is_empty() || merged_at_raw.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(merged_at_raw.to_owned())
    };
    Some(ApiPr {
        url,
        state,
        merged_at,
        changed_files: changed_files_raw.parse::<i64>().unwrap_or(0),
        additions: additions_raw.parse::<i64>().unwrap_or(0),
        deletions: deletions_raw.parse::<i64>().unwrap_or(0),
    })
}

fn classify_pr(pr: ApiPr) -> PrStatus {
    // Branch-keyed query already guarantees the PR was opened against
    // this execution's engine-supplied head branch — no SHA matching
    // needed. (Pre-incident-001 the detector ran a SHA-keyed query and
    // had to gate on `head.sha` matching a local commit to reject the
    // squash-merge-on-`main` misbind; branch-keyed detection makes that
    // gate structurally unnecessary because a sibling worker's
    // bookmark cannot share this execution's branch name.)
    if pr.merged_at.is_some() {
        return PrStatus::Merged { url: pr.url };
    }
    if pr.state.eq_ignore_ascii_case("closed") {
        return PrStatus::Closed { url: pr.url };
    }
    // OPEN. A PR has real changes if ANY of the three diff-stat fields
    // is positive.  `changed_files` alone is unreliable: GitHub computes
    // it asynchronously and `gh pr list` can return 0 for a freshly-pushed
    // branch before the computation finishes.  `additions` and `deletions`
    // are populated by the same pipeline but are often available sooner.
    // If ALL three are zero the PR is tentatively empty; `detect_pr` runs
    // a secondary verification call against the full PR endpoint before
    // surfacing `EmptyDiff` to callers.
    let has_changes = pr.changed_files > 0 || pr.additions > 0 || pr.deletions > 0;
    if has_changes {
        PrStatus::Fresh { url: pr.url }
    } else {
        PrStatus::EmptyDiff { url: pr.url }
    }
}

/// `gh pr list -R <slug> --head <branch> --state all` — return the
/// single PR for `branch`, or `Ok(None)` if no PR exists with that
/// head in `repo_slug`. `Err(_)` is reserved for tool / network failures.
///
/// `gh pr list --head` returns at most one open PR (GitHub enforces a
/// unique open PR per head branch), and historical closed/merged PRs
/// for the same head are extremely unlikely in practice because each
/// execution gets a unique branch name. We pass `--limit 1` defensively
/// — if multiple historical rows happen to exist, we want the most
/// recent (which `gh pr list` returns first).
async fn query_pr_for_branch(repo_slug: &str, branch: &str) -> Result<Option<ApiPr>> {
    let stdout = run_gh(
        &[
            "pr",
            "list",
            "-R",
            repo_slug,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "url,state,mergedAt,changedFiles,additions,deletions",
            "--jq",
            r#".[0] | select(.) | [(.url // ""), (.state // ""), (.mergedAt // ""), ((.changedFiles // 0) | tostring), ((.additions // 0) | tostring), ((.deletions // 0) | tostring)] | @tsv"#,
        ],
        &format!("gh pr list -R {repo_slug} --head {branch}"),
    )
    .await?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(parse_api_pr_tsv(trimmed))
}

/// Prefix-agnostic cold-path fallback (issue #1145): find a PR whose head
/// branch *ends in* `suffix` — i.e. `<any-prefix>/<suffix>` — in
/// `repo_slug`. Used when the exact `--head <boss/suffix>` query in
/// [`query_pr_for_branch`] finds nothing because the worker honoured a
/// product `worker_branch_prefix` and opened its PR under a different
/// prefix.
///
/// `gh pr list --head` only matches a full branch name, so there is no
/// server-side suffix filter; we list candidate PRs and filter in Rust by
/// [`branch_work_item_suffix`]. The work-item suffix is unique per
/// execution within a repo (the execution id or a hash of it), so at most
/// one PR can match — this preserves the incident-001 cross-execution
/// safety property (the query is still scoped to the product's repo and
/// keyed on an execution-unique token, never a shared SHA).
///
/// We scan open PRs first (the freshly-opened PR we are racing to
/// associate is open), bounded by `--limit`. If the page fills without a
/// match we emit a `warn!` rather than silently giving up, so a truncated
/// scan is visible.
async fn query_pr_by_branch_suffix(repo_slug: &str, suffix: &str) -> Result<Option<ApiPr>> {
    if suffix.is_empty() {
        return Ok(None);
    }
    const SCAN_LIMIT: usize = 100;
    let stdout = run_gh(
        &[
            "pr",
            "list",
            "-R",
            repo_slug,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "url,state,mergedAt,changedFiles,additions,deletions,headRefName",
            "--jq",
            r#".[] | [(.url // ""), (.state // ""), (.mergedAt // ""), ((.changedFiles // 0) | tostring), ((.additions // 0) | tostring), ((.deletions // 0) | tostring), (.headRefName // "")] | @tsv"#,
        ],
        &format!("gh pr list -R {repo_slug} --state all (suffix scan)"),
    )
    .await?;
    let mut rows = 0usize;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows += 1;
        // The shared 6-field parser ignores trailing columns, so pull the
        // 7th `headRefName` field out separately for the suffix filter.
        let head_ref = line.split('\t').nth(6).unwrap_or("").trim();
        if head_ref.is_empty() {
            continue;
        }
        if branch_work_item_suffix(head_ref) != suffix {
            continue;
        }
        if let Some(pr) = parse_api_pr_tsv(line) {
            return Ok(Some(pr));
        }
    }
    if rows >= SCAN_LIMIT {
        // Downgraded from `warn!` (2026-07-14 incident, T2608 / T2612): in a
        // busy repo with well over `SCAN_LIMIT` total PRs, hitting the row
        // cap is the expected steady state every single time the worker
        // genuinely has no PR yet — not evidence of a truncated search. This
        // fired on essentially every PR-less Stop while a worker legitimately
        // waited on a build, drowning out the rarer case worth a human's
        // attention (a worker that already pushed under a non-`boss/` prefix
        // whose PR is genuinely beyond the scanned page). `debug!` keeps the
        // information available without the log-volume cost.
        tracing::debug!(
            repo = %repo_slug,
            suffix,
            scanned = rows,
            "pr_detect: suffix scan hit the {SCAN_LIMIT}-PR limit without a match; a PR on a non-`boss/` prefix may exist beyond the scanned page",
        );
    }
    Ok(None)
}

/// Secondary diff-stat verification via the full PR endpoint.
///
/// The `commits/{sha}/pulls` response can report `changed_files == 0`
/// (and likewise `additions`/`deletions`) before GitHub finishes its
/// async diff computation on a freshly pushed branch.  This function
/// queries the authoritative per-PR endpoint and returns `true` when
/// the PR has at least one added or deleted line, so callers can
/// override an ambiguous `EmptyDiff` classification with `Fresh`.
///
/// An `Err` here means the secondary check itself failed (network blip,
/// `gh` auth issue, etc.). Callers must propagate this as a detector
/// failure rather than treating it as confirmation of an empty diff.
async fn verify_pr_diff_nonempty(repo_slug: &str, pr_url: &str) -> Result<bool> {
    let pr_number = pr_number_from_url(pr_url).ok_or_else(|| anyhow!("cannot parse PR number from URL: {pr_url}"))?;
    let endpoint = format!("repos/{repo_slug}/pulls/{pr_number}");
    let stdout = run_gh(
        &[
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            "((.additions // 0) + (.deletions // 0))",
        ],
        &format!("gh api {endpoint}"),
    )
    .await?;
    let total: i64 = stdout
        .trim()
        .parse()
        .with_context(|| format!("unexpected output from `gh api {endpoint}`: {:?}", stdout.trim()))?;
    Ok(total > 0)
}

/// Pull `owner/repo` out of a remote URL. Handles both SSH
/// (`git@github.com:owner/repo.git`) and HTTPS
/// (`https://github.com/owner/repo[.git]`) shapes.
pub(crate) fn parse_repo_slug(remote_url: &str) -> Result<String> {
    let (owner, repo) = git_utils::repo_slug::parse_github_owner_repo(remote_url)?;
    Ok(format!("{owner}/{repo}"))
}

/// Queues an automatic probe for `run_id`. The shape mirrors
/// `ServerState::queue_probe` but is exposed via a trait so the
/// completion handler can be unit-tested without standing up the full
/// app server. Implementations must be cheap and infallible — probes
/// that can't be delivered are dropped silently at injection time
/// (see `dispatch_probe_on_stop` in `app.rs`).
pub trait ProbeQueuer: Send + Sync {
    /// Push `text` onto the FIFO of probes for `run_id`. The next
    /// `Stop` event for the run pops one and `SendToPane`'s it as if
    /// the human had typed it.
    fn queue_probe(&self, run_id: &str, text: &str);

    /// Drop every not-yet-delivered probe queued for `run_id`.
    ///
    /// Exists for the escalation/blocker suppression path: a probe
    /// minted on an earlier Stop (e.g. a `PROBE_NO_PR` nudge whose
    /// `SendToPane` failed and was requeued for retry — see
    /// `dispatch_probe_on_stop`) can still be sitting in the queue when
    /// a *later* Stop reveals the worker is blocked. `dispatch_probe_on_stop`
    /// pops whatever is queued for a run on every `Stop` regardless of
    /// that Stop's own completion outcome, so a stale probe like that
    /// would otherwise fire even though this Stop just suppressed the
    /// nudge — the worker sees "You stopped without producing a PR"
    /// injected into its pane on the very turn it reported `[blocked]`.
    /// Called whenever `on_stop_inner` finds an unresolved worker
    /// signal, so no queued nudge survives into a Stop where nudging is
    /// suppressed.
    fn clear_pending_probes(&self, run_id: &str);
}

/// `ProbeQueuer` that drops everything — used when the test harness
/// doesn't need to assert on probe wiring.
#[derive(Debug, Default)]
pub struct NoopProbeQueuer;

impl ProbeQueuer for NoopProbeQueuer {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
    fn clear_pending_probes(&self, _run_id: &str) {}
}

/// Orchestrates the on-Stop completion flow: detect PR, transition
/// state in the work DB, release the cube lease, publish the right
/// invalidation events. Stateless — keeps the wiring side at the call
/// site (`app.rs`) thin.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerCompletionHandler {
    work_db: Arc<WorkDb>,
    pr_detector: Arc<dyn PrDetector>,
    cube_client: Arc<dyn CubeClient>,
    publisher: Arc<dyn ExecutionPublisher>,
    pane_releaser: Arc<dyn WorkerPaneReleaser>,
    probe_queuer: Arc<dyn ProbeQueuer>,
    /// Primary-path PR URL staging. The events-socket dispatcher in
    /// `app.rs` populates this from `PostToolUse` Bash hook events
    /// whose `tool_response.stdout` carries a `gh pr create` (or
    /// `gh pr view` / `gh pr edit`) URL. When `on_stop` /
    /// `recheck_for_pr` fires, peek this cache first: if a URL is
    /// staged we trust it verbatim (`PrStatus::Fresh`) and skip the
    /// `jj log` + `gh api commits/{sha}/pulls` reconstruction
    /// entirely. Reconstruction stays as the cold-path fallback for
    /// engine-restart recovery (the cache lives in memory only).
    ///
    /// Defaults to an empty cache so test sites that don't exercise
    /// the staging path get the same behaviour they always had —
    /// nothing is staged → fall through to `pr_detector`.
    staged_pr_urls: Arc<crate::pr_url_capture::StagedPrUrlCache>,
    /// In-memory set recording `revision_implementation` executions that ran a
    /// `jj git push` command since their last Stop boundary. Populated by the
    /// `PostToolUse` hook dispatcher; consumed (and cleared) by
    /// `on_stop_inner`'s SHA-delta `Contributed` gate to distinguish the
    /// revision's own push from a concurrent parent-worker push.
    staged_revision_pushes: Arc<crate::pr_url_capture::StagedRevisionPushCache>,
    /// Toggleable feature flags (incident 001 AI #5). Consulted by
    /// `on_stop_inner` and `recheck_for_pr` to decide whether the
    /// cold-path PR fallback is permitted to run. Defaults to a
    /// store whose only state is the registry defaults — tests that
    /// don't wire one in get the historical behaviour
    /// (`detect_pr_cold_fallback` defaults ON).
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Layer-2 defence-in-depth verifier. Before a staged PR URL drives
    /// the in_review transition, the verifier fetches the PR's
    /// `headRefName` and confirms it matches this execution's expected
    /// branch (`boss/<execution_id>`). A mismatch means the URL was
    /// staged from an unrelated Bash invocation — it is dropped and the
    /// cold-path detector runs instead.
    ///
    /// Defaults to `CommandBranchVerifier` (shells out to `gh pr view`).
    /// Tests that exercise the staged-URL path must wire in a stub via
    /// [`Self::with_branch_verifier`] to avoid live network calls.
    branch_verifier: Arc<dyn BranchVerifier>,
    /// Engine-wide counter registry. Defaults to a fresh local registry
    /// with the PR-capture counters pre-registered so tests that do not
    /// call `with_metrics` still get valid increments. Production wires
    /// in the shared engine registry via `with_metrics` after construction.
    metrics: Arc<Registry>,
    /// GitHub probe used for the on-transition CI-status pre-fetch.
    /// When a task moves to Review the handler spawns a background task
    /// that probes the new PR's CI state so the UI card has a real icon
    /// from the first poll rather than waiting for the merge-poller sweep.
    /// Defaults to [`NoopMergeProbe`]; production wires in the shared
    /// [`CommandMergeProbe`] via [`Self::with_merge_probe`].
    merge_probe: Arc<dyn MergeProbe>,
    /// Base backoff between the conflict stop gate's `mergeable=UNKNOWN`
    /// re-probes (see [`crate::conflict_stop_gate`]). Defaults to
    /// [`conflict_stop_gate::DEFAULT_UNKNOWN_RETRY_BACKOFF`]; tests set it
    /// to zero via [`Self::with_conflict_unknown_backoff`] so the
    /// indeterminate path doesn't sleep.
    conflict_unknown_backoff: std::time::Duration,
    /// Circuit breaker for the auto-nudge loop. Every nudge site routes
    /// through [`Self::nudge_or_park`], which records the nudge against
    /// this breaker; once `max_unproductive_nudges` consecutive nudges
    /// fire with no state change the execution is parked instead of
    /// nudged again (the Worf-incident fix). Shared via `Arc` so the
    /// per-execution counters survive across the multiple `on_stop`
    /// calls of a single worker session. Defaults to a fresh breaker.
    nudge_breaker: Arc<NudgeBreaker>,
    /// Cap on consecutive unproductive auto-nudges before the breaker
    /// trips. Defaults to [`DEFAULT_MAX_UNPRODUCTIVE_NUDGES`]; tests
    /// override it via [`Self::with_max_unproductive_nudges`].
    max_unproductive_nudges: u32,
    /// Time-bounded suppression tracker for the [`crate::build_wait`]
    /// signal: a worker narrating that it is waiting on a backgrounded
    /// build/test gate must not be nudged (each nudge just manufactures the
    /// next Stop and burns the breaker cap above), but that trust is not
    /// indefinite — see [`crate::build_wait_tracker`] for the incident this
    /// exists to fix. Shared via `Arc` so per-execution state survives
    /// across the multiple `on_stop` calls of a single worker session.
    build_wait_tracker: Arc<BuildWaitTracker>,
    /// How long a continuously-reported build-wait is trusted before
    /// [`Self::nudge_or_park`] stops suppressing and falls back to the
    /// normal nudge/park flow. Defaults to
    /// [`DEFAULT_BUILD_WAIT_HORIZON_SECS`]; tests override it via
    /// [`Self::with_build_wait_horizon_secs`].
    build_wait_horizon_secs: i64,
    /// Maximum number of automated reviewer passes per PR (P992 design §7).
    /// When a producing task's `review_cycle` reaches this value the engine
    /// skips the next reviewer pass and advances to human Review directly.
    /// Defaults to [`crate::config::DEFAULT_MAX_REVIEW_CYCLES`]; production
    /// wires in the value from `WorkConfig` via
    /// [`Self::with_max_review_cycles`].
    max_review_cycles: usize,
    /// Minimum changed-line count required to trigger a reviewer pass when
    /// `last_reviewed_sha` is set (P992 design §8). Pushes whose effective
    /// diff (new head vs. last-reviewed head) totals fewer lines than this
    /// threshold are skipped as trivial. Zero (the conservative default)
    /// means skip only when the diff is literally empty (pure rebase with
    /// no file-content changes). Production wires in the value from
    /// `WorkConfig` via [`Self::with_min_review_changed_lines`].
    min_review_changed_lines: u64,
    /// Kill-switch for the revision-triggered-review experiment
    /// (2026-07-01): when `true`, a `revision_implementation` execution that
    /// pushes new commits to its parent PR re-triggers a `pr_review` pass,
    /// same as the first push does — closing the gap where CI-fix,
    /// conflict-resolution, and operator-filed revisions landed unreviewed.
    /// Production wires in `WorkConfig.enable_revision_triggered_reviews`
    /// (default `true`) via [`Self::with_enable_revision_triggered_reviews`].
    /// Defaults to `false` here (not `true`, unlike the other review knobs
    /// above) so the large body of revision-completion tests written before
    /// this feature existed — which assert the legacy "revision advances
    /// straight to in_review with no reviewer" behaviour — keep passing
    /// without being individually updated; tests exercising the new
    /// behaviour opt in explicitly.
    enable_revision_triggered_reviews: bool,
    /// PR state checker passed to `create_revision` in
    /// `finalize_pr_review_pass`. Defaults to [`GhPrStateChecker`] (shells
    /// out to `gh pr view`); tests inject `FakePrStateChecker::always(Open)`
    /// via [`Self::with_pr_state_checker`] to avoid live network calls.
    pr_state_checker: Arc<dyn crate::work::PrStateChecker>,
    /// Engine-owned base directory for worker structured-output artifacts
    /// (review findings / task followups). Mirrors the dir the spawn path
    /// resolves via [`crate::structured_output::default_dir`]; both are the
    /// same in production. Tests point it at a tempdir via
    /// [`Self::with_structured_output_dir`] so they can seed/inspect the
    /// artifact without touching the shared system temp dir.
    structured_output_dir: std::path::PathBuf,
    /// Clock the auto-nudge debounce guard reads from
    /// ([`crate::nudge_breaker::MIN_RENUDGE_INTERVAL`]). Defaults to the
    /// real wall clock (`Instant::now`) — correct for production, where
    /// consecutive Stops for the same execution really are seconds to
    /// minutes apart (a worker's own turn latency). Tests that
    /// intentionally drive several `on_stop` calls back-to-back to
    /// exercise the circuit breaker's *count* (rather than its timing)
    /// wire in an auto-advancing fake clock via [`Self::with_now_fn`] so
    /// each call lands past the debounce window without a real sleep;
    /// dedicated debounce tests wire in a clock they control directly.
    now_fn: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
}

/// Outcome of [`WorkerCompletionHandler::try_retire_cleared_blocking_signal`].
enum BlockingSignalOutcome {
    /// The blocking signal was cleared and the attempt retired; the caller
    /// must return this outcome directly.
    Retired(StopOutcome),
    /// Nothing was retired. Carries whatever probe context the method
    /// already gathered along the way — `None` when it never got far
    /// enough to probe (kind mismatch, no live attempt, probe failure, or
    /// the PR is merged/closed), `Some` when it probed the PR and found an
    /// active conflict attempt whose mergeability was not yet `Clean`.
    /// Boxed: `ConflictSignalPrefetch` carries a full `PrLifecycleProbe`
    /// and is much larger than the common `Retired`/`NotRetired(None)`
    /// cases, so boxing avoids over-sizing every `BlockingSignalOutcome`
    /// (clippy::large_enum_variant).
    NotRetired(Option<Box<ConflictSignalPrefetch>>),
}

/// Probe context [`WorkerCompletionHandler::try_retire_cleared_blocking_signal`]
/// already gathered, reused by [`WorkerCompletionHandler::conflict_revision_stop_refusal`]
/// so it does not re-derive the parent attempt or re-probe a PR its caller
/// just probed microseconds earlier.
struct ConflictSignalPrefetch {
    /// The PR probe result. Never `Clean` for the caller's `conflict_attempt`
    /// — `try_retire_cleared_blocking_signal` already retires on `Clean`.
    ///
    /// This struct intentionally carries only the probe, not a
    /// has-active-attempt boolean: `conflict_revision_stop_refusal` always
    /// recomputes ownership itself via `has_active_conflict_attempt`, since
    /// this call site's parent-attempt lookup has no crz_id to match
    /// against and would silently bypass the ownership check if reused.
    probe: crate::merge_poller::PrLifecycleProbe,
}

/// What [`WorkerCompletionHandler::force_release`] actually did. Surfaced
/// (rather than swallowed as `()`) so a caller tearing down a specific
/// row — e.g. a deleted work item's live execution — can log one line
/// that ties the row id to the concrete outcome instead of leaving a
/// future zombie report to reconstruct it from execution-id-only lines
/// scattered across this function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceReleaseOutcome {
    /// No live worker pane was mapped (mid-spawn or already released).
    /// The cube lease is deliberately left held for the in-flight
    /// `run_execution` to reap and release once its spawn settles (T981).
    HeldForInFlightSpawn,
    /// The pane was reaped but the execution held no lease columns —
    /// already released by a prior call, or never leased.
    NoLeaseHeld,
    /// The pane was reaped and the cube lease released.
    Released { lease_id: String },
    /// The pane was reaped, but clearing the execution's workspace
    /// columns in the DB failed before a cube release was attempted.
    WorkspaceColumnClearFailed,
    /// The pane was reaped and workspace columns cleared, but the cube
    /// CLI call to release the lease failed.
    LeaseReleaseFailed { lease_id: String },
}

impl ForceReleaseOutcome {
    /// Short label for structured logging — stable across variant field
    /// changes so log queries filtering on it don't need updating.
    fn label(&self) -> &'static str {
        match self {
            Self::HeldForInFlightSpawn => "held_for_in_flight_spawn",
            Self::NoLeaseHeld => "no_lease_held",
            Self::Released { .. } => "released",
            Self::WorkspaceColumnClearFailed => "workspace_column_clear_failed",
            Self::LeaseReleaseFailed { .. } => "lease_release_failed",
        }
    }
}

#[async_trait]
impl crate::coordinator::ExecutionStartedHook for WorkerCompletionHandler {
    async fn on_execution_started(&self, execution_id: &str) {
        // Inherent method already does the work; this just satisfies
        // the trait the coordinator depends on.
        WorkerCompletionHandler::on_execution_started(self, execution_id).await
    }
}

#[async_trait]
impl crate::coordinator::AutomationPreemptor for WorkerCompletionHandler {
    /// Route dispatcher-initiated preemption through the exact teardown
    /// `bossctl agents stop` and the stale-worker sweep use: pane
    /// release + `reap_worker_process_tree` SIGTERM/SIGKILL ladder over
    /// the worker's whole process group, pool-slot release, then cube
    /// lease release.
    ///
    /// The `HeldForInFlightSpawn` → [`PreemptOutcome::MidSpawn`] mapping
    /// is the load-bearing one: `force_release` refuses to release a
    /// mid-spawn worker's lease (T981 — cube would re-lease a workspace
    /// the in-flight spawn is about to occupy), so the dispatcher must
    /// learn that nothing was torn down and abandon the preemption rather
    /// than requeue work whose worker is still very much alive.
    ///
    /// `NoLeaseHeld` counts as released: the pane was found and reaped
    /// (a `NoLiveWorker` pane short-circuits to `HeldForInFlightSpawn`
    /// above it), so the slot is free and the victim holds no cube
    /// resources to leak.
    async fn preempt_worker(&self, execution_id: &str) -> PreemptOutcome {
        match self.force_release(execution_id).await {
            ForceReleaseOutcome::Released { .. } | ForceReleaseOutcome::NoLeaseHeld => PreemptOutcome::Released,
            ForceReleaseOutcome::HeldForInFlightSpawn => PreemptOutcome::MidSpawn,
            outcome @ (ForceReleaseOutcome::LeaseReleaseFailed { .. }
            | ForceReleaseOutcome::WorkspaceColumnClearFailed) => {
                tracing::warn!(
                    execution_id,
                    outcome = outcome.label(),
                    "preemption teardown did not complete cleanly",
                );
                PreemptOutcome::Failed
            }
        }
    }
}

/// Outcome of the resume-bounce SHA-delta gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShaDeltaGateOutcome {
    /// The chore had a bound `pr_url`, snapshot+current SHA were
    /// fetched, and the SHAs differ — this run moved the bound PR.
    /// Caller should finalize via `finalize_pr_transition(InReview)`.
    /// `head_now` is the fetched current head; callers that suppress
    /// this outcome should persist it as the new baseline so subsequent
    /// sweeps do not re-trigger on the same delta.
    Contributed { pr_url: String, head_now: String },
    /// The chore had a bound `pr_url`, snapshot+current SHA were
    /// fetched, and the SHAs are equal — this run did not move the
    /// bound PR. Caller nudges the worker to push to the *existing*
    /// bound PR (never `gh pr create`), bounded by the circuit breaker,
    /// without falling through to the cold-path branch detector.
    /// `head_now` is the fetched current head (== baseline).
    NoContribution { pr_url: String, head_now: String },
    /// The gate could not evaluate (no bound PR, no snapshot, or a
    /// fetch failure). Caller falls through to the existing
    /// branch-keyed cold-path detector — preserves pre-change
    /// behaviour for the new-PR flow.
    Inapplicable,
}

/// Attention-item `kind` filed when the auto-nudge circuit breaker
/// parks an execution. Distinct kind so the coordinator/UI can surface
/// "worker parked: nudge loop bounded" separately from other attention
/// flows, and so repeated Stops dedupe against an already-open item.
pub const NUDGE_BREAKER_ATTENTION_KIND: &str = "nudge_breaker_tripped";

/// Attention-item kind filed when a reviewer worker exhausts its re-prompts
/// without ever producing a readable `ReviewResult` (neither the
/// structured-output artifact nor the transcript fallback validated). The
/// producing task is advanced to `in_review` without a revision, so this item
/// is the human-visible record that the PR proceeded *unreviewed* — replacing
/// the old silent drop.
pub const REVIEW_RESULT_GIVEUP_ATTENTION_KIND: &str = "review_result_missing";

/// Probe text dispatched when a worker stops without producing any PR
/// for its branch. Phrased so a worker that already finished the work
/// will simply push and open one, but a worker that's blocked has an
/// out to explain itself rather than churning.
pub const PROBE_NO_PR: &str = "You stopped without producing a PR for this work. \
If the work is complete, open the PR with `cube pr create --branch <bookmark>` (pushes the \
branch and opens the PR in one step, jj-aware, no GIT_DIR needed). If a PR already exists \
for this branch, push any new commits with `cube pr update --branch <bookmark>` instead — \
do not open a duplicate. If you're blocked, explain what you need.";

/// Extract the set of required-check names a `ci_remediations` attempt
/// was opened to fix, parsed from its `failed_checks` JSON snapshot
/// (each entry carries a `"name"` field; see `ci_watch::FailedCheckRecord`).
/// An empty array, malformed JSON, or entries without a name yield an
/// empty list — callers treat that as "no targeted-check information"
/// and fall back to requiring whole-PR `Clean`.
fn targeted_check_names(failed_checks_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(failed_checks_json)
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Array(arr) => Some(arr),
            _ => None,
        })
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the CI blocking signal *this attempt was opened for* is now
/// cleared on the bound PR.
///
/// The original heuristic keyed solely on whole-PR `Clean` (every
/// required check terminal-green). That misses a legitimate completion:
/// a remediation opened for one specific failing check (e.g. the "Pull
/// Request Description" check, often fixed by a metadata-only
/// `gh pr edit` with no commit) whose own check has gone green while
/// *other, unrelated* required checks remain red or pending. Such an
/// attempt has done its job and must be retired — not nudged forever to
/// "push your commits".
///
/// Decision table:
/// - `Clean`   → cleared (all required green; trivially clears any attempt).
/// - `Failing` → cleared iff none of the attempt's targeted checks are
///   among the currently-failing set. Remaining failures belong to other
///   checks and will drive their own remediation once the parent snaps
///   back to `in_review`.
/// - `InFlight`→ never cleared: at least one required check is still
///   non-terminal and we cannot tell from this aggregate whether the
///   targeted check specifically has reached terminal-green yet. Stay
///   conservative; the next sweep re-evaluates once checks terminalize.
///
/// When the attempt carries no parseable targeted-check names, only the
/// `Clean` case clears it — preserving the pre-change behaviour.
fn ci_attempt_signal_cleared(attempt_failed_checks: &str, ci: &OpenPrCiStatus) -> bool {
    match ci {
        OpenPrCiStatus::Clean => true,
        OpenPrCiStatus::InFlight => false,
        OpenPrCiStatus::Failing { failures } => {
            let targeted = targeted_check_names(attempt_failed_checks);
            if targeted.is_empty() {
                return false;
            }
            let failing_names: std::collections::HashSet<&str> = failures.iter().map(|f| f.name.as_str()).collect();
            !targeted.iter().any(|name| failing_names.contains(name.as_str()))
        }
    }
}

/// Whether an open PR's mergeability is good enough to call a worker's
/// deliverable satisfied at a Stop boundary where the head never moved.
///
/// `Clean` always qualifies and `Conflict` never does. `Unknown` — GitHub
/// still recomputing mergeability — is the interesting case: for an
/// ordinary execution it is tolerated (CI cleanliness carries the
/// decision there, and the merge poller re-blocks the PR later if the
/// recompute settles on CONFLICTING). For a **merge-conflict revision**
/// it must not qualify: clearing the conflict is that revision's entire
/// deliverable, `merge_conflict_revision` already waives the CI half of
/// the test, so accepting `Unknown` would finalize the run on no
/// evidence at all — precisely the "took the worker's word" failure
/// (spinyfin/mono#2070, 2026-07-23) this gate exists to prevent.
fn mergeability_satisfies_deliverable(mergeability: OpenPrMergeability, merge_conflict_revision: bool) -> bool {
    match mergeability {
        OpenPrMergeability::Clean => true,
        OpenPrMergeability::Conflict => false,
        OpenPrMergeability::Unknown => !merge_conflict_revision,
    }
}

/// Probe text dispatched when a PR is already bound to the worker's
/// chore (a resume, or a `ci_remediation` exec whose sibling
/// `chore_implementation` opened the PR). The worker must NEVER be told
/// to `gh pr create` here — its job is to push fixes to the existing
/// PR's branch. Phrased so a worker with nothing left to do can say so
/// rather than churning; the circuit breaker bounds repeats.
pub fn probe_push_to_existing_pr(pr_url: &str) -> String {
    format!(
        "A PR already exists for this work: {pr_url}. Do NOT open a new PR. If you have local \
commits, push them to the existing PR's branch with `cube pr update --branch <bookmark>`. If your \
changes are already pushed or there is nothing left to do, say so — explain your status instead of \
re-running."
    )
}

/// Probe text dispatched when a PR exists but the worker has local
/// commits that haven't been pushed yet — the PR is stale.
pub const PROBE_STALE_PR: &str = "A PR exists for this branch, but your local commits \
are ahead of the PR's head. Push the new commits with `cube pr update --branch <bookmark>` \
so the PR reflects your latest work, or explain why the local commits should not \
be pushed.";

/// Probe text dispatched when a PR exists and head_match is satisfied,
/// but the PR contains no file changes (`changed_files == 0`). The
/// worker likely pushed an empty commit without making any edits.
pub const PROBE_EMPTY_PR: &str = "The PR you opened has an empty diff — no files were \
changed. This usually means you committed and pushed without making any edits. \
Run `jj diff -r @` to verify your working-copy changes. If the diff is empty, \
you have not made any changes — do not keep this PR open. Either make the required \
edits and push them, or close the PR and explain what went wrong.";

/// What happened during a stop event handler invocation. The runtime
/// only logs this; tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Stop arrived for a run id that doesn't map to a known execution
    /// (e.g., test infra, agent runs).
    UnknownExecution,
    /// Execution was already in a terminal status — no transition.
    AlreadyTerminal,
    /// The Stop arrived for an execution that is a stale prior occupant
    /// of a reused (warm-cached) cube workspace — a newer live execution
    /// now claims the same workspace. The event leaked from a stale
    /// `boss-event` hook registration left in the re-leased workspace;
    /// processing it would mis-attribute completion or release the live
    /// run's workspace. Quiet outcome — no transition, no reap. The
    /// newest execution's own Stop drives its completion.
    SupersededInWorkspace,
    /// Execution had no workspace_path recorded.
    NoWorkspace,
    /// `gh` failed with a non-"no-PR" error; surfaced as awaiting input.
    DetectorFailed,
    /// No PR yet — worker is idle awaiting input.
    AwaitingInput,
    /// AI #6 / incident 001: the Stop hook fired for an execution in
    /// `running` status with an empty staged-URL cache. The fallback
    /// is reserved for `waiting_human`; the worker is still alive and
    /// any positive result would race against its own in-flight push.
    /// Quiet outcome — no probe, no publish, no transition.
    RunningNoStagedPr,
    /// AI #5 / incident 001: the human has flipped the
    /// `detect_pr_cold_fallback` feature flag OFF via the debug pane,
    /// so the cold-path fallback is suppressed. With no staged URL
    /// the engine treats the empty staging as "no PR pushed" — the
    /// chore stays in `waiting_human` for the human to resolve by
    /// hand. Quiet outcome — no probe, no publish, no transition.
    FallbackDisabledByFlag,
    /// PR detected; work item moved to `in_review` and execution finalised.
    PrDetected { pr_url: String },
    /// PR detected and already merged at Stop time; work item moved
    /// straight to `done` and execution finalised.
    PrMerged { pr_url: String },
    /// PR exists but local commits are ahead of its head sha. The
    /// worker is probed to push the missing commits; the work item
    /// stays in its current state until the next Stop reports a fresh PR.
    ///
    /// Post-incident-001 the branch-keyed detector cannot produce this
    /// classification (there is no SHA matching to fail). The variant
    /// is kept for callers that already pattern-match on it.
    StalePr { pr_url: String, reason: String },
    /// PR exists and head_match, but has zero file changes. The worker
    /// is probed to make real edits or close the PR; the work item
    /// stays in its current state.
    EmptyDiffPr { pr_url: String },
    /// The auto-nudge circuit breaker tripped: the worker was nudged
    /// `max_unproductive_nudges` consecutive times with no new commit,
    /// PR, or state transition. The execution is parked (an attention
    /// item is filed and an `AttentionItemCreated` event published)
    /// instead of being nudged again. `reason` is the human-readable
    /// explanation recorded on the attention item.
    NudgeBreakerParked { reason: String },
    /// The worker emitted an `[effort-escalation]` or `[blocked]` marker on
    /// a prior Stop and the filed attention item is still unresolved: the
    /// "produce a PR" auto-nudge is suppressed for this execution (never
    /// queued via `nudge_or_park`) instead of dogging a worker that already
    /// declared itself stuck awaiting coordinator direction (incident
    /// 2026-07-02, exec_18b5243e65ff188_2d / T2085). `reason` names the
    /// pending signal kind(s). The execution stays `waiting_human`; no
    /// probe is sent. Resolving the attention item (coordinator ack)
    /// resumes normal nudging on the next Stop.
    EscalationPending { reason: String },
    /// The worker's Stop-boundary text matched the [`crate::build_wait`]
    /// heuristic — it is narrating that it is legitimately waiting on a
    /// backgrounded build/test gate (2026-07-14 incident, T2608 / T2612:
    /// `exec_18c21add1416b5e8_3b`, `exec_18c21ba9b3fd2ef8_9e`). The
    /// "produce a PR" auto-nudge is suppressed — and, critically, the
    /// circuit breaker in [`crate::nudge_breaker`] is never even consulted,
    /// so this Stop does not burn any of its cap — for as long as
    /// [`crate::build_wait_tracker::BuildWaitTracker`] considers the wait
    /// still within its horizon. `waited_secs` is how long this execution
    /// has been continuously reporting the wait. The execution stays
    /// `waiting_human`; no probe is sent, nothing is parked. Once the
    /// horizon elapses, the normal nudge/park flow resumes automatically
    /// (no coordinator action required — unlike [`StopOutcome::EscalationPending`]).
    BuildWaitPending { waited_secs: i64 },
    /// The worker is a conflict-resolution or CI-failure revision that
    /// stopped without pushing, but the blocking signal was already
    /// cleared (conflict: PR `mergeable`; CI: required checks green)
    /// before this run started. The active `conflict_resolutions` or
    /// `ci_remediations` attempt is retired as `succeeded`, the parent
    /// task is snapped back to `in_review`, and the execution is
    /// finalised. No nudge is sent.
    SignalAlreadyCleared { pr_url: String },
    /// The bound PR was in a deliverable-satisfied state at Stop time
    /// — open with CI clean and no merge conflict, or already merged —
    /// even though the worker did not push new commits this run.
    /// Detected by `try_finalize_satisfied_deliverable_on_stop` in
    /// the `NoContribution` branch of `on_stop_inner`. The execution
    /// is finalised and the worker reaped without nudging, preventing
    /// the "nothing left to do" zombie loop (T740 follow-on).
    DeliverableSatisfied { pr_url: String },
    /// A CI-remediation worker classified the failure as flaky/infra and
    /// re-triggered the failing job (`boss engine ci mark-retriggered`),
    /// which stamped the `ci_flaky_retriggered` signal on the parent.
    /// There is genuinely nothing to push, so the completion path parks
    /// the worker — awaiting the CI retry or a human decision — instead of
    /// probing it for a diff. No nudge is sent; the execution is left
    /// `waiting_human`. This is the fix for the stuck-loop bug where the
    /// engine re-derived the same flaky verdict on every probe.
    FlakyRetriggered { pr_url: String },
    /// Maint task 6: an `automation_triage` execution finished and its
    /// final message was run through the marker-protocol outcome detector.
    /// `outcome` is the `automation_runs.outcome` discriminator recorded
    /// (`produced_task` / `skipped` / `failed_will_retry`). The execution is
    /// finalised (`completed`) and its pane/workspace released regardless of
    /// which marker (if any) the agent emitted.
    AutomationTriage { outcome: String },
    /// P3b: an `answer_agent` execution finished. `replied` is `true` when
    /// the agent posted its reply mid-session via `CommentsPostAnswer`
    /// before Stop fired, `false` when the session ended without one (the
    /// finalizer marks the run `failed` and posts an apology thread entry so
    /// the comment doesn't sit in `answering` forever). The execution is
    /// finalised (`completed`) and its pane/workspace released either way.
    AnswerAgent { replied: bool },
    /// P992 task 7: a primary-implementation worker's PR was detected and
    /// an independent reviewer pass has been enqueued. The producing task
    /// remains in `active` (Doing column) until the reviewer resolves.
    ReviewerEnqueued { pr_url: String },
    /// P992 task 7: a `pr_review` reviewer execution finished and the
    /// producing task has been advanced to `in_review`.
    ReviewPassCompleted { pr_url: String },
    /// A `pr_review` reviewer execution stopped without a readable
    /// `ReviewResult` — neither the structured-output artifact nor the
    /// transcript fallback yielded a valid object. The engine queued a probe
    /// asking the reviewer to (re-)write the artifact and left the execution
    /// live (non-terminal); the next Stop re-runs the finalizer. This replaces
    /// the old silent "advance to in_review with no revision" drop. Bounded by
    /// the auto-nudge breaker — once it trips, the finalizer advances to
    /// `in_review` without a revision and files an attention.
    ReviewPassAwaitingResult,
    /// P992 task 8: a `pr_review` reviewer execution found qualifying findings
    /// (at least one `critical`/`high` severity or `regression` category) and
    /// created a revision task on the producing task. The producing task is
    /// advanced to `in_review`; the revision is dispatched on the general
    /// worker pool to apply the feedback. Nothing is posted to GitHub.
    ReviewPassRevisionCreated { pr_url: String, revision_task_id: String },
    /// T1868: a primary-implementation worker (`chore_implementation` /
    /// `task_implementation`) verified its assigned work was already done —
    /// the change is already on `main`, the diff is empty, and there is
    /// genuinely nothing to commit/push/open a PR for — and emitted the
    /// sanctioned [`NO_CHANGES_NEEDED`](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER)
    /// marker. The task is closed as `done` WITHOUT a PR and the execution is
    /// finalised. No nudge is sent. This is the fix for the produce-a-PR nudge
    /// loop on a worker that correctly found nothing to do. `work_item_id` is
    /// the closed task/chore.
    NoChangesNeeded { work_item_id: String },
    /// Unexpected DB failure while recording completion.
    DbError,
}

/// Number of transcript-read attempts [`WorkerCompletionHandler::read_final_triage_message`]
/// makes before concluding a triage transcript genuinely has no assistant
/// text. Mirrors the linear-backoff shape of `ci_log_reader::run_capture`'s
/// `ETXTBSY` retry, sized for a local disk flush rather than a subprocess
/// spawn: attempts sleep `TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * attempt`
/// between reads, for a worst-case total wait of
/// `TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * (1+2+3+4) = 300ms` — comfortably
/// wider than the single-digit-millisecond flush race seen in the field
/// (assistant text written 33ms before the finaliser's read, which still
/// lost the race with zero retries).
const TRIAGE_TRANSCRIPT_READ_ATTEMPTS: u32 = 5;
const TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS: u64 = 20;

/// Outcome of reading a finished triage execution's final assistant message
/// from its transcript (see [`WorkerCompletionHandler::read_final_triage_message`]).
///
/// Distinguishing these states is what makes a `failed_will_retry` triage run
/// diagnosable from the run-history `detail`: "produced no transcript" (worker
/// session never started), "transcript unreadable", and "no assistant prose"
/// are very different failures from "the worker spoke but emitted no marker",
/// yet all four previously collapsed to the bare string
/// "triage ended without a decision marker".
#[derive(Debug, Clone, PartialEq, Eq)]
enum TriageTranscript {
    /// The final assistant text message — the one the marker parser scans.
    FinalMessage(String),
    /// No `transcript_path` was recorded for the execution. The worker session
    /// likely never started (or its run row was never linked to a transcript).
    NoPath,
    /// A transcript path was recorded but the file could not be read (lookup
    /// error or filesystem read error).
    Unreadable,
    /// The transcript parsed but contained no assistant text event across
    /// every retried read (see [`TRIAGE_TRANSCRIPT_READ_ATTEMPTS`]) — the
    /// worker emitted only tool calls / thinking, or crashed before any
    /// prose. `event_count` is the non-assistant-text event count from the
    /// last read, so the run-history detail can distinguish "transcript was
    /// entirely empty" from "transcript recorded activity but no prose".
    NoAssistantText { event_count: usize },
}

impl TriageTranscript {
    /// The final assistant message text, or `None` for any state in which no
    /// message could be read. Lets callers that only need the text (e.g. the
    /// `pr_review` finaliser) ignore the failure-state distinction.
    fn into_message(self) -> Option<String> {
        match self {
            TriageTranscript::FinalMessage(text) => Some(text),
            TriageTranscript::NoPath | TriageTranscript::Unreadable | TriageTranscript::NoAssistantText { .. } => None,
        }
    }
}

/// Build the `failed_will_retry` detail for a triage run that yielded no
/// usable decision, from the transcript readback state.
///
/// The `FinalMessage` arm keeps the stable "triage ended without a decision
/// marker" prefix (so existing log greps / dashboards keep matching) and
/// appends a bounded, single-line tail of what the agent actually said — the
/// single most useful breadcrumb when debugging why a marker was missing.
fn triage_no_decision_detail(transcript: &TriageTranscript) -> String {
    match transcript {
        TriageTranscript::FinalMessage(text) => format!(
            "triage ended without a decision marker; final message tail: {}",
            tail_snippet(text, 200)
        ),
        TriageTranscript::NoPath => "triage produced no transcript (no transcript path \
             recorded; the worker session may have failed to start)"
            .to_owned(),
        TriageTranscript::Unreadable => "triage transcript could not be read from disk".to_owned(),
        // Two genuinely different conditions, previously conflated into one
        // string that asserted "worker emitted no prose" even when the
        // transcript showed the worker was clearly active (a claim the
        // event-count evidence didn't support). After
        // `TRIAGE_TRANSCRIPT_READ_ATTEMPTS` retries drained the Stop-boundary
        // flush race, `event_count == 0` means the transcript itself never
        // materialised any content; `event_count > 0` means the worker acted
        // (tool calls / thinking) but never produced assistant prose.
        TriageTranscript::NoAssistantText { event_count: 0 } => "triage transcript contained no events at all \
             (no assistant text, tool calls, or thinking recorded before stopping)"
            .to_owned(),
        TriageTranscript::NoAssistantText { event_count } => format!(
            "triage transcript recorded {event_count} event(s) (tool calls / thinking) but no \
             assistant text after waiting out the Stop-boundary transcript-flush window; worker \
             emitted no prose before stopping"
        ),
    }
}

/// The trailing window (in characters) of the collapsed final message that the
/// skip-recovery conclusion scan considers. Scoped to the *tail* so a
/// mid-investigation "no warnings in module X" line early in the run cannot trip
/// a skip; only the worker's closing words count.
const SKIP_RECOVERY_TAIL_CHARS: usize = 400;

/// Phrases whose presence in the final tail VETO skip-recovery — the worker was
/// still mid-verification / deferring a decision, not concluding, so the run
/// must stay `failed_will_retry`. These mirror the field-evidence tails ("I'll
/// wait for checkleft to finish before deciding", "The authoritative checkleft
/// run is in progress. Let me wait for it to complete", "Let me do one
/// confirming check", "Let me broaden the check to the whole repo to be
/// thorough"). Over-matching here is safe: it only keeps the conservative
/// `failed_will_retry` default; it can never *cause* a false skip.
const SKIP_RECOVERY_DEFERRAL_VETOES: &[&str] = &[
    "wait",
    "in progress",
    "still running",
    "is running",
    "before deciding",
    "to be thorough",
    "one more",
    "one confirming",
    "let me ",
    "i'll ",
    "i will ",
    "going to",
    "let's ",
];

/// Phrases that affirmatively conclude there is no actionable work — a
/// clean-repo / no-warnings verdict. At least one must appear in the final tail
/// for skip-recovery to fire.
const SKIP_RECOVERY_NO_WORK_SIGNALS: &[&str] = &[
    "no warnings",
    "no compiler warning",
    "no compilation warning",
    "no clippy",
    "no lint",
    "nothing to do",
    "nothing actionable",
    "no actionable",
    "nothing to fix",
    "no action needed",
    "no action required",
    "already clean",
    "is clean",
    "are clean",
    "no issues found",
    "no findings",
    "no work to do",
    "no changes needed",
];

/// Skip marker-recovery — the symmetric counterpart to the produced-task
/// marker-recovery in [`WorkerCompletionHandler::finalize_automation_triage`].
///
/// When a triage run created **no** task and emitted **no** valid decision
/// marker, but its final message plainly concluded there is nothing to do, this
/// returns `Some(reason)` so the run is recorded `skipped` rather than looping
/// `failed_will_retry` forever (each retry burning a full worker session while
/// re-proving a repo that is already clean).
///
/// Deliberately conservative — a *false* skip merely defers to the automation's
/// next scheduled fire (cheap), whereas a false `failed_will_retry` re-runs a
/// whole session. It fires ONLY when:
/// - the decision is [`TriageDecision::NoDecision`], and
/// - a final assistant message exists (not a `NoPath`/`Unreadable`/no-prose
///   state — there is nothing to conclude from), and
/// - the *tail* of that message affirmatively concludes "no work" AND shows no
///   mid-verification / deferred-intent language (the field-evidence failure
///   shape, which must stay `failed_will_retry`).
fn recover_skip_reason(decision: &TriageDecision, transcript: &TriageTranscript) -> Option<String> {
    if !matches!(decision, TriageDecision::NoDecision) {
        return None;
    }
    let TriageTranscript::FinalMessage(text) = transcript else {
        return None;
    };
    if !tail_concludes_no_work(text) {
        return None;
    }
    Some(tail_snippet(text, 200))
}

/// Scan the tail of a triage worker's final message for an affirmative
/// "nothing to do" conclusion with no deferred-intent / mid-verification
/// language. See [`recover_skip_reason`] for why this is intentionally strict.
fn tail_concludes_no_work(text: &str) -> bool {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let chars: Vec<char> = one_line.chars().collect();
    let start = chars.len().saturating_sub(SKIP_RECOVERY_TAIL_CHARS);
    let tail: String = chars[start..].iter().collect();
    if SKIP_RECOVERY_DEFERRAL_VETOES.iter().any(|v| tail.contains(v)) {
        return false;
    }
    SKIP_RECOVERY_NO_WORK_SIGNALS.iter().any(|s| tail.contains(s))
}

/// Collapse `text` to a single-line tail of at most `max_chars` characters for
/// embedding in a run-history `detail`. Whitespace runs (including newlines)
/// collapse to single spaces; when truncated, the result is prefixed with `…`
/// so it reads as a tail rather than a head.
fn tail_snippet(text: &str, max_chars: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "(empty)".to_owned();
    }
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= max_chars {
        one_line
    } else {
        let tail: String = chars[chars.len() - max_chars..].iter().collect();
        format!("…{tail}")
    }
}

/// Elapsed whole seconds since `timestamp`, a unix-seconds string as stored
/// by `now_string()` (used for `WorkExecution::started_at`/`created_at`).
/// Returns `None` if `timestamp` is unparseable. Used to log how long a
/// `pr_review` pass took (2026-07-01 revision-review experiment) so the
/// engine surfaces can track cost without a schema change.
fn elapsed_secs_since(timestamp: &str) -> Option<i64> {
    let then: i64 = timestamp.parse().ok()?;
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    Some((now - then).max(0))
}

fn work_item_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.id.clone(),
    }
}

/// Whether completing a primary-implementation execution with a fresh PR
/// should trigger an independent reviewer pass (P992 design §1). When this
/// returns true, the producing task's column transition is held in
/// `PendingReview`/Doing until the reviewer finalises.
///
/// `RevisionImplementation` is handled separately, at the call site in
/// [`WorkerCompletionHandler::finalize_pr_transition`]: every revision kind
/// (reviewer-spawned, CI-fix, conflict-resolution, and human/operator-
/// initiated) that pushes new commits to its parent PR re-triggers a
/// reviewer pass, gated only by the
/// [`WorkerCompletionHandler::enable_revision_triggered_reviews`] kill-switch
/// (2026-07-01 revision-review experiment — closes the gap where only the
/// first push on a PR was ever reviewed).
fn should_enqueue_reviewer_for_primary(kind: &ExecutionKind) -> bool {
    matches!(
        kind,
        ExecutionKind::ChoreImplementation | ExecutionKind::TaskImplementation
    )
}

#[cfg(test)]
mod tests;
