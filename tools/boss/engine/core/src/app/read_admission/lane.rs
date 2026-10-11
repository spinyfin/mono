//! Exhaustive classification: new RPCs must explicitly choose an admission lane.
use boss_protocol::FrontendRequest as R;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ReadLane {
    Bulk,
    Live,
    Ordered,
}
// Classification and inventory share one definition; the audit is independent.
macro_rules! define_lanes {
    ($($live:ident $( { $($fields:tt)* } )?),* $(,)?) => {
        #[cfg(test)]
        pub(super) const LIVE_VARIANTS: &[&str] = &[$(stringify!($live)),*];
pub(in crate::app) fn read_lane(request: &R) -> ReadLane {
    match request {
        R::AuditProductEffort { .. }
        | R::ResolveProjectDesignDoc { .. }
        | R::CommentsBannerState { .. }
        | R::CommentsGet { .. }
        | R::CommentsList { .. }
        | R::DebugLiveStatusPipeline
        | R::EvaluateDispatchAdmission { .. }
        | R::EvaluateEditorialRules { .. }
        | R::ExecutionTranscript { .. }
        | R::FindWorkItemsByPr { .. }
        | R::GetAttentionGroup { .. }
        | R::GetAttentionItem { .. }
        | R::GetAutomation { .. }
        | R::GetAutomationOpenTaskCount { .. }
        | R::GetAutomationState
        | R::GetCiBudget { .. }
        | R::GetCiRemediation { .. }
        | R::GetConflictHotspots { .. }
        | R::GetConflictResolution { .. }
        | R::GetCoordinatorHandoff
        | R::GetCostWindowReport { .. }
        | R::GetDecision { .. }
        | R::GetDispatchConcurrency
        | R::GetDispatchState
        | R::GetDriverQuotaUsage { .. }
        | R::GetDriverTrafficSplit
        | R::GetExecution { .. }
        | R::GetHost { .. }
        | R::GetIdea { .. }
        | R::GetPrBody { .. }
        | R::GetProductDesignDoc { .. }
        | R::GetPrStatus { refresh: false, .. }
        | R::GetReviewGuideContent { .. }
        | R::GetReviewGuideSummary { .. }
        | R::GetRun { .. }
        | R::GetSelectedProduct
        | R::GetSettings
        | R::GetTaskRuntime { .. }
        | R::GetTopCostConsumers { .. }
        | R::GetWorkerContext { .. }
        | R::GetWorkItem { .. }
        | R::GetWorkItemByShortId { .. }
        | R::GetWorkItemCostReport { .. }
        | R::GetWorkTree { .. }
        | R::ListAnswerAgentRuns { .. }
        | R::ListAttachments { .. }
        | R::ListAttachmentsForWorkItem { .. }
        | R::ListAttentionGroups { .. }
        | R::ListAttentionItems { .. }
        | R::ListAttentionItemsForWorkItem { .. }
        | R::ListAttentionMerges { .. }
        | R::ListAutomationDedupSuppressions { .. }
        | R::ListAutomationRuns { .. }
        | R::ListAutomations { .. }
        | R::ListAutomationTasks { .. }
        | R::ListChores { .. }
        | R::ListCiRemediations { .. }
        | R::ListConflictResolutions { .. }
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
        | R::ListPlannerRuns { .. }
        | R::ListProductDesignDocs { .. }
        | R::ListProducts
        | R::ListProjects { .. }
        | R::ListProposals { .. }
        | R::ListRevisions { .. }
        | R::ListRuns { .. }
        | R::ListTasks { .. }
        | R::TailRunTranscript { .. }
        | R::GitHubAuthStatus
        | R::TrunkStatus
        | R::ListOperatorQuestions { .. }
        | R::ListCoordinatorGuidance { .. } => ReadLane::Bulk,
        $(R::$live $( { $($fields)* } )?)|* => ReadLane::Live,
        R::AbandonCiRemediation { .. }
        | R::AbandonConflictResolution { .. }
        | R::AcceptDeferredScopeAttention { .. }
        | R::ActionAttentionGroup { .. }
        | R::AddDependency { .. }
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
        | R::CreateChore { .. }
        | R::CreateDecision { .. }
        | R::CreateExecution { .. }
        | R::CreateIdea { .. }
        | R::CreateInvestigation { .. }
        | R::CreateManyChores { .. }
        | R::CreateManyTasks { .. }
        | R::CreateProduct { .. }
        | R::CreateProject { .. }
        | R::CreateRevision { .. }
        | R::CreateRun { .. }
        | R::CreateTask { .. }
        | R::CreateTaskFromDeferredScopeAttention { .. }
        | R::DeleteAutomation { .. }
        | R::DeleteIdea { .. }
        | R::DeleteWorkItem { .. }
        | R::DisableAutomation { .. }
        | R::DismissAttention { .. }
        | R::EnableAutomation { .. }
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
        | R::MarkCiRemediationSucceededViaRebase { .. }
        | R::MarkConflictResolutionFailed { .. }
        | R::MergeWhenReady { .. }
        | R::MetricsReset { .. }
        | R::MoveWorkItemOnBoard { .. }
        | R::OpenDocument { .. }
        | R::OpenLiveWorkspaceTerminal { .. }
        | R::OpenReviewTerminal { .. }
        | R::PlanProject { .. }
        | R::ProbeRun { .. }
        | R::ReapRun { .. }
        | R::RecordEffortEscalation { .. }
        | R::RecordProducerSideConflict { .. }
        | R::RecreateCoordinator { .. }
        | R::RegisterAppSession
        | R::RegisterCapabilities { .. }
        | R::ReleaseHoldRun { .. }
        | R::ReleaseProject { .. }
        | R::ReleaseReviewTerminal { .. }
        | R::RemoveDependency { .. }
        | R::RemoveHost { .. }
        | R::RemoveHostTag { .. }
        | R::ReorderProjectTasks { .. }
        | R::ReportSelectedProduct { .. }
        | R::RequestExecution { .. }
        | R::RestoreWorkItem { .. }
        | R::RetirePane { .. }
        | R::RetryCiRemediation { .. }
        | R::RetryConflictResolution { .. }
        | R::RetryReviewGuide { .. }
        | R::RevealWorkItem { .. }
        | R::RevokeDecision { .. }
        | R::RunAutomation { .. }
        | R::SendInputToWorker { .. }
        | R::SetAutomationPaused { .. }
        | R::SetCiBudget { .. }
        | R::SetCoordinatorHandoff { .. }
        | R::SetDispatchConcurrency { .. }
        | R::SetDispatchPaused { .. }
        | R::SetDriverTrafficSplit { .. }
        | R::SetFeatureFlag { .. }
        | R::SetHostEnabled { .. }
        | R::SetLiveStatusEnabled { .. }
        | R::SetProductDefaultDriver { .. }
        | R::SetProductDefaultModel { .. }
        | R::SetProductEditorialRules { .. }
        | R::SetProductExternalTracker { .. }
        | R::SetProductMergeMechanism { .. }
        | R::SetProjectDesignDoc { .. }
        | R::SetSetting { .. }
        | R::SetTaskDocPointer { .. }
        | R::Shutdown { .. }
        | R::SpawnCapabilityRestored
        | R::StartProjectPostmortem { .. }
        | R::StopRun { .. }
        | R::SubmitAttachment { .. }
        | R::SubmitProposal { .. }
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
        | R::UpdateWorkItem { .. }
        | R::GetPrStatus { refresh: true, .. } => ReadLane::Ordered,
    }
}
    };
}
define_lanes! {
    GetEngineHealth,
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
