//! Review yield: a pure builder over a snapshot of stored review data.
//!
//! The storage layer reads, for each review batch in a window, the findings in
//! its members' accepted reports and verdict, the batch's frozen
//! classification, and what became of the remediation its verdict minted. It
//! hands those over as [`YieldBatch`]es; [`build_review_yield_report`] turns
//! them into a [`ReviewYieldReport`]. Nothing here reads a database, so every
//! rule below is tested with literal inputs.
//!
//! Three definitions are shared with the code that decides things, not
//! restated: the severity gate (`gate_basis`), the Deep thresholds
//! (`deep_criteria_for`) and the post-merge size-eligibility rule
//! (`post_merge_size_eligible`), all in [`crate::parsing`].

use std::collections::BTreeMap;

use boss_protocol::{
    DeepCriterion, DeepCriterionRow, LabelCount, PostMergeYield, RemediationYield, ReviewBatchMemberRole,
    ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewProfile, ReviewYieldArm,
    ReviewYieldArmReport, ReviewYieldDecisions, ReviewYieldDeep, ReviewYieldEligibility, ReviewYieldPhase,
    ReviewYieldPhaseReport, ReviewYieldReport, ReviewYieldRequest, ReviewYieldRoleRow,
};
use serde::Deserialize;

use crate::parsing::{
    DeepInputs, GateBasis, deep_criteria_for, deep_criterion_rule, gate_basis, is_critical_or_high,
    post_merge_size_eligibility_rule, post_merge_size_eligible,
};
use crate::types::{ReviewFindingCategory, ReviewFindingSeverity};

/// A finding as the yield report reads it. Everything else in a stored
/// report or verdict (titles, evidence, locations) is not needed to count
/// findings, so a payload from an older schema still reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct YieldFinding {
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
}

#[derive(Deserialize)]
struct StoredFindings {
    findings: Vec<YieldFinding>,
}

#[derive(Deserialize)]
struct StoredPayload {
    #[serde(default)]
    report: Option<StoredFindings>,
    #[serde(default)]
    verdict: Option<StoredFindings>,
}

/// The findings in a stored `review_report` proposal payload, or `None` when
/// the payload cannot be read.
pub fn findings_from_report_payload(payload_json: &str) -> Option<Vec<YieldFinding>> {
    let payload: StoredPayload = serde_json::from_str(payload_json).ok()?;
    payload.report.map(|stored| stored.findings)
}

/// The findings in a stored `review_verdict` proposal payload, or `None` when
/// the payload cannot be read.
pub fn findings_from_verdict_payload(payload_json: &str) -> Option<Vec<YieldFinding>> {
    let payload: StoredPayload = serde_json::from_str(payload_json).ok()?;
    payload.verdict.map(|stored| stored.findings)
}

/// One persisted reviewer or supervisor attempt in a batch.
#[derive(Debug, Clone)]
pub struct YieldMember {
    pub role: ReviewBatchMemberRole,
    pub status: ReviewBatchMemberStatus,
    /// The experiment arm set recorded on the member's execution (canonical
    /// JSON from experiment key to arm), `None` when nothing was recorded or
    /// the member has no execution.
    pub experiment_arms: Option<String>,
    /// The findings in the member's accepted report or verdict. `None` when
    /// it did not report, or when its stored payload could not be read.
    pub findings: Option<Vec<YieldFinding>>,
}

/// Whether remediation was a revision on the open PR or a follow-up that
/// opens its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemediationKind {
    Revision,
    FollowUp,
}

/// The remediation task a verdict minted, as it stands when the report is
/// built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YieldRemediation {
    pub kind: RemediationKind,
    /// The task has not finished.
    pub open: bool,
    /// Positive evidence it changed code.
    pub changed_code: bool,
    /// The PR it belongs to is recorded as merged.
    pub merged: bool,
}

/// The applied verdict row of a batch.
#[derive(Debug, Clone)]
pub struct YieldVerdict {
    /// The gate outcome the engine recorded (`completed_with_findings`,
    /// `completed_clean`, `stale_head`, `dropped_duplicate_head`, ...).
    pub gate_outcome: String,
    /// The engine's own answer to the severity gate, as recorded.
    pub engine_revision_warranted: bool,
    pub remediation: Option<YieldRemediation>,
}

/// One review batch in the window.
#[derive(Debug, Clone, bon::Builder)]
pub struct YieldBatch {
    pub phase: ReviewBatchPhase,
    /// The pre-merge review round, 1-based: earlier non-stale verdicts for
    /// the same PR, plus one. Unused for a post-merge batch.
    pub round: u32,
    /// `None` when the stored classification could not be read.
    pub classification: Option<ReviewClassification>,
    #[builder(default)]
    pub members: Vec<YieldMember>,
    pub verdict: Option<YieldVerdict>,
}

/// Build the report for `batches`, which the caller has already limited to
/// the request's window.
pub fn build_review_yield_report(
    request: &ReviewYieldRequest,
    generated_at_epoch_s: i64,
    batches: &[YieldBatch],
) -> ReviewYieldReport {
    let experiment = request.experiment.as_deref();
    let mut arms: BTreeMap<Option<ReviewYieldArm>, ArmAcc> = BTreeMap::new();
    if experiment.is_none() {
        arms.insert(None, ArmAcc::default());
    }
    for batch in batches {
        let arm = experiment.map(|key| batch_arm(key, &batch.members));
        arms.entry(arm).or_default().add_batch(batch);
    }

    let mut still_open = 0;
    let mut unreadable = 0;
    let arm_reports: Vec<ReviewYieldArmReport> = arms
        .into_iter()
        .map(|(arm, acc)| {
            still_open += acc.still_open();
            unreadable += acc.unreadable();
            acc.into_report(arm)
        })
        .collect();

    let mut notes = Vec::new();
    if still_open > 0 {
        notes.push(format!(
            "{still_open} remediation task(s) have not finished and are counted as still open; the changed-code and \
             merged counts for a recent window are floors."
        ));
    }
    if unreadable > 0 {
        notes.push(format!(
            "{unreadable} stored payload(s) or classification(s) could not be read and are left out of the counts \
             they would have fed."
        ));
    }
    if let Some(key) = experiment
        && !arm_reports.iter().any(|report| {
            matches!(
                report.arm,
                Some(
                    ReviewYieldArm::Treatment
                        | ReviewYieldArm::Control
                        | ReviewYieldArm::Stopped
                        | ReviewYieldArm::Mixed
                )
            )
        })
    {
        notes.push(format!(
            "No review in this window recorded the experiment `{key}`. Check the key, or the window: reviews \
             launched before the experiment ran are not enrolled."
        ));
    }

    ReviewYieldReport::builder()
        .generated_at_epoch_s(generated_at_epoch_s)
        .since_epoch_s(request.since_epoch_s)
        .until_epoch_s(request.until_epoch_s)
        .maybe_experiment(request.experiment.clone())
        .arms(arm_reports)
        .notes(notes)
        .build()
}

/// The arm a member's own execution recorded for `experiment`, or `None` when
/// it carries no arm set to read.
fn member_arm(experiment: &str, member: &YieldMember) -> Option<ReviewYieldArm> {
    let text = member.experiment_arms.as_deref()?;
    let arms: BTreeMap<String, String> = serde_json::from_str(text).ok()?;
    Some(match arms.get(experiment).map(String::as_str) {
        None => ReviewYieldArm::NotEnrolled,
        Some("treatment") => ReviewYieldArm::Treatment,
        Some("control") => ReviewYieldArm::Control,
        Some("stopped") => ReviewYieldArm::Stopped,
        Some(_) => return None,
    })
}

/// The arm a batch ran in: the one every member that recorded an arm set
/// agrees on. A member with no recorded set (never launched, or launched
/// before arms were recorded) has no arm of its own and does not make the
/// batch mixed.
fn batch_arm(experiment: &str, members: &[YieldMember]) -> ReviewYieldArm {
    let mut recorded = members.iter().filter_map(|member| member_arm(experiment, member));
    let Some(first) = recorded.next() else {
        return ReviewYieldArm::NotRecorded;
    };
    if recorded.all(|arm| arm == first) {
        first
    } else {
        ReviewYieldArm::Mixed
    }
}

const ROLE_ORDER: [ReviewBatchMemberRole; 5] = [
    ReviewBatchMemberRole::ClaudeReviewer,
    ReviewBatchMemberRole::CodexReviewer,
    ReviewBatchMemberRole::GrokReviewer,
    ReviewBatchMemberRole::Supervisor,
    ReviewBatchMemberRole::PostMergeReviewer,
];

fn role_index(role: ReviewBatchMemberRole) -> usize {
    ROLE_ORDER.iter().position(|candidate| *candidate == role).unwrap_or(0)
}

/// The member whose output is the batch's decision: the supervisor before
/// merge, the sole reviewer after. The latest attempt that reported.
fn deciding_member(batch: &YieldBatch) -> Option<&YieldMember> {
    batch
        .members
        .iter()
        .rfind(|member| member.status == ReviewBatchMemberStatus::Reported && is_deciding(member.role))
}

fn is_deciding(role: ReviewBatchMemberRole) -> bool {
    matches!(
        role,
        ReviewBatchMemberRole::Supervisor | ReviewBatchMemberRole::PostMergeReviewer
    )
}

/// The serde name of a category, the label reports group by. Pinned to the
/// serialized form by a test.
fn category_label(category: &ReviewFindingCategory) -> &'static str {
    match category {
        ReviewFindingCategory::Correctness => "correctness",
        ReviewFindingCategory::Regression => "regression",
        ReviewFindingCategory::Architecture => "architecture",
        ReviewFindingCategory::Readability => "readability",
        ReviewFindingCategory::Tests => "tests",
        ReviewFindingCategory::EdgeCase => "edgecase",
        ReviewFindingCategory::Duplication => "duplication",
        ReviewFindingCategory::DeferredScope => "deferred_scope",
        ReviewFindingCategory::AgentIsms => "agent_isms",
    }
}

/// Findings counted from a batch's deciding review.
#[derive(Clone, Copy, Default)]
struct FindingCounts {
    total: u64,
    critical_or_high: u64,
}

/// What one batch contributes to the decision and post-merge figures.
struct BatchYield {
    has_verdict: bool,
    findings: FindingCounts,
    passed_gate: bool,
    /// `None` when the deciding member's findings could not be read.
    basis: Option<GateBasis>,
    remediation: Option<YieldRemediation>,
}

impl BatchYield {
    fn of(batch: &YieldBatch) -> Self {
        let findings = deciding_member(batch).and_then(|member| member.findings.as_deref());
        Self {
            has_verdict: batch.verdict.is_some(),
            findings: findings.map_or_else(FindingCounts::default, |found| FindingCounts {
                total: found.len() as u64,
                critical_or_high: found
                    .iter()
                    .filter(|finding| is_critical_or_high(&finding.severity))
                    .count() as u64,
            }),
            passed_gate: batch
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.engine_revision_warranted),
            basis: findings.map(|found| gate_basis(found.iter().map(|f| (&f.severity, &f.category)))),
            remediation: batch.verdict.as_ref().and_then(|verdict| verdict.remediation),
        }
    }
}

fn add_remediation(total: &mut RemediationYield, remediation: &YieldRemediation) {
    total.minted += 1;
    total.still_open += u64::from(remediation.open);
    total.changed_code += u64::from(remediation.changed_code);
    total.merged += u64::from(remediation.merged);
    total.changed_code_and_merged += u64::from(remediation.changed_code && remediation.merged);
}

fn add_post_merge(total: &mut PostMergeYield, batch: &BatchYield) {
    total.batches += 1;
    total.verdicts += u64::from(batch.has_verdict);
    total.findings += batch.findings.total;
    total.critical_or_high_findings += batch.findings.critical_or_high;
    total.passed_gate += u64::from(batch.passed_gate);
    if let Some(remediation) = &batch.remediation {
        add_remediation(&mut total.remediation, remediation);
        if batch.basis == Some(GateBasis::CriticalOrHigh) {
            add_remediation(&mut total.from_critical_or_high, remediation);
        }
    }
}

fn sorted_counts<K: Into<String>>(counts: BTreeMap<K, u64>) -> Vec<LabelCount> {
    let mut counts: Vec<LabelCount> = counts
        .into_iter()
        .map(|(label, count)| LabelCount {
            label: label.into(),
            count,
        })
        .collect();
    counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    counts
}

struct RoleAcc {
    row: ReviewYieldRoleRow,
    categories: BTreeMap<&'static str, u64>,
}

impl RoleAcc {
    fn new(role: ReviewBatchMemberRole) -> Self {
        Self {
            row: ReviewYieldRoleRow::builder().role(role).reviews(0).build(),
            categories: BTreeMap::new(),
        }
    }
}

/// Fold one batch's verdict into a phase's decisions and its gate outcomes.
fn add_decision(
    decisions: &mut ReviewYieldDecisions,
    gate_outcomes: &mut BTreeMap<String, u64>,
    verdict: &YieldVerdict,
    batch: &BatchYield,
) {
    decisions.verdicts += 1;
    *gate_outcomes.entry(verdict.gate_outcome.clone()).or_default() += 1;
    match batch.basis {
        None => decisions.unreadable_verdicts += 1,
        Some(basis) if batch.passed_gate => match basis {
            GateBasis::CriticalOrHigh => decisions.passed_on_critical_or_high += 1,
            GateBasis::AlwaysOnCategoryOnly => decisions.passed_on_always_on_category_only += 1,
            GateBasis::NotPassed => {}
        },
        Some(_) => {}
    }
    decisions.passed_gate += u64::from(batch.passed_gate);
    let Some(remediation) = &batch.remediation else {
        return;
    };
    match remediation.kind {
        RemediationKind::Revision => add_remediation(&mut decisions.revisions, remediation),
        RemediationKind::FollowUp => add_remediation(&mut decisions.followups, remediation),
    }
    match batch.basis {
        Some(GateBasis::CriticalOrHigh) => add_remediation(&mut decisions.from_critical_or_high, remediation),
        Some(GateBasis::AlwaysOnCategoryOnly) => {
            add_remediation(&mut decisions.from_always_on_category_only, remediation);
        }
        Some(GateBasis::NotPassed) | None => {}
    }
}

struct PhaseAcc {
    batches: u64,
    batches_without_verdict: u64,
    roles: [RoleAcc; 5],
    decisions: ReviewYieldDecisions,
    gate_outcomes: BTreeMap<String, u64>,
}

impl PhaseAcc {
    fn new() -> Self {
        Self {
            batches: 0,
            batches_without_verdict: 0,
            roles: ROLE_ORDER.map(RoleAcc::new),
            decisions: ReviewYieldDecisions::default(),
            gate_outcomes: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
struct CriterionAcc {
    pre_merge_batches: u64,
    post_merge: PostMergeYield,
}

#[derive(Default)]
struct DeepAcc {
    pre_merge_batches: u64,
    post_merge_batches: u64,
    no_current_criterion: u64,
    unreadable_classifications: u64,
    criteria: [CriterionAcc; 5],
}

#[derive(Default)]
struct EligibilityAcc {
    stays_eligible: PostMergeYield,
    would_be_excluded: PostMergeYield,
}

struct ArmAcc {
    batches: u64,
    phases: [PhaseAcc; 4],
    deep: DeepAcc,
    eligibility: EligibilityAcc,
    /// Stored reports and verdicts that could not be read, each counted once.
    unreadable_payloads: u64,
}

impl Default for ArmAcc {
    fn default() -> Self {
        Self {
            batches: 0,
            phases: std::array::from_fn(|_| PhaseAcc::new()),
            deep: DeepAcc::default(),
            eligibility: EligibilityAcc::default(),
            unreadable_payloads: 0,
        }
    }
}

impl ArmAcc {
    fn add_batch(&mut self, batch: &YieldBatch) {
        self.batches += 1;
        let yielded = BatchYield::of(batch);
        let phase = ReviewYieldPhase::for_batch(batch.phase, batch.round);
        let phase_index = ReviewYieldPhase::ALL
            .iter()
            .position(|candidate| *candidate == phase)
            .unwrap_or(0);
        let acc = &mut self.phases[phase_index];
        acc.batches += 1;
        match &batch.verdict {
            Some(verdict) => add_decision(&mut acc.decisions, &mut acc.gate_outcomes, verdict, &yielded),
            None => acc.batches_without_verdict += 1,
        }
        for member in batch
            .members
            .iter()
            .filter(|member| member.status == ReviewBatchMemberStatus::Reported)
        {
            let role = &mut acc.roles[role_index(member.role)];
            role.row.reviews += 1;
            let Some(findings) = &member.findings else {
                role.row.unreadable_reports += 1;
                self.unreadable_payloads += 1;
                continue;
            };
            let basis = gate_basis(findings.iter().map(|f| (&f.severity, &f.category)));
            role.row.passing_gate += u64::from(basis != GateBasis::NotPassed);
            role.row.passing_on_always_on_category_only += u64::from(basis == GateBasis::AlwaysOnCategoryOnly);
            for finding in findings {
                role.row.findings += 1;
                match finding.severity {
                    ReviewFindingSeverity::Critical => role.row.by_severity.critical += 1,
                    ReviewFindingSeverity::High => role.row.by_severity.high += 1,
                    ReviewFindingSeverity::Medium => role.row.by_severity.medium += 1,
                    ReviewFindingSeverity::Low => role.row.by_severity.low += 1,
                }
                *role.categories.entry(category_label(&finding.category)).or_default() += 1;
            }
        }
        // A verdict with no reported deciding member has no payload to read at
        // all; one whose member reported unreadably was counted just above.
        if yielded.has_verdict && deciding_member(batch).is_none() {
            self.unreadable_payloads += 1;
        }

        let Some(classification) = &batch.classification else {
            self.deep.unreadable_classifications += 1;
            return;
        };
        if classification.profile == ReviewProfile::Deep {
            let criteria = deep_criteria_for(classification);
            match batch.phase {
                ReviewBatchPhase::PreMerge => self.deep.pre_merge_batches += 1,
                ReviewBatchPhase::PostMerge => self.deep.post_merge_batches += 1,
            }
            if criteria.is_empty() {
                self.deep.no_current_criterion += 1;
            }
            for criterion in criteria {
                let row = &mut self.deep.criteria[criterion_index(criterion)];
                match batch.phase {
                    ReviewBatchPhase::PreMerge => row.pre_merge_batches += 1,
                    ReviewBatchPhase::PostMerge => add_post_merge(&mut row.post_merge, &yielded),
                }
            }
        }
        if batch.phase == ReviewBatchPhase::PostMerge {
            let eligible = post_merge_size_eligible(&DeepInputs::from_classification(classification));
            let side = if eligible {
                &mut self.eligibility.stays_eligible
            } else {
                &mut self.eligibility.would_be_excluded
            };
            add_post_merge(side, &yielded);
        }
    }

    fn still_open(&self) -> u64 {
        self.phases
            .iter()
            .map(|phase| {
                let d = &phase.decisions;
                d.revisions.still_open + d.followups.still_open
            })
            .sum()
    }

    /// Stored payloads and classifications that could not be read.
    fn unreadable(&self) -> u64 {
        self.unreadable_payloads + self.deep.unreadable_classifications
    }

    fn into_report(self, arm: Option<ReviewYieldArm>) -> ReviewYieldArmReport {
        let phases = ReviewYieldPhase::ALL
            .into_iter()
            .zip(self.phases)
            .map(|(phase, acc)| {
                let roles = acc
                    .roles
                    .into_iter()
                    .filter(|role| role.row.reviews > 0)
                    .map(|mut role| {
                        role.row.by_category = sorted_counts(role.categories);
                        role.row
                    })
                    .collect();
                let mut decisions = acc.decisions;
                decisions.gate_outcomes = sorted_counts(acc.gate_outcomes);
                ReviewYieldPhaseReport::builder()
                    .phase(phase)
                    .decisions(decisions)
                    .batches(acc.batches)
                    .batches_without_verdict(acc.batches_without_verdict)
                    .roles(roles)
                    .build()
            })
            .collect();

        let criteria = DeepCriterion::ALL
            .into_iter()
            .zip(self.deep.criteria)
            .map(|(criterion, acc)| {
                DeepCriterionRow::builder()
                    .criterion(criterion)
                    .rule(deep_criterion_rule(criterion))
                    .pre_merge_batches(acc.pre_merge_batches)
                    .post_merge(acc.post_merge)
                    .build()
            })
            .collect();
        let deep = ReviewYieldDeep::builder()
            .pre_merge_batches(self.deep.pre_merge_batches)
            .post_merge_batches(self.deep.post_merge_batches)
            .no_current_criterion(self.deep.no_current_criterion)
            .unreadable_classifications(self.deep.unreadable_classifications)
            .criteria(criteria)
            .build();
        let eligibility = ReviewYieldEligibility::builder()
            .rule(post_merge_size_eligibility_rule())
            .stays_eligible(self.eligibility.stays_eligible)
            .would_be_excluded(self.eligibility.would_be_excluded)
            .build();

        ReviewYieldArmReport::builder()
            .maybe_arm(arm)
            .deep(deep)
            .post_merge_eligibility(eligibility)
            .batches(self.batches)
            .phases(phases)
            .build()
    }
}

fn criterion_index(criterion: DeepCriterion) -> usize {
    DeepCriterion::ALL
        .iter()
        .position(|candidate| *candidate == criterion)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use boss_protocol::SeverityCounts;

    use super::*;
    use crate::parsing::{PrReviewMetadata, classify_pr_review_metadata};

    const PRE: ReviewBatchPhase = ReviewBatchPhase::PreMerge;
    const POST: ReviewBatchPhase = ReviewBatchPhase::PostMerge;

    fn finding(severity: ReviewFindingSeverity, category: ReviewFindingCategory) -> YieldFinding {
        YieldFinding { severity, category }
    }

    fn reported(role: ReviewBatchMemberRole, findings: Vec<YieldFinding>) -> YieldMember {
        YieldMember {
            role,
            status: ReviewBatchMemberStatus::Reported,
            experiment_arms: None,
            findings: Some(findings),
        }
    }

    fn with_arms(mut member: YieldMember, arms: &str) -> YieldMember {
        member.experiment_arms = Some(arms.to_owned());
        member
    }

    fn remediation(kind: RemediationKind, open: bool, changed_code: bool, merged: bool) -> YieldRemediation {
        YieldRemediation {
            kind,
            open,
            changed_code,
            merged,
        }
    }

    fn verdict(outcome: &str, warranted: bool, remediation: Option<YieldRemediation>) -> YieldVerdict {
        YieldVerdict {
            gate_outcome: outcome.to_owned(),
            engine_revision_warranted: warranted,
            remediation,
        }
    }

    fn classified(additions: i64, files: &[&str]) -> ReviewClassification {
        classify_pr_review_metadata(&PrReviewMetadata {
            additions: Some(additions),
            changed_files: Some(files.iter().map(|file| (*file).to_owned()).collect()),
            deletions: Some(0),
        })
    }

    /// A pre-merge batch of one supervisor that reported `findings`.
    fn pre_batch(round: u32, findings: Vec<YieldFinding>, verdict: Option<YieldVerdict>) -> YieldBatch {
        YieldBatch::builder()
            .phase(PRE)
            .round(round)
            .maybe_classification(Some(classified(10, &["src/lib.rs"])))
            .members(vec![reported(ReviewBatchMemberRole::Supervisor, findings)])
            .maybe_verdict(verdict)
            .build()
    }

    fn post_batch(
        classification: ReviewClassification,
        findings: Vec<YieldFinding>,
        verdict: Option<YieldVerdict>,
    ) -> YieldBatch {
        YieldBatch::builder()
            .phase(POST)
            .round(1)
            .maybe_classification(Some(classification))
            .members(vec![reported(ReviewBatchMemberRole::PostMergeReviewer, findings)])
            .maybe_verdict(verdict)
            .build()
    }

    fn request(experiment: Option<&str>) -> ReviewYieldRequest {
        ReviewYieldRequest {
            since_epoch_s: 100,
            until_epoch_s: 200,
            experiment: experiment.map(str::to_owned),
        }
    }

    fn build(experiment: Option<&str>, batches: &[YieldBatch]) -> ReviewYieldReport {
        build_review_yield_report(&request(experiment), 300, batches)
    }

    fn phase(report: &ReviewYieldReport, which: ReviewYieldPhase) -> &ReviewYieldPhaseReport {
        report.arms[0].phases.iter().find(|p| p.phase == which).unwrap()
    }

    use ReviewFindingCategory::{Architecture, Correctness, Readability, Regression};
    use ReviewFindingSeverity::{Critical, High, Low, Medium};

    #[test]
    fn an_empty_window_still_reports_every_phase_in_order() {
        let report = build(None, &[]);
        assert_eq!(
            (report.since_epoch_s, report.until_epoch_s, report.generated_at_epoch_s),
            (100, 200, 300)
        );
        assert_eq!(report.arms.len(), 1);
        let arm = &report.arms[0];
        assert_eq!(arm.arm, None);
        assert_eq!(arm.batches, 0);
        let phases: Vec<_> = arm.phases.iter().map(|p| p.phase).collect();
        assert_eq!(phases, ReviewYieldPhase::ALL.to_vec());
        assert!(
            arm.phases
                .iter()
                .all(|p| p.roles.is_empty() && p.decisions.verdicts == 0)
        );
        assert_eq!(arm.deep.criteria.len(), 5);
        assert!(report.notes.is_empty());
    }

    #[test]
    fn findings_count_by_role_severity_and_category() {
        let batch = YieldBatch::builder()
            .phase(PRE)
            .round(1)
            .members(vec![
                reported(ReviewBatchMemberRole::CodexReviewer, vec![finding(Low, Readability)]),
                reported(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    vec![
                        finding(High, Correctness),
                        finding(Medium, Correctness),
                        finding(Low, Architecture),
                    ],
                ),
                // A member that never reported is not a review.
                YieldMember {
                    role: ReviewBatchMemberRole::GrokReviewer,
                    status: ReviewBatchMemberStatus::Failed,
                    experiment_arms: None,
                    findings: None,
                },
                // A report that cannot be read is a review with unknown findings.
                YieldMember {
                    role: ReviewBatchMemberRole::Supervisor,
                    status: ReviewBatchMemberStatus::Reported,
                    experiment_arms: None,
                    findings: None,
                },
            ])
            .build();
        let report = build(None, &[batch]);
        let roles = &phase(&report, ReviewYieldPhase::PreMergeRound1).roles;

        let order: Vec<_> = roles.iter().map(|row| row.role).collect();
        assert_eq!(
            order,
            [
                ReviewBatchMemberRole::ClaudeReviewer,
                ReviewBatchMemberRole::CodexReviewer,
                ReviewBatchMemberRole::Supervisor
            ],
            "fixed role order, only roles that reported"
        );
        let claude = &roles[0];
        assert_eq!((claude.reviews, claude.findings, claude.unreadable_reports), (1, 3, 0));
        assert_eq!(
            claude.by_severity,
            SeverityCounts {
                critical: 0,
                high: 1,
                medium: 1,
                low: 1
            }
        );
        let categories: Vec<_> = claude.by_category.iter().map(|c| (c.label.as_str(), c.count)).collect();
        assert_eq!(categories, [("correctness", 2), ("architecture", 1)]);
        let supervisor = &roles[2];
        assert_eq!(
            (supervisor.reviews, supervisor.findings, supervisor.unreadable_reports),
            (1, 0, 1)
        );
    }

    #[test]
    fn a_report_is_tested_against_the_gate_on_its_own_findings() {
        let batch = YieldBatch::builder()
            .phase(PRE)
            .round(1)
            .members(vec![
                reported(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    vec![finding(High, Correctness), finding(Low, Regression)],
                ),
                reported(ReviewBatchMemberRole::CodexReviewer, vec![finding(Low, Regression)]),
                reported(
                    ReviewBatchMemberRole::GrokReviewer,
                    vec![finding(Medium, Correctness), finding(Low, Readability)],
                ),
                // Unreadable reports are in neither count.
                YieldMember {
                    role: ReviewBatchMemberRole::Supervisor,
                    status: ReviewBatchMemberStatus::Reported,
                    experiment_arms: None,
                    findings: None,
                },
            ])
            .build();
        let report = build(None, &[batch]);
        let roles = &phase(&report, ReviewYieldPhase::PreMergeRound1).roles;
        let gate = |role: ReviewBatchMemberRole| {
            let row = roles.iter().find(|row| row.role == role).unwrap();
            (row.passing_gate, row.passing_on_always_on_category_only)
        };
        assert_eq!(
            gate(ReviewBatchMemberRole::ClaudeReviewer),
            (1, 0),
            "a high finding carries it"
        );
        assert_eq!(
            gate(ReviewBatchMemberRole::CodexReviewer),
            (1, 1),
            "only a low regression carries it"
        );
        assert_eq!(
            gate(ReviewBatchMemberRole::GrokReviewer),
            (0, 0),
            "a medium correctness finding does not"
        );
        assert_eq!(gate(ReviewBatchMemberRole::Supervisor), (0, 0));
    }

    #[test]
    fn rounds_and_post_merge_land_in_their_own_phase() {
        let batches = [
            pre_batch(1, vec![], None),
            pre_batch(2, vec![], None),
            pre_batch(3, vec![], None),
            pre_batch(7, vec![], None),
            post_batch(classified(2_000, &["src/lib.rs"]), vec![], None),
        ];
        let report = build(None, &batches);
        let counts: Vec<_> = report.arms[0].phases.iter().map(|p| (p.phase, p.batches)).collect();
        assert_eq!(
            counts,
            [
                (ReviewYieldPhase::PreMergeRound1, 1),
                (ReviewYieldPhase::PreMergeRound2, 1),
                (ReviewYieldPhase::PreMergeRound3Plus, 2),
                (ReviewYieldPhase::PostMerge, 1),
            ]
        );
        assert_eq!(report.arms[0].batches, 5);
        assert!(
            report.arms[0]
                .phases
                .iter()
                .all(|p| p.batches_without_verdict == p.batches)
        );
    }

    #[test]
    fn decisions_separate_critical_high_from_always_on_category_only() {
        let batches = [
            // Passed on a high finding; its revision changed code and merged.
            pre_batch(
                1,
                vec![finding(High, Correctness)],
                Some(verdict(
                    "completed_with_findings",
                    true,
                    Some(remediation(RemediationKind::Revision, false, true, true)),
                )),
            ),
            // Passed only because a low regression is always on; the revision is still open.
            pre_batch(
                1,
                vec![finding(Low, Regression), finding(Medium, Correctness)],
                Some(verdict(
                    "completed_with_findings",
                    true,
                    Some(remediation(RemediationKind::Revision, true, false, false)),
                )),
            ),
            // Passed the gate but minted nothing: a stale head.
            pre_batch(
                1,
                vec![finding(Critical, Correctness)],
                Some(verdict("stale_head", true, None)),
            ),
            // Did not pass: a medium correctness finding is below the gate.
            pre_batch(
                1,
                vec![finding(Medium, Correctness)],
                Some(verdict("completed_clean", false, None)),
            ),
            // No verdict at all (a failed batch).
            pre_batch(1, vec![], None),
        ];
        let report = build(None, &batches);
        let round1 = phase(&report, ReviewYieldPhase::PreMergeRound1);
        let d = &round1.decisions;

        assert_eq!((round1.batches, round1.batches_without_verdict), (5, 1));
        assert_eq!(d.verdicts, 4);
        assert_eq!(d.passed_gate, 3);
        assert_eq!(d.passed_on_critical_or_high, 2, "the high and the critical");
        assert_eq!(d.passed_on_always_on_category_only, 1);
        assert_eq!(d.unreadable_verdicts, 0);
        let outcomes: Vec<_> = d.gate_outcomes.iter().map(|o| (o.label.as_str(), o.count)).collect();
        assert_eq!(
            outcomes,
            [
                ("completed_with_findings", 2),
                ("completed_clean", 1),
                ("stale_head", 1)
            ]
        );
        assert_eq!(
            d.revisions,
            RemediationYield {
                minted: 2,
                still_open: 1,
                changed_code: 1,
                merged: 1,
                changed_code_and_merged: 1
            }
        );
        assert_eq!(d.followups, RemediationYield::default());
        assert_eq!(d.from_critical_or_high.minted, 1);
        assert_eq!(d.from_critical_or_high.changed_code_and_merged, 1);
        assert_eq!(d.from_always_on_category_only.minted, 1);
        assert_eq!(d.from_always_on_category_only.still_open, 1);
        assert_eq!(report.notes.len(), 1, "{:?}", report.notes);
        assert!(report.notes[0].starts_with("1 remediation task(s) have not finished"));
    }

    #[test]
    fn a_verdict_whose_findings_cannot_be_read_has_an_unknown_basis_not_a_zero() {
        let mut batch = pre_batch(
            1,
            vec![],
            Some(verdict(
                "completed_with_findings",
                true,
                Some(remediation(RemediationKind::Revision, false, true, true)),
            )),
        );
        batch.members[0].findings = None;
        let report = build(None, &[batch]);
        let d = &phase(&report, ReviewYieldPhase::PreMergeRound1).decisions;
        assert_eq!((d.verdicts, d.passed_gate, d.unreadable_verdicts), (1, 1, 1));
        assert_eq!(
            (d.passed_on_critical_or_high, d.passed_on_always_on_category_only),
            (0, 0)
        );
        // The remediation still happened and is still counted, by kind.
        assert_eq!(d.revisions.minted, 1);
        assert_eq!(
            d.from_critical_or_high.minted + d.from_always_on_category_only.minted,
            0
        );
        // The one unreadable payload is reported once, not once per figure.
        assert!(report.notes[0].starts_with("1 stored payload(s) or classification(s)"));
    }

    #[test]
    fn follow_ups_are_counted_apart_from_revisions() {
        let batch = post_batch(
            classified(2_000, &["src/lib.rs"]),
            vec![finding(Critical, Correctness)],
            Some(verdict(
                "completed_with_findings",
                true,
                Some(remediation(RemediationKind::FollowUp, false, true, true)),
            )),
        );
        let report = build(None, &[batch]);
        let d = &phase(&report, ReviewYieldPhase::PostMerge).decisions;
        assert_eq!(d.followups.minted, 1);
        assert_eq!(d.revisions.minted, 0);
        let roles = &phase(&report, ReviewYieldPhase::PostMerge).roles;
        assert_eq!(roles[0].role, ReviewBatchMemberRole::PostMergeReviewer);
        assert_eq!(roles[0].by_severity.critical, 1);
    }

    #[test]
    fn deep_breakdown_counts_criteria_and_yield_per_criterion() {
        let many_files: Vec<String> = (0..26).map(|i| format!("src/core/file_{i}.rs")).collect();
        let many_files: Vec<&str> = many_files.iter().map(String::as_str).collect();
        let by_lines = classified(1_500, &["src/lib.rs"]);
        let by_files = classified(100, &many_files);
        let both = classified(1_500, &many_files);
        let fixed = |kind, merged| Some(remediation(kind, false, true, merged));
        let passed = |outcome: Option<YieldRemediation>| Some(verdict("completed_with_findings", true, outcome));

        let batches = [
            // Pre-merge Deep batches: lines; files; both.
            YieldBatch::builder()
                .phase(PRE)
                .round(1)
                .maybe_classification(Some(by_lines.clone()))
                .build(),
            YieldBatch::builder()
                .phase(PRE)
                .round(1)
                .maybe_classification(Some(by_files.clone()))
                .build(),
            YieldBatch::builder()
                .phase(PRE)
                .round(1)
                .maybe_classification(Some(both.clone()))
                .build(),
            // A Standard pre-merge batch is not in the breakdown.
            pre_batch(1, vec![], None),
            // Post-merge Deep batches.
            post_batch(
                by_lines,
                vec![finding(High, Correctness)],
                passed(fixed(RemediationKind::FollowUp, true)),
            ),
            post_batch(
                by_files,
                vec![finding(Low, Regression)],
                passed(fixed(RemediationKind::FollowUp, false)),
            ),
            post_batch(both, vec![], Some(verdict("completed_clean", false, None))),
        ];
        let report = build(None, &batches);
        let deep = &report.arms[0].deep;

        assert_eq!((deep.pre_merge_batches, deep.post_merge_batches), (3, 3));
        assert_eq!((deep.no_current_criterion, deep.unreadable_classifications), (0, 0));
        let row = |criterion| deep.criteria.iter().find(|r| r.criterion == criterion).unwrap();

        let lines = row(DeepCriterion::ChangedLines);
        assert_eq!(lines.rule, "more than 1000 changed lines");
        assert_eq!(lines.pre_merge_batches, 2, "by_lines and both");
        assert_eq!(lines.post_merge.batches, 2);
        assert_eq!(lines.post_merge.findings, 1);
        assert_eq!(lines.post_merge.critical_or_high_findings, 1);
        assert_eq!(lines.post_merge.passed_gate, 1);
        assert_eq!(lines.post_merge.remediation.changed_code_and_merged, 1);
        assert_eq!(lines.post_merge.from_critical_or_high.minted, 1);

        let files = row(DeepCriterion::ChangedFiles);
        assert_eq!(files.pre_merge_batches, 2, "by_files and both");
        assert_eq!(files.post_merge.batches, 2);
        assert_eq!(files.post_merge.remediation.minted, 1);
        assert_eq!(files.post_merge.remediation.merged, 0);
        assert_eq!(
            files.post_merge.from_critical_or_high.minted, 0,
            "the files-only follow-up came from a low regression, below the severity bar"
        );

        for untouched in [
            DeepCriterion::Subsystems,
            DeepCriterion::ProductionLanguages,
            DeepCriterion::ComplexityFlags,
        ] {
            assert_eq!(row(untouched).pre_merge_batches, 0, "{untouched}");
            assert_eq!(row(untouched).post_merge.batches, 0, "{untouched}");
        }
        let order: Vec<_> = deep.criteria.iter().map(|r| r.criterion).collect();
        assert_eq!(order, DeepCriterion::ALL.to_vec());
    }

    #[test]
    fn a_stored_deep_batch_that_no_longer_fires_any_criterion_is_counted_apart() {
        // Stored as Deep, but its inputs sit under every current threshold.
        let mut classification = classified(10, &["src/lib.rs"]);
        classification.profile = ReviewProfile::Deep;
        let batch = YieldBatch::builder()
            .phase(PRE)
            .round(1)
            .maybe_classification(Some(classification))
            .build();
        let report = build(None, &[batch]);
        let deep = &report.arms[0].deep;
        assert_eq!((deep.pre_merge_batches, deep.no_current_criterion), (1, 1));
        assert!(deep.criteria.iter().all(|row| row.pre_merge_batches == 0));
    }

    #[test]
    fn an_unreadable_classification_is_counted_and_excluded_from_the_breakdown() {
        let batch = YieldBatch::builder().phase(POST).round(1).build();
        let report = build(None, &[batch]);
        let arm = &report.arms[0];
        assert_eq!(arm.deep.unreadable_classifications, 1);
        assert_eq!(arm.post_merge_eligibility.stays_eligible.batches, 0);
        assert_eq!(arm.post_merge_eligibility.would_be_excluded.batches, 0);
        assert_eq!(arm.batches, 1);
        assert!(report.notes[0].starts_with("1 stored payload(s) or classification(s)"));
    }

    #[test]
    fn post_merge_batches_split_by_the_size_eligibility_rule() {
        let followup = |merged| Some(remediation(RemediationKind::FollowUp, false, true, merged));
        let passed = |outcome| Some(verdict("completed_with_findings", true, outcome));
        // A 400-line PR with two complexity flags is Standard, not Deep, and
        // a flagged Deep PR under the floor is Deep but loses eligibility.
        let flagged_small = classified(300, &["src/auth/x.rs", "src/schema/y.rs"]);
        let big = classified(1_200, &["src/lib.rs"]);
        let batches = [
            post_batch(big, vec![finding(High, Correctness)], passed(followup(true))),
            post_batch(
                flagged_small,
                vec![finding(Critical, Correctness)],
                passed(followup(true)),
            ),
        ];
        let report = build(None, &batches);
        let eligibility = &report.arms[0].post_merge_eligibility;
        assert_eq!(
            eligibility.rule,
            "more than 1000 changed lines or 25 changed files, or 2 or more complexity flags and at least 400 changed lines"
        );
        assert_eq!(eligibility.stays_eligible.batches, 1);
        assert_eq!(eligibility.would_be_excluded.batches, 1);
        // The guardrail figure: merged critical/high follow-ups from PRs the
        // treatment would have excluded.
        assert_eq!(eligibility.would_be_excluded.from_critical_or_high.merged, 1);
        assert_eq!(eligibility.stays_eligible.from_critical_or_high.merged, 1);
    }

    #[test]
    fn rows_split_by_the_recorded_arm_and_each_arm_gets_every_phase() {
        let treated = |arms: &str| {
            YieldBatch::builder()
                .phase(PRE)
                .round(1)
                .maybe_classification(Some(classified(10, &["src/lib.rs"])))
                .members(vec![with_arms(
                    reported(ReviewBatchMemberRole::Supervisor, vec![finding(High, Correctness)]),
                    arms,
                )])
                .maybe_verdict(Some(verdict("completed_with_findings", true, None)))
                .build()
        };
        let batches = [
            treated(r#"{"post_merge_review_slim":"treatment"}"#),
            treated(r#"{"post_merge_review_slim":"treatment"}"#),
            treated(r#"{"post_merge_review_slim":"control"}"#),
            treated(r#"{"other_experiment":"treatment"}"#),
            treated("{}"),
            treated("not json"),
            // No execution, or an execution that recorded nothing.
            pre_batch(1, vec![], None),
            treated(r#"{"post_merge_review_slim":"stopped"}"#),
        ];
        let report = build(Some("post_merge_review_slim"), &batches);

        assert_eq!(report.experiment.as_deref(), Some("post_merge_review_slim"));
        let arms: Vec<_> = report.arms.iter().map(|a| (a.arm.unwrap(), a.batches)).collect();
        assert_eq!(
            arms,
            [
                (ReviewYieldArm::Treatment, 2),
                (ReviewYieldArm::Control, 1),
                (ReviewYieldArm::Stopped, 1),
                (ReviewYieldArm::NotEnrolled, 2),
                (ReviewYieldArm::NotRecorded, 2),
            ]
        );
        for arm in &report.arms {
            assert_eq!(arm.phases.len(), 4);
        }
        assert_eq!(report.arms[0].phases[0].decisions.passed_on_critical_or_high, 2);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn a_batch_is_mixed_only_when_members_that_recorded_an_arm_disagree() {
        let member = |arms: Option<&str>| {
            let mut member = reported(ReviewBatchMemberRole::ClaudeReviewer, vec![]);
            member.experiment_arms = arms.map(str::to_owned);
            member
        };
        let treatment = r#"{"k":"treatment"}"#;
        let control = r#"{"k":"control"}"#;
        assert_eq!(
            batch_arm("k", &[member(Some(treatment)), member(Some(treatment))]),
            ReviewYieldArm::Treatment
        );
        assert_eq!(
            batch_arm("k", &[member(Some(treatment)), member(Some(control))]),
            ReviewYieldArm::Mixed
        );
        assert_eq!(
            batch_arm("k", &[member(Some(treatment)), member(Some(r#"{"k":"stopped"}"#))]),
            ReviewYieldArm::Mixed
        );
        assert_eq!(
            batch_arm("k", &[member(Some(treatment)), member(None)]),
            ReviewYieldArm::Treatment,
            "a member that never recorded a set has no arm of its own"
        );
        assert_eq!(
            batch_arm("k", &[member(None), member(None)]),
            ReviewYieldArm::NotRecorded
        );
        assert_eq!(batch_arm("k", &[]), ReviewYieldArm::NotRecorded);
        assert_eq!(
            batch_arm("k", &[member(Some("{}")), member(Some(treatment))]),
            ReviewYieldArm::Mixed,
            "not running for one launch and treated for another is a mixture"
        );
        assert_eq!(
            batch_arm("k", &[member(Some(r#"{"k":"surprise"}"#))]),
            ReviewYieldArm::NotRecorded,
            "an arm this build does not know is not read as any arm"
        );
    }

    #[test]
    fn an_experiment_nobody_recorded_says_so() {
        let report = build(Some("never_ran"), &[pre_batch(1, vec![], None)]);
        assert_eq!(report.arms.len(), 1);
        assert_eq!(report.arms[0].arm, Some(ReviewYieldArm::NotRecorded));
        assert!(
            report.notes.iter().any(|n| n.contains("`never_ran`")),
            "{:?}",
            report.notes
        );

        let empty = build(Some("never_ran"), &[]);
        assert!(empty.arms.is_empty());
        assert!(empty.notes.iter().any(|n| n.contains("`never_ran`")));
    }

    #[test]
    fn stored_payloads_read_leniently_and_unreadable_ones_are_none() {
        let report = r#"{"batch_id":"b","target_sha":"s","report":{
            "summary":"x","coverage":{"files_inspected":[]},"new_field_from_the_future":1,
            "findings":[{"severity":"high","category":"edgecase","file":"f","title":"t","anything":true},
                        {"severity":"low","category":"agent_isms","file":"g"}]}}"#;
        let found = findings_from_report_payload(report).unwrap();
        assert_eq!(
            found,
            [
                finding(High, ReviewFindingCategory::EdgeCase),
                finding(Low, ReviewFindingCategory::AgentIsms)
            ]
        );
        // A report payload has no verdict and a verdict payload has no report.
        assert!(findings_from_verdict_payload(report).is_none());
        let verdict = r#"{"batch_id":"b","verdict":{"findings":[]}}"#;
        assert_eq!(findings_from_verdict_payload(verdict), Some(vec![]));
        assert!(findings_from_report_payload(verdict).is_none());

        assert!(findings_from_report_payload("not json").is_none());
        assert!(
            findings_from_report_payload(r#"{"report":{"findings":[{"severity":"urgent","category":"tests"}]}}"#)
                .is_none()
        );
        assert!(
            findings_from_report_payload(r#"{"report":{"findings":[{"severity":"low","category":"vibes"}]}}"#)
                .is_none()
        );
        assert!(findings_from_report_payload(r#"{"report":{}}"#).is_none());
    }

    #[test]
    fn category_labels_are_the_serialized_names() {
        for category in [
            ReviewFindingCategory::Correctness,
            ReviewFindingCategory::Regression,
            ReviewFindingCategory::Architecture,
            ReviewFindingCategory::Readability,
            ReviewFindingCategory::Tests,
            ReviewFindingCategory::EdgeCase,
            ReviewFindingCategory::Duplication,
            ReviewFindingCategory::DeferredScope,
            ReviewFindingCategory::AgentIsms,
        ] {
            assert_eq!(serde_json::to_value(&category).unwrap(), category_label(&category));
        }
    }
}
