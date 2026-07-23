//! `FrontendEvent` — split out of `wire.rs` to keep it under the repo's
//! file-size check (CHECKS.yaml `file/size`, 3000-line limit). Pure
//! structural move — no behavioural change. `use super::*` inherits the
//! parent module's full import set rather than re-deriving the subset
//! this enum actually needs.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendEvent {
    Hello {
        session_id: String,
    },
    Subscribed {
        topics: Vec<String>,
        current_revision: u64,
    },
    Unsubscribed {
        topics: Vec<String>,
    },
    TopicEvent {
        topic: String,
        revision: u64,
        origin_session_id: String,
        origin_request_id: Option<String>,
        event: TopicEventPayload,
    },
    ProductsList {
        products: Vec<Product>,
    },
    ProjectsList {
        product_id: String,
        projects: Vec<Project>,
    },
    TasksList {
        product_id: String,
        project_id: Option<String>,
        tasks: Vec<Task>,
    },
    ChoresList {
        product_id: String,
        chores: Vec<Task>,
    },
    RevisionsList {
        product_id: String,
        revisions: Vec<Task>,
    },
    WorkTree {
        product: Product,
        projects: Vec<Project>,
        tasks: Vec<Task>,
        chores: Vec<Task>,
        #[serde(default)]
        task_runtimes: Vec<TaskRuntime>,
        #[serde(default)]
        dependencies: Vec<WorkItemDependency>,
    },
    WorkItemResult {
        item: WorkItem,
    },
    /// Reply for [`FrontendRequest::FindWorkItemsByPr`]. `matches` carries
    /// one entry per work item whose `pr_url` resolves to `pr_number`
    /// (across all kinds and products); each entry bundles the revisions
    /// in that PR's chain. An empty `matches` means no work item is bound
    /// to the PR; more than one entry means the PR number is ambiguous
    /// across repos.
    WorkItemsByPrResult {
        pr_number: i64,
        matches: Vec<PrWorkItemMatch>,
    },
    WorkItemCreated {
        item: WorkItem,
    },
    /// Response to a batch create (`CreateManyTasks` /
    /// `CreateManyChores`). Carries every row inserted by the batch in
    /// the order the caller submitted them. Per-item subscribers can
    /// keep treating each entry as if it had arrived via a regular
    /// `WorkItemCreated` event — the engine also publishes the usual
    /// `work_invalidated` topic event covering the full id list, so
    /// kanban consumers reload once.
    WorkItemsCreated {
        items: Vec<WorkItem>,
    },
    WorkItemUpdated {
        item: WorkItem,
    },
    ProjectTasksReordered {
        project_id: String,
        task_ids: Vec<String>,
    },
    ExecutionsList {
        work_item_id: Option<String>,
        executions: Vec<WorkExecution>,
    },
    /// Reply for [`FrontendRequest::GetTaskRuntime`]. Carries the
    /// engine's view of the currently dispatched (or most-recent)
    /// execution and run for one work item. Fields are `None` when no
    /// execution exists yet.
    TaskRuntimeResult {
        runtime: TaskRuntime,
    },
    ExecutionResult {
        execution: WorkExecution,
    },
    ExecutionCreated {
        execution: WorkExecution,
    },
    ExecutionRequested {
        execution: WorkExecution,
    },
    /// Reply for [`FrontendRequest::TriggerPrReview`]. Carries the
    /// freshly-enqueued (or reused, if one was already queued) `pr_review`
    /// execution, the work item it targets, and the PR URL under review.
    PrReviewTriggered {
        execution: WorkExecution,
        work_item_id: String,
        pr_url: String,
    },
    RunsList {
        execution_id: String,
        runs: Vec<WorkRun>,
    },
    RunResult {
        run: WorkRun,
    },
    RunCreated {
        run: WorkRun,
    },
    AttentionItemsList {
        execution_id: String,
        items: Vec<WorkAttentionItem>,
    },
    AttentionItemResult {
        item: WorkAttentionItem,
    },
    AttentionItemCreated {
        item: WorkAttentionItem,
    },
    AttentionItemsForWorkItemList {
        work_item_id: String,
        items: Vec<WorkAttentionItem>,
    },
    AttentionItemUpdated {
        item: WorkAttentionItem,
    },
    /// `task` is the followup filed from the source `deferred_scope` item.
    /// Boxed: an inline `Task` here would make this variant far larger than
    /// its siblings (clippy::large_enum_variant) — serializes identically.
    AttentionItemConverted {
        item: WorkAttentionItem,
        task: Box<Task>,
    },
    DeferredScopeAttentionsList {
        product_id: String,
        items: Vec<DeferredScopeAttention>,
    },
    /// Reply for [`FrontendRequest::ListAttentionGroups`]. `members`
    /// carries the member rows for every group in `groups`, flattened
    /// across groups (the client buckets them by `Attention::group_id`)
    /// so the Notifications window can render inline answer controls
    /// without a follow-up round-trip per group.
    AttentionGroupsList {
        product_id: String,
        groups: Vec<AttentionGroup>,
        #[serde(default)]
        members: Vec<Attention>,
    },
    /// Reply for [`FrontendRequest::GetAttentionGroup`]. `members` are
    /// the group's rows in display order.
    AttentionGroupResult {
        group: AttentionGroup,
        #[serde(default)]
        members: Vec<Attention>,
    },
    /// Reply for [`FrontendRequest::CreateAttention`]. Also pushed as a
    /// live-update event on the owning product's work-tree topic so the
    /// Notifications window and doc viewer update without polling.
    AttentionCreated {
        attention: Attention,
        group: AttentionGroup,
    },
    /// Pushed whenever a group's state or a member's answer_state
    /// changes (e.g. after [`FrontendRequest::AnswerAttention`] or
    /// [`FrontendRequest::DismissAttention`]). `members` carries the
    /// group's refreshed rows so the UI reflects the new answer states.
    AttentionGroupUpdated {
        group: AttentionGroup,
        #[serde(default)]
        members: Vec<Attention>,
    },
    /// Pushed after [`FrontendRequest::ActionAttentionGroup`] succeeds.
    /// Carries the group in its terminal `actioned` state plus the
    /// produced artifact reference so the UI can render a jump link.
    /// `members` are the group's now-terminal rows.
    AttentionGroupActioned {
        group: AttentionGroup,
        #[serde(default)]
        members: Vec<Attention>,
    },
    /// Reply to [`FrontendRequest::ListAttentionMerges`]. `merges` are the
    /// `attention_merges` rows recorded against `attention_id`, chronological.
    AttentionMergesList {
        attention_id: String,
        merges: Vec<AttentionMerge>,
    },
    WorkItemDeleted {
        id: String,
    },
    /// Reply to [`FrontendRequest::RestoreWorkItem`]. Carries the
    /// now-live work item so the CLI can echo its friendly id / name.
    WorkItemRestored {
        item: WorkItem,
    },
    WorkError {
        message: String,
    },
    /// Returned instead of `WorkError` when a create is rejected because
    /// a non-deleted task/chore in the same product has an identical name
    /// and was created within the last 60 seconds. Carries enough info for
    /// the CLI to display a helpful message and for `--json` consumers to
    /// act on the existing row. Pass `force_duplicate: true` in the input
    /// to bypass the guard and insert unconditionally.
    WorkItemDuplicateBlocked {
        /// Primary id of the existing row that triggered the guard.
        existing_id: String,
        /// Friendly short id of the existing row (e.g. `439`).
        existing_short_id: i64,
        /// The name that triggered the match.
        name: String,
        /// Seconds elapsed since the existing row was created.
        age_secs: i64,
    },
    Error {
        message: String,
    },
    /// Engine confirms the calling session is now the registered app
    /// session, and any prior registration was invalidated.
    AppSessionRegistered,
    /// Engine's live pool-size configuration, pushed to the macOS app
    /// immediately after [`FrontendEvent::AppSessionRegistered`] so the
    /// app's `WorkersWorkspaceModel` knows the exact slot ranges to
    /// accept for `SpawnWorkerPane` requests. The engine is the source
    /// of truth; the app must configure itself from this on every
    /// connection rather than relying on independently-maintained
    /// hardcoded counts that drift when pool sizes change.
    EnginePoolConfig {
        worker_slots: u8,
        automation_slots: u8,
        review_slots: u8,
        /// `--model` slug for the Boss coordinator session. Sourced from the
        /// engine's `coordinator_model` setting (`BOSS_COORDINATOR_MODEL`,
        /// default `"opus"`) — independent of the worker effort→model table.
        coordinator_model: String,
    },
    /// Engine confirms the Boss session pid was registered.
    BossSessionRegistered,
    /// Engine confirms a probe was queued for the given run. The
    /// engine-minted `probe_id` lets callers correlate a queued probe
    /// with the eventual [`FrontendEvent::ProbeReplied`] push, which
    /// arrives on the [`probe_topic`] for `run_id` once the worker's
    /// follow-up Stop boundary lands. `urgent` echoes the flag from
    /// the originating [`FrontendRequest::ProbeRun`] call so the
    /// caller can confirm the delivery semantics that were accepted.
    ProbeQueued {
        run_id: String,
        probe_id: String,
        /// Echoes the `urgent` flag from the originating `ProbeRun`
        /// request. When `true`, the probe will be delivered at the
        /// next `PostToolUse` boundary rather than the next `Stop`.
        #[serde(default)]
        urgent: bool,
    },
    /// Push: the worker for `run_id` has replied to a previously
    /// dispatched probe. Emitted on the Stop boundary that follows
    /// the dispatch (so callers can correlate "probe goes in" with
    /// "next assistant turn comes out"). `text` is the assistant
    /// turn the engine extracted from the worker's transcript;
    /// `probe_id` matches the value [`FrontendEvent::ProbeQueued`]
    /// returned for the originating [`FrontendRequest::ProbeRun`]
    /// call. Pushed on the [`probe_topic`] for `run_id`.
    ProbeReplied {
        run_id: String,
        probe_id: String,
        text: String,
    },
    /// Push: an urgent probe write could not be confirmed delivered.
    /// NOT proof of loss — left `Unconfirmed` (not auto-re-queued, to
    /// avoid duplicate delivery); the observer decides on redelivery.
    ProbeDeliveryEscalated {
        run_id: String,
        probe_id: String,
        reason: String,
    },
    /// Engine acknowledges a stop request — the pane release has
    /// been kicked off and (if applicable) the cube workspace lease
    /// released. The reply does not wait for the libghostty pane to
    /// fully drain; teardown is asynchronous.
    RunStopped {
        run_id: String,
    },
    /// Engine acknowledges a focus request — the worker pane has
    /// been raised in the macOS app. Carries the resolved `slot_id`
    /// so the caller (e.g. `bossctl agents focus`) can confirm which
    /// slot was raised when the agent reference was a crew name or
    /// run id.
    WorkerPaneFocused {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges a `SendInputToWorker` request — the text
    /// has been written into the worker pane via the same surface a
    /// user-typed keystroke takes. Carries the resolved `slot_id` so
    /// the caller (e.g. `bossctl agents send`) can confirm which
    /// pane was targeted when the agent reference was a crew name
    /// or run id.
    WorkerInputSent {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges an interrupt request — an Esc keystroke
    /// has been delivered to the worker pane's pty. Carries the
    /// resolved `slot_id` so the caller can confirm which slot was
    /// interrupted when the agent reference was a crew name or run
    /// id.
    WorkerPaneInterrupted {
        run_id: String,
        slot_id: u8,
    },
    /// Engine asks the registered app session to perform a pane
    /// operation. The app must reply with a
    /// [`FrontendRequest::EngineResponse`] carrying the same
    /// `request_id`.
    EngineRequest {
        request_id: String,
        request: EngineToAppRequest,
    },
    /// Snapshot of every allocated worker slot's live state. Used as
    /// both the response to [`FrontendRequest::ListWorkerLiveStates`]
    /// and the body of pushes on the `worker.live_states` topic. The
    /// list is the entire snapshot, not a delta — receivers can
    /// blindly replace their local map.
    WorkerLiveStatesList {
        states: Vec<LiveWorkerState>,
    },
    /// Engine confirms an execution has been cancelled. The cancelled
    /// row's status is now `cancelled`; resource teardown (pane
    /// release, cube workspace release) is asynchronous.
    ExecutionCancelled {
        execution: WorkExecution,
    },
    /// Engine confirms a manual orphan reap. The execution row is now
    /// in the terminal `orphaned` status; its cube workspace lease has
    /// intentionally been left intact so a fresh execution can resume
    /// against the same branch.
    RunReaped {
        run_id: String,
        execution: WorkExecution,
    },
    /// Engine acknowledges a [`FrontendRequest::RetirePane`] — the
    /// husk pane in `slot_id` has been instructed to tear down (or the
    /// app reported the slot as already unknown/idle) and any
    /// lingering engine-side bookkeeping for the slot has been
    /// cleared.
    PaneRetired {
        slot_id: u8,
    },
    /// Reply to [`FrontendRequest::ListHuskPanes`]. Empty when the app
    /// has no panes the engine isn't already tracking as live.
    HuskPanesList {
        panes: Vec<HostedPaneEntry>,
    },
    /// Trailing transcript chunk for a run. `lines` are the raw JSONL
    /// lines the engine read off the recorded transcript path
    /// (newest-last). `truncated` is set when the file had more lines
    /// than were returned.
    RunTranscriptTail {
        run_id: String,
        transcript_path: String,
        lines: Vec<String>,
        truncated: bool,
    },
    /// Rendered transcript for an execution. Segments are ordered by
    /// `seq` (conversation order). `is_live` is true when the execution
    /// is still running; `complete` is the inverse. The app renders
    /// segments lazily — one segment at a time — to avoid building a
    /// full MarkdownUI AST for the entire document.
    ExecutionTranscriptResult {
        execution_id: String,
        segments: Vec<TranscriptSegment>,
        /// True when the execution status is still running / waiting_human.
        is_live: bool,
        /// True when the execution has a terminal status (complement of `is_live`).
        complete: bool,
    },
    /// The transcript file for an execution is unavailable — the
    /// worker may never have started a Claude Code session, the JSONL
    /// was rotated or GC'd, or no `transcript_path` row was ever
    /// recorded. `reason` is a human-readable explanation.
    ExecutionTranscriptUnavailable {
        execution_id: String,
        reason: String,
    },
    /// Snapshot of the cube workspace pool. The engine proxies
    /// `cube --json workspace list`; each entry corresponds to one
    /// workspace cube knows about, annotated (when the engine has
    /// matching state) with the execution id currently leasing it.
    WorkspacePoolSummaryResult {
        workspaces: Vec<WorkspacePoolEntry>,
    },
    /// Snapshot of every engine worker pool's own claim bookkeeping,
    /// as requested by [`FrontendRequest::WorkerPoolSummary`].
    WorkerPoolSummaryResult {
        pools: Vec<WorkerPoolEntry>,
    },
    /// Engine confirms a dependency edge has been added. Returns the
    /// row that was inserted (or the existing row if the call was an
    /// idempotent re-add).
    DependencyAdded {
        edge: WorkItemDependency,
    },
    /// Engine confirms a dependency edge has been removed (or that no
    /// matching edge existed to begin with — also a success).
    DependencyRemoved {
        dependent_id: String,
        prerequisite_id: String,
        relation: String,
        removed: bool,
    },
    /// Edge listing for a single work item, with prerequisites and
    /// dependents in two parallel lists.
    DependencyList {
        view: WorkItemDependencyView,
    },
    /// Resolved edge listing — same shape as
    /// [`Self::DependencyList`] but each side carries the peer's
    /// status and name already joined in.
    DependencyDetail {
        detail: WorkItemDependencyDetail,
    },
    /// Response to [`FrontendRequest::SetLiveStatusEnabled`]. Carries
    /// the resulting enabled flag for the slot so the caller can
    /// distinguish "applied" from "already in that state" if it
    /// wants.
    LiveStatusEnabledSet {
        slot_id: u8,
        enabled: bool,
    },
    /// Snapshot of which slots currently have the live-status
    /// summarizer disabled. The UI uses this to render the toggle
    /// state on the Agents-tab worker row.
    LiveStatusDisabledSlotsList {
        slot_ids: Vec<u8>,
    },
    /// One-shot diagnostic snapshot of the live-status pipeline, in
    /// response to [`FrontendRequest::DebugLiveStatusPipeline`]. The
    /// full shape is documented on [`crate::LiveStatusDebugReport`].
    LiveStatusDebugReportEvent {
        report: crate::LiveStatusDebugReport,
    },
    /// Response to [`FrontendRequest::ResolveProjectDesignDoc`]: the
    /// resolved pointer state for a single project. Carried inline
    /// (not flattened) so the kanban can deserialise straight into a
    /// `ResolveProjectDesignDocOutput` without going through the
    /// envelope.
    ProjectDesignDocResolved {
        output: ResolveProjectDesignDocOutput,
    },
    /// Response to
    /// [`FrontendRequest::MarkConflictResolutionFailed`]: the
    /// post-update `conflict_resolutions` row. Carries the full row
    /// so the CLI can pretty-print "attempt foo flipped to failed,
    /// reason bar" without a follow-up `get`.
    ConflictResolutionMarkedFailed {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::ClassifyCiRemediation`]: the
    /// `ci_remediations` row after the `triage_class` column has been
    /// stamped.
    CiRemediationClassified {
        attempt: CiRemediation,
    },
    /// Response to [`FrontendRequest::MarkCiRemediationFailed`]: the
    /// row after the flip to `failed`.
    CiRemediationMarkedFailed {
        attempt: CiRemediation,
    },
    /// Response to [`FrontendRequest::MarkCiRemediationRetriggered`]:
    /// the row after the engine logged the retrigger. `new_id` echoes
    /// the provider id the worker passed in for the CLI's "marker
    /// recorded" line.
    CiRemediationRetriggered {
        attempt: CiRemediation,
        new_id: String,
    },
    /// HONORED response to
    /// [`FrontendRequest::MarkCiRemediationSucceededViaRebase`]: the
    /// engine independently re-probed live CI and verified every
    /// required check passing on the PR's current head SHA before
    /// flipping the row. `budget_refunded` reports whether the engine
    /// decremented `tasks.ci_attempts_used` — `false` when the attempt
    /// was a `retrigger` (which didn't consume budget to begin with) or
    /// when the row was already terminal (idempotent echo).
    CiRemediationSucceededViaRebase {
        attempt: CiRemediation,
        budget_refunded: bool,
    },
    /// REJECTED response to
    /// [`FrontendRequest::MarkCiRemediationSucceededViaRebase`]: the
    /// engine re-probed live CI and did NOT find it green on the PR's
    /// current head SHA (`live_sha`). `status` is a human-readable
    /// description of what the probe actually saw (failing checks,
    /// still-pending, PR closed). The attempt row is untouched and
    /// stays actionable — no budget refund, no `CiRemediationSucceeded`
    /// event. The CLI surfaces this as a non-zero exit so the receipt is
    /// an honest pass/fail (T2764 postmortem: a worker calling this verb
    /// before CI has settled must get a rejection, not a recorded lie).
    CiRemediationSucceededViaRebaseRejected {
        attempt_id: String,
        work_item_id: String,
        pr_url: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_sha: Option<String>,
    },
    /// HONORED response to [`FrontendRequest::MarkCiRemediationNoop`]:
    /// the engine independently re-probed live CI and verified every
    /// required check passing on `validated_sha` (the PR's current
    /// head SHA at probe time). The attempt has been flipped to
    /// `succeeded` and the parent unblocked out of the remediation
    /// loop. `observed_sha` echoes what the worker reported, so the
    /// CLI can note when the head advanced and the engine re-validated
    /// against the new SHA.
    CiRemediationNoopValidated {
        attempt: CiRemediation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validated_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_sha: Option<String>,
    },
    /// REJECTED response to [`FrontendRequest::MarkCiRemediationNoop`]:
    /// the engine re-probed live CI and did NOT find it green on the
    /// PR's current head SHA (`live_sha`). `status` is a human-readable
    /// description of what the probe actually saw (failing checks,
    /// still-pending, PR closed). The attempt row is untouched and
    /// stays actionable — the worker has not escaped the failure. The
    /// CLI surfaces this as a non-zero exit so the receipt is an
    /// honest pass/fail.
    CiRemediationNoopRejected {
        attempt_id: String,
        work_item_id: String,
        pr_url: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_sha: Option<String>,
    },
    /// Response to [`FrontendRequest::ListConflictResolutions`]: the
    /// filtered set of rows, ordered freshest-first.
    ConflictResolutionsList {
        attempts: Vec<ConflictResolution>,
    },
    /// Response to [`FrontendRequest::GetConflictHotspots`].
    ConflictHotspots {
        report: ConflictHotspotReport,
    },
    /// Response to [`FrontendRequest::GetConflictResolution`]: a single
    /// row by id.
    ConflictResolution {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::RetryConflictResolution`]: the
    /// row after the reset to `pending`. The engine has already
    /// re-flipped the parent work item back to `blocked:
    /// merge_conflict` so the dispatcher can pick up the new attempt.
    ConflictResolutionRetried {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::AbandonConflictResolution`]: the
    /// row after the flip to `abandoned`.
    ConflictResolutionMarkedAbandoned {
        attempt: ConflictResolution,
    },
    /// Activity-feed push: a fresh conflict-resolution attempt has been
    /// created for an in-review PR and a worker is about to take over
    /// (Phase 4 / design Q8). Broadcast on the parent product's
    /// work-tree topic so the macOS app can render an activity-feed
    /// entry without having to poll.
    ConflictResolutionStarted {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: the engine observed the parent PR back at
    /// mergeable and retired the conflict-resolution attempt. The
    /// parent has been flipped from `blocked: merge_conflict` back to
    /// `in_review`; the attempt row is `succeeded`; the worker's cube
    /// workspace lease has been released (if not already).
    ConflictResolutionSucceeded {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: a conflict-resolution attempt terminated in
    /// `failed`. Emitted when the worker calls
    /// `boss engine conflicts mark-failed`, when the completion path's
    /// catch-all (`no_push_no_stop_condition`) fires, or any other
    /// terminal-failure transition. The parent remains `blocked:
    /// merge_conflict`; the user is the next actor.
    ConflictResolutionFailed {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: a conflict-resolution attempt terminated in
    /// `abandoned`. Distinct from `failed` in that the engine stepped
    /// away on purpose (PR closed, parent merged externally, manual
    /// override). The parent has typically already moved out of
    /// `blocked: merge_conflict` by some other path.
    ConflictResolutionAbandoned {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Advisory push (Layer 4 / T11, `stacked-pr-auto-structuring`): two
    /// *in-flight* PR branches are predicted to conflict with each other —
    /// their changed-file sets overlap on non-mechanical files — so the
    /// engine offers to restack the newer PR (`dependent`) on top of the
    /// older one (`base`), turning the would-be conflict into an ordered
    /// stack the `auto_rebase` machinery keeps in sync. Purely advisory:
    /// nothing is mutated and no PR is retargeted until the offer is
    /// accepted (the accept-and-convert step lands with the `auto_rebase`
    /// flow, which is designed but not yet shipped). `base` is the PR with
    /// the lower number (the older, likelier-to-merge-first branch);
    /// `dependent` is the higher-numbered one that would rebase onto it.
    StackProposalOffered {
        product_id: String,
        base_pr_url: String,
        base_pr_number: i64,
        dependent_pr_url: String,
        dependent_pr_number: i64,
        overlapping_files: Vec<String>,
    },
    /// Activity-feed push: a fresh CI-remediation attempt has been
    /// created for an in-review PR (design §"CI worker spawn",
    /// Phase 8 #22). `attempt_kind` is `"fix"` or `"retrigger"` —
    /// the engine's pre-spawn triage decision.
    CiRemediationStarted {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        attempt_kind: String,
    },
    /// Activity-feed push: the engine observed the parent PR back at
    /// CI clean and retired the remediation attempt. The parent has
    /// been flipped from `blocked: ci_failure` back to `in_review`;
    /// the attempt row is `succeeded`.
    CiRemediationSucceeded {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: the engine observed the parent PR back at
    /// CI clean and cleared the `blocked: ci_failure` status, but there
    /// was no active remediation attempt to retire (the prior attempt was
    /// already terminal — failed, abandoned, or succeeded via the rebase
    /// path). Distinct from `CiRemediationSucceeded` because the
    /// clearance was NOT driven by an auto-fix: the UI should clear the
    /// `ci failing` badge but must NOT set the `ci auto-fixed` badge.
    CiFailureCleared {
        product_id: String,
        work_item_id: String,
        pr_url: String,
    },
    /// Activity-feed push: a CI-remediation attempt terminated in
    /// `failed`. Emitted when the worker calls
    /// `boss engine ci mark-failed` or when the completion path's
    /// catch-all fires. The parent remains `blocked: ci_failure`.
    CiRemediationFailed {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: a CI-remediation attempt was abandoned
    /// (engine declined to spawn — opt-out, suppression, or
    /// budget-related path).
    CiRemediationAbandoned {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: the engine has given up auto-fixing this
    /// PR's CI. The parent is now `blocked: ci_failure_exhausted` and
    /// the user is the next actor — typically via
    /// `boss engine ci retry <work-item-id>`.
    CiRemediationExhausted {
        product_id: String,
        work_item_id: String,
        pr_url: String,
        attempts_used: i64,
        budget: i64,
    },
    /// Activity-feed push / notification: a CI-remediation worker
    /// classified the failure as flaky/infra and re-triggered the failing
    /// job rather than pushing a code change (`boss engine ci
    /// mark-retriggered`). The engine has stamped the
    /// `ci_flaky_retriggered` signal on the parent and will NOT keep
    /// probing the worker for a diff — the task is now "awaiting CI retry /
    /// human decision." `new_run_id` is the provider id of the re-triggered
    /// run the human can watch; the recommended human action is
    /// `boss engine ci retry <work-item-id>` (or to intervene). Distinct
    /// from `CiRemediationSucceeded` (CI is not yet green) and from
    /// `CiRemediationFailed` (the worker did not give up — it deflected to
    /// infra).
    CiRemediationFlakyRetriggered {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        new_run_id: String,
    },
    /// Soft alert (design §Phase 12 #39): a PR's required CI has been
    /// `InFlight` continuously for the duration named in `level`
    /// without producing a definitive result — most commonly because
    /// the provider never started a queued job. `level` is the
    /// human-readable bucket the engine crossed on this probe (e.g.
    /// `"30m"` or `"2h"`); `elapsed_seconds` carries the precise
    /// observed duration. Emitted at most once per bucket per
    /// `(work_item_id, head_sha)` pair so the UI / log doesn't churn
    /// on every poll.
    CiNeverStartsAlert {
        product_id: String,
        work_item_id: String,
        pr_url: String,
        head_sha: String,
        level: String,
        elapsed_seconds: i64,
    },
    /// Response to [`FrontendRequest::AuditProductEffort`]. Carries
    /// the per-marker under-classification analysis for one
    /// product. Read-only snapshot; the engine recomputes from
    /// scratch each call.
    EffortAuditReport {
        report: crate::EffortAuditReport,
    },
    /// Response to [`FrontendRequest::RecordEffortEscalation`].
    /// Carries the inserted row with engine-assigned `id` and
    /// `created_at`.
    EffortEscalationRecorded {
        event: crate::EffortEscalation,
    },
    /// Response to [`FrontendRequest::ListPlannerRuns`]: every
    /// `planner_runs` audit row for the project, newest first.
    PlannerRunsList {
        project_id: String,
        runs: Vec<crate::PlannerRun>,
    },
    /// Response to [`FrontendRequest::PlanProject`]. `outcome` is one of
    /// the `PLANNER_OUTCOME_*` tags (e.g. `"staged"`, `"no_breakdown"`,
    /// `"rejected_cycle"`) for a real run, or a `"preview_"`-prefixed
    /// variant for a `dry_run` preview (see the CLI's rendering of this
    /// event). `message` is a human-readable summary. `proposal` carries
    /// the full task-graph proposal only when the plan produced a valid
    /// one — a real `staged` apply or a successful dry-run preview.
    PlanProjectResult {
        project_id: String,
        outcome: String,
        message: String,
        #[serde(default)]
        created: usize,
        #[serde(default)]
        edges: usize,
        #[serde(default)]
        skipped: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal: Option<crate::PlannerOutput>,
    },
    /// Response to [`FrontendRequest::ReleaseProject`].
    ReleaseProjectResult {
        project_id: String,
        run_id: String,
        released: usize,
    },
    /// Success reply to [`FrontendRequest::SubmitProposal`]: the row is
    /// durable. `proposal` is the persisted `worker_proposals` row —
    /// engine-assigned `id` (`prp_…`), the canonicalised `payload_json`, and
    /// the `idempotency_key` that was used (the derived one when the caller
    /// omitted it, so a subsequent call can pass it back explicitly).
    ///
    /// `already_submitted` distinguishes "this call created the row" from
    /// "an identical submission already existed and this call was a no-op".
    /// It is a *successful* outcome either way — replay safety is the point —
    /// so a caller that treats it as an error is misreading the contract; the
    /// only sane use is to soften the message it prints.
    ///
    /// Every row lands in `state: proposed` today: the apply pipeline is a
    /// later task, so nothing observes these rows yet.
    ProposalSubmitted {
        proposal: WorkerProposal,
        already_submitted: bool,
    },
    /// Failure reply to [`FrontendRequest::SubmitProposal`] or
    /// [`FrontendRequest::ListProposals`]. Sent instead of the generic
    /// [`FrontendEvent::WorkError`] so the caller can branch on
    /// `error.code` and, for a validation failure, point at the offending
    /// fields by name rather than re-printing prose.
    ProposalRejected {
        error: ProposalSubmissionError,
    },
    /// Refusal reply to *any* [`FrontendRequest`] that a worker-tier
    /// connection is not permitted to call. Sent instead of executing the
    /// verb, from a gate that runs before dispatch — so no handler ever sees
    /// the request.
    ///
    /// Distinct from the generic [`FrontendEvent::WorkError`] because the
    /// remediation is structured: [`WorkerTierDenial::use_instead`] names the
    /// verb to call instead (usually a `boss propose …`), which is the whole
    /// point of converting the mediation policy from prompt-enforced to
    /// engine-enforced.
    WorkerTierDenied {
        denial: WorkerTierDenial,
    },
    /// Response to [`FrontendRequest::ListProposals`]: every proposal filed
    /// against `work_item_id` across all its executions, newest first, with
    /// dispositions attached. `work_item_id` echoes the scope the engine
    /// derived from the caller's attributed execution.
    ProposalsList {
        work_item_id: String,
        proposals: Vec<WorkerProposal>,
    },
    /// Reply for [`FrontendRequest::GetWorkerContext`]: the sanitized
    /// one-call bundle for the caller's own work item, resolved from its
    /// attributed execution. Boxed: an inline `WorkerContextBundle` here
    /// would make this variant far larger than its siblings
    /// (clippy::large_enum_variant) — serializes identically.
    WorkerContextResult {
        bundle: Box<WorkerContextBundle>,
    },
    /// Response to [`FrontendRequest::UnpopulateProject`]. `deleted`
    /// carries the ids of tasks soft-deleted; `preserved` carries the
    /// tasks that already had an execution (released and dispatched)
    /// and were left alone.
    UnpopulateProjectResult {
        project_id: String,
        run_id: String,
        deleted: Vec<String>,
        preserved: Vec<UnpopulatePreservedTask>,
    },
    /// Response to [`FrontendRequest::ListFeatureFlags`]: a snapshot
    /// of every registered engine feature flag plus its current
    /// effective value. Order is registry order so the debug pane
    /// renders flags in a stable, predictable sequence.
    FeatureFlagsList {
        flags: Vec<FeatureFlagSnapshot>,
    },
    /// Response to [`FrontendRequest::SetFeatureFlag`]: the engine has
    /// updated and persisted the named flag to `enabled`. Receiving
    /// this event is the debug pane's "reload confirmed" signal — the
    /// new value is in effect immediately for all subsequent
    /// consumer-side checks.
    FeatureFlagSet {
        name: String,
        enabled: bool,
    },
    /// Response to [`FrontendRequest::GetEngineVersion`]: identifies
    /// the running engine binary. `binary_fingerprint` is the most
    /// reliable signal — it is a truncated SHA-256 of the engine
    /// binary's on-disk bytes (same algorithm as
    /// `boss_engine::build_info::binary_fingerprint`). The macOS app
    /// computes the same hash for its bundled engine file and compares;
    /// a mismatch means the running engine pre-dates the current app
    /// bundle and should be replaced. `git_sha` and `build_time` are
    /// included for human-readable logging only; they may be "unknown"
    /// in dev builds.
    EngineVersionResult {
        git_sha: String,
        build_time: String,
        binary_fingerprint: String,
    },
    /// Response to [`FrontendRequest::GetEngineHealth`]: the engine's
    /// current user-visible configuration health. Empty `issues` means
    /// the engine is healthy; a non-empty list is the UI's signal to
    /// render the banner / settings-pane warning.
    EngineHealthResult {
        report: EngineHealthReport,
    },
    /// Response to [`FrontendRequest::GetSettings`]: a snapshot of every
    /// registered per-installation setting and its current value.
    SettingsList {
        settings: Vec<SettingSnapshot>,
    },
    /// Response to [`FrontendRequest::SetSetting`]: the engine has
    /// persisted the new value. The macOS Settings window uses this as
    /// the "saved" signal to commit the toggle state.
    SettingSet {
        key: String,
        enabled: bool,
    },
    /// Response to [`FrontendRequest::ListHosts`]: every registered
    /// host (including `local`) with its capabilities.
    HostsList {
        hosts: Vec<HostSnapshot>,
    },
    /// Response to [`FrontendRequest::GetHost`] or
    /// [`FrontendRequest::AddHost`]: one host with all capabilities.
    HostResult {
        host: HostSnapshot,
    },
    /// Response to [`FrontendRequest::SetHostEnabled`],
    /// [`FrontendRequest::AddHostTag`], or
    /// [`FrontendRequest::RemoveHostTag`]: the updated host snapshot.
    HostUpdated {
        host: HostSnapshot,
    },
    /// Response to [`FrontendRequest::RemoveHost`]: the host has been
    /// deleted. `id` is the id that was removed.
    HostRemoved {
        id: String,
    },
    /// Response to [`FrontendRequest::MetricsShowLive`]: the
    /// in-memory snapshot for `name`. `entry` is `None` when no
    /// counter or gauge with that name is registered in the current
    /// engine binary.
    MetricsShowLiveResult {
        entry: Option<MetricLiveEntry>,
    },
    /// Response to [`FrontendRequest::MetricsListLive`]: every
    /// registered counter and gauge as a flat list, sorted by name.
    /// Stale entries (rehydrated from `state.db` but no matching
    /// handle in the current binary) are included so the debug pane
    /// can surface historical values. Counters and gauges are
    /// interleaved in name order — the `kind` field distinguishes them.
    MetricsListLiveResult {
        entries: Vec<MetricLiveEntry>,
    },
    /// Response to [`FrontendRequest::MetricsReset`]. Reports how
    /// many counters and gauges were zeroed so the caller can print a
    /// meaningful confirmation. `name = None` means "all" was
    /// requested.
    MetricsResetDone {
        name: Option<String>,
        counters_reset: u64,
        gauges_reset: u64,
    },
    /// Response to [`FrontendRequest::KickPrReconcilers`]. `kicked`
    /// is `true` when the engine forwarded the signal to the merge
    /// poller; `false` when the kick was dropped because the engine
    /// has not yet started the poller (race at startup — treat as a
    /// no-op).
    PrReconcilersKicked {
        kicked: bool,
    },
    /// Response to [`FrontendRequest::SetDispatchPaused`] and
    /// [`FrontendRequest::GetDispatchState`]. Carries the current pause state
    /// and, when paused, the epoch-seconds timestamp at which it was set.
    DispatchStateResult {
        paused: bool,
        /// Epoch seconds when dispatch was paused. `None` when `paused = false`.
        #[serde(skip_serializing_if = "Option::is_none")]
        paused_since_epoch_s: Option<u64>,
        /// Whether the pause exempts `pr_review` executions (operator pause); `false` for a breaker pause.
        #[serde(default)]
        reviews_exempt: bool,
    },
    /// Response to [`FrontendRequest::SetAutomationPaused`] and
    /// [`FrontendRequest::GetAutomationState`]. Carries the current
    /// automation-pause state and, when paused, the epoch-seconds
    /// timestamp at which it was set. Independent of
    /// [`FrontendEvent::DispatchStateResult`] — see
    /// [`FrontendRequest::SetAutomationPaused`] for the scope.
    AutomationStateResult {
        paused: bool,
        /// Epoch seconds when automation was paused. `None` when `paused = false`.
        #[serde(skip_serializing_if = "Option::is_none")]
        paused_since_epoch_s: Option<u64>,
    },
    /// Response to [`FrontendRequest::SyncProductExternalTracker`].
    /// Emitted when the engine begins the on-demand reconcile pass
    /// for the named product. The pass runs synchronously; this event
    /// is the "pass started" confirmation rather than a streaming
    /// progress push.
    ExternalTrackerSyncStarted {
        product_id: String,
    },
    /// Response to [`FrontendRequest::ListCiRemediations`]: the
    /// filtered set of `ci_remediations` rows, ordered freshest-first.
    CiRemediationsList {
        attempts: Vec<CiRemediation>,
    },
    /// Response to [`FrontendRequest::GetCiRemediation`]: a single
    /// `ci_remediations` row by id.
    CiRemediation {
        attempt: CiRemediation,
    },
    /// Response to [`FrontendRequest::RetryCiRemediation`]: the parent
    /// work item's CI budget after the retry path ran. Echoes the
    /// `work_item_id` the engine resolved (so a caller that passed an
    /// attempt id can confirm the parent). `was_exhausted` indicates
    /// whether the parent was in `blocked: ci_failure_exhausted` at
    /// the time of the call (and is now back at `in_review`); `false`
    /// means the parent wasn't exhausted and the retry was a counter
    /// reset only.
    CiRemediationRetryDone {
        work_item_id: String,
        budget: CiBudgetSnapshot,
        was_exhausted: bool,
    },
    /// Response to [`FrontendRequest::AbandonCiRemediation`]: the row
    /// after the flip to `abandoned`.
    CiRemediationMarkedAbandoned {
        attempt: CiRemediation,
    },
    /// Response to [`FrontendRequest::GetCiBudget`].
    CiBudget {
        budget: CiBudgetSnapshot,
    },
    /// Response to [`FrontendRequest::SetCiBudget`]: the post-update
    /// snapshot of the work item's CI budget.
    CiBudgetUpdated {
        budget: CiBudgetSnapshot,
    },
    /// Response to [`FrontendRequest::ListEngineAttempts`]: the
    /// projected and merged row set, ordered freshest-first across the
    /// three attempt subsystems.
    EngineAttemptsList {
        attempts: Vec<EngineAttemptListEntry>,
    },
    /// Engine acknowledges a reveal request — the macOS app has
    /// switched to the kanban, scrolled the target card into view, and
    /// started its transient highlight. Carries the resolved canonical
    /// `id` so `bossctl reveal` can confirm which item was highlighted.
    WorkItemRevealed {
        id: String,
    },
    /// Response to [`FrontendRequest::Shutdown`] when the supplied
    /// token matched the engine's. The engine sends this immediately
    /// before starting graceful shutdown so the caller has a
    /// well-defined "accepted, you should now wait for the socket to
    /// close" signal.
    ShutdownAccepted,
    /// Response to [`FrontendRequest::Shutdown`] when the supplied
    /// token did not match. The engine logs an audit record and keeps
    /// running. `reason` is a short stable label
    /// (`token_mismatch`, `token_missing`) so callers can distinguish
    /// auth failures from unrelated socket errors without parsing a
    /// human string.
    ShutdownRejected {
        reason: String,
    },
    /// Pushed whenever the GitHub OAuth auth state changes — in
    /// response to [`FrontendRequest::GitHubAuthStatus`] and
    /// proactively as the device-flow poll loop advances. The UI
    /// renders whatever state the engine pushes without polling.
    GitHubAuthState {
        state: GitHubAuthStateDto,
    },

    /// Reply to [`FrontendRequest::TrunkSetToken`] / [`FrontendRequest::TrunkStatus`].
    /// `source` is `"env"` / `"keychain"` when `configured` is `true`, `None`
    /// otherwise. `queue_check` is the live `getQueue` smoke-check outcome —
    /// `None` when no token is configured, or when there is no
    /// `trunk_queue`-mechanism product yet to probe against (`note` explains
    /// which).
    TrunkStatus {
        configured: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_check: Option<TrunkQueueCheckDto>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },

    // --- Comments in the markdown viewer (Phase 2) replies. ---
    /// Reply to `comments_create` / `comments_dismiss` /
    /// `comments_set_status` / `comments_update_anchor` / `comments_post_answer`.
    CommentResult {
        comment: WorkComment,
    },
    /// Reply to `comments_list`. Each comment carries its thread entries and
    /// whether an answer-agent run is currently in flight for it — design
    /// `comment-triggered-document-revisions.md` §"UI / thread behavior".
    CommentsList {
        artifact_kind: String,
        artifact_id: String,
        comments: Vec<CommentWithThread>,
    },
    /// Reply to `comments_banner_state`.
    CommentsBannerState {
        artifact_kind: String,
        artifact_id: String,
        #[serde(flatten)]
        state: CommentsBannerState,
    },
    /// Reply to `comments_resolve`: each active comment paired with its
    /// resolution against the supplied plain text.
    CommentsResolved {
        artifact_kind: String,
        artifact_id: String,
        comments: Vec<ResolvedComment>,
    },
    /// Reply to `CommentsReviseDoc`.
    CommentsReviseDocResult {
        outcome: ReviseDocOutcome,
    },
    /// Response to [`FrontendRequest::OpenReviewTerminal`]: the engine
    /// has leased a workspace, fetched the PR branch, and created a new
    /// jj commit atop `<branch>@origin`. The app should open a Ghostty
    /// terminal window rooted at `workspace_path` and send
    /// [`FrontendRequest::ReleaseReviewTerminal`] with `lease_id` when
    /// the window closes to avoid leaking the lease.
    ReviewTerminalReady {
        work_item_id: String,
        workspace_path: String,
        lease_id: String,
    },
    /// Response to [`FrontendRequest::OpenLiveWorkspaceTerminal`]: the
    /// work item has a live execution with an already-leased cube
    /// workspace at `workspace_path`. The app should open a Ghostty
    /// terminal window rooted there. There is no matching "release"
    /// request — the lease belongs to the running worker, not to this
    /// terminal window, so it is left untouched when the window closes.
    LiveWorkspaceTerminalReady {
        work_item_id: String,
        workspace_path: String,
    },
    /// Response to [`FrontendRequest::MergeWhenReady`]: the engine has
    /// successfully initiated the merge process for the PR. `action`
    /// identifies what happened: `"enqueued"` (PR added to the repo's
    /// merge queue), `"auto_merge_enabled"` (auto-merge enabled; PR
    /// will merge once required checks pass), `"merged"` (PR was
    /// merged directly because all checks were already passing), or
    /// `"trunk_enqueued"` (PR submitted to a `trunk_queue`-mechanism
    /// product's Trunk merge queue; also the reply for a duplicate click
    /// while an intent is already active). The PR-reconciler is kicked on
    /// the engine side so the kanban state refreshes promptly without
    /// waiting for the next periodic sweep.
    MergeWhenReadyAccepted {
        work_item_id: String,
        pr_url: String,
        action: String,
    },

    // --- Automation replies (maintenance-tasks.md T2) ---
    /// Response to [`FrontendRequest::CreateAutomation`].
    AutomationCreated {
        automation: Automation,
    },
    /// Response to [`FrontendRequest::ListAutomations`].
    AutomationsList {
        product_id: String,
        automations: Vec<Automation>,
        /// Open-task count keyed by automation id, batched in the same
        /// round-trip via a correlated subquery. Absent (empty map) on old
        /// engines that do not populate this field.
        #[serde(default)]
        open_task_counts: std::collections::HashMap<String, i64>,
    },
    /// Response to [`FrontendRequest::GetAutomation`].
    AutomationResult {
        automation: Automation,
    },
    /// Response to [`FrontendRequest::UpdateAutomation`],
    /// [`FrontendRequest::EnableAutomation`], or
    /// [`FrontendRequest::DisableAutomation`].
    AutomationUpdated {
        automation: Automation,
    },
    /// Response to [`FrontendRequest::DeleteAutomation`].
    AutomationDeleted {
        automation_id: String,
    },
    /// Response to [`FrontendRequest::GetAutomationOpenTaskCount`].
    AutomationOpenTaskCount {
        automation_id: String,
        count: i64,
    },
    /// Snapshot of an `automation_runs` row. Used to surface individual
    /// run records over the wire (e.g. for future CLI `boss automation runs`).
    AutomationRunResult {
        run: AutomationRun,
    },

    // --- Editorial controls replies ---
    /// Response to [`FrontendRequest::ListEditorialActions`]: the audit
    /// rows for the product, ordered freshest-first.
    EditorialActionsList {
        product_id: String,
        actions: Vec<EditorialAction>,
    },
    /// Response to [`FrontendRequest::EvaluateEditorialRules`]: the
    /// outcome of running the product's rules against the supplied body.
    /// `decision` is `"allow"`, `"rewrite"`, or `"deny"`. `findings`
    /// lists human-readable descriptions of every triggered rule.
    /// `rewritten_body` is present (and differs from the input) when
    /// `decision == "rewrite"`.
    EditorialRulesEvaluated {
        product_id: String,
        decision: String,
        findings: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rewritten_body: Option<String>,
    },
    /// Response to [`FrontendRequest::ListAutomationRuns`].
    AutomationRunsList {
        automation_id: String,
        runs: Vec<AutomationRun>,
    },
    /// Response to [`FrontendRequest::ListAutomationDedupSuppressions`].
    AutomationDedupSuppressionsList {
        automation_id: String,
        suppressions: Vec<AutomationDedupSuppression>,
    },
    /// Response to [`FrontendRequest::ListAutomationTasks`].
    AutomationTasksList {
        automation_id: String,
        tasks: Vec<Task>,
    },
    /// Response to [`FrontendRequest::RunAutomation`] when the fire was
    /// accepted and enqueued.
    AutomationRunEnqueued {
        automation_id: String,
    },
}
