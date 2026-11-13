//! The per-request replay, admission, and authorization contract.
//!
//! Every [`FrontendRequest`] has four independent per-request properties:
//! whether a request whose outcome is unknown may be sent again (replay),
//! which engine admission lane it uses, whether a worker session may call it,
//! and whether a progression session may call it. [`request_contract`] answers
//! all four for each request.
//!
//! [`request_contract`] is a single exhaustive `match` with no catch-all, so a
//! new `FrontendRequest` variant does not compile until someone deliberately
//! chooses every answer for it. Requests whose answers depend on their payload
//! (`force_duplicate`, `redeliver`, `refresh`, `UpdateWorkItem` patch fields)
//! are classified by the branch that applies. This crate only states the
//! contract; the client turns [`ReplayContract`] into replay windows, the
//! engine turns [`AdmissionLane`] into queueing behavior, and worker-policy
//! turns [`WorkerAccess`] / [`ProgressionAccess`] into denial objects. None of
//! those consumers are imported here.
//!
//! Worker allow never implies progression allow: each row names both. Nothing
//! here trusts a tier supplied in an RPC payload; the engine still classifies
//! the authenticated connection and still scopes rows and handler authorization
//! itself.
//!
//! ## Replay
//!
//! A connect-phase failure means nothing was sent, so every request is safe to
//! retry then. The hard case is a drop **after** (or possibly after) the
//! request was written: the engine may have applied it before dying, and
//! sending it again could apply it twice.
//!
//! * [`ReplayContract::Always`]: a second send is harmless.
//!   * *Reads*: every `Get*` and `List*` request, plus the few read-only verbs
//!     that do not follow that naming (`CommentsGet`, `CommentsList`,
//!     `CommentsBannerState`, `TailRunTranscript`, `ExecutionTranscript`,
//!     `FindWorkItemsByPr`, `WorkerPoolSummary`, `WorkspacePoolSummary`,
//!     `TrunkStatus`, `GitHubAuthStatus`, `ProbeStatus`, `MetricsListLive`,
//!     `MetricsShowLive`, `DebugLiveStatusPipeline`).
//!   * *Declarative setters*: every `Set*` request writes an absolute value, so
//!     repeating it leaves the same state.
//!   * *Documented idempotent writes* (each is called out as idempotent in its
//!     `FrontendRequest` doc comment): `AbandonCiRemediation`,
//!     `AbandonConflictResolution`, `AddDependency`, `DisableAutomation`,
//!     `EnableAutomation`, `MarkCiRemediationSucceededViaRebase`,
//!     `ReleaseHoldRun`, `RestoreWorkItem`, `RetirePane`, `RevealWorkItem`,
//!     `RevokeDecision`, `StopRun`.
//!   * *Keyed by an engine-side idempotency key*: `SubmitProposal` (unique on
//!     `(execution_id, idempotency_key)`, and the engine derives a key from the
//!     payload when the caller gives none) and `SubmitAttachment` (unique on
//!     `(execution_id, content_digest)`). A replay returns the original row
//!     instead of making a second one.
//!   * `GetCoordinatorHandoff` only when `redeliver` is false; a redelivery
//!     re-pushes the handoff to the coordinator and is [`Never`](ReplayContract::Never).
//! * [`ReplayContract::DuplicateGuarded`]: protected by the engine's
//!   duplicate-create guard, which refuses a same-named task/chore/
//!   investigation in the same product created within its look-back window.
//!   Applies to `CreateTask`, `CreateChore` and `CreateInvestigation`, and only
//!   when `force_duplicate` is false: with it set the guard is off. The window
//!   itself is the client's concern, not part of this contract.
//! * [`ReplayContract::Never`]: creates without a guard, state transitions,
//!   fan-out commands, and `Shutdown`.
//!
//! ## Admission
//!
//! * [`AdmissionLane::Bulk`]: reads that may touch the network or large
//!   result sets and queue behind the bounded bulk lane.
//! * [`AdmissionLane::Live`]: the audited set of cheap, local-only reads that
//!   must stay responsive while the bulk lane is saturated. Membership needs
//!   an audit: the handler may read only local process state, local SQL, or the
//!   local cube socket. Test-only live names are generated from the same macros
//!   the match uses; the tests compare them with an independently written audit
//!   list. `Live` is constructed only at those macro match arms.
//! * [`AdmissionLane::Ordered`]: everything else, handled in request order and
//!   never subject to read admission. `GetPrStatus` is `Bulk` unless `refresh`
//!   is set, since a refresh probe must not hold a bulk slot.
//!
//! ## Worker access
//!
//! * [`WorkerAccess::Allow`]: taxonomy/comment/execution/proposal reads, the
//!   sanctioned telemetry writes, `SubmitProposal`, and `GetEngineVersion`.
//! * [`WorkerAccess::Redirect`]: a taxonomy write with a `boss propose …` the
//!   worker should call instead. `UpdateWorkItem` selects the redirect from
//!   the patch (`pr_url` → pr-created, `effort_level` → effort-escalation,
//!   otherwise the general propose route).
//! * [`WorkerAccess::Taxonomy`]: a taxonomy write with no worker-facing route.
//! * [`WorkerAccess::Runtime`]: dispatch, panes, transcripts, hosts, engine
//!   config — the runtime half of worker isolation.
//! * [`WorkerAccess::Coordinator`]: coordinator/app verbs (session, planner,
//!   automation management, operator comment actions).
//!
//! ## Progression access
//!
//! An independent Allow/Deny for every variant. Progression may read the work
//! taxonomy and submit proposals; worker-only sanctioned writes stay denied.

use crate::FrontendRequest;
use crate::FrontendRequest as R;
use crate::WorkItemPatch;

/// Whether a request that was already sent may be sent again after the
/// connection dropped with its outcome unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayContract {
    /// Safe to resend at any time.
    Always,
    /// Safe to resend only while the engine's duplicate-create guard can still
    /// see the original, so a replay is refused instead of creating a twin.
    DuplicateGuarded,
    /// Never resend; report the outcome as unknown.
    Never,
}

/// Which engine admission lane a request uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionLane {
    /// A bounded read that queues behind the bulk lane.
    Bulk,
    /// An audited local-only read that has its own lane.
    Live,
    /// Handled in request order, outside read admission.
    Ordered,
}

/// The `boss propose …` a denied worker should call instead of a taxonomy write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRedirect {
    FollowupTask,
    Attention,
    PrCreated,
    EffortEscalation,
    DeferredScope,
    General,
}

impl WorkerRedirect {
    /// The command string carried on the typed denial.
    pub const fn as_command(self) -> &'static str {
        match self {
            Self::FollowupTask => "boss propose followup-task",
            Self::Attention => "boss propose attention",
            Self::PrCreated => "boss propose pr-created",
            Self::EffortEscalation => "boss propose effort-escalation",
            Self::DeferredScope => "boss propose deferred-scope",
            Self::General => "boss propose (pr-created, effort-escalation, blocked, ...)",
        }
    }
}

/// Whether a worker-tier connection may execute a request, and how a denial is
/// classified. Independent of replay, admission, and progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerAccess {
    Allow,
    Redirect(WorkerRedirect),
    Taxonomy,
    Runtime,
    Coordinator,
}

/// Whether a progression-tier connection may execute a request. Independent of
/// worker access: worker allow never implies progression allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressionAccess {
    Allow,
    Deny,
}

/// Everything the client and the engine need to know about a request before
/// they handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestContract {
    pub replay: ReplayContract,
    pub admission: AdmissionLane,
    pub worker: WorkerAccess,
    pub progression: ProgressionAccess,
}

impl RequestContract {
    const fn new(
        replay: ReplayContract,
        admission: AdmissionLane,
        worker: WorkerAccess,
        progression: ProgressionAccess,
    ) -> Self {
        Self {
            replay,
            admission,
            worker,
            progression,
        }
    }
}

const fn bulk_read(worker: WorkerAccess, progression: ProgressionAccess) -> RequestContract {
    RequestContract::new(ReplayContract::Always, AdmissionLane::Bulk, worker, progression)
}

const fn ordered_replayable(worker: WorkerAccess, progression: ProgressionAccess) -> RequestContract {
    RequestContract::new(ReplayContract::Always, AdmissionLane::Ordered, worker, progression)
}

const fn bulk_unreplayable(worker: WorkerAccess, progression: ProgressionAccess) -> RequestContract {
    RequestContract::new(ReplayContract::Never, AdmissionLane::Bulk, worker, progression)
}

const fn ordered_unreplayable(worker: WorkerAccess, progression: ProgressionAccess) -> RequestContract {
    RequestContract::new(ReplayContract::Never, AdmissionLane::Ordered, worker, progression)
}

fn guarded_create(force_duplicate: bool, worker: WorkerAccess, progression: ProgressionAccess) -> RequestContract {
    RequestContract::new(
        if force_duplicate {
            ReplayContract::Never
        } else {
            ReplayContract::DuplicateGuarded
        },
        AdmissionLane::Ordered,
        worker,
        progression,
    )
}

fn update_work_item_worker_access(patch: &WorkItemPatch) -> WorkerAccess {
    WorkerAccess::Redirect(if patch.pr_url.is_some() {
        WorkerRedirect::PrCreated
    } else if patch.effort_level.is_some() {
        WorkerRedirect::EffortEscalation
    } else {
        WorkerRedirect::General
    })
}

// Live variants are listed in the two macros below; the match constructs `Live`
// only at those arms, and the test-only inventory is the concatenation of the
// same lists. rustfmt does not format macro bodies, so keep these lists (and
// only these lists) in the macros.
macro_rules! live_worker_allowed {
    ($callback:ident) => {
        $callback! {
            GetEngineVersion,
        }
    };
}
macro_rules! live_runtime_denied {
    ($callback:ident) => {
        $callback! {
            GetEngineHealth,
            GetEnginePoolConfig,
            ListHostedPaneStatuses,
            ListTmuxWorkerStatuses,
            ListWorkerLiveStates,
            MetricsListLive,
            MetricsShowLive { .. },
            ProbeStatus { .. },
            WorkerPoolSummary,
            WorkspacePoolSummary,
        }
    };
}
macro_rules! live_pattern {
    ($($live:ident $( { $($fields:tt)* } )?),* $(,)?) => {
        $(R::$live $( { $($fields)* } )?)|*
    };
}
#[cfg(test)]
macro_rules! live_names {
    ($($live:ident $( { $($fields:tt)* } )?),* $(,)?) => {
        &[$(stringify!($live)),*]
    };
}
#[cfg(test)]
pub(crate) fn live_variants() -> impl Iterator<Item = &'static str> {
    live_worker_allowed!(live_names)
        .iter()
        .copied()
        .chain(live_runtime_denied!(live_names).iter().copied())
}

/// The replay, admission, and authorization contract of `request`; see the
/// module docs.
pub fn request_contract(request: &FrontendRequest) -> RequestContract {
    match request {
        // Payload-dependent: the duplicate guard is off under `force_duplicate`.
        // Worker/progression do not follow that flag.
        R::CreateTask { input } => guarded_create(
            input.force_duplicate,
            WorkerAccess::Redirect(WorkerRedirect::FollowupTask),
            ProgressionAccess::Deny,
        ),
        R::CreateChore { input } => guarded_create(
            input.force_duplicate,
            WorkerAccess::Redirect(WorkerRedirect::FollowupTask),
            ProgressionAccess::Deny,
        ),
        R::CreateInvestigation { input } => guarded_create(
            input.force_duplicate,
            WorkerAccess::Redirect(WorkerRedirect::FollowupTask),
            ProgressionAccess::Deny,
        ),
        // Payload-dependent: a redelivery re-pushes the handoff.
        R::GetCoordinatorHandoff { redeliver } => RequestContract::new(
            if *redeliver {
                ReplayContract::Never
            } else {
                ReplayContract::Always
            },
            AdmissionLane::Bulk,
            WorkerAccess::Coordinator,
            ProgressionAccess::Deny,
        ),
        // Payload-dependent: a refresh probe must not hold a bulk slot.
        R::GetPrStatus { refresh, .. } => RequestContract::new(
            ReplayContract::Always,
            if *refresh {
                AdmissionLane::Ordered
            } else {
                AdmissionLane::Bulk
            },
            WorkerAccess::Allow,
            ProgressionAccess::Allow,
        ),
        // Payload-dependent: the redirect names the propose verb that matches
        // the field the worker was trying to set.
        R::UpdateWorkItem { patch, .. } => RequestContract::new(
            ReplayContract::Never,
            AdmissionLane::Ordered,
            update_work_item_worker_access(patch),
            ProgressionAccess::Deny,
        ),

        live_worker_allowed!(live_pattern) => RequestContract::new(
            ReplayContract::Always,
            AdmissionLane::Live,
            WorkerAccess::Allow,
            ProgressionAccess::Allow,
        ),
        live_runtime_denied!(live_pattern) => RequestContract::new(
            ReplayContract::Always,
            AdmissionLane::Live,
            WorkerAccess::Runtime,
            ProgressionAccess::Deny,
        ),

        R::CommentsBannerState { .. }
        | R::CommentsGet { .. }
        | R::CommentsList { .. }
        | R::FindWorkItemsByPr { .. }
        | R::GetAttentionGroup { .. }
        | R::GetAttentionItem { .. }
        | R::GetDecision { .. }
        | R::GetExecution { .. }
        | R::GetPrBody { .. }
        | R::GetProductDesignDoc { .. }
        | R::GetRun { .. }
        | R::GetWorkItem { .. }
        | R::GetWorkItemByShortId { .. }
        | R::GetWorkRuntimePatch { .. }
        | R::GetWorkTree { .. }
        | R::GetWorkerContext { .. }
        | R::ListAnswerAgentRuns { .. }
        | R::ListAttachments { .. }
        | R::ListAttentionGroups { .. }
        | R::ListAttentionItems { .. }
        | R::ListAttentionItemsForWorkItem { .. }
        | R::ListChores { .. }
        | R::ListDecisions { .. }
        | R::ListDeferredScopeAttentions { .. }
        | R::ListDependencies { .. }
        | R::ListDependenciesDetailed { .. }
        | R::ListExecutions { .. }
        | R::ListOperatorQuestions { .. }
        | R::ListProductDesignDocs { .. }
        | R::ListProducts
        | R::ListProjects { .. }
        | R::ListProposals { .. }
        | R::ListRevisions { .. }
        | R::ListRuns { .. }
        | R::ListTaskFieldAudit { .. }
        | R::ListTasks { .. } => bulk_read(WorkerAccess::Allow, ProgressionAccess::Allow),

        R::GetAutomation { .. }
        | R::GetAutomationOpenTaskCount { .. }
        | R::GetAutomationState
        | R::GetCiBudget { .. }
        | R::GetCiRemediation { .. }
        | R::GetConflictHotspots { .. }
        | R::GetConflictResolution { .. }
        | R::GetCostWindowReport { .. }
        | R::GetIdea { .. }
        | R::GetSolComparisonReport
        | R::GetStuckReport { .. }
        | R::GetTopCostConsumers { .. }
        | R::GetWorkItemCostReport { .. }
        | R::GitHubAuthStatus
        | R::ListAttentionMerges { .. }
        | R::ListAutomationDedupSuppressions { .. }
        | R::ListAutomationRuns { .. }
        | R::ListAutomationTasks { .. }
        | R::ListAutomations { .. }
        | R::ListCiRemediations { .. }
        | R::ListConflictResolutions { .. }
        | R::ListCoordinatorGuidance { .. }
        | R::ListEditorialActions { .. }
        | R::ListIdeas { .. }
        | R::ListPlannerRuns { .. }
        | R::TrunkStatus => bulk_read(WorkerAccess::Coordinator, ProgressionAccess::Deny),

        R::DebugLiveStatusPipeline
        | R::ExecutionTranscript { .. }
        | R::GetCodexSolPercentage
        | R::GetDispatchConcurrency
        | R::GetDispatchFocus
        | R::GetDispatchState
        | R::GetDriverQuotaUsage { .. }
        | R::GetDriverTrafficSplit
        | R::GetHost { .. }
        | R::GetOffsiteBackup
        | R::GetReviewGuideContent { .. }
        | R::GetReviewGuideSummary { .. }
        | R::GetSelectedProduct
        | R::GetSettings
        | R::GetTaskRuntime { .. }
        | R::ListAttachmentsForWorkItem { .. }
        | R::ListEngineAttempts { .. }
        | R::ListFeatureFlags
        | R::ListHosts
        | R::ListLiveStatusDisabledSlots
        | R::TailRunTranscript { .. } => bulk_read(WorkerAccess::Runtime, ProgressionAccess::Deny),

        R::SubmitProposal { .. } => ordered_replayable(WorkerAccess::Allow, ProgressionAccess::Allow),

        R::MarkCiRemediationSucceededViaRebase { .. }
        | R::SetProjectDesignDoc { .. }
        | R::SetTaskDocPointer { .. }
        | R::SubmitAttachment { .. } => ordered_replayable(WorkerAccess::Allow, ProgressionAccess::Deny),

        R::AbandonCiRemediation { .. }
        | R::AbandonConflictResolution { .. }
        | R::DisableAutomation { .. }
        | R::EnableAutomation { .. }
        | R::ReleaseHoldRun { .. }
        | R::RevokeDecision { .. }
        | R::SetAutomationPaused { .. }
        | R::SetCiBudget { .. }
        | R::SetCoordinatorHandoff { .. } => ordered_replayable(WorkerAccess::Coordinator, ProgressionAccess::Deny),

        R::RetirePane { .. }
        | R::RevealWorkItem { .. }
        | R::SetCodexSolPercentage { .. }
        | R::SetDispatchConcurrency { .. }
        | R::SetDispatchPaused { .. }
        | R::SetDriverTrafficSplit { .. }
        | R::SetFeatureFlag { .. }
        | R::SetHostEnabled { .. }
        | R::SetLiveStatusEnabled { .. }
        | R::SetOffsiteBackup { .. }
        | R::SetSetting { .. }
        | R::StopRun { .. }
        | R::UpdateDispatchFocus { .. } => ordered_replayable(WorkerAccess::Runtime, ProgressionAccess::Deny),

        R::AddDependency { .. }
        | R::ListCriticalPriorityRequests
        | R::RestoreWorkItem { .. }
        | R::SetProductDefaultDriver { .. }
        | R::SetProductDefaultModel { .. }
        | R::SetProductEditorialRules { .. }
        | R::SetProductExternalTracker { .. }
        | R::SetProductMergeMechanism { .. } => ordered_replayable(WorkerAccess::Taxonomy, ProgressionAccess::Deny),

        R::ResolveProjectDesignDoc { .. } => bulk_unreplayable(WorkerAccess::Allow, ProgressionAccess::Allow),

        R::AuditProductEffort { .. } | R::EvaluateEditorialRules { .. } | R::ResolveExecutionLabels { .. } => {
            bulk_unreplayable(WorkerAccess::Coordinator, ProgressionAccess::Deny)
        }

        R::EvaluateDispatchAdmission { .. } => bulk_unreplayable(WorkerAccess::Runtime, ProgressionAccess::Deny),

        R::ClassifyCiRemediation { .. }
        | R::CommentsPostAnswer { .. }
        | R::CommentsRecordGuideOutcome { .. }
        | R::CreateAutomationTask { .. }
        | R::MarkCiRemediationFailed { .. }
        | R::MarkCiRemediationNoop { .. }
        | R::MarkCiRemediationRetriggered { .. }
        | R::MarkConflictResolutionFailed { .. }
        | R::RecordProducerSideConflict { .. } => ordered_unreplayable(WorkerAccess::Allow, ProgressionAccess::Deny),

        R::AnswerOperatorQuestion { .. }
        | R::CommentsCreate { .. }
        | R::CommentsDismiss { .. }
        | R::CommentsPostFollowup { .. }
        | R::CommentsResolve { .. }
        | R::CommentsReviseDoc { .. }
        | R::CommentsSetIntent { .. }
        | R::CommentsSetStatus { .. }
        | R::CommentsUpdateAnchor { .. }
        | R::CreateAutomation { .. }
        | R::CreateDecision { .. }
        | R::CreateIdea { .. }
        | R::DeleteAutomation { .. }
        | R::DeleteIdea { .. }
        | R::EngineResponse { .. }
        | R::GitHubAuthCancel
        | R::GitHubAuthDisconnect
        | R::GitHubAuthStart
        | R::GraduateIdea { .. }
        | R::HoldRun { .. }
        | R::KickPrReconcilers
        | R::MergeWhenReady { .. }
        | R::PlanProject { .. }
        | R::RecreateCoordinator { .. }
        | R::RegisterAppSession
        | R::RegisterCapabilities { .. }
        | R::ReleaseProject { .. }
        | R::ReportSelectedProduct { .. }
        | R::RequestWorkerPaneAttach { .. }
        | R::RetryCiRemediation { .. }
        | R::RetryConflictResolution { .. }
        | R::RunAutomation { .. }
        | R::Shutdown { .. }
        | R::SpawnCapabilityRestored
        | R::StartProjectPostmortem { .. }
        | R::SupersedeDecision { .. }
        | R::SyncProductExternalTracker { .. }
        | R::TriggerPrReview { .. }
        | R::TrunkSetToken { .. }
        | R::UnpopulateProject { .. }
        | R::UpdateAutomation { .. }
        | R::UpdateIdea { .. } => ordered_unreplayable(WorkerAccess::Coordinator, ProgressionAccess::Deny),

        R::CreateAttention { .. } | R::CreateAttentionItem { .. } => ordered_unreplayable(
            WorkerAccess::Redirect(WorkerRedirect::Attention),
            ProgressionAccess::Deny,
        ),

        R::AcceptDeferredScopeAttention { .. } | R::CreateTaskFromDeferredScopeAttention { .. } => {
            ordered_unreplayable(
                WorkerAccess::Redirect(WorkerRedirect::DeferredScope),
                ProgressionAccess::Deny,
            )
        }

        R::RecordEffortEscalation { .. } => ordered_unreplayable(
            WorkerAccess::Redirect(WorkerRedirect::EffortEscalation),
            ProgressionAccess::Deny,
        ),

        R::CreateManyChores { .. } | R::CreateManyTasks { .. } => ordered_unreplayable(
            WorkerAccess::Redirect(WorkerRedirect::FollowupTask),
            ProgressionAccess::Deny,
        ),

        R::AddHost { .. }
        | R::AddHostTag { .. }
        | R::CancelExecution { .. }
        | R::CreateExecution { .. }
        | R::CreateRun { .. }
        | R::FocusWorkerPane { .. }
        | R::GenerateReviewGuide { .. }
        | R::InterruptWorkerPane { .. }
        | R::MetricsReset { .. }
        | R::OpenDocument { .. }
        | R::OpenLiveWorkspaceTerminal { .. }
        | R::OpenReviewTerminal { .. }
        | R::ProbeRun { .. }
        | R::ReapRun { .. }
        | R::ReleaseReviewTerminal { .. }
        | R::RemoveHost { .. }
        | R::RemoveHostTag { .. }
        | R::RequestExecution { .. }
        | R::RetryReviewGuide { .. }
        | R::SendInputToWorker { .. }
        | R::Subscribe { .. }
        | R::Unsubscribe { .. } => ordered_unreplayable(WorkerAccess::Runtime, ProgressionAccess::Deny),

        R::ActionAttentionGroup { .. }
        | R::AnswerAttention { .. }
        | R::CreateProduct { .. }
        | R::CreateProject { .. }
        | R::CreateRevision { .. }
        | R::DeleteWorkItem { .. }
        | R::DismissAttention { .. }
        | R::LinkWorkItemExternalRef { .. }
        | R::MoveWorkItemOnBoard { .. }
        | R::ParkWorkItem { .. }
        | R::RemoveDependency { .. }
        | R::ReorderProjectTasks { .. }
        | R::RequestCriticalPriority { .. }
        | R::ResolveCriticalPriorityRequest { .. }
        | R::UnlinkWorkItemExternalRef { .. } => ordered_unreplayable(WorkerAccess::Taxonomy, ProgressionAccess::Deny),
    }
}

#[cfg(test)]
#[path = "request_contract_tests.rs"]
mod tests;
