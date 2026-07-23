//! Field-level sanitization of rows on their way out to a worker.
//!
//! ## Why a whole pass instead of edits in four handlers
//!
//! Execution and run rows are the only rows that straddle the isolation
//! boundary: their taxonomy columns (status, PR binding, timestamps) are
//! exactly what a worker needs to see, while `transcript_path` and the
//! host/pid columns belong to the runtime half that stays closed (design
//! §"Read-only model access and the exposure boundary": "Sanitization is
//! field-level where a row mixes halves").
//!
//! Applying that in each read handler would work today and rot tomorrow —
//! the next verb to return a `WorkExecution` would have to remember. So
//! sanitization runs once per outbound event, at the connection's single
//! write choke point, and therefore covers responses and topic pushes alike
//! without any handler opting in.
//!
//! ## The forbidden-key test is the real guard
//!
//! The four fields the design names are `transcript_path`, `host_id`,
//! `remote_pid`, and `shell_pid`. `transcript_path` and `artifacts_path` are
//! on the wire today: the other two are `work_executions` / `work_runs`
//! **columns that `mappers.rs` never maps into [`WorkExecution`] or
//! [`WorkRun`]**. Stripping a field that does not exist is not something the
//! type system can express, so [`SANITIZED_EXECUTION_FIELDS`] is asserted
//! against the *serialized JSON* in tests instead. If someone later adds
//! `host_id` to `WorkExecution`, that test fails and this module has to grow
//! a line — which is the outcome we want, rather than a silent leak.

use boss_protocol::{FrontendEvent, WorkExecution, WorkRun};

/// The JSON keys that must never appear in an execution or run row sent to a
/// worker. Asserted against serialized rows in this crate's tests, so a
/// future field addition that reintroduces one of these fails loudly.
pub const SANITIZED_EXECUTION_FIELDS: &[&str] = &[
    "transcript_path",
    "artifacts_path",
    "host_id",
    "remote_pid",
    "shell_pid",
];

/// Strip the runtime-half fields from one execution row.
///
/// A no-op today — [`WorkExecution`] carries none of
/// [`SANITIZED_EXECUTION_FIELDS`] on the wire — but it exists so the
/// sanitizing pass has an obvious place to grow when it does, and so the
/// wiring is already correct at every call site.
fn sanitize_execution(execution: WorkExecution) -> WorkExecution {
    execution
}

/// Strip the runtime-half fields from one run row.
///
/// `transcript_path` is an absolute path into the engine's transcript store,
/// and handing it to a worker would turn a taxonomy read into a
/// filesystem-level route to another execution's transcript — the exact
/// thing `TailRunTranscript` is denied for. `artifacts_path` is the same
/// class of leak: it is where `$BOSS_STRUCTURED_OUTPUT` files (review
/// verdicts, followup manifests) for a run live, so it is stripped for the
/// same reason even though no production writer populates it today.
fn sanitize_run(mut run: WorkRun) -> WorkRun {
    run.transcript_path = None;
    run.artifacts_path = None;
    run
}

/// Sanitize one outbound event for a worker-tier connection.
///
/// This is a `match` with **no wildcard arm**, mirroring the discipline
/// [`worker_verb_decision`](crate::worker_verb_decision) uses on the request
/// side: every `FrontendEvent` variant is named explicitly, either in the
/// run/execution-carrying arms above or in the single pass-through arm below.
/// A variant added to `FrontendEvent` tomorrow will not compile until someone
/// decides which bucket it belongs in — the same compile-error-as-review-
/// prompt property the crate doc claims for the boundary as a whole. The
/// pass-through arm covers variants that either carry no rows straddling the
/// boundary, or are replies to verbs a worker cannot call in the first place
/// (belt-and-braces — the verb gate already stopped those).
pub fn sanitize_event_for_worker(event: FrontendEvent) -> FrontendEvent {
    match event {
        FrontendEvent::ExecutionResult { execution } => FrontendEvent::ExecutionResult {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionsList {
            work_item_id,
            executions,
        } => FrontendEvent::ExecutionsList {
            work_item_id,
            executions: executions.into_iter().map(sanitize_execution).collect(),
        },
        FrontendEvent::ExecutionCreated { execution } => FrontendEvent::ExecutionCreated {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionRequested { execution } => FrontendEvent::ExecutionRequested {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionCancelled { execution } => FrontendEvent::ExecutionCancelled {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::PrReviewTriggered {
            execution,
            work_item_id,
            pr_url,
        } => FrontendEvent::PrReviewTriggered {
            execution: sanitize_execution(execution),
            work_item_id,
            pr_url,
        },
        FrontendEvent::RunReaped { run_id, execution } => FrontendEvent::RunReaped {
            run_id,
            execution: sanitize_execution(execution),
        },
        FrontendEvent::RunResult { run } => FrontendEvent::RunResult { run: sanitize_run(run) },
        FrontendEvent::RunCreated { run } => FrontendEvent::RunCreated { run: sanitize_run(run) },
        FrontendEvent::RunsList { execution_id, runs } => FrontendEvent::RunsList {
            execution_id,
            runs: runs.into_iter().map(sanitize_run).collect(),
        },
        passthrough @ (FrontendEvent::Hello { .. }
        | FrontendEvent::Subscribed { .. }
        | FrontendEvent::Unsubscribed { .. }
        | FrontendEvent::TopicEvent { .. }
        | FrontendEvent::ProductsList { .. }
        | FrontendEvent::ProjectsList { .. }
        | FrontendEvent::TasksList { .. }
        | FrontendEvent::ChoresList { .. }
        | FrontendEvent::RevisionsList { .. }
        | FrontendEvent::WorkTree { .. }
        | FrontendEvent::WorkItemResult { .. }
        | FrontendEvent::WorkItemsByPrResult { .. }
        | FrontendEvent::WorkItemCreated { .. }
        | FrontendEvent::WorkItemsCreated { .. }
        | FrontendEvent::WorkItemUpdated { .. }
        | FrontendEvent::ProjectTasksReordered { .. }
        | FrontendEvent::TaskRuntimeResult { .. }
        | FrontendEvent::AttentionItemsList { .. }
        | FrontendEvent::AttentionItemResult { .. }
        | FrontendEvent::AttentionItemCreated { .. }
        | FrontendEvent::AttentionItemsForWorkItemList { .. }
        | FrontendEvent::AttentionItemUpdated { .. }
        | FrontendEvent::AttentionItemConverted { .. }
        | FrontendEvent::DeferredScopeAttentionsList { .. }
        | FrontendEvent::AttentionGroupsList { .. }
        | FrontendEvent::AttentionGroupResult { .. }
        | FrontendEvent::AttentionCreated { .. }
        | FrontendEvent::AttentionGroupUpdated { .. }
        | FrontendEvent::AttentionGroupActioned { .. }
        | FrontendEvent::AttentionMergesList { .. }
        | FrontendEvent::WorkItemDeleted { .. }
        | FrontendEvent::WorkItemRestored { .. }
        | FrontendEvent::WorkError { .. }
        | FrontendEvent::WorkItemDuplicateBlocked { .. }
        | FrontendEvent::Error { .. }
        | FrontendEvent::AppSessionRegistered
        | FrontendEvent::EnginePoolConfig { .. }
        | FrontendEvent::BossSessionRegistered
        | FrontendEvent::ProbeQueued { .. }
        | FrontendEvent::ProbeReplied { .. }
        | FrontendEvent::ProbeDeliveryEscalated { .. }
        | FrontendEvent::RunStopped { .. }
        | FrontendEvent::WorkerPaneFocused { .. }
        | FrontendEvent::WorkerInputSent { .. }
        | FrontendEvent::WorkerPaneInterrupted { .. }
        | FrontendEvent::EngineRequest { .. }
        | FrontendEvent::WorkerLiveStatesList { .. }
        | FrontendEvent::PaneRetired { .. }
        | FrontendEvent::HuskPanesList { .. }
        | FrontendEvent::RunTranscriptTail { .. }
        | FrontendEvent::ExecutionTranscriptResult { .. }
        | FrontendEvent::ExecutionTranscriptUnavailable { .. }
        | FrontendEvent::WorkspacePoolSummaryResult { .. }
        | FrontendEvent::WorkerPoolSummaryResult { .. }
        | FrontendEvent::DependencyAdded { .. }
        | FrontendEvent::DependencyRemoved { .. }
        | FrontendEvent::DependencyList { .. }
        | FrontendEvent::DependencyDetail { .. }
        | FrontendEvent::LiveStatusEnabledSet { .. }
        | FrontendEvent::LiveStatusDisabledSlotsList { .. }
        | FrontendEvent::LiveStatusDebugReportEvent { .. }
        | FrontendEvent::ProjectDesignDocResolved { .. }
        | FrontendEvent::ConflictResolutionMarkedFailed { .. }
        | FrontendEvent::CiRemediationClassified { .. }
        | FrontendEvent::CiRemediationMarkedFailed { .. }
        | FrontendEvent::CiRemediationRetriggered { .. }
        | FrontendEvent::CiRemediationSucceededViaRebase { .. }
        | FrontendEvent::CiRemediationSucceededViaRebaseRejected { .. }
        | FrontendEvent::CiRemediationNoopValidated { .. }
        | FrontendEvent::CiRemediationNoopRejected { .. }
        | FrontendEvent::ConflictResolutionsList { .. }
        | FrontendEvent::ConflictHotspots { .. }
        | FrontendEvent::ConflictResolution { .. }
        | FrontendEvent::ConflictResolutionRetried { .. }
        | FrontendEvent::ConflictResolutionMarkedAbandoned { .. }
        | FrontendEvent::ConflictResolutionStarted { .. }
        | FrontendEvent::ConflictResolutionSucceeded { .. }
        | FrontendEvent::ConflictResolutionFailed { .. }
        | FrontendEvent::ConflictResolutionAbandoned { .. }
        | FrontendEvent::StackProposalOffered { .. }
        | FrontendEvent::CiRemediationStarted { .. }
        | FrontendEvent::CiRemediationSucceeded { .. }
        | FrontendEvent::CiFailureCleared { .. }
        | FrontendEvent::CiRemediationFailed { .. }
        | FrontendEvent::CiRemediationAbandoned { .. }
        | FrontendEvent::CiRemediationExhausted { .. }
        | FrontendEvent::CiRemediationFlakyRetriggered { .. }
        | FrontendEvent::CiNeverStartsAlert { .. }
        | FrontendEvent::EffortAuditReport { .. }
        | FrontendEvent::EffortEscalationRecorded { .. }
        | FrontendEvent::PlannerRunsList { .. }
        | FrontendEvent::PlanProjectResult { .. }
        | FrontendEvent::ReleaseProjectResult { .. }
        | FrontendEvent::ProposalSubmitted { .. }
        | FrontendEvent::ProposalRejected { .. }
        | FrontendEvent::WorkerTierDenied { .. }
        | FrontendEvent::ProposalsList { .. }
        | FrontendEvent::UnpopulateProjectResult { .. }
        | FrontendEvent::FeatureFlagsList { .. }
        | FrontendEvent::FeatureFlagSet { .. }
        | FrontendEvent::EngineVersionResult { .. }
        | FrontendEvent::EngineHealthResult { .. }
        | FrontendEvent::SettingsList { .. }
        | FrontendEvent::SettingSet { .. }
        | FrontendEvent::HostsList { .. }
        | FrontendEvent::HostResult { .. }
        | FrontendEvent::HostUpdated { .. }
        | FrontendEvent::HostRemoved { .. }
        | FrontendEvent::MetricsShowLiveResult { .. }
        | FrontendEvent::MetricsListLiveResult { .. }
        | FrontendEvent::MetricsResetDone { .. }
        | FrontendEvent::PrReconcilersKicked { .. }
        | FrontendEvent::DispatchStateResult { .. }
        | FrontendEvent::AutomationStateResult { .. }
        | FrontendEvent::ExternalTrackerSyncStarted { .. }
        | FrontendEvent::CiRemediationsList { .. }
        | FrontendEvent::CiRemediation { .. }
        | FrontendEvent::CiRemediationRetryDone { .. }
        | FrontendEvent::CiRemediationMarkedAbandoned { .. }
        | FrontendEvent::CiBudget { .. }
        | FrontendEvent::CiBudgetUpdated { .. }
        | FrontendEvent::EngineAttemptsList { .. }
        | FrontendEvent::WorkItemRevealed { .. }
        | FrontendEvent::ShutdownAccepted
        | FrontendEvent::ShutdownRejected { .. }
        | FrontendEvent::GitHubAuthState { .. }
        | FrontendEvent::TrunkStatus { .. }
        | FrontendEvent::CommentResult { .. }
        | FrontendEvent::CommentsList { .. }
        | FrontendEvent::CommentsBannerState { .. }
        | FrontendEvent::CommentsResolved { .. }
        | FrontendEvent::CommentsReviseDocResult { .. }
        | FrontendEvent::ReviewTerminalReady { .. }
        | FrontendEvent::LiveWorkspaceTerminalReady { .. }
        | FrontendEvent::MergeWhenReadyAccepted { .. }
        | FrontendEvent::AutomationCreated { .. }
        | FrontendEvent::AutomationsList { .. }
        | FrontendEvent::AutomationResult { .. }
        | FrontendEvent::AutomationUpdated { .. }
        | FrontendEvent::AutomationDeleted { .. }
        | FrontendEvent::AutomationOpenTaskCount { .. }
        | FrontendEvent::AutomationRunResult { .. }
        | FrontendEvent::EditorialActionsList { .. }
        | FrontendEvent::EditorialRulesEvaluated { .. }
        | FrontendEvent::AutomationRunsList { .. }
        | FrontendEvent::AutomationDedupSuppressionsList { .. }
        | FrontendEvent::AutomationTasksList { .. }
        | FrontendEvent::AutomationRunEnqueued { .. }) => passthrough,
    }
}
