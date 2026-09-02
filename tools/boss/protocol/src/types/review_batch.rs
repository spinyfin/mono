//! Durable review-batch contract shared by classification, scheduling, and
//! persistence.
//!
//! A batch freezes the immutable PR target and its metadata-derived review
//! profile before any reviewer is scheduled. Member rows then record one
//! role-specific attempt, including the resolved provider model and effort.

use serde::{Deserialize, Serialize};

/// The review profile selected from immutable PR metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewProfile {
    Light,
    Standard,
    Deep,
}

impl ReviewProfile {
    /// The model capability tier selected for this profile.
    pub const fn model_tier(self) -> ReviewModelTier {
        match self {
            Self::Light => ReviewModelTier::Fast,
            Self::Standard => ReviewModelTier::Balanced,
            Self::Deep => ReviewModelTier::Strong,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Standard => "standard",
            Self::Deep => "deep",
        }
    }
}

impl std::fmt::Display for ReviewProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "light" => Ok(Self::Light),
            "standard" => Ok(Self::Standard),
            "deep" => Ok(Self::Deep),
            _ => Err(format!("unknown review profile: {value}")),
        }
    }
}

/// A driver's model-selection capability tier for review work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewModelTier {
    Fast,
    Balanced,
    Strong,
}

/// Lexical complexity signals derived from changed paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewComplexityFlag {
    DatabaseSchemaMigration,
    AuthPermissionsSandbox,
    SchedulerConcurrencyLifecycle,
    BuildReleaseDependency,
}

/// Production-language buckets counted by the profile classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLanguageBucket {
    Rust,
    Swift,
    Starlark,
    Shell,
    Web,
    Other,
}

/// Required GitHub metadata unavailable while a profile was classified.
///
/// Missing metadata conservatively selects [`ReviewProfile::Standard`], but
/// remains visible in the persisted snapshot rather than being treated as a
/// small change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMetadataField {
    Additions,
    Deletions,
    ChangedFiles,
}

/// Complete immutable input and result of PR review classification.
///
/// `changed_files` preserves the raw GitHub paths. The remaining collections
/// are normalized, deterministic derivations used for audit and policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewClassification {
    pub changed_files: Vec<String>,
    pub complexity_flags: Vec<ReviewComplexityFlag>,
    pub has_production_code: bool,
    pub metadata_missing: Vec<ReviewMetadataField>,
    pub production_languages: Vec<ReviewLanguageBucket>,
    pub profile: ReviewProfile,
    pub subsystem_buckets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<i64>,
}

/// Whether a batch targets an open PR head or a landed merge commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBatchPhase {
    PreMerge,
    PostMerge,
}

impl ReviewBatchPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreMerge => "pre_merge",
            Self::PostMerge => "post_merge",
        }
    }
}

impl std::fmt::Display for ReviewBatchPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewBatchPhase {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pre_merge" => Ok(Self::PreMerge),
            "post_merge" => Ok(Self::PostMerge),
            _ => Err(format!("unknown review batch phase: {value}")),
        }
    }
}

/// Lifecycle state of a review batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBatchStatus {
    Collecting,
    Supervising,
    Applying,
    Completed,
    Failed,
}

impl ReviewBatchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Supervising => "supervising",
            Self::Applying => "applying",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ReviewBatchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewBatchStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "collecting" => Ok(Self::Collecting),
            "supervising" => Ok(Self::Supervising),
            "applying" => Ok(Self::Applying),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("unknown review batch status: {value}")),
        }
    }
}

/// Fixed role of an independently persisted review member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBatchMemberRole {
    ClaudeReviewer,
    CodexReviewer,
    GrokReviewer,
    Supervisor,
    PostMergeReviewer,
}

impl ReviewBatchMemberRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeReviewer => "claude_reviewer",
            Self::CodexReviewer => "codex_reviewer",
            Self::GrokReviewer => "grok_reviewer",
            Self::Supervisor => "supervisor",
            Self::PostMergeReviewer => "post_merge_reviewer",
        }
    }
}

impl std::fmt::Display for ReviewBatchMemberRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewBatchMemberRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude_reviewer" => Ok(Self::ClaudeReviewer),
            "codex_reviewer" => Ok(Self::CodexReviewer),
            "grok_reviewer" => Ok(Self::GrokReviewer),
            "supervisor" => Ok(Self::Supervisor),
            "post_merge_reviewer" => Ok(Self::PostMergeReviewer),
            _ => Err(format!("unknown review batch member role: {value}")),
        }
    }
}

/// Per-attempt member state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBatchMemberStatus {
    Pending,
    Running,
    Reported,
    Failed,
}

impl ReviewBatchMemberStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Reported => "reported",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ReviewBatchMemberStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewBatchMemberStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "reported" => Ok(Self::Reported),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("unknown review batch member status: {value}")),
        }
    }
}

/// One immutable review target and its classification snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewBatch {
    pub id: String,
    pub cycle_root_id: String,
    pub base_sha: String,
    pub classification: ReviewClassification,
    pub created_at: String,
    pub phase: ReviewBatchPhase,
    pub pr_number: i64,
    pub pr_url: String,
    pub status: ReviewBatchStatus,
    pub target_sha: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_verdict_proposal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
}

/// One persisted reviewer or supervisor attempt within a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewBatchMember {
    pub id: String,
    pub batch_id: String,
    pub attempt: i64,
    pub created_at: String,
    pub provider_effort: String,
    pub requested_driver: String,
    pub resolved_model: String,
    pub role: ReviewBatchMemberRole,
    pub status: ReviewBatchMemberStatus,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_proposal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_maps_to_the_expected_model_tier() {
        assert_eq!(ReviewProfile::Light.model_tier(), ReviewModelTier::Fast);
        assert_eq!(ReviewProfile::Standard.model_tier(), ReviewModelTier::Balanced);
        assert_eq!(ReviewProfile::Deep.model_tier(), ReviewModelTier::Strong);
    }

    #[test]
    fn review_batch_discriminators_round_trip() {
        assert_eq!("pre_merge".parse(), Ok(ReviewBatchPhase::PreMerge));
        assert_eq!("supervising".parse(), Ok(ReviewBatchStatus::Supervising));
        assert_eq!("grok_reviewer".parse(), Ok(ReviewBatchMemberRole::GrokReviewer));
        assert_eq!("reported".parse(), Ok(ReviewBatchMemberStatus::Reported));
    }
}
