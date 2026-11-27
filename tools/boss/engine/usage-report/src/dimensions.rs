//! Launch-record experiment attribution and execution pipeline stages.
use std::collections::{BTreeMap, BTreeSet};

use boss_feature_flags::ExperimentArmSet;
use boss_protocol::{
    CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_CONFLICT_RECOVERY_PREFIX, CREATED_VIA_MERGE_CONFLICT_PREFIX,
    CREATED_VIA_PR_REVIEW_POST_MERGE_PREFIX, CREATED_VIA_PR_REVIEW_PREFIX, CREATED_VIA_PROGRESSION_PREFIX,
    ExecutionKind, ReviewBatchMemberRole, UsageExperimentArm, UsageObservedExperiment,
};

use crate::classify::RunFacts;
use crate::record::UsageTaskRecord;

pub(crate) fn arm_value(arms: Option<&ExperimentArmSet>, key: &str) -> &'static str {
    match arms {
        None => "not_recorded",
        Some(arms) => arms.get(key).map_or("not_enrolled", |arm| arm.as_str()),
    }
}

pub(crate) fn experiment_value(recorded: Option<&str>, experiment: Option<&str>) -> String {
    let arms = recorded.and_then(|s| ExperimentArmSet::decode(s).ok());
    match experiment {
        Some(key) => arm_value(arms.as_ref(), key).to_owned(),
        None => arms.map_or_else(|| "not_recorded".to_owned(), |arms| arms.encode()),
    }
}

/// Review role wins over task provenance: a review of a CI fix is still a review.
/// Unknown execution kinds or review roles remain unclassified.
pub(crate) fn pipeline_stage(kind: &str, role: Option<&str>, task: Option<&UsageTaskRecord>) -> &'static str {
    let Ok(kind) = kind.parse::<ExecutionKind>() else {
        return "unclassified";
    };
    match kind {
        ExecutionKind::PrReview => match role.map(str::parse::<ReviewBatchMemberRole>) {
            Some(Ok(ReviewBatchMemberRole::Supervisor)) => "review_supervisor",
            Some(Ok(ReviewBatchMemberRole::PostMergeReviewer)) => "post_merge_review",
            None
            | Some(Ok(
                ReviewBatchMemberRole::ClaudeReviewer
                | ReviewBatchMemberRole::CodexReviewer
                | ReviewBatchMemberRole::GrokReviewer,
            )) => "pre_merge_review",
            Some(Err(_)) => "unclassified",
        },
        ExecutionKind::PrReviewGuide => "review_guide",
        ExecutionKind::Progression => "progression",
        ExecutionKind::CiRemediation => "ci_fix_revision",
        ExecutionKind::ConflictResolution => "conflict_revision",
        ExecutionKind::AutomationTriage => "automation_triage",
        ExecutionKind::AnswerAgent => "answer_agent",
        ExecutionKind::ProjectDesign => "project_design",
        ExecutionKind::ProductDesign => "product_design",
        ExecutionKind::TaskImplementation
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::InvestigationImplementation => {
            let source = task.and_then(|task| task.created_via.as_deref()).unwrap_or("");
            if source.starts_with(CREATED_VIA_PR_REVIEW_POST_MERGE_PREFIX) {
                "post_merge_follow_up"
            } else if source.starts_with(CREATED_VIA_PR_REVIEW_PREFIX) {
                "review_finding_revision"
            } else if source.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
                "ci_fix_revision"
            } else if source.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
                || source.starts_with(CREATED_VIA_CONFLICT_RECOVERY_PREFIX)
            {
                "conflict_revision"
            } else if source.starts_with(CREATED_VIA_PROGRESSION_PREFIX) {
                "progression_minted_work"
            } else if source.is_empty() || boss_protocol::is_known_created_via(source) {
                "filed_work"
            } else {
                "unclassified"
            }
        }
    }
}

/// Counts are window-wide and independent of the selected experiment, filters
/// and row cap. Distinct families are counted within each arm, never summed
/// across arms to pretend that a changed assignment is a new family.
pub(crate) fn observed_experiments<'a>(
    runs: impl IntoIterator<Item = &'a RunFacts<'a>>,
) -> Vec<UsageObservedExperiment> {
    let runs: Vec<_> = runs.into_iter().collect();
    let keys: BTreeSet<_> = runs
        .iter()
        .filter_map(|f| f.experiment_arms.as_ref())
        .flat_map(|arms| arms.iter().map(|(key, _)| key))
        .collect();
    keys.into_iter()
        .map(|key| {
            let mut counts: BTreeMap<&str, (u32, BTreeSet<&str>)> = BTreeMap::new();
            for facts in &runs {
                let arm = arm_value(facts.experiment_arms.as_ref(), key);
                let family = facts
                    .rec
                    .experiment_family_id
                    .as_deref()
                    .or_else(|| facts.task.map(|t| t.chain_root_id.as_str()))
                    .unwrap_or(&facts.rec.work_item_id);
                let entry = counts.entry(arm).or_default();
                entry.0 += 1;
                entry.1.insert(family);
            }
            UsageObservedExperiment {
                experiment: key.to_owned(),
                arms: counts
                    .into_iter()
                    .map(|(arm, (runs, families))| UsageExperimentArm {
                        arm: arm.to_owned(),
                        runs,
                        families: families.len() as u32,
                    })
                    .collect(),
            }
        })
        .collect()
}
