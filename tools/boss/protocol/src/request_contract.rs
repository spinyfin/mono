//! The per-request replay and admission contract.
//!
//! Every [`FrontendRequest`] has two per-request properties that the client
//! and the engine consume independently: whether a request whose outcome is
//! unknown may be sent again (replay), and which engine admission lane it
//! uses. [`request_contract`] answers both for each request.
//!
//! [`request_contract`] is a single exhaustive `match` with no catch-all, so a
//! new `FrontendRequest` variant does not compile until someone deliberately
//! chooses both answers for it. Requests whose answers depend on their payload
//! (`force_duplicate`, `redeliver`, `refresh`) are classified by the branch
//! that applies. This crate only states the contract; the client turns
//! [`ReplayContract`] into replay windows and the engine turns
//! [`AdmissionLane`] into queueing behavior, and neither is imported here.
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
//!   local cube socket. `LIVE_VARIANTS` (test-only) is generated from the same
//!   macro list the match uses; the tests compare it with an independently
//!   written audit list.
//! * [`AdmissionLane::Ordered`]: everything else, handled in request order and
//!   never subject to read admission. `GetPrStatus` is `Bulk` unless `refresh`
//!   is set, since a refresh probe must not hold a bulk slot.

use crate::FrontendRequest;
use crate::FrontendRequest as R;

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

/// Everything the client and the engine need to know about a request before
/// they handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestContract {
    pub replay: ReplayContract,
    pub admission: AdmissionLane,
}

impl RequestContract {
    const fn new(replay: ReplayContract, admission: AdmissionLane) -> Self {
        Self { replay, admission }
    }
}

const BULK_READ: RequestContract = RequestContract::new(ReplayContract::Always, AdmissionLane::Bulk);
const ORDERED_REPLAYABLE: RequestContract = RequestContract::new(ReplayContract::Always, AdmissionLane::Ordered);
const GUARDED_CREATE: RequestContract = RequestContract::new(ReplayContract::DuplicateGuarded, AdmissionLane::Ordered);
const BULK_UNREPLAYABLE: RequestContract = RequestContract::new(ReplayContract::Never, AdmissionLane::Bulk);
const ORDERED_UNREPLAYABLE: RequestContract = RequestContract::new(ReplayContract::Never, AdmissionLane::Ordered);

// The Live variants are listed once; the macros below expand that list into the
// `request_contract` pattern and the test-only audit inventory, so `Live` can
// only come from here. rustfmt does not format macro bodies, so keep this list
// (and only this list) in the macro; `request_contract` itself stays an
// ordinary, formatted function.
macro_rules! with_live_variants {
    ($callback:ident) => {
        $callback! {
            GetEngineHealth,
            GetEnginePoolConfig,
            GetEngineVersion,
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
pub(crate) const LIVE_VARIANTS: &[&str] = with_live_variants!(live_names);

fn guarded_create(force_duplicate: bool) -> RequestContract {
    if force_duplicate {
        ORDERED_UNREPLAYABLE
    } else {
        GUARDED_CREATE
    }
}

/// The replay and admission contract of `request`; see the module docs.
pub fn request_contract(request: &FrontendRequest) -> RequestContract {
    match request {
        // Payload-dependent: the duplicate guard is off under `force_duplicate`.
        R::CreateTask { input } => guarded_create(input.force_duplicate),
        R::CreateChore { input } => guarded_create(input.force_duplicate),
        R::CreateInvestigation { input } => guarded_create(input.force_duplicate),
        // Payload-dependent: a redelivery re-pushes the handoff.
        R::GetCoordinatorHandoff { redeliver } => {
            if *redeliver {
                BULK_UNREPLAYABLE
            } else {
                BULK_READ
            }
        }
        // Payload-dependent: a refresh probe must not hold a bulk slot.
        R::GetPrStatus { refresh, .. } => {
            if *refresh {
                ORDERED_REPLAYABLE
            } else {
                BULK_READ
            }
        }

        with_live_variants!(live_pattern) => RequestContract::new(ReplayContract::Always, AdmissionLane::Live),

        R::CommentsBannerState { .. }
        | R::CommentsGet { .. }
        | R::CommentsList { .. }
        | R::DebugLiveStatusPipeline
        | R::ExecutionTranscript { .. }
        | R::FindWorkItemsByPr { .. }
        | R::GetAttentionGroup { .. }
        | R::GetAttentionItem { .. }
        | R::GetAutomation { .. }
        | R::GetAutomationOpenTaskCount { .. }
        | R::GetAutomationState
        | R::GetCiBudget { .. }
        | R::GetCiRemediation { .. }
        | R::GetCodexSolPercentage
        | R::GetConflictHotspots { .. }
        | R::GetConflictResolution { .. }
        | R::GetCostWindowReport { .. }
        | R::GetDecision { .. }
        | R::GetDispatchConcurrency
        | R::GetDispatchFocus
        | R::GetDispatchState
        | R::GetDriverQuotaUsage { .. }
        | R::GetDriverTrafficSplit
        | R::GetExecution { .. }
        | R::GetHost { .. }
        | R::GetIdea { .. }
        | R::GetOffsiteBackup
        | R::GetPrBody { .. }
        | R::GetProductDesignDoc { .. }
        | R::GetReviewGuideContent { .. }
        | R::GetReviewGuideSummary { .. }
        | R::GetRun { .. }
        | R::GetSelectedProduct
        | R::GetSettings
        | R::GetSolComparisonReport
        | R::GetStuckReport { .. }
        | R::GetTaskRuntime { .. }
        | R::GetTopCostConsumers { .. }
        | R::GetWorkItem { .. }
        | R::GetWorkItemByShortId { .. }
        | R::GetWorkItemCostReport { .. }
        | R::GetWorkRuntimePatch { .. }
        | R::GetWorkTree { .. }
        | R::GetWorkerContext { .. }
        | R::GitHubAuthStatus
        | R::ListAnswerAgentRuns { .. }
        | R::ListAttachments { .. }
        | R::ListAttachmentsForWorkItem { .. }
        | R::ListAttentionGroups { .. }
        | R::ListAttentionItems { .. }
        | R::ListAttentionItemsForWorkItem { .. }
        | R::ListAttentionMerges { .. }
        | R::ListAutomationDedupSuppressions { .. }
        | R::ListAutomationRuns { .. }
        | R::ListAutomationTasks { .. }
        | R::ListAutomations { .. }
        | R::ListChores { .. }
        | R::ListCiRemediations { .. }
        | R::ListConflictResolutions { .. }
        | R::ListCoordinatorGuidance { .. }
        | R::ListDecisions { .. }
        | R::ListDeferredScopeAttentions { .. }
        | R::ListDependencies { .. }
        | R::ListDependenciesDetailed { .. }
        | R::ListEditorialActions { .. }
        | R::ListEngineAttempts { .. }
        | R::ListExecutions { .. }
        | R::ListFeatureFlags
        | R::ListHosts
        | R::ListIdeas { .. }
        | R::ListLiveStatusDisabledSlots
        | R::ListOperatorQuestions { .. }
        | R::ListPlannerRuns { .. }
        | R::ListProductDesignDocs { .. }
        | R::ListProducts
        | R::ListProjects { .. }
        | R::ListProposals { .. }
        | R::ListRevisions { .. }
        | R::ListRuns { .. }
        | R::ListTaskFieldAudit { .. }
        | R::ListTasks { .. }
        | R::TailRunTranscript { .. }
        | R::TrunkStatus => BULK_READ,

        R::AuditProductEffort { .. }
        | R::EvaluateDispatchAdmission { .. }
        | R::EvaluateEditorialRules { .. }
        | R::ResolveExecutionLabels { .. }
        | R::ResolveProjectDesignDoc { .. } => BULK_UNREPLAYABLE,

        R::AbandonCiRemediation { .. }
        | R::AbandonConflictResolution { .. }
        | R::AddDependency { .. }
        | R::DisableAutomation { .. }
        | R::EnableAutomation { .. }
        | R::ListCriticalPriorityRequests
        | R::MarkCiRemediationSucceededViaRebase { .. }
        | R::ReleaseHoldRun { .. }
        | R::RestoreWorkItem { .. }
        | R::RetirePane { .. }
        | R::RevealWorkItem { .. }
        | R::RevokeDecision { .. }
        | R::SetAutomationPaused { .. }
        | R::SetCiBudget { .. }
        | R::SetCodexSolPercentage { .. }
        | R::SetCoordinatorHandoff { .. }
        | R::SetDispatchConcurrency { .. }
        | R::SetDispatchPaused { .. }
        | R::SetDriverTrafficSplit { .. }
        | R::SetFeatureFlag { .. }
        | R::SetHostEnabled { .. }
        | R::SetLiveStatusEnabled { .. }
        | R::SetOffsiteBackup { .. }
        | R::SetProductDefaultDriver { .. }
        | R::SetProductDefaultModel { .. }
        | R::SetProductEditorialRules { .. }
        | R::SetProductExternalTracker { .. }
        | R::SetProductMergeMechanism { .. }
        | R::SetProjectDesignDoc { .. }
        | R::SetSetting { .. }
        | R::SetTaskDocPointer { .. }
        | R::StopRun { .. }
        | R::SubmitAttachment { .. }
        | R::SubmitProposal { .. }
        | R::UpdateDispatchFocus { .. } => ORDERED_REPLAYABLE,

        R::AcceptDeferredScopeAttention { .. }
        | R::ActionAttentionGroup { .. }
        | R::AddHost { .. }
        | R::AddHostTag { .. }
        | R::AnswerAttention { .. }
        | R::AnswerOperatorQuestion { .. }
        | R::CancelExecution { .. }
        | R::ClassifyCiRemediation { .. }
        | R::CommentsCreate { .. }
        | R::CommentsDismiss { .. }
        | R::CommentsPostAnswer { .. }
        | R::CommentsPostFollowup { .. }
        | R::CommentsRecordGuideOutcome { .. }
        | R::CommentsResolve { .. }
        | R::CommentsReviseDoc { .. }
        | R::CommentsSetIntent { .. }
        | R::CommentsSetStatus { .. }
        | R::CommentsUpdateAnchor { .. }
        | R::CreateAttention { .. }
        | R::CreateAttentionItem { .. }
        | R::CreateAutomation { .. }
        | R::CreateAutomationTask { .. }
        | R::CreateDecision { .. }
        | R::CreateExecution { .. }
        | R::CreateIdea { .. }
        | R::CreateManyChores { .. }
        | R::CreateManyTasks { .. }
        | R::CreateProduct { .. }
        | R::CreateProject { .. }
        | R::CreateRevision { .. }
        | R::CreateRun { .. }
        | R::CreateTaskFromDeferredScopeAttention { .. }
        | R::DeleteAutomation { .. }
        | R::DeleteIdea { .. }
        | R::DeleteWorkItem { .. }
        | R::DismissAttention { .. }
        | R::EngineResponse { .. }
        | R::FocusWorkerPane { .. }
        | R::GenerateReviewGuide { .. }
        | R::GitHubAuthCancel
        | R::GitHubAuthDisconnect
        | R::GitHubAuthStart
        | R::GraduateIdea { .. }
        | R::HoldRun { .. }
        | R::InterruptWorkerPane { .. }
        | R::KickPrReconcilers
        | R::LinkWorkItemExternalRef { .. }
        | R::MarkCiRemediationFailed { .. }
        | R::MarkCiRemediationNoop { .. }
        | R::MarkCiRemediationRetriggered { .. }
        | R::MarkConflictResolutionFailed { .. }
        | R::MergeWhenReady { .. }
        | R::MetricsReset { .. }
        | R::MoveWorkItemOnBoard { .. }
        | R::OpenDocument { .. }
        | R::OpenLiveWorkspaceTerminal { .. }
        | R::OpenReviewTerminal { .. }
        | R::ParkWorkItem { .. }
        | R::PlanProject { .. }
        | R::ProbeRun { .. }
        | R::ReapRun { .. }
        | R::RecordEffortEscalation { .. }
        | R::RecordProducerSideConflict { .. }
        | R::RecreateCoordinator { .. }
        | R::RegisterAppSession
        | R::RegisterCapabilities { .. }
        | R::ReleaseProject { .. }
        | R::ReleaseReviewTerminal { .. }
        | R::RemoveDependency { .. }
        | R::RemoveHost { .. }
        | R::RemoveHostTag { .. }
        | R::ReorderProjectTasks { .. }
        | R::ReportSelectedProduct { .. }
        | R::RequestCriticalPriority { .. }
        | R::RequestExecution { .. }
        | R::RequestWorkerPaneAttach { .. }
        | R::ResolveCriticalPriorityRequest { .. }
        | R::RetryCiRemediation { .. }
        | R::RetryConflictResolution { .. }
        | R::RetryReviewGuide { .. }
        | R::RunAutomation { .. }
        | R::SendInputToWorker { .. }
        | R::Shutdown { .. }
        | R::SpawnCapabilityRestored
        | R::StartProjectPostmortem { .. }
        | R::Subscribe { .. }
        | R::SupersedeDecision { .. }
        | R::SyncProductExternalTracker { .. }
        | R::TriggerPrReview { .. }
        | R::TrunkSetToken { .. }
        | R::UnlinkWorkItemExternalRef { .. }
        | R::UnpopulateProject { .. }
        | R::Unsubscribe { .. }
        | R::UpdateAutomation { .. }
        | R::UpdateIdea { .. }
        | R::UpdateWorkItem { .. } => ORDERED_UNREPLAYABLE,
    }
}

#[cfg(test)]
#[path = "request_contract_tests.rs"]
mod tests;
