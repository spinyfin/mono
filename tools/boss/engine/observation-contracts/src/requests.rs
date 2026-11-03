use async_trait::async_trait;
use boss_pr_review_sources::PinnedComparison;
use boss_protocol::{ExecutionStatus, WorkExecution};
use boss_work_db::{LatePrCandidate, PendingMergeCheck, ReviewBatchDispatch, WorkDb};

use crate::probe::PrLifecycleProbe;

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
    /// **Currently unreachable in production.** The process-group
    /// discriminator this variant's `descendant_count` used to come from
    /// was disproved (driver helpers and ordinary tool subprocesses may
    /// legitimately run in their own process group, indistinguishable from
    /// a delegated child) and removed — see
    /// `background_children`'s module doc. The production probe
    /// now reports indeterminate on every Stop, so this variant only fires
    /// today from a test-supplied probe, pending a driver/harness-owned
    /// delegated-work signal to replace the process-group walk.
    BackgroundChildrenPending { descendant_count: usize, waited_secs: i64 },
    /// An operator placed an explicit hold on this execution via
    /// `bossctl agents hold` (see `hold_registry`). The
    /// idle-park flow is skipped entirely — no nudge, no breaker
    /// consultation, no park — until the operator releases the hold or
    /// the run ends. `reason` is the operator-supplied explanation, if
    /// any.
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
    /// are different findings to an operator reading back why a run is
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
        observed: PinnedComparison,
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
        "Pre-merge review for {pr_url} was deferred because the review pool has no free \
         reservation for another pre-merge batch. The review starts once a batch completes \
         and the deferred-admission sweep retries. Dismiss this item once that \
         pass starts."
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
