//! Bounded frontend read admission. Internal work and mutations never enter
//! this lane. Acquire the connection permit before the engine permit so a
//! single pipelined connection cannot occupy the engine's whole wait queue.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Queued (not yet active) reads allowed per active permit on one connection.
const PENDING_PER_ACTIVE: usize = 4;

pub(super) const BUSY: &str = "engine busy, retry: read admission limit exceeded";

pub(super) struct ReadAdmission {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    per_connection: usize,
    wait: Duration,
}

pub(super) struct ConnectionReads {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

pub(super) struct WaitingRead {
    engine: Arc<ReadAdmission>,
    connection: Arc<ConnectionReads>,
    _engine_pending: OwnedSemaphorePermit,
    _connection_pending: OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
}

impl Default for ReadAdmission {
    fn default() -> Self {
        Self::new(
            setting("BOSS_RPC_READ_CONCURRENCY", 32),
            setting("BOSS_RPC_READ_PER_CONNECTION", 16),
            setting("BOSS_RPC_READ_QUEUE", 128),
            Duration::from_millis(setting("BOSS_RPC_READ_WAIT_MS", 500) as u64),
        )
    }
}

fn setting(name: &str, default: usize) -> usize {
    let value = boss_engine_utils::env_parse::env_parsed_or(name, default);
    if value > 0 && value <= 1_000_000 {
        value
    } else {
        tracing::warn!(name, value, default, "invalid read admission setting; using default");
        default
    }
}

impl ReadAdmission {
    pub(super) fn live() -> Self {
        Self::new(
            setting("BOSS_RPC_LIVE_READ_CONCURRENCY", 8),
            setting("BOSS_RPC_LIVE_READ_PER_CONNECTION", 2),
            setting("BOSS_RPC_LIVE_READ_QUEUE", 32),
            Duration::from_millis(setting("BOSS_RPC_READ_WAIT_MS", 250) as u64),
        )
    }

    pub(super) fn new(global: usize, per_connection: usize, queue: usize, wait: Duration) -> Self {
        Self {
            active: Arc::new(Semaphore::new(global)),
            pending: Arc::new(Semaphore::new(queue)),
            per_connection,
            wait,
        }
    }

    pub(super) fn outstanding_per_connection(&self) -> usize {
        self.per_connection.saturating_mul(PENDING_PER_ACTIVE + 1)
    }

    pub(super) fn connection(&self) -> Arc<ConnectionReads> {
        Arc::new(ConnectionReads {
            active: Arc::new(Semaphore::new(self.per_connection)),
            pending: Arc::new(Semaphore::new(self.per_connection.saturating_mul(PENDING_PER_ACTIVE))),
        })
    }

    /// Bound task allocation as well as active handlers. Full queues reject
    /// immediately; accepted waiters share one deadline across both limits.
    pub(super) fn enqueue(self: &Arc<Self>, connection: &Arc<ConnectionReads>) -> Result<WaitingRead, &'static str> {
        let local = connection.pending.clone().try_acquire_owned().map_err(|_| BUSY)?;
        let global = self.pending.clone().try_acquire_owned().map_err(|_| BUSY)?;
        Ok(WaitingRead {
            engine: self.clone(),
            connection: connection.clone(),
            _engine_pending: global,
            _connection_pending: local,
            deadline: tokio::time::Instant::now() + self.wait,
        })
    }
}

impl WaitingRead {
    pub(super) async fn acquire(self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), &'static str> {
        tokio::time::timeout_at(self.deadline, async {
            let local = self.connection.active.clone().acquire_owned().await.map_err(|_| BUSY)?;
            let global = self.engine.active.clone().acquire_owned().await.map_err(|_| BUSY)?;
            Ok((local, global))
        })
        .await
        .map_err(|_| BUSY)?
    }
}

/// Status used by `agents list` has its own reserved live-work path.
/// `GetPrStatus` with `refresh: true` awaits a GitHub probe, so it is
/// outside the bulk lane because its per-execution refresh budget already
/// bounds probes. Design-doc reads deliberately use the bulk budget through
/// their network fetch and correlated response; they have no separate
/// foreground budget and must not escape admission on cache misses.
/// Mutations, subscriptions, and worker proposals remain ordered on their
/// connection. Bulk read replies are correlated by request id.
pub(super) fn is_bulk_read(request: &boss_protocol::FrontendRequest) -> bool {
    use boss_protocol::FrontendRequest as R;
    matches!(
        request,
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
    )
}

pub(super) fn is_live_read(request: &boss_protocol::FrontendRequest) -> bool {
    use boss_protocol::FrontendRequest as R;
    matches!(
        request,
        R::GetEngineHealth
            | R::GetEngineVersion
            | R::GitHubAuthStatus
            | R::ListHostedPaneStatuses
            | R::ListTmuxWorkerStatuses
            | R::ListWorkerLiveStates
            | R::MetricsListLive
            | R::MetricsShowLive { .. }
            | R::ProbeStatus { .. }
            | R::TrunkStatus
            | R::WorkerPoolSummary
            | R::WorkspacePoolSummary
    )
}

#[cfg(test)]
mod burst_tests;
#[cfg(test)]
mod tests;
