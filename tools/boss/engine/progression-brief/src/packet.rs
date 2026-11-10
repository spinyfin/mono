//! In-memory diagnosis packet assembled from stored engine state.

use boss_protocol::{
    CiRemediation, ConflictResolution, DependencyEdge, OperatorQuestionRecord, Task, WorkAttentionItem,
};
use boss_work_db::{ExecutionDiagnosis, ProductPrCiSnapshot, ProgressionCreateBudget, StatusAuditRow};
use sha2::{Digest, Sha256};

/// Fingerprint of the current stuck episode (design 2.5):
/// `hash(stuck_kind, blocked_reason, pr_head_sha or NULL, latest terminal execution id)`.
pub fn compute_stuck_fingerprint(
    stuck_kind: &str,
    blocked_reason: Option<&str>,
    pr_head_sha: Option<&str>,
    latest_terminal_execution_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(stuck_kind.as_bytes());
    hasher.update(b"|");
    hasher.update(blocked_reason.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(pr_head_sha.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(latest_terminal_execution_id.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

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
    /// The worker's own words, from the open declaration attention or the
    /// stored operator question.
    pub verbatim: String,
    /// The engine's wrapper text (the sweep's note on the item), shown apart
    /// from the worker's words so the agent never classifies engine prose.
    pub sweep_note: Option<String>,
}

/// Prior progression run the packet lists: the execution row plus its declaration.
#[derive(Debug, Clone)]
pub struct PriorProgressionRun {
    pub execution_id: String,
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
    /// 2.5 allows one progression redispatch per item until a non-progression
    /// work run completes; this is that bound minus the redispatches the
    /// execution history shows since the last such run.
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
