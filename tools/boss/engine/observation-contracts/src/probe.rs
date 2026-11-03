use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use boss_github::RequiredCheckFailure;

/// One slice of GitHub-reported PR lifecycle state, captured by a
/// single `gh pr view` round-trip. Carries everything the poller's
/// sweep dispatch needs to route to merge/conflict/CI/clear paths.
///
/// The "four-state" naming in the design doc refers to the leaf
/// values of [`PrLifecycleState`] — `Open(...)` (with its own
/// mergeability + ci sub-state), `Merged`, `ClosedUnmerged`.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrLifecycleProbe {
    pub url: String,
    pub state: PrLifecycleState,
    /// Sha of the PR's base ref at probe time. Captured for the
    /// conflict-resolution flow (`conflict_resolutions.base_sha_at_trigger`,
    /// design Q3); currently informational for the merge poller.
    /// `None` when GitHub didn't report one (rare; usually means the
    /// PR has been force-detached from its base).
    pub base_ref_oid: Option<String>,
    /// Sha of the PR's head ref at probe time. The CI-watch path's
    /// idempotency key (`(work_item_id, head_sha_at_trigger,
    /// attempt_kind)`) needs this; `None` when GitHub didn't report
    /// it (rare).
    pub head_ref_oid: Option<String>,
    /// Name of the PR's head branch (e.g. `"my-feature"`). Required by
    /// the conflict-resolution attempt row (`head_branch` column); `None`
    /// when GitHub didn't report it.
    pub head_ref_name: Option<String>,
    /// Name of the PR's base branch (e.g. `"main"`). Required by the
    /// conflict-resolution attempt row (`base_branch` column); `None`
    /// when GitHub didn't report it.
    pub base_ref_name: Option<String>,
    /// Labels currently applied to the PR. Carried so the
    /// conflict-watch / auto-rebase / ci-watch paths can honour the
    /// per-PR opt-out label (`boss/no-auto-rebase`, design Q7 /
    /// Phase 6 #18) without a second `gh` round trip.
    pub labels: Vec<String>,
    /// Review-gating state derived from GitHub's `reviewDecision` and
    /// `reviews` fields. Used by the merge poller to update the
    /// `review_required_state` / `review_required_detail` columns on
    /// the task row for display in the macOS kanban Review-lane card.
    pub review: PrReviewState,
    /// Whether the PR is currently in GitHub's merge queue at probe time.
    /// Derived from `mergeQueueEntry` — non-null means in queue, null means
    /// not queued. Used to render the merging indicator on Review-lane cards
    /// (replaces the CI icon while the PR is merging).
    #[builder(default)]
    pub in_merge_queue: bool,
    /// GitHub's raw `mergeQueueEntry.state` (e.g. `"AWAITING_CHECKS"`,
    /// `"MERGEABLE"`, `"LOCKED"`, `"QUEUED"`, `"UNMERGEABLE"`). `None` when
    /// `in_merge_queue` is `false` or GitHub omitted the field.
    pub merge_queue_entry_state: Option<String>,
    /// GitHub's raw `mergeQueueEntry.position` (1-indexed queue position).
    /// `None` when `in_merge_queue` is `false` or GitHub omitted the field.
    pub merge_queue_position: Option<i64>,
    /// GitHub's raw `mergeQueueEntry.enqueuedAt` (RFC 3339 timestamp of when
    /// the PR entered the queue). `None` when `in_merge_queue` is `false` or
    /// GitHub omitted the field.
    pub merge_queue_enqueued_at: Option<String>,
    /// Raw `mergeable` string from GitHub (e.g. `"MERGEABLE"`, `"CONFLICTING"`,
    /// `"UNKNOWN"`). Carried through so callers can log the exact GitHub signal
    /// that drove each transition decision without a second round trip.
    #[builder(default)]
    pub raw_mergeable: String,
    /// Raw `mergeStateStatus` string from GitHub (e.g. `"CLEAN"`, `"DIRTY"`,
    /// `"BLOCKED"`, `"BEHIND"`, `"UNKNOWN"`). Paired with `raw_mergeable`
    /// for diagnosability on transition log lines.
    #[builder(default)]
    pub raw_merge_state_status: String,
    /// Whether GitHub's auto-merge is currently armed for this PR
    /// (`autoMergeRequest` non-null) — the PR will merge automatically once
    /// required checks and reviews pass, independent of whether it is also
    /// in a merge queue. Used to render the "Merging" kanban section for a
    /// task that requested Merge When Ready but hasn't reached the queue
    /// (or the repo has no queue at all).
    #[builder(default)]
    pub auto_merge_enabled: bool,
    /// GitHub's raw `autoMergeRequest.enabledAt` (RFC 3339). `None` when
    /// `auto_merge_enabled` is `false` or GitHub omitted the field. Used
    /// only as a deterministic secondary ordering key for the Merging
    /// section (earlier-armed PRs sort above later ones).
    pub auto_merge_enabled_at: Option<String>,
    /// Trunk's own `"Trunk Merge Queue (<branch>)"` check, when it has
    /// terminally failed on this head — GitHub's record that a Trunk
    /// merge-queue episode for this PR was evicted.
    ///
    /// Deliberately carried *beside* the `ci` axis rather than folded into
    /// it: `classify_ci` excludes this leaf from the required-failure set
    /// on purpose (it is not a CI run and must not spawn a `pr_branch_ci`
    /// remediation), but the observation itself is what
    /// `trunk_queue_adopt` needs to notice a queue episode Boss did
    /// not initiate. `None` on every non-Trunk repo, and on a Trunk repo
    /// whose PR has no live or concluded-badly episode.
    pub trunk_queue_check_failure: Option<TrunkQueueCheckFailure>,
    /// GitHub's `mergeCommit.oid` — the SHA of the actual merge commit
    /// landed on the base branch, distinct from `head_ref_oid` (the PR
    /// branch's own tip) for a real merge or squash commit. `None` while
    /// the PR is open, and also on the rare merged PR for which GitHub
    /// omitted the field. Used by the merge poller's post-merge review
    /// trigger to key the immutable review target on the exact landed
    /// tree rather than the pre-merge head.
    pub merge_commit_oid: Option<String>,
    /// GitHub's `updatedAt` for the PR (RFC 3339): the last time anything
    /// on the PR changed (push, comment, review, label, status). Persisted
    /// to `tasks.pr_updated_at` so the Review column can order its Pending
    /// section by recency. `None` when GitHub omitted the field.
    pub updated_at: Option<String>,
}

/// Lifecycle states the poller reacts to. The split between
/// `Open(Clean)` and `Open(Conflict)` is the load-bearing addition
/// for the merge-conflict design — they share `state='OPEN'` on the
/// GitHub side and are disambiguated by `mergeable` /
/// `mergeStateStatus`. The `Open` variant carries the joint
/// (mergeability, CI) status (design §Q1's `OpenPrStatus`). `Merged`
/// is what the original poller detected. `ClosedUnmerged` is
/// captured for completeness (per the closed-unmerged design); the
/// current sweep treats it as a no-op (a PR force-deleted out of
/// review is the user's problem, not the poller's), preserving prior
/// behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrStatus),
    Merged,
    ClosedUnmerged,
}

/// Joint mergeability + CI status for an open PR. The two signals
/// share a probe round-trip and a single sweep dispatch (design §Q1's
/// "Composing the CI signal into the same probe"). The merge-poller
/// match expression routes on the pair: a conflict pre-empts CI
/// detection (the conflict-resolver owns the slot first); both clean
/// drives the retire path; CI-only failures fan out to `ci_watch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPrStatus {
    pub mergeability: OpenPrMergeability,
    pub ci: OpenPrCiStatus,
}

impl OpenPrStatus {
    /// Mergeable, CI clean — the steady-state "in_review and happy"
    /// shape. Used both by the production parser and by tests that
    /// only care about one of the two signals.
    pub fn clean() -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the conflict signal
    /// (the corresponding `ci` slot is `Clean`).
    pub fn conflict_only() -> Self {
        Self {
            mergeability: OpenPrMergeability::Conflict,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the CI-failing
    /// signal (the corresponding `mergeability` slot is `Clean`).
    pub fn ci_failing(failures: Vec<RequiredCheckFailure>) -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing { failures },
        }
    }

    /// GitHub returned `mergeable=UNKNOWN` — mergeability indeterminate,
    /// CI clean. Used by tests that exercise the UNKNOWN skip path.
    pub fn unknown_mergeability() -> Self {
        Self {
            mergeability: OpenPrMergeability::Unknown,
            ci: OpenPrCiStatus::Clean,
        }
    }
}

/// Whether an open PR's head ref currently merges cleanly into its
/// base. Derived from GitHub's `mergeable` + `mergeStateStatus`
/// pair.
///
/// `UNKNOWN` (GitHub is mid-recompute) maps to the `Unknown` variant —
/// neither conflict-detection nor conflict-retire fires while mergeability
/// is indeterminate; the next sweep picks up the definitive `MERGEABLE`
/// or `CONFLICTING` result. Using `Unknown` instead of mapping to `Clean`
/// prevents phantom `blocked→in_review` transitions when GitHub returns
/// `UNKNOWN` transiently right after a base-branch move (root cause of
/// the conflict_watch blocked↔in_review flap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrMergeability {
    Clean,
    Conflict,
    /// GitHub's `mergeable` field is `UNKNOWN` — the mergeability check
    /// is still being computed asynchronously. Skip all conflict-watch
    /// transitions this sweep; re-poll next sweep for a definitive answer.
    Unknown,
}

/// CI status of an open PR's required checks at probe time. Derived
/// from `statusCheckRollup` after collapsing by name (latest leaf per
/// check name; design §Q1) and applying the closed failure-conclusion
/// set against required checks only.
///
/// `Clean` means every required check is either `COMPLETED+SUCCESS`,
/// `NEUTRAL`, or `SKIPPED`. `Failing` carries the set of failing required
/// checks for the worker prompt and is reported only once the rollup is
/// *terminal* — every required check has reached a terminal conclusion and
/// at least one failed. `InFlight` is the wait state — at least one
/// required check has not reached a terminal conclusion yet; we do not
/// trigger a CI-fix attempt on it. `InFlight` dominates `Failing`: a
/// failing leaf alongside still-running checks reads as `InFlight`, so a
/// transient/early failure mid-run never spawns a moot remediation or
/// lights the "ci failing" badge (the `auto-retire` path is symmetric — it
/// waits for terminal success across the board, design §Q5 / "Auto-retire"
/// requires *all* checks at SUCCESS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrCiStatus {
    Clean,
    Failing { failures: Vec<RequiredCheckFailure> },
    InFlight,
}

/// Review-gating state of a PR at probe time. Derived from
/// GitHub's `reviewDecision` field and the `reviews` array.
///
/// `Required` maps to `REVIEW_REQUIRED` — at least one approving
/// review is still needed. `Approved` means all required reviewers
/// have approved; the `reviewers` list carries their login names
/// for the tooltip. `ChangesRequested` means at least one reviewer
/// blocked the PR; `reviewers` lists who. `Unknown` is the
/// fallback when GitHub omitted the field or returned an
/// unrecognised value (e.g., no branch protection configured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrReviewState {
    Required,
    Approved { reviewers: Vec<String> },
    ChangesRequested { reviewers: Vec<String> },
    Unknown,
}

impl PrReviewState {
    /// Stable DB string for the `review_required_state` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PrReviewState::Required => "required",
            PrReviewState::Approved { .. } => "approved",
            PrReviewState::ChangesRequested { .. } => "changes_requested",
            PrReviewState::Unknown => "unknown",
        }
    }

    /// Reviewer login names for the tooltip, if available.
    pub fn reviewers(&self) -> &[String] {
        match self {
            PrReviewState::Approved { reviewers } | PrReviewState::ChangesRequested { reviewers } => reviewers,
            _ => &[],
        }
    }
}

/// A terminally-failed `"Trunk Merge Queue (<branch>)"` leaf observed on a
/// PR's head — i.e. GitHub's own record that a Trunk merge-queue episode
/// for this PR ended in an eviction.
///
/// Deliberately *not* a [`RequiredCheckFailure`]: this must never be mixed
/// into the `pr_branch_ci` failure set (see `classify_ci`'s exclusion),
/// and the distinct type is what makes that impossible by construction
/// rather than by convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkQueueCheckFailure {
    /// Verbatim leaf name, e.g. `"Trunk Merge Queue (main)"`.
    pub name: String,
    /// Uppercased terminal conclusion, e.g. `"FAILURE"`.
    pub conclusion: String,
    /// Trunk's own episode page, e.g.
    /// `https://app.trunk.io/<org>/merge-queue/<queue-uuid>/<pr-number>`.
    /// Empty when GitHub omitted it. Diagnostic only — the queue uuid in
    /// that path is per-queue, not per-episode, so it is not usable as an
    /// idempotency key.
    pub details_url: String,
    /// The check run's `completedAt`, verbatim RFC 3339 (e.g.
    /// `"2026-07-28T09:14:02Z"`). `None` for a `StatusContext` leaf, which
    /// has no such field, and on any provider that omits it.
    ///
    /// This is the *episode* discriminator. A PR head sha is not one: a PR
    /// can enter the queue more than once on an unchanged head (a human
    /// cancels and re-checks the box; or Trunk retires the entry for a base
    /// mismatch and Boss's own attention item tells them to requeue), and
    /// Trunk concludes a fresh check run for each attempt. Pairing this with
    /// the head sha is what lets a genuinely new episode be adopted while a
    /// re-observation of the same one is not — see
    /// `WorkDb::trunk_merge_intent_adopted_at_episode`. It is
    /// also the only datum that bounds how *old* an eviction is, which
    /// `trunk_queue_adopt` uses to refuse to resurrect an episode a
    /// human walked away from days ago.
    pub completed_at: Option<String>,
}

/// Probe the lifecycle state of a single PR. Implemented for
/// production by shelling out to `gh`; test doubles can stub it
/// directly.
#[async_trait]
pub trait MergeProbe: Send + Sync {
    /// Returns the latest lifecycle state for `pr_url`. Errors are
    /// reserved for tool / network failures; "PR doesn't exist" is
    /// reported as `Ok` with `state=ClosedUnmerged` so the poller's
    /// in-review-stays-in-review behaviour is preserved (a deleted
    /// PR's row stays where it was).
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe>;

    /// Probe every PR in `pr_urls` in as few round trips as possible,
    /// keyed by URL (duplicates in the input collapse to one entry).
    /// Errors are carried as `String` rather than `anyhow::Error` so the
    /// result map stays cheaply clonable across the sweep's fan-out call
    /// sites.
    ///
    /// The default implementation probes each PR individually via
    /// [`Self::probe`] — used by [`NoopMergeProbe`] and every test double,
    /// none of which have a batched transport to gain from. `CommandMergeProbe`
    /// overrides this to issue one aliased GraphQL query for the whole batch
    /// instead of one `gh pr view` per PR (the PR-reconciler batching
    /// follow-up from the GitHub event-detection investigation, §9.1).
    async fn probe_batch(&self, pr_urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let mut out = HashMap::new();
        for url in pr_urls {
            if out.contains_key(url) {
                continue;
            }
            let result = self.probe(url).await.map_err(|err| err.to_string());
            out.insert(url.clone(), result);
        }
        out
    }
}

/// `MergeProbe` that always returns an error — used as the default in
/// contexts that do not need real GitHub probing (e.g. unit tests that
/// never reach the CI-fetch path).
#[derive(Debug, Default)]
pub struct NoopMergeProbe;

#[async_trait]
impl MergeProbe for NoopMergeProbe {
    async fn probe(&self, _pr_url: &str) -> Result<PrLifecycleProbe> {
        anyhow::bail!("NoopMergeProbe: no real probe configured")
    }
}
