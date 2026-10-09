//! Which requests may be sent a second time after the connection dropped.
//!
//! A connect-phase failure means nothing was sent, so *every* request is
//! safe to retry then. A failed write is the same only when zero bytes
//! reached the socket; a partial write or a failed flush may have delivered
//! the request, and is treated like a drop after send. The hard case is a
//! drop **after** (or possibly after) the request was written: the engine may have applied it before dying, and sending it
//! again could apply it twice. [`replay_safety`] decides, per request, whether
//! that second send is safe. The classification is an exhaustive `match`
//! with no catch-all, so a new `FrontendRequest` variant does not compile
//! until someone deliberately classifies it here; a variant a reviewer
//! cannot show to be harmless to repeat belongs under
//! [`ReplaySafety::Never`] (“outcome unknown, check state”).
//!
//! ## The classes
//!
//! * **Always** — a second send is harmless.
//!   * *Reads*: every `Get*` and `List*` request, plus the few read-only
//!     verbs that do not follow that naming (`CommentsGet`, `CommentsList`,
//!     `CommentsBannerState`, `TailRunTranscript`, `ExecutionTranscript`,
//!     `FindWorkItemsByPr`, `WorkerPoolSummary`, `WorkspacePoolSummary`,
//!     `TrunkStatus`, `GitHubAuthStatus`, `ProbeStatus`, `MetricsListLive`,
//!     `MetricsShowLive`, `DebugLiveStatusPipeline`).
//!   * *Declarative setters*: every `Set*` request writes an absolute value,
//!     so repeating it leaves the same state.
//!   * *Documented idempotent writes* (each is called out as idempotent in
//!     its `FrontendRequest` doc comment): `AbandonCiRemediation`,
//!     `AbandonConflictResolution`, `AddDependency`, `DisableAutomation`,
//!     `EnableAutomation`, `MarkCiRemediationSucceededViaRebase`,
//!     `ReleaseHoldRun`, `RestoreWorkItem`, `RetirePane`, `RevealWorkItem`,
//!     `RevokeDecision`, `StopRun`.
//!   * *Keyed by an engine-side idempotency key*: `SubmitProposal` (unique on
//!     `(execution_id, idempotency_key)`, and the engine derives a key from
//!     the payload when the caller gives none) and `SubmitAttachment` (unique
//!     on `(execution_id, content_digest)`). A replay returns the original
//!     row instead of making a second one.
//! * **Within(window)** — protected by the engine's duplicate-create guard,
//!   which refuses a same-named task/chore/investigation in the same product
//!   created in the last [`DUPLICATE_GUARD_WINDOW`]. A replay inside the
//!   window is refused instead of creating a twin, and the client turns that
//!   refusal into the existing item (see `BossClient::send_request`). The
//!   guard only looks back that far, so a replay after a long outage could
//!   miss an original that *was* created; the client therefore only replays
//!   while the original send is still inside the window (minus a safety
//!   margin), re-checks that at the moment of each resend, caps the reconnect
//!   wait to the remaining window, and otherwise reports “outcome unknown”.
//!   Applies to `CreateTask`, `CreateChore` and `CreateInvestigation`,
//!   and only when `force_duplicate` is false — with it set the guard is off.
//! * **Never** — everything else: creates without a guard (`CreateProject`,
//!   `CreateProduct`, `CreateRevision`, the `CreateMany*` batches,
//!   `CreateExecution`, `RequestExecution`, …), state transitions
//!   (`UpdateWorkItem`, `MoveWorkItemOnBoard`, `Mark*Failed`, …), fan-out
//!   commands (`SendInputToWorker`, `RunAutomation`), `Shutdown`, and any
//!   variant not listed above. A drop after send surfaces as
//!   [`OutcomeUnknown`](crate::OutcomeUnknown) and is never retried.

use std::time::Duration;

use boss_protocol::FrontendRequest;

/// Mirror of the engine's `DUPLICATE_GUARD_WINDOW_SECS`
/// (`engine/core/src/work.rs`). Duplicated rather than imported so the CLI
/// does not depend on the engine crate; the engine constant is the source of
/// truth and this one is deliberately conservative to stay valid if it grows.
pub const DUPLICATE_GUARD_WINDOW: Duration = Duration::from_secs(60);
/// Replay a guarded create only while at least this fraction (1/N) of the
/// guard window remains, so clock skew and the reconnect itself cannot push
/// the replay past the guard's horizon (10s of the default 60s).
const DUPLICATE_GUARD_MARGIN_DIVISOR: u32 = 6;

/// Whether a request that was already sent may be sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySafety {
    /// Safe to resend at any time.
    Always,
    /// Safe to resend only if no more than this long has passed since the
    /// original send (a bounded engine-side duplicate guard).
    Within(Duration),
    /// Never resend; report the outcome as unknown.
    Never,
}

impl ReplaySafety {
    /// May the request be resent when `since_first_send` has elapsed since it
    /// was first written to the engine?
    pub fn allows_replay_after(self, since_first_send: Duration) -> bool {
        match self {
            Self::Always => true,
            Self::Within(window) => since_first_send < window,
            Self::Never => false,
        }
    }
}

/// The wire name of a request (its serde `type` tag, e.g. `submit_proposal`).
pub fn request_name(request: &FrontendRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("type").and_then(|tag| tag.as_str().map(str::to_owned)))
        .unwrap_or_else(|| "<unknown request>".to_owned())
}

/// Classify `request`; see the module docs for the full table.
pub fn replay_safety(request: &FrontendRequest) -> ReplaySafety {
    replay_safety_with(request, DUPLICATE_GUARD_WINDOW)
}

/// [`replay_safety`] for an engine whose duplicate guard looks back
/// `guard_window` (injectable so tests can shrink it).
pub fn replay_safety_with(request: &FrontendRequest, guard_window: Duration) -> ReplaySafety {
    let guarded = ReplaySafety::Within(guard_window - guard_window / DUPLICATE_GUARD_MARGIN_DIVISOR);
    match request {
        FrontendRequest::CreateTask { input } if !input.force_duplicate => guarded,
        FrontendRequest::CreateChore { input } if !input.force_duplicate => guarded,
        FrontendRequest::CreateInvestigation { input } if !input.force_duplicate => guarded,

        FrontendRequest::AbandonCiRemediation { .. }
        | FrontendRequest::AbandonConflictResolution { .. }
        | FrontendRequest::AddDependency { .. }
        | FrontendRequest::CommentsBannerState { .. }
        | FrontendRequest::CommentsGet { .. }
        | FrontendRequest::CommentsList { .. }
        | FrontendRequest::DebugLiveStatusPipeline
        | FrontendRequest::DisableAutomation { .. }
        | FrontendRequest::EnableAutomation { .. }
        | FrontendRequest::ExecutionTranscript { .. }
        | FrontendRequest::FindWorkItemsByPr { .. }
        | FrontendRequest::GetAttentionGroup { .. }
        | FrontendRequest::GetAttentionItem { .. }
        | FrontendRequest::GetAutomation { .. }
        | FrontendRequest::GetAutomationOpenTaskCount { .. }
        | FrontendRequest::GetAutomationState
        | FrontendRequest::GetCiBudget { .. }
        | FrontendRequest::GetCiRemediation { .. }
        | FrontendRequest::GetConflictHotspots { .. }
        | FrontendRequest::GetConflictResolution { .. }
        | FrontendRequest::GetCoordinatorHandoff
        | FrontendRequest::GetCostWindowReport { .. }
        | FrontendRequest::GetDecision { .. }
        | FrontendRequest::GetDispatchConcurrency
        | FrontendRequest::GetDispatchState
        | FrontendRequest::GetDriverQuotaUsage { .. }
        | FrontendRequest::GetDriverTrafficSplit
        | FrontendRequest::GetEngineHealth
        | FrontendRequest::GetEngineVersion
        | FrontendRequest::GetExecution { .. }
        | FrontendRequest::GetHost { .. }
        | FrontendRequest::GetIdea { .. }
        | FrontendRequest::GetPrBody { .. }
        | FrontendRequest::GetProductDesignDoc { .. }
        | FrontendRequest::GetPrStatus { .. }
        | FrontendRequest::GetReviewGuideContent { .. }
        | FrontendRequest::GetReviewGuideSummary { .. }
        | FrontendRequest::GetRun { .. }
        | FrontendRequest::GetSelectedProduct
        | FrontendRequest::GetSettings
        | FrontendRequest::GetTaskRuntime { .. }
        | FrontendRequest::GetTopCostConsumers { .. }
        | FrontendRequest::GetWorkerContext { .. }
        | FrontendRequest::GetWorkItem { .. }
        | FrontendRequest::GetWorkItemByShortId { .. }
        | FrontendRequest::GetWorkItemCostReport { .. }
        | FrontendRequest::GetWorkTree { .. }
        | FrontendRequest::GitHubAuthStatus
        | FrontendRequest::ListAnswerAgentRuns { .. }
        | FrontendRequest::ListAttachments { .. }
        | FrontendRequest::ListAttachmentsForWorkItem { .. }
        | FrontendRequest::ListAttentionGroups { .. }
        | FrontendRequest::ListAttentionItems { .. }
        | FrontendRequest::ListAttentionItemsForWorkItem { .. }
        | FrontendRequest::ListAttentionMerges { .. }
        | FrontendRequest::ListAutomationDedupSuppressions { .. }
        | FrontendRequest::ListAutomationRuns { .. }
        | FrontendRequest::ListAutomations { .. }
        | FrontendRequest::ListAutomationTasks { .. }
        | FrontendRequest::ListChores { .. }
        | FrontendRequest::ListCiRemediations { .. }
        | FrontendRequest::ListConflictResolutions { .. }
        | FrontendRequest::ListDecisions { .. }
        | FrontendRequest::ListDeferredScopeAttentions { .. }
        | FrontendRequest::ListDependencies { .. }
        | FrontendRequest::ListDependenciesDetailed { .. }
        | FrontendRequest::ListEditorialActions { .. }
        | FrontendRequest::ListEngineAttempts { .. }
        | FrontendRequest::ListExecutions { .. }
        | FrontendRequest::ListFeatureFlags
        | FrontendRequest::ListHostedPaneStatuses
        | FrontendRequest::ListHosts
        | FrontendRequest::ListIdeas { .. }
        | FrontendRequest::ListLiveStatusDisabledSlots
        | FrontendRequest::ListOperatorQuestions { .. }
        | FrontendRequest::ListPlannerRuns { .. }
        | FrontendRequest::ListProductDesignDocs { .. }
        | FrontendRequest::ListProducts
        | FrontendRequest::ListProjects { .. }
        | FrontendRequest::ListProposals { .. }
        | FrontendRequest::ListRevisions { .. }
        | FrontendRequest::ListRuns { .. }
        | FrontendRequest::ListTasks { .. }
        | FrontendRequest::ListTmuxWorkerStatuses
        | FrontendRequest::ListWorkerLiveStates
        | FrontendRequest::MarkCiRemediationSucceededViaRebase { .. }
        | FrontendRequest::MetricsListLive
        | FrontendRequest::MetricsShowLive { .. }
        | FrontendRequest::ProbeStatus { .. }
        | FrontendRequest::ReleaseHoldRun { .. }
        | FrontendRequest::RestoreWorkItem { .. }
        | FrontendRequest::RetirePane { .. }
        | FrontendRequest::RevealWorkItem { .. }
        | FrontendRequest::RevokeDecision { .. }
        | FrontendRequest::SetAutomationPaused { .. }
        | FrontendRequest::SetCiBudget { .. }
        | FrontendRequest::SetCoordinatorHandoff { .. }
        | FrontendRequest::SetDispatchConcurrency { .. }
        | FrontendRequest::SetDispatchPaused { .. }
        | FrontendRequest::SetDriverTrafficSplit { .. }
        | FrontendRequest::SetFeatureFlag { .. }
        | FrontendRequest::SetHostEnabled { .. }
        | FrontendRequest::SetLiveStatusEnabled { .. }
        | FrontendRequest::SetProductDefaultDriver { .. }
        | FrontendRequest::SetProductDefaultModel { .. }
        | FrontendRequest::SetProductEditorialRules { .. }
        | FrontendRequest::SetProductExternalTracker { .. }
        | FrontendRequest::SetProductMergeMechanism { .. }
        | FrontendRequest::SetProjectDesignDoc { .. }
        | FrontendRequest::SetSetting { .. }
        | FrontendRequest::SetTaskDocPointer { .. }
        | FrontendRequest::StopRun { .. }
        | FrontendRequest::SubmitAttachment { .. }
        | FrontendRequest::SubmitProposal { .. }
        | FrontendRequest::TailRunTranscript { .. }
        | FrontendRequest::TrunkStatus
        | FrontendRequest::WorkerPoolSummary
        | FrontendRequest::WorkspacePoolSummary => ReplaySafety::Always,

        FrontendRequest::AcceptDeferredScopeAttention { .. }
        | FrontendRequest::ActionAttentionGroup { .. }
        | FrontendRequest::AddHost { .. }
        | FrontendRequest::AddHostTag { .. }
        | FrontendRequest::AnswerAttention { .. }
        | FrontendRequest::AnswerOperatorQuestion { .. }
        | FrontendRequest::AuditProductEffort { .. }
        | FrontendRequest::CancelExecution { .. }
        | FrontendRequest::ClassifyCiRemediation { .. }
        | FrontendRequest::CommentsCreate { .. }
        | FrontendRequest::CommentsDismiss { .. }
        | FrontendRequest::CommentsPostAnswer { .. }
        | FrontendRequest::CommentsPostFollowup { .. }
        | FrontendRequest::CommentsRecordGuideOutcome { .. }
        | FrontendRequest::CommentsResolve { .. }
        | FrontendRequest::CommentsReviseDoc { .. }
        | FrontendRequest::CommentsSetIntent { .. }
        | FrontendRequest::CommentsSetStatus { .. }
        | FrontendRequest::CommentsUpdateAnchor { .. }
        | FrontendRequest::CreateAttention { .. }
        | FrontendRequest::CreateAttentionItem { .. }
        | FrontendRequest::CreateAutomation { .. }
        | FrontendRequest::CreateAutomationTask { .. }
        | FrontendRequest::CreateChore { .. }
        | FrontendRequest::CreateDecision { .. }
        | FrontendRequest::CreateExecution { .. }
        | FrontendRequest::CreateIdea { .. }
        | FrontendRequest::CreateInvestigation { .. }
        | FrontendRequest::CreateManyChores { .. }
        | FrontendRequest::CreateManyTasks { .. }
        | FrontendRequest::CreateProduct { .. }
        | FrontendRequest::CreateProject { .. }
        | FrontendRequest::CreateRevision { .. }
        | FrontendRequest::CreateRun { .. }
        | FrontendRequest::CreateTask { .. }
        | FrontendRequest::CreateTaskFromDeferredScopeAttention { .. }
        | FrontendRequest::DeleteAutomation { .. }
        | FrontendRequest::DeleteIdea { .. }
        | FrontendRequest::DeleteWorkItem { .. }
        | FrontendRequest::DismissAttention { .. }
        | FrontendRequest::EngineResponse { .. }
        | FrontendRequest::EvaluateDispatchAdmission { .. }
        | FrontendRequest::EvaluateEditorialRules { .. }
        | FrontendRequest::FocusWorkerPane { .. }
        | FrontendRequest::GenerateReviewGuide { .. }
        | FrontendRequest::GitHubAuthCancel
        | FrontendRequest::GitHubAuthDisconnect
        | FrontendRequest::GitHubAuthStart
        | FrontendRequest::GraduateIdea { .. }
        | FrontendRequest::HoldRun { .. }
        | FrontendRequest::InterruptWorkerPane { .. }
        | FrontendRequest::KickPrReconcilers
        | FrontendRequest::LinkWorkItemExternalRef { .. }
        | FrontendRequest::MarkCiRemediationFailed { .. }
        | FrontendRequest::MarkCiRemediationNoop { .. }
        | FrontendRequest::MarkCiRemediationRetriggered { .. }
        | FrontendRequest::MarkConflictResolutionFailed { .. }
        | FrontendRequest::MergeWhenReady { .. }
        | FrontendRequest::MetricsReset { .. }
        | FrontendRequest::MoveWorkItemOnBoard { .. }
        | FrontendRequest::OpenDocument { .. }
        | FrontendRequest::OpenLiveWorkspaceTerminal { .. }
        | FrontendRequest::OpenReviewTerminal { .. }
        | FrontendRequest::PlanProject { .. }
        | FrontendRequest::ProbeRun { .. }
        | FrontendRequest::ReapRun { .. }
        | FrontendRequest::RecordEffortEscalation { .. }
        | FrontendRequest::RecordProducerSideConflict { .. }
        | FrontendRequest::RecreateCoordinator { .. }
        | FrontendRequest::RegisterAppSession
        | FrontendRequest::RegisterCapabilities { .. }
        | FrontendRequest::ReleaseProject { .. }
        | FrontendRequest::ReleaseReviewTerminal { .. }
        | FrontendRequest::RemoveDependency { .. }
        | FrontendRequest::RemoveHost { .. }
        | FrontendRequest::RemoveHostTag { .. }
        | FrontendRequest::ReorderProjectTasks { .. }
        | FrontendRequest::ReportSelectedProduct { .. }
        | FrontendRequest::RequestExecution { .. }
        | FrontendRequest::ResolveProjectDesignDoc { .. }
        | FrontendRequest::RetryCiRemediation { .. }
        | FrontendRequest::RetryConflictResolution { .. }
        | FrontendRequest::RetryReviewGuide { .. }
        | FrontendRequest::RunAutomation { .. }
        | FrontendRequest::SendInputToWorker { .. }
        | FrontendRequest::Shutdown { .. }
        | FrontendRequest::SpawnCapabilityRestored
        | FrontendRequest::StartProjectPostmortem { .. }
        | FrontendRequest::Subscribe { .. }
        | FrontendRequest::SupersedeDecision { .. }
        | FrontendRequest::SyncProductExternalTracker { .. }
        | FrontendRequest::TriggerPrReview { .. }
        | FrontendRequest::TrunkSetToken { .. }
        | FrontendRequest::UnlinkWorkItemExternalRef { .. }
        | FrontendRequest::UnpopulateProject { .. }
        | FrontendRequest::Unsubscribe { .. }
        | FrontendRequest::UpdateAutomation { .. }
        | FrontendRequest::UpdateIdea { .. }
        | FrontendRequest::UpdateWorkItem { .. } => ReplaySafety::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{CreateChoreInput, CreateProjectInput, CreateTaskInput, ProposalKind};

    fn task(force: bool) -> FrontendRequest {
        FrontendRequest::CreateTask {
            input: CreateTaskInput::builder()
                .product_id("prod_1")
                .project_id("proj_1")
                .name("a task")
                .force_duplicate(force)
                .build(),
        }
    }

    #[test]
    fn reads_and_setters_are_always_replayable() {
        assert_eq!(replay_safety(&FrontendRequest::GetEngineVersion), ReplaySafety::Always);
        assert_eq!(replay_safety(&FrontendRequest::ListProducts), ReplaySafety::Always);
        assert_eq!(
            replay_safety(&FrontendRequest::SetDispatchPaused {
                paused: true,
                reason: None
            }),
            ReplaySafety::Always
        );
    }

    #[test]
    fn proposals_are_keyed_by_execution_and_replayable() {
        let request = FrontendRequest::SubmitProposal {
            run_id: "exec_1".into(),
            kind: ProposalKind::RunDone,
            payload: serde_json::json!({}),
            idempotency_key: None,
        };
        assert_eq!(replay_safety(&request), ReplaySafety::Always);
    }

    #[test]
    fn guarded_creates_are_replayable_only_inside_the_guard_window() {
        let safety = replay_safety(&task(false));
        assert!(matches!(safety, ReplaySafety::Within(window) if window < DUPLICATE_GUARD_WINDOW));
        assert!(safety.allows_replay_after(Duration::from_secs(5)));
        assert!(!safety.allows_replay_after(Duration::from_secs(55)));
        assert!(!safety.allows_replay_after(Duration::from_secs(300)));
    }

    #[test]
    fn force_duplicate_turns_the_guard_off() {
        assert_eq!(replay_safety(&task(true)), ReplaySafety::Never);
        let chore = FrontendRequest::CreateChore {
            input: CreateChoreInput::builder()
                .product_id("prod_1")
                .name("c")
                .force_duplicate(true)
                .build(),
        };
        assert_eq!(replay_safety(&chore), ReplaySafety::Never);
    }

    #[test]
    fn unguarded_writes_and_unknown_verbs_are_never_replayed() {
        let project = FrontendRequest::CreateProject {
            input: CreateProjectInput::builder().product_id("prod_1").name("p").build(),
        };
        assert_eq!(replay_safety(&project), ReplaySafety::Never);
        assert_eq!(
            replay_safety(&FrontendRequest::Shutdown { token: "t".into() }),
            ReplaySafety::Never
        );
        assert!(!ReplaySafety::Never.allows_replay_after(Duration::ZERO));
    }

    #[test]
    fn request_name_is_the_wire_tag() {
        assert_eq!(request_name(&FrontendRequest::ListProducts), "list_products");
    }
}
