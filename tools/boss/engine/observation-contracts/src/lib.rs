//! Shared observation contracts for completion and merge polling.

pub mod conflict_stop_gate;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use boss_github::RequiredCheckFailure;
use boss_protocol::{Attention, AttentionGroup, ExecutionStatus, WorkExecution};
use boss_work_db::{LatePrCandidate, PendingMergeCheck, ReviewBatchDispatch, WorkDb};

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
    /// per-PR opt-out label (`boss/no-auto-rebase`) without a second
    /// `gh` round trip.
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

/// One `RemovedFromMergeQueueEvent` entry from the PR's timeline.
#[derive(Debug, Clone)]
pub struct MergeQueueDequeueEvent {
    pub reason: String,
    /// Time GitHub recorded the dequeue event. An intent created after this
    /// event must not be retired by historical timeline evidence.
    pub created_at: Option<String>,
    /// PR head that was in effect when this timeline event was emitted.
    /// Derived from the preceding `PullRequestCommit` /
    /// `HeadRefForcePushedEvent` in the same ordered timeline connection.
    /// `None` means the bounded timeline window did not carry enough
    /// evidence to attribute the queue failure to a PR head, so callers
    /// must not mint a CI-failure revision from it.
    pub pr_head_oid: Option<String>,
    /// `beforeCommit.oid` — the synthetic merge SHA that failed CI.
    /// `None` when GitHub omitted it (edge case for non-CI reasons).
    pub before_commit_oid: Option<String>,
    /// First parent of the failed queue construction, possibly including queued siblings.
    pub queue_base_oid: Option<String>,
}

/// Reads dequeue evidence from a PR's merge-queue timeline.
#[async_trait]
pub trait MergeQueueTimeline: Send + Sync {
    async fn fetch_dequeues(&self, pr_url: &str) -> anyhow::Result<Vec<MergeQueueDequeueEvent>>;
}

/// Default for callers without a timeline transport.
#[derive(Debug, Default)]
pub struct UnsupportedMergeQueueTimeline;

#[async_trait]
impl MergeQueueTimeline for UnsupportedMergeQueueTimeline {
    async fn fetch_dequeues(&self, _pr_url: &str) -> anyhow::Result<Vec<MergeQueueDequeueEvent>> {
        anyhow::bail!("queue timeline lookup unsupported by this verifier")
    }
}

/// What happened during a stop event handler invocation. The runtime
/// only logs this; tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The engine could not retrieve a required structured-output artifact
    /// from the remote host after the worker reached its terminal boundary.
    RemoteCollectionFailed { detail: String },
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
    /// The Stop hook fired for an execution in
    /// `running` status with an empty staged-URL cache. The fallback
    /// is reserved for `waiting_human`; the worker is still alive and
    /// any positive result would race against its own in-flight push.
    /// Quiet outcome — no probe, no publish, no transition.
    RunningNoStagedPr,
    /// The human has flipped the
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
    /// The branch-keyed detector cannot produce this
    /// classification (there is no SHA matching to fail). The variant
    /// is kept for callers that already pattern-match on it.
    StalePr { pr_url: String, reason: String },
    /// PR exists and head_match, but has zero file changes. The worker
    /// is probed to make real edits or close the PR; the work item
    /// stays in its current state.
    EmptyDiffPr { pr_url: String },
    /// The auto-nudge circuit breaker tripped: the worker was nudged
    /// `max_unproductive_nudges` consecutive times with no new commit,
    /// PR, or state transition. The execution fails and releases its resources;
    /// the task records the failure reason and is excluded from automatic retry.
    /// The variant name is retained for existing consumers; no park is created.
    NudgeBreakerParked { reason: String },
    /// The nudge breaker rejected an otherwise valid probe because the same
    /// fingerprint was queued too recently. No probe was delivered; a
    /// recurring background-child recheck retains its intent and retries.
    NudgeDebounced,
    /// The worker emitted an `[effort-escalation]` or `[blocked]` marker on
    /// a prior Stop and the filed attention item is still unresolved: the
    /// "produce a PR" auto-nudge is suppressed for this execution (never
    /// queued via `nudge_or_park`) instead of dogging a worker that already
    /// declared itself stuck awaiting coordinator direction. `reason` names the
    /// pending signal kind(s). The execution stays `waiting_human` with its
    /// lease and pane attached; no probe is sent. There is no time bound —
    /// this is the AGENTS.md mandated-stop channel (`boss propose blocked`
    /// without a terminal `done`). The coordinator-visible attention item
    /// is the release valve (`bossctl probe`), not the nudge breaker.
    /// Resolving the attention item (coordinator ack) resumes normal
    /// nudging on the next Stop.
    EscalationPending { reason: String },
    /// The worker's Stop-boundary text matched the `build_wait`
    /// heuristic — it is narrating that it is legitimately waiting on a
    /// backgrounded build/test gate. The
    /// "produce a PR" auto-nudge is suppressed — and, critically, the
    /// circuit breaker in `nudge_breaker` is never even consulted,
    /// so this Stop does not burn any of its cap — for as long as
    /// `BuildWaitTracker` considers the wait
    /// still within its horizon. `waited_secs` is how long this execution
    /// has been continuously reporting the wait. The execution stays
    /// `waiting_human`; no probe is sent, nothing is parked. Once the
    /// horizon elapses, the normal nudge/park flow resumes automatically
    /// (no coordinator action required — unlike [`StopOutcome::EscalationPending`]).
    BuildWaitPending { waited_secs: i64 },
    /// The worker's Stop-boundary process tree still has live delegated
    /// descendants — e.g. a backgrounded subagent spawned via the harness
    /// Agent tool that has not yet reported back. Same suppression shape as [`Self::BuildWaitPending`]
    /// (checked before `nudge_breaker` is even consulted, so this
    /// Stop does not burn any of its cap) but a distinct signal:
    /// process-tree-based rather than text-narration-based.
    /// `descendant_count` is how many delegated descendants
    /// `background_children` found; `waited_secs` is how long this
    /// execution has been continuously reporting delegated descendants. The
    /// execution stays `waiting_human`; no probe is sent, nothing is parked. Once
    /// the background-children horizon elapses, the normal
    /// nudge/park flow resumes automatically.
    ///
    /// The production probe reports indeterminate because process groups do
    /// not distinguish delegated work from ordinary subprocesses. Only an
    /// injected probe can currently produce this outcome.
    BackgroundChildrenPending { descendant_count: usize, waited_secs: i64 },
    /// An explicit hold is active via `bossctl agents hold`. The idle-park
    /// flow sends no nudge and does not consult the breaker until the hold
    /// is released or the run ends. `reason` is the explanation supplied
    /// with the hold, if any.
    Held { reason: Option<String> },
    /// The worker declared a time-bounded wait via `boss propose wait`
    /// that has not yet expired. The produce-a-PR auto-nudge is suppressed
    /// and the circuit breaker is not consulted. `reason` is the worker's
    /// free-text explanation; `expires_at_epoch` is when nudging resumes.
    /// The 2-hour stale-worker reap and other safety checks keep firing.
    WorkerWaitPending { reason: String, expires_at_epoch: i64 },
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
    /// the "nothing left to do" zombie loop.
    ///
    /// For a `revision_implementation` the health-alone arm of that gate
    /// additionally requires that the SHA-delta gate has NOT proven the
    /// run contributed nothing — see `health_alone_satisfies_deliverable`.
    /// A revision is dispatched into an already-healthy PR, so without
    /// that requirement this outcome fires on the first Stop of every
    /// revision whose parent PR happens to be open and CI-clean, which is
    /// the normal case.
    DeliverableSatisfied { pr_url: String },
    /// The bound PR looks satisfied, but this run has not declared itself
    /// finished (`boss propose done`) and the run-done backstop is holding
    /// it rather than terminalizing on the PR's health — see
    /// `run_done_backstop`. Quiet outcome: no probe, no breaker
    /// consumption, no transition, no reap. The execution stays live.
    ///
    /// `reason` names *which* hold applied — the worker has live background
    /// children, or the silence horizon has not yet elapsed — because those
    /// are different findings when inspecting why a run is
    /// still open: the first says the worker is demonstrably working, the
    /// second says nothing is known either way yet.
    ///
    /// This is the outcome that replaces
    /// [`Self::DeliverableSatisfied`] for an undeclared run. It is
    /// deliberately not a terminal state: a run that never declares reaches
    /// [`Self::RunUndeclaredParked`] via the ask, or is reclaimed by
    /// `stale_worker_sweep`, but is never quietly recorded as a
    /// success.
    AwaitingRunDoneDeclaration { reason: String },
    /// The run-done backstop asked an undeclared run whether it was
    /// finished, the ask went unanswered until the auto-nudge breaker
    /// tripped, and the run was parked for a human. `run_undeclared_at` is
    /// stamped on the execution row and a
    /// `RUN_UNDECLARED_ATTENTION_KIND` attention filed, so the ending is
    /// distinguishable — in stored state and on the row — from a run that
    /// declared itself done. `waited_secs` is how long the run was silent
    /// before the backstop first asked.
    ///
    /// The execution stays `waiting_human`; it is NOT `completed`. An
    /// undeclared run that produced nothing must never be recorded as a
    /// successful completion, which is the failure this whole path exists
    /// to end.
    RunUndeclaredParked { reason: String, waited_secs: i64 },
    /// A CI-remediation worker classified the failure as flaky/infra and
    /// re-triggered the failing job (`boss engine ci mark-retriggered`),
    /// which stamped the `ci_flaky_retriggered` signal on the parent.
    /// There is genuinely nothing to push, so the completion path parks
    /// the worker — awaiting the CI retry or a human decision — instead of
    /// probing it for a diff. No nudge is sent; the execution is left
    /// `waiting_human`. This is the fix for the stuck-loop bug where the
    /// engine re-derived the same flaky verdict on every probe.
    FlakyRetriggered { pr_url: String },
    /// An `automation_triage` execution finished and its
    /// final message was run through the marker-protocol outcome detector.
    /// `outcome` is the `automation_runs.outcome` discriminator recorded
    /// (`produced_task` / `skipped` / `failed_will_retry`). The execution is
    /// finalised (`completed`) and its pane/workspace released regardless of
    /// which marker (if any) the agent emitted.
    AutomationTriage { outcome: String },
    /// An `answer_agent` execution finished. `replied` is `true` when
    /// the agent posted its reply mid-session via `CommentsPostAnswer`
    /// before Stop fired, `false` when the session ended without one (the
    /// finalizer marks the run `failed` and posts an apology thread entry so
    /// the comment doesn't sit in `answering` forever). The execution is
    /// finalised (`completed`) and its pane/workspace released either way.
    AnswerAgent { replied: bool },
    /// A `pr_review_guide` execution finished. `published` is `true` when its
    /// raw output passed validation and was durably published as the
    /// series' new readable version (subject to the publication fence — a
    /// stale comparison still finalizes the execution but does not advance
    /// the pointer), `false` when validation failed or the driver produced
    /// no usable text (the attempt is recorded `failed`/`superseded` either
    /// way). The execution is finalised (`completed`) and its pane/
    /// workspace released regardless.
    ReviewGuide { published: bool },
    /// A primary-implementation worker's PR was detected and
    /// an independent reviewer pass has been enqueued. The producing task
    /// remains in `active` (Doing column) until the reviewer resolves.
    ReviewerEnqueued { pr_url: String },
    /// A `pr_review` reviewer execution finished and the
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
    /// A `pr_review` reviewer execution found qualifying findings
    /// (at least one `critical`/`high` severity or `regression` category) and
    /// created a revision task on the producing task. The producing task is
    /// advanced to `in_review`; the revision is dispatched on the general
    /// worker pool to apply the feedback. Nothing is posted to GitHub.
    ReviewPassRevisionCreated { pr_url: String, revision_task_id: String },
    /// A primary-implementation worker (`chore_implementation` /
    /// `task_implementation`) verified its assigned work was already done —
    /// the change is already on `main`, the diff is empty, and there is
    /// genuinely nothing to commit/push/open a PR for — and emitted the
    /// `boss propose done --outcome no-changes-needed` declaration. The task is closed as `done` WITHOUT a PR and the execution is
    /// finalised. No nudge is sent. This is the fix for the produce-a-PR nudge
    /// loop on a worker that correctly found nothing to do. `work_item_id` is
    /// the closed task/chore.
    NoChangesNeeded { work_item_id: String },
    /// The run's driver reported its own terminal turn boundary as an
    /// unrecoverable error — Codex's rollout `task_complete.error`, or a
    /// stdout `turn.failed` / fatal top-level `error` envelope
    /// (`TurnEnd::reason` = `boss_protocol::StopReason::Other`).
    /// The process that produced this Stop has already exited on its own;
    /// there is nothing left to nudge or wait on. The execution is marked
    /// `failed` (not left `running`/`waiting_human`), its cube lease and
    /// pane are released, and an attention item naming the provider's error
    /// text is filed. No nudge probe is queued — this bypasses every
    /// kind-specific finalizer (including the `pr_review` ReviewResult
    /// nudge loop) so a driver-reported fatal error can never wedge an
    /// execution on a dead process the way an unparsed provider error used
    /// to.
    DriverTerminalError { detail: String },
    /// Unexpected DB failure while recording completion.
    DbError,
    /// The pre-completion probe pass in
    /// `dispatch_worker_event_fanout` delivered
    /// a probe into the pane for this boundary, so the worker is entitled to
    /// a turn to act on it before the generic PR-detection/nudge/park
    /// decision runs — that decision is deferred to the `Stop` the granted
    /// turn produces. Everything upstream of that decision inside
    /// `on_stop_inner` — the stale-Stop supersession guard, the `stop_seen`
    /// stamp, `StopReason::Other` driver-fatal-error handling, and every
    /// kind-specific finalizer (`automation_triage`, `answer_agent`,
    /// `pr_review`, the CI-remediation flaky-retrigger park) — still ran
    /// normally for this boundary; only the terminal decision was withheld.
    DeferredForProbeTurn,
    /// A `run_done` declaration's acceptance IS the completion signal — see
    /// `completion::run_done_declaration` and `app::proposals::handle_submit_proposal`.
    /// The execution is finalised synchronously at submit time, not at a
    /// later Stop boundary that may never arrive. Fired for `blocked` (the
    /// run is over without delivering) or `delivered` with no PR the engine
    /// could resolve. Both fail the execution and, for a task still in
    /// `active`/`todo`, record `blocked:worker_failed` before releasing the
    /// lease and pane; neither creates a deliberate park. A parent already
    /// in `in_review` or a domain-owned blocked reason is left in that
    /// recovery state — see `WorkDb::record_worker_failure`.
    /// `delivered` with a resolvable PR instead reaches [`Self::PrDetected`],
    /// and `no_changes_needed` reaches [`Self::NoChangesNeeded`] — both
    /// existing variants, reused because their semantics already fit.
    RunDoneDeclaredWithoutDelivery { detail: String },
    /// A primary execution's completion resolved to a PR that is already
    /// bound to a different work item (`pr_binding_guard`). Nothing was
    /// bound: the execution failed visibly, the task row records `detail`
    /// as its blocked reason, and an attention item was filed. A PR that
    /// belongs to another row is never this execution's deliverable.
    ForeignPrRefused { pr_url: String, detail: String },
}

/// Completion-side requests the merge poller makes after a probe.
/// Production implements this on `WorkerCompletionHandler`; the poller
/// holds `&dyn CompletionRequests` so it does not import the handler.
#[async_trait]
pub trait CompletionRequests: Send + Sync {
    fn review_guide_source_capture_enabled(&self) -> bool;
    fn pending_background_nudge_execution_ids(&self) -> Vec<String>;
    async fn recheck_background_nudge(&self, execution_id: &str) -> Option<StopOutcome>;
    fn review_batch_fanout_enabled(&self) -> bool;
    fn review_pool_size(&self) -> usize;
    async fn force_release(&self, execution_id: &str);
    async fn record_parent_pr_merge_cancellation(
        &self,
        execution: &WorkExecution,
        prior_status: ExecutionStatus,
        chain_root_id: &str,
    );
    async fn force_stop_execution(&self, execution_id: &str);
    async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome;
    async fn recheck_for_pr_late(&self, candidate: &LatePrCandidate) -> StopOutcome;
    fn reconcile_review_guide_source_from_probe(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        observation_sequence: i64,
    );
    fn speculative_conflict_prediction_enabled(&self) -> bool;
    fn stacked_pr_auto_structuring_enabled(&self) -> bool;
    fn mechanical_rebase_enabled(&self) -> bool;
    fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer;
}

/// Production or test strategy for creating a pre-merge review batch from
/// a producing execution's PR. Tests inject a stub that skips `gh`.
#[async_trait]
pub trait ReviewBatchEnqueuer: Send + Sync {
    async fn enqueue(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch>;
}

/// Writes PR poll state after an on-transition CI pre-fetch.
///
/// Completion holds this so it does not import the merge poller's
/// `update_pr_poll_state`. Production wires the poller's adapter;
/// tests keep [`NoopPrPollStateWriter`].
#[async_trait]
pub trait PrPollStateWriter: Send + Sync {
    async fn update_pr_poll_state(
        &self,
        work_db: &WorkDb,
        candidate: &PendingMergeCheck,
        probe: &PrLifecycleProbe,
        caller: &str,
    );
}

/// Default writer: drops poll-state updates. Tests that never reach the
/// CI-fetch path keep this; production replaces it at composition time.
#[derive(Debug, Default)]
pub struct NoopPrPollStateWriter;

#[async_trait]
impl PrPollStateWriter for NoopPrPollStateWriter {
    async fn update_pr_poll_state(
        &self,
        _work_db: &WorkDb,
        _candidate: &PendingMergeCheck,
        _probe: &PrLifecycleProbe,
        _caller: &str,
    ) {
    }
}

/// File the review-admission-deferred attention item used when the
/// review pool has no free reservation for another pre-merge batch.
pub fn file_admission_deferred_attention(work_db: &WorkDb, work_item_id: &str, pr_url: &str) {
    let title = "Automated reviewer: waiting for a review-pool slot";
    let body = format!(
        "Pre-merge review for {pr_url} was deferred because it cannot occupy a review-pool \
         reservation right now: the pool is full, or dispatch focus / an all-spawns pause is \
         holding this PR. The review starts once a reservation is free and this PR is \
         allowed to dispatch. Dismiss this item once that pass starts."
    );
    if let Err(err) = work_db.upsert_external_tracker_attention(
        work_item_id,
        boss_work_db::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
        title,
        &body,
    ) {
        tracing::warn!(
            work_item_id,
            ?err,
            "failed to file review-admission-deferred attention item"
        );
    }
}

#[async_trait]
impl<T: CompletionRequests + ?Sized> CompletionRequests for std::sync::Arc<T> {
    fn review_guide_source_capture_enabled(&self) -> bool {
        T::review_guide_source_capture_enabled(self.as_ref())
    }

    fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
        T::pending_background_nudge_execution_ids(self.as_ref())
    }

    async fn recheck_background_nudge(&self, execution_id: &str) -> Option<StopOutcome> {
        T::recheck_background_nudge(self.as_ref(), execution_id).await
    }

    fn review_batch_fanout_enabled(&self) -> bool {
        T::review_batch_fanout_enabled(self.as_ref())
    }

    fn review_pool_size(&self) -> usize {
        T::review_pool_size(self.as_ref())
    }

    async fn force_release(&self, execution_id: &str) {
        T::force_release(self.as_ref(), execution_id).await;
    }

    async fn record_parent_pr_merge_cancellation(
        &self,
        execution: &WorkExecution,
        prior_status: ExecutionStatus,
        chain_root_id: &str,
    ) {
        T::record_parent_pr_merge_cancellation(self.as_ref(), execution, prior_status, chain_root_id).await;
    }

    async fn force_stop_execution(&self, execution_id: &str) {
        T::force_stop_execution(self.as_ref(), execution_id).await;
    }

    async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome {
        T::recheck_for_pr(self.as_ref(), execution_id).await
    }

    async fn recheck_for_pr_late(&self, candidate: &LatePrCandidate) -> StopOutcome {
        T::recheck_for_pr_late(self.as_ref(), candidate).await
    }

    fn reconcile_review_guide_source_from_probe(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        observation_sequence: i64,
    ) {
        T::reconcile_review_guide_source_from_probe(
            self.as_ref(),
            root_task_id,
            pr_url,
            base_sha,
            head_sha,
            observation_sequence,
        );
    }

    fn speculative_conflict_prediction_enabled(&self) -> bool {
        T::speculative_conflict_prediction_enabled(self.as_ref())
    }

    fn stacked_pr_auto_structuring_enabled(&self) -> bool {
        T::stacked_pr_auto_structuring_enabled(self.as_ref())
    }

    fn mechanical_rebase_enabled(&self) -> bool {
        T::mechanical_rebase_enabled(self.as_ref())
    }

    fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer {
        T::review_batch_enqueuer(self.as_ref())
    }
}

/// Design-doc and task-doc pointer callbacks fired when a completing (or
/// already-merged) PR belongs to a design or doc-producing task.
///
/// Completion and merge polling both call these; the implementation owns
/// the PR scan and the pointer writes, so neither service imports it.
#[async_trait]
pub trait DesignDetection: Send + Sync {
    async fn on_task_doc_pr_detected(&self, work_db: &WorkDb, task_id: &str, product_id: &str, pr_url: &str);

    async fn on_task_doc_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    );

    async fn on_design_pr_detected(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
    );

    async fn on_design_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    );
}

/// An attention group and the members it just created, when a reconcile
/// pass produced any.
pub type AttentionsCreated = Option<(AttentionGroup, Vec<Attention>)>;

/// Design-question and follow-up attention extraction that runs when a
/// worker's PR completes. Each `reconcile_*` is the primary channel and each
/// `extract_*_backstop` the supervisor-extraction fallback the caller gates
/// behind a feature flag.
#[async_trait]
pub trait AttentionsDetection: Send + Sync {
    async fn reconcile_design_doc_questions(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        project_id: &str,
        pr_url: &str,
        merged: bool,
    ) -> AttentionsCreated;

    async fn extract_doc_questions_backstop(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        project_id: &str,
        pr_url: &str,
        merged: bool,
    ) -> AttentionsCreated;

    async fn reconcile_task_followups(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        execution_id: &str,
        structured_output_dir: Option<&Path>,
        transcript_path: Option<&str>,
    ) -> AttentionsCreated;

    async fn extract_followups_backstop(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        execution_id: &str,
        transcript_path: Option<&str>,
    ) -> AttentionsCreated;
}

/// Detection that does nothing. For tests that never reach the detectors.
#[derive(Debug, Default)]
pub struct NoopDocDetection;

#[async_trait]
impl DesignDetection for NoopDocDetection {
    async fn on_task_doc_pr_detected(&self, _: &WorkDb, _: &str, _: &str, _: &str) {}

    async fn on_task_doc_pr_merged(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: Option<&str>) {}

    async fn on_design_pr_detected(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: &str) {}

    async fn on_design_pr_merged(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: &str, _: Option<&str>) {}
}

#[async_trait]
impl AttentionsDetection for NoopDocDetection {
    async fn reconcile_design_doc_questions(
        &self,
        _: &WorkDb,
        _: &str,
        _: &str,
        _: &str,
        _: bool,
    ) -> AttentionsCreated {
        None
    }

    async fn extract_doc_questions_backstop(
        &self,
        _: &WorkDb,
        _: &str,
        _: &str,
        _: &str,
        _: bool,
    ) -> AttentionsCreated {
        None
    }

    async fn reconcile_task_followups(
        &self,
        _: &WorkDb,
        _: &str,
        _: &str,
        _: Option<&Path>,
        _: Option<&str>,
    ) -> AttentionsCreated {
        None
    }

    async fn extract_followups_backstop(&self, _: &WorkDb, _: &str, _: &str, _: Option<&str>) -> AttentionsCreated {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;

    use super::*;

    fn clean_probe(url: &str) -> PrLifecycleProbe {
        PrLifecycleProbe::builder()
            .url(url)
            .state(PrLifecycleState::Open(OpenPrStatus::clean()))
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build()
    }

    struct RecordingProbe {
        results: HashMap<String, Result<PrLifecycleProbe, String>>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MergeProbe for RecordingProbe {
        async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
            self.calls.lock().expect("call log").push(pr_url.to_owned());
            match self.results.get(pr_url) {
                Some(Ok(probe)) => Ok(probe.clone()),
                Some(Err(err)) => anyhow::bail!("{err}"),
                None => anyhow::bail!("no probe fixture for {pr_url}"),
            }
        }
    }

    #[tokio::test]
    async fn probe_batch_dedupes_and_preserves_first_seen_order() {
        let probe = RecordingProbe {
            results: HashMap::from([
                ("https://example/a".to_owned(), Ok(clean_probe("https://example/a"))),
                ("https://example/b".to_owned(), Ok(clean_probe("https://example/b"))),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let out = probe
            .probe_batch(&[
                "https://example/a".to_owned(),
                "https://example/b".to_owned(),
                "https://example/a".to_owned(),
            ])
            .await;
        assert_eq!(
            probe.calls.lock().expect("call log").as_slice(),
            ["https://example/a", "https://example/b"]
        );
        assert!(out["https://example/a"].is_ok());
        assert!(out["https://example/b"].is_ok());
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn probe_batch_isolates_errors() {
        let probe = RecordingProbe {
            results: HashMap::from([
                ("https://example/ok".to_owned(), Ok(clean_probe("https://example/ok"))),
                ("https://example/bad".to_owned(), Err("network down".to_owned())),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let out = probe
            .probe_batch(&["https://example/ok".to_owned(), "https://example/bad".to_owned()])
            .await;
        assert!(out["https://example/ok"].is_ok());
        let err = out["https://example/bad"].as_ref().expect_err("bad url errors");
        assert!(err.contains("network down"), "{err}");
    }

    #[tokio::test]
    async fn noop_merge_probe_errors() {
        let err = NoopMergeProbe
            .probe("https://example/pr")
            .await
            .expect_err("noop probe must fail");
        assert!(
            err.to_string().contains("NoopMergeProbe: no real probe configured"),
            "{err}"
        );
    }

    #[test]
    fn open_pr_status_constructors() {
        assert_eq!(
            OpenPrStatus::clean(),
            OpenPrStatus {
                mergeability: OpenPrMergeability::Clean,
                ci: OpenPrCiStatus::Clean,
            }
        );
        assert_eq!(OpenPrStatus::conflict_only().mergeability, OpenPrMergeability::Conflict);
        assert_eq!(
            OpenPrStatus::unknown_mergeability().mergeability,
            OpenPrMergeability::Unknown
        );
        let failing = OpenPrStatus::ci_failing(Vec::new());
        assert!(matches!(failing.ci, OpenPrCiStatus::Failing { .. }));
        assert_eq!(failing.mergeability, OpenPrMergeability::Clean);
    }

    #[test]
    fn pr_review_state_db_strings_and_reviewers() {
        assert_eq!(PrReviewState::Required.as_db_str(), "required");
        assert_eq!(
            PrReviewState::Approved {
                reviewers: vec!["ada".to_owned()]
            }
            .as_db_str(),
            "approved"
        );
        assert_eq!(
            PrReviewState::ChangesRequested {
                reviewers: vec!["grace".to_owned()]
            }
            .as_db_str(),
            "changes_requested"
        );
        assert_eq!(PrReviewState::Unknown.as_db_str(), "unknown");
        assert_eq!(
            PrReviewState::Approved {
                reviewers: vec!["ada".to_owned()]
            }
            .reviewers(),
            ["ada"]
        );
        assert!(PrReviewState::Required.reviewers().is_empty());
    }
}
