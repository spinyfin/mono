//! Review yield report (`boss cost review-yield`): what the automated reviews
//! in a time window found, what passed the revision gate, and whether the
//! remediation it minted changed code and merged.
//!
//! The report is read-only and built from stored review data: the findings in
//! each batch member's accepted report or verdict, the batch's frozen
//! classification, the verdict row the applier wrote, and the remediation task
//! that row points at. Nothing here is captured for the report's sake.
//!
//! Counts are counts of what was stored. A payload that cannot be read is
//! counted as unreadable, never as zero findings; a remediation that is still
//! open is counted as open, never as not merged.

use serde::{Deserialize, Serialize};

use super::review_batch::{ReviewBatchMemberRole, ReviewBatchPhase};

/// The payload of [`crate::FrontendRequest::GetReviewYieldReport`]: the window
/// (half-open on `pr_review_batches.created_at`) and, optionally, the
/// experiment whose recorded arm splits the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewYieldRequest {
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    /// Experiment key. When set, the report is split by the arm that experiment
    /// recorded on each review's executions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
}

/// Where a review batch sits in a PR's life.
///
/// A pre-merge round is the number of earlier non-stale verdicts for the same
/// PR, plus one: it counts reviews that produced a decision, so a batch that
/// failed without a verdict does not advance the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReviewYieldPhase {
    #[serde(rename = "pre_merge_round_1")]
    PreMergeRound1,
    #[serde(rename = "pre_merge_round_2")]
    PreMergeRound2,
    #[serde(rename = "pre_merge_round_3_plus")]
    PreMergeRound3Plus,
    #[serde(rename = "post_merge")]
    PostMerge,
}

impl ReviewYieldPhase {
    pub const ALL: [ReviewYieldPhase; 4] = [
        Self::PreMergeRound1,
        Self::PreMergeRound2,
        Self::PreMergeRound3Plus,
        Self::PostMerge,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreMergeRound1 => "pre_merge_round_1",
            Self::PreMergeRound2 => "pre_merge_round_2",
            Self::PreMergeRound3Plus => "pre_merge_round_3_plus",
            Self::PostMerge => "post_merge",
        }
    }

    /// The bucket for a batch of `phase` that is review round `round`
    /// (1-based; ignored for a post-merge batch).
    pub const fn for_batch(phase: ReviewBatchPhase, round: u32) -> Self {
        match (phase, round) {
            (ReviewBatchPhase::PostMerge, _) => Self::PostMerge,
            (ReviewBatchPhase::PreMerge, 0 | 1) => Self::PreMergeRound1,
            (ReviewBatchPhase::PreMerge, 2) => Self::PreMergeRound2,
            (ReviewBatchPhase::PreMerge, _) => Self::PreMergeRound3Plus,
        }
    }
}

impl std::fmt::Display for ReviewYieldPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A condition of the Deep review profile. A PR is classified Deep when any
/// one fires; the thresholds live beside the classifier in `boss-pr-review`
/// and the report states each rule in [`DeepCriterionRow::rule`], so the two
/// cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeepCriterion {
    ChangedLines,
    ChangedFiles,
    Subsystems,
    ProductionLanguages,
    ComplexityFlags,
}

impl DeepCriterion {
    pub const ALL: [DeepCriterion; 5] = [
        Self::ChangedLines,
        Self::ChangedFiles,
        Self::Subsystems,
        Self::ProductionLanguages,
        Self::ComplexityFlags,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChangedLines => "changed_lines",
            Self::ChangedFiles => "changed_files",
            Self::Subsystems => "subsystems",
            Self::ProductionLanguages => "production_languages",
            Self::ComplexityFlags => "complexity_flags",
        }
    }
}

impl std::fmt::Display for DeepCriterion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The arm of an experiment a review batch ran in, from the arm set recorded
/// on its members' executions.
///
/// Declaration order is the order arms are reported in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewYieldArm {
    Treatment,
    Control,
    /// Every launched member recorded the experiment as stopped.
    Stopped,
    /// Members recorded different arms, so the batch is in neither.
    Mixed,
    /// A set was recorded and the experiment was not running for it.
    NotEnrolled,
    /// No arm set was recorded for the launched members.
    NotRecorded,
    /// A recorded arm set failed the shared decoder.
    Unreadable,
}

impl ReviewYieldArm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Treatment => "treatment",
            Self::Control => "control",
            Self::Stopped => "stopped",
            Self::Mixed => "mixed",
            Self::NotEnrolled => "not_enrolled",
            Self::NotRecorded => "not_recorded",
            Self::Unreadable => "unreadable",
        }
    }
}

impl std::fmt::Display for ReviewYieldArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A label and how many times it occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelCount {
    pub label: String,
    pub count: u64,
}

/// Findings by severity. Every severity is always present, so a table has
/// fixed columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: u64,
    pub high: u64,
    pub medium: u64,
    pub low: u64,
}

/// What came of the remediation tasks a set of verdicts minted.
///
/// `changed_code` is positive evidence only: a revision whose executions
/// recorded a different PR head after than before, or a follow-up that opened
/// a PR. `merged` is the PR the remediation belongs to being recorded as
/// merged (a revision's parent PR, a follow-up's own PR). A remediation that
/// has not finished is in `still_open` and in neither of the others yet, so
/// for a recent window `changed_code` and `merged` are floors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationYield {
    pub minted: u64,
    pub still_open: u64,
    pub changed_code: u64,
    pub merged: u64,
    pub changed_code_and_merged: u64,
}

/// One reviewer role's reports in one phase and its own execution arm.
/// Remediation is credited once per role named in accepted sources, and to
/// the deciding role; missing legacy sources leave leaf-role yield incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldRoleRow {
    pub role: ReviewBatchMemberRole,
    /// Members of this role that reported.
    pub reviews: u64,
    /// Reviews whose stored report could not be read; their findings are not in
    /// the counts below.
    #[serde(default)]
    #[builder(default)]
    pub unreadable_reports: u64,
    #[serde(default)]
    #[builder(default)]
    pub findings: u64,
    #[serde(default)]
    #[builder(default)]
    pub by_severity: SeverityCounts,
    /// Readable reports whose own findings pass the revision gate. A leaf
    /// reviewer does not decide, so this is what its report would have done
    /// alone; for a deciding role it follows its verdict's findings.
    #[serde(default)]
    #[builder(default)]
    pub passing_gate: u64,
    /// Of those, the reports that pass only because of a low or medium
    /// finding in an always-on category.
    #[serde(default)]
    #[builder(default)]
    pub passing_on_always_on_category_only: u64,
    /// Non-zero categories, most frequent first.
    #[serde(default)]
    #[builder(default)]
    pub by_category: Vec<LabelCount>,
    /// Remediation from accepted findings naming this role, once per batch.
    #[serde(default)]
    #[builder(default)]
    pub revisions: RemediationYield,
    #[serde(default)]
    #[builder(default)]
    pub followups: RemediationYield,
}

/// What the deciding review of each batch (the supervisor before merge, the
/// sole reviewer after) concluded, and what became of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldDecisions {
    /// Minted remediations whose verdict has missing legacy source attribution.
    #[serde(default)]
    #[builder(default)]
    pub remediations_without_source_attribution: u64,
    /// Batches with an applied verdict.
    #[serde(default)]
    #[builder(default)]
    pub verdicts: u64,
    /// Verdicts whose findings could not be read, so their gate basis is
    /// unknown.
    #[serde(default)]
    #[builder(default)]
    pub unreadable_verdicts: u64,
    /// Verdicts the revision gate passed, as the engine recorded it.
    #[serde(default)]
    #[builder(default)]
    pub passed_gate: u64,
    /// Passed with at least one critical or high finding.
    #[serde(default)]
    #[builder(default)]
    pub passed_on_critical_or_high: u64,
    /// Passed with no critical or high finding: only a low or medium finding
    /// in one of the always-on categories carried it.
    #[serde(default)]
    #[builder(default)]
    pub passed_on_always_on_category_only: u64,
    /// Verdicts by recorded gate outcome, most frequent first. Shows how many
    /// passed verdicts minted nothing (a stale head, a duplicate head).
    #[serde(default)]
    #[builder(default)]
    pub gate_outcomes: Vec<LabelCount>,
    #[serde(default)]
    #[builder(default)]
    pub revisions: RemediationYield,
    #[serde(default)]
    #[builder(default)]
    pub followups: RemediationYield,
    /// Remediation minted from verdicts that passed on a critical or high
    /// finding. Revisions and follow-ups together.
    #[serde(default)]
    #[builder(default)]
    pub from_critical_or_high: RemediationYield,
    /// Remediation minted from verdicts that passed only on an always-on
    /// category finding.
    #[serde(default)]
    #[builder(default)]
    pub from_always_on_category_only: RemediationYield,
}

/// One review phase: its reports by role and its decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldPhaseReport {
    pub phase: ReviewYieldPhase,
    pub decisions: ReviewYieldDecisions,
    #[serde(default)]
    #[builder(default)]
    pub batches: u64,
    /// Batches with no applied verdict: failed, still running, or not applied.
    #[serde(default)]
    #[builder(default)]
    pub batches_without_verdict: u64,
    #[serde(default)]
    #[builder(default)]
    pub roles: Vec<ReviewYieldRoleRow>,
}

/// The yield of a set of post-merge review batches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(default)]
pub struct PostMergeYield {
    #[builder(default)]
    pub batches: u64,
    #[builder(default)]
    pub verdicts: u64,
    #[builder(default)]
    pub findings: u64,
    #[builder(default)]
    pub critical_or_high_findings: u64,
    #[builder(default)]
    pub passed_gate: u64,
    /// Follow-ups minted by these batches' verdicts.
    #[builder(default)]
    pub remediation: RemediationYield,
    /// The follow-ups minted from verdicts that passed on a critical or high
    /// finding.
    #[builder(default)]
    pub from_critical_or_high: RemediationYield,
}

/// One Deep criterion across the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct DeepCriterionRow {
    pub criterion: DeepCriterion,
    /// The rule as the classifier states it, with its thresholds.
    pub rule: String,
    /// Deep pre-merge batches in which the criterion fired. A batch can fire
    /// several, so the rows do not sum to the batch count.
    #[serde(default)]
    #[builder(default)]
    pub pre_merge_batches: u64,
    /// The yield of the Deep post-merge batches in which it fired.
    #[serde(default)]
    #[builder(default)]
    pub post_merge: PostMergeYield,
}

/// Which criteria fired for batches classified Deep, re-derived from each
/// batch's stored classification inputs with the classifier's current
/// thresholds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldDeep {
    #[serde(default)]
    #[builder(default)]
    pub pre_merge_batches: u64,
    #[serde(default)]
    #[builder(default)]
    pub post_merge_batches: u64,
    /// Batches stored as Deep in which no criterion fires under the current
    /// thresholds (the thresholds changed after the batch was classified).
    #[serde(default)]
    #[builder(default)]
    pub no_current_criterion: u64,
    /// Batches whose stored classification could not be read; they are in
    /// neither this section nor the eligibility split.
    #[serde(default)]
    #[builder(default)]
    pub unreadable_classifications: u64,
    #[serde(default)]
    #[builder(default)]
    pub criteria: Vec<DeepCriterionRow>,
}

/// Post-merge batches split by whether the size-eligibility rule would still
/// admit them to a post-merge review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldEligibility {
    /// The size-eligibility rule, with its thresholds.
    pub rule: String,
    #[serde(default)]
    #[builder(default)]
    pub stays_eligible: PostMergeYield,
    #[serde(default)]
    #[builder(default)]
    pub would_be_excluded: PostMergeYield,
    #[serde(default)]
    #[builder(default)]
    pub unknown_size: PostMergeYield,
}

/// The report for one arm (or, with no experiment, for the whole window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldArmReport {
    /// `None` when the request named no experiment.
    pub arm: Option<ReviewYieldArm>,
    pub deep: ReviewYieldDeep,
    pub post_merge_eligibility: ReviewYieldEligibility,
    #[serde(default)]
    #[builder(default)]
    pub batches: u64,
    /// One entry per [`ReviewYieldPhase`], in order.
    #[serde(default)]
    #[builder(default)]
    pub phases: Vec<ReviewYieldPhaseReport>,
}

/// Reply to [`crate::FrontendRequest::GetReviewYieldReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewYieldReport {
    pub generated_at_epoch_s: i64,
    pub since_epoch_s: i64,
    pub until_epoch_s: i64,
    /// The experiment the report is split by, echoed from the request.
    pub experiment: Option<String>,
    /// Without an experiment, one entry for the whole window. With one, one
    /// entry per arm observed in the window, in [`ReviewYieldArm`] order.
    #[serde(default)]
    #[builder(default)]
    pub arms: Vec<ReviewYieldArmReport>,
    #[serde(default)]
    #[builder(default)]
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrontendRequest;

    #[test]
    fn rounds_bucket_by_decided_reviews_and_post_merge_ignores_the_round() {
        use ReviewBatchPhase::{PostMerge, PreMerge};
        let bucket = |phase, round| ReviewYieldPhase::for_batch(phase, round);
        assert_eq!(bucket(PreMerge, 0), ReviewYieldPhase::PreMergeRound1);
        assert_eq!(bucket(PreMerge, 1), ReviewYieldPhase::PreMergeRound1);
        assert_eq!(bucket(PreMerge, 2), ReviewYieldPhase::PreMergeRound2);
        assert_eq!(bucket(PreMerge, 3), ReviewYieldPhase::PreMergeRound3Plus);
        assert_eq!(bucket(PreMerge, 9), ReviewYieldPhase::PreMergeRound3Plus);
        assert_eq!(bucket(PostMerge, 1), ReviewYieldPhase::PostMerge);
        assert_eq!(bucket(PostMerge, 4), ReviewYieldPhase::PostMerge);
    }

    #[test]
    fn enum_names_match_their_wire_form() {
        for phase in ReviewYieldPhase::ALL {
            assert_eq!(serde_json::to_value(phase).unwrap(), phase.as_str());
        }
        for criterion in DeepCriterion::ALL {
            assert_eq!(serde_json::to_value(criterion).unwrap(), criterion.as_str());
        }
        for arm in [
            ReviewYieldArm::Treatment,
            ReviewYieldArm::Control,
            ReviewYieldArm::Stopped,
            ReviewYieldArm::Mixed,
            ReviewYieldArm::NotEnrolled,
            ReviewYieldArm::NotRecorded,
            ReviewYieldArm::Unreadable,
        ] {
            assert_eq!(serde_json::to_value(arm).unwrap(), arm.as_str());
        }
    }

    #[test]
    fn the_request_decodes_flat_beside_its_tag_and_experiment_is_optional() {
        let bare: FrontendRequest = serde_json::from_value(serde_json::json!({
            "type": "get_review_yield_report",
            "since_epoch_s": 100,
            "until_epoch_s": 200,
        }))
        .unwrap();
        let FrontendRequest::GetReviewYieldReport(request) = bare else {
            panic!("expected GetReviewYieldReport");
        };
        assert_eq!(
            request,
            ReviewYieldRequest {
                since_epoch_s: 100,
                until_epoch_s: 200,
                experiment: None,
            }
        );

        let named = FrontendRequest::GetReviewYieldReport(ReviewYieldRequest {
            since_epoch_s: 1,
            until_epoch_s: 2,
            experiment: Some("post_merge_review_slim".to_owned()),
        });
        let json = serde_json::to_value(&named).unwrap();
        assert_eq!(json["type"], "get_review_yield_report");
        assert_eq!(json["experiment"], "post_merge_review_slim");
        let FrontendRequest::GetReviewYieldReport(decoded) = serde_json::from_value(json).unwrap() else {
            panic!("expected GetReviewYieldReport");
        };
        assert_eq!(decoded.experiment.as_deref(), Some("post_merge_review_slim"));
        assert_eq!((decoded.since_epoch_s, decoded.until_epoch_s), (1, 2));
    }
}
