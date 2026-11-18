//! In-memory diagnosis packet assembled from stored engine state.

use boss_protocol::{
    CiRemediation, ConflictResolution, DependencyEdge, OperatorQuestionRecord, Task, WorkAttentionItem,
};
use boss_work_db::{ExecutionDiagnosis, ProductPrCiSnapshot, ProgressionCreateBudget, ReviewVerdict, StatusAuditRow};

/// One neighbour on a dependency edge, with any live execution the packet names.
#[derive(Debug, Clone)]
pub struct Neighbour {
    pub edge: DependencyEdge,
    pub live_execution: Option<(String, String)>,
}

/// The 2.9 declared-blocker text the agent must classify, when present.
#[derive(Debug, Clone)]
pub struct DeclaredBlocker {
    pub producer: String,
    /// The worker's own words from its applied declaration, nudge-breaker
    /// attention, or open question on the same item and execution.
    pub verbatim: String,
    /// The engine's wrapper text (the sweep's note on the item), shown apart
    /// from the worker's words so the agent never classifies engine prose.
    pub sweep_note: Option<String>,
}

/// Prior progression run the packet lists: the execution row plus its declaration.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct PriorProgressionRun {
    pub execution_id: String,
    pub fingerprint: Option<String>,
    pub actions: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub run_done_outcome: Option<String>,
    pub last_error: Option<String>,
}

/// Remaining verb budget shown in the catalogue section.
#[derive(Debug, Clone)]
pub struct VerbBudget {
    pub create_used: i64,
    pub create_max: i64,
    pub create_window_secs: i64,
    /// One progression redispatch per item and stuck kind, reset by a completed
    /// work run on that item, including work started by progression.
    pub redispatch_remaining: u32,
}

/// Assembled diagnosis packet. Rendered into the progression prompt.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ProgressionBriefPacket {
    pub item: Task,
    pub product_name: String,
    pub project_name: Option<String>,
    pub stuck_kind: String,
    pub stuck_since: Option<i64>,
    pub failure_basis: Option<String>,
    pub ledger: Vec<StatusAuditRow>,
    pub prerequisites: Vec<Neighbour>,
    pub dependents: Vec<Neighbour>,
    pub executions: Vec<ExecutionDiagnosis>,
    pub churn_window_terminal_count: i64,
    pub declared_blocker: Option<DeclaredBlocker>,
    pub pr_url: Option<String>,
    /// Set when the PR belongs to the revision chain's root rather than the
    /// stuck item itself: the root's id.
    pub pr_owner: Option<String>,
    pub pr_mergeable: Option<String>,
    pub pr_merge_state_status: Option<String>,
    pub pr_head_sha: Option<String>,
    pub pr_observed_at: Option<String>,
    pub ci_required_state: Option<String>,
    pub ci_required_detail: Option<String>,
    pub review_required_state: Option<String>,
    /// Boss's own AI-review state for the PR owner (not GitHub's review gate).
    pub ai_review_state: Option<String>,
    pub ai_review_stage: Option<String>,
    pub review_cycle: i64,
    /// Most recent recorded AI-review verdict for the PR owner; `None` means
    /// unknown, never a clean verdict.
    pub latest_review: Option<ReviewVerdict>,
    /// The product's configured git remote, used by the conflict probe.
    pub repo_remote_url: Option<String>,
    pub conflict_attempts: Vec<ConflictResolution>,
    pub ci_attempts: Vec<CiRemediation>,
    pub other_pr_ci: Vec<ProductPrCiSnapshot>,
    /// Attentions on the item and its chain, each with its owning work item id.
    pub attentions: Vec<(String, WorkAttentionItem)>,
    /// Operator questions on the item and its chain (each carries its owner).
    pub questions: Vec<OperatorQuestionRecord>,
    pub prior_progression: Vec<PriorProgressionRun>,
    pub fingerprint: String,
    pub budget: VerbBudget,
    pub create_budget: ProgressionCreateBudget,
}
