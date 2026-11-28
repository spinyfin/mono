//! PR-family outcomes per experiment arm (`boss cost experiment`).
//!
//! The Sol comparison (`super::sol`) generalised to any experiment key. The
//! work database projects each PR family into a [`FamilyOutcomeRecord`]; this
//! module decides which arm each family is in, leaves out the families that
//! cannot be compared, and folds the rest into per-arm and per-driver figures.
//! Nothing here reads a database or a flag, so every rule below is unit tested
//! on plain records.
//!
//! ## Enrolment
//!
//! A family enrols in the arm its first launched execution that records the
//! experiment drew. That is also the only arm it can have: a family in which
//! another launched execution recorded the other arm, or recorded nothing for
//! the experiment, is *mixed*, and a family in which any execution recorded
//! `stopped` is *stopped*. Both are left out of the arms and counted, so a
//! flag flipped while a family was in flight can neither move work between the
//! arms nor hide that it happened. A family no execution of which recorded the
//! experiment was never in it and is not counted at all.

use std::collections::{BTreeMap, BTreeSet};

use boss_protocol::{ArmOutcomes, DriverOutcomes, ExperimentOutcomes, FamilyOutcomeMetrics};
use boss_usage_report::RunFold;

use super::fold_record;
pub use boss_engine_storage_values::FamilyOutcomeRecord;

/// Arm label of the one arm of the all-families figure.
pub const ALL_FAMILIES_ARM: &str = "all";
/// Driver label for a family whose first launch recorded no driver.
pub const UNKNOWN_DRIVER: &str = "unknown";

const TREATMENT: &str = "treatment";
const CONTROL: &str = "control";
const STOPPED: &str = "stopped";

/// Where one family stands for one experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Enrolment<'a> {
    /// No execution recorded the experiment: the family was never in it.
    NotEnrolled,
    /// Every launched execution recorded this one arm.
    Arm(&'a str),
    /// Launched executions recorded different arms, or some recorded none.
    Mixed,
    /// Some execution recorded `stopped`.
    Stopped,
}

fn enrolment<'a>(family: &'a FamilyOutcomeRecord) -> Enrolment<'a> {
    let mut recorded = family.launch_arms.iter().flatten().map(String::as_str);
    let Some(first) = recorded.next() else {
        return Enrolment::NotEnrolled;
    };
    if family.launch_arms.iter().flatten().any(|arm| arm == STOPPED) {
        return Enrolment::Stopped;
    }
    // Everything launched must have recorded the arm the family enrolled in;
    // a launch that recorded nothing for the experiment is as mixed as one that
    // recorded the other arm.
    if family.launch_arms.iter().all(|arm| arm.as_deref() == Some(first)) {
        Enrolment::Arm(first)
    } else {
        Enrolment::Mixed
    }
}

/// Fold the families of `experiment` (or every family, when `None`) into
/// per-arm outcomes.
///
/// With an experiment, the arms are `treatment` then `control`, both always
/// present so an empty arm reads as zero families and not as a missing one.
/// Without one, every family is in the single arm [`ALL_FAMILIES_ARM`]:
/// `launch_arms` is not consulted and nothing is excluded.
pub fn build_experiment_outcomes(experiment: Option<&str>, families: &[FamilyOutcomeRecord]) -> ExperimentOutcomes {
    let (mut mixed, mut stopped) = (0u64, 0u64);
    let arm_labels: &[&str] = if experiment.is_some() {
        &[TREATMENT, CONTROL]
    } else {
        &[ALL_FAMILIES_ARM]
    };
    let mut cohorts: BTreeMap<&str, Vec<&FamilyOutcomeRecord>> =
        arm_labels.iter().map(|arm| (*arm, Vec::new())).collect();
    for family in families {
        if experiment.is_none() {
            cohorts.entry(ALL_FAMILIES_ARM).or_default().push(family);
            continue;
        }
        match enrolment(family) {
            Enrolment::NotEnrolled => {}
            Enrolment::Mixed => mixed += 1,
            Enrolment::Stopped => stopped += 1,
            // An arm this build does not know (a future arm value) is not a
            // family it can compare, so it is counted with the mixed ones.
            Enrolment::Arm(arm) => match cohorts.get_mut(arm) {
                Some(cohort) => cohort.push(family),
                None => mixed += 1,
            },
        }
    }
    let arms = arm_labels.iter().map(|arm| arm_outcomes(arm, &cohorts[arm])).collect();
    ExperimentOutcomes {
        experiment: experiment.map(str::to_owned),
        arms,
        mixed_families: mixed,
        stopped_families: stopped,
        notes: notes(experiment),
    }
}

fn arm_outcomes(arm: &str, cohort: &[&FamilyOutcomeRecord]) -> ArmOutcomes {
    let mut by_driver: BTreeMap<&str, Vec<&FamilyOutcomeRecord>> = BTreeMap::new();
    for family in cohort {
        by_driver
            .entry(family.first_driver.as_deref().unwrap_or(UNKNOWN_DRIVER))
            .or_default()
            .push(family);
    }
    ArmOutcomes {
        arm: arm.to_owned(),
        outcomes: metrics(cohort),
        by_driver: by_driver
            .into_iter()
            .map(|(driver, families)| DriverOutcomes {
                driver: driver.to_owned(),
                outcomes: metrics(&families),
            })
            .collect(),
    }
}

fn metrics(cohort: &[&FamilyOutcomeRecord]) -> FamilyOutcomeMetrics {
    let families = cohort.len() as u64;
    let observed = cohort.iter().filter(|family| family.first_ci.is_some()).count() as u64;
    let passes = cohort.iter().filter(|family| family.first_ci == Some(true)).count() as u64;
    let merged: Vec<_> = cohort.iter().filter(|family| family.merged).collect();
    let merged_families = merged.len() as u64;
    let revisions = merged.iter().map(|family| family.revisions).sum();
    let followups = merged.iter().map(|family| family.followups).sum();
    let review_batches = cohort.iter().map(|family| family.review_batches).sum();
    let findings = cohort.iter().map(|family| family.findings).sum();

    let mut cost = RunFold::default();
    let mut gaps = BTreeSet::new();
    let (mut measured_tokens, mut measured_families) = (0u64, 0u64);
    for family in &merged {
        let mut one = RunFold::default();
        for run in &family.runs {
            fold_record(&mut cost, run, false, &mut gaps);
            fold_record(&mut one, run, false, &mut gaps);
        }
        let one = one.finish();
        // A family with no recorded run has no measurement either, so it is
        // unmeasured and not a zero-token family.
        if !family.runs.is_empty() && one.runs_unmeasured == 0 {
            measured_tokens += one.total_tokens as u64;
            measured_families += 1;
        }
    }
    FamilyOutcomeMetrics::builder()
        .families(families)
        .merged_families(merged_families)
        .ci_observed_families(observed)
        .first_attempt_ci_passes(passes)
        .revisions_before_merge(revisions)
        .review_batches(review_batches)
        .review_findings(findings)
        .post_merge_followups(followups)
        .merged_cost(cost.finish())
        .maybe_first_attempt_ci_pass_rate(ratio(passes, observed))
        .maybe_revisions_per_merged_pr(ratio(revisions, merged_families))
        .maybe_review_batches_per_pr(ratio(review_batches, families))
        .maybe_review_findings_per_pr(ratio(findings, families))
        .maybe_post_merge_followups_per_merged_pr(ratio(followups, merged_families))
        .maybe_tokens_per_merged_family(ratio(measured_tokens, measured_families))
        .merged_families_tokens_unmeasured(merged_families - measured_families)
        .build()
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator != 0).then(|| numerator as f64 / denominator as f64)
}

fn notes(experiment: Option<&str>) -> Vec<String> {
    let mut notes = Vec::new();
    match experiment {
        Some(_) => {
            notes.push("A PR family is in the arm its first launched execution that recorded the experiment drew. A family in which another execution recorded the other arm, or nothing for the experiment, is mixed, and a family in which any execution recorded stopped is stopped; both are left out of the arms and counted. A family that never recorded the experiment is not counted.".into());
        }
        None => {
            notes.push("Every PR family whose first launch the family outcome ledger saw is in one arm. Families that launched before the ledger opened are not tracked.".into());
        }
    }
    notes.extend([
        "Per-driver rows group families by the driver of the family's first launched execution; later executions in the family may have run on another driver.".into(),
        "First-attempt CI is the first terminal CI result on the first head published after the family's first launch. Unobserved families are left out of the rate, with the denominator shown.".into(),
        "Revisions and post-merge follow-ups are counted over merged families. Review batches are pre-merge batches started; findings sum accepted automated review verdicts before the merge (all verdicts for an open PR). Both are per family, merged or not.".into(),
        "Tokens per merged family average only merged families whose every run, up to the merge, was measured, with the excluded count reported; unmeasured runs are never priced as zero. Post-merge reviews run after the merge and are not in this figure: read them with `boss cost rows`.".into(),
    ]);
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_storage_values::CostRunRecord;

    fn run(tokens: Option<i64>) -> CostRunRecord {
        CostRunRecord::builder()
            .run_id("run")
            .execution_id("execution")
            .work_item_id("item")
            .work_item_name("work")
            .execution_kind("task_implementation")
            .execution_status("completed")
            .run_status("completed")
            .created_at_epoch_s(1)
            .model("model")
            .maybe_input_tokens(tokens)
            .build()
    }

    /// A merged family that recorded `arms` at its launches, with one run.
    fn family(arms: &[Option<&str>]) -> FamilyOutcomeRecord {
        FamilyOutcomeRecord::builder()
            .family_root("root")
            .launch_arms(arms.iter().map(|arm| arm.map(str::to_owned)).collect())
            .merged(true)
            .revisions(0)
            .review_batches(0)
            .findings(0)
            .followups(0)
            .runs(vec![run(Some(100))])
            .build()
    }

    fn arm<'a>(outcomes: &'a ExperimentOutcomes, name: &str) -> &'a ArmOutcomes {
        outcomes.arms.iter().find(|arm| arm.arm == name).unwrap()
    }

    #[test]
    fn a_family_enrols_in_the_arm_every_launch_recorded() {
        let outcomes = build_experiment_outcomes(
            Some("alpha"),
            &[
                family(&[Some("treatment")]),
                family(&[Some("treatment"), Some("treatment"), Some("treatment")]),
                family(&[Some("control"), Some("control")]),
            ],
        );
        assert_eq!(outcomes.experiment.as_deref(), Some("alpha"));
        assert_eq!(arm(&outcomes, "treatment").outcomes.families, 2);
        assert_eq!(arm(&outcomes, "control").outcomes.families, 1);
        assert_eq!((outcomes.mixed_families, outcomes.stopped_families), (0, 0));
    }

    #[test]
    fn both_arms_are_present_and_empty_reads_as_zero_not_missing() {
        let outcomes = build_experiment_outcomes(Some("alpha"), &[]);
        let names: Vec<_> = outcomes.arms.iter().map(|arm| arm.arm.as_str()).collect();
        assert_eq!(names, ["treatment", "control"]);
        let treatment = &arm(&outcomes, "treatment").outcomes;
        assert_eq!(treatment.families, 0);
        assert_eq!(treatment.first_attempt_ci_pass_rate, None);
        assert_eq!(treatment.tokens_per_merged_family, None);
        assert!(arm(&outcomes, "treatment").by_driver.is_empty());
    }

    #[test]
    fn a_family_that_never_recorded_the_experiment_is_not_counted_anywhere() {
        let outcomes = build_experiment_outcomes(Some("alpha"), &[family(&[None]), family(&[None, None]), family(&[])]);
        assert_eq!(arm(&outcomes, "treatment").outcomes.families, 0);
        assert_eq!(arm(&outcomes, "control").outcomes.families, 0);
        assert_eq!((outcomes.mixed_families, outcomes.stopped_families), (0, 0));
    }

    #[test]
    fn a_family_whose_launches_disagree_is_mixed_and_counted_not_compared() {
        let outcomes = build_experiment_outcomes(
            Some("alpha"),
            &[
                // The other arm at a later launch.
                family(&[Some("treatment"), Some("control")]),
                // A launch that recorded nothing for the experiment, before and after.
                family(&[None, Some("treatment")]),
                family(&[Some("control"), None]),
                // An arm this build cannot compare.
                family(&[Some("holdout")]),
                family(&[Some("control")]),
            ],
        );
        assert_eq!(outcomes.mixed_families, 4);
        assert_eq!(arm(&outcomes, "treatment").outcomes.families, 0);
        assert_eq!(arm(&outcomes, "control").outcomes.families, 1);
    }

    #[test]
    fn any_stopped_execution_leaves_the_family_out_whichever_arm_it_held() {
        let outcomes = build_experiment_outcomes(
            Some("alpha"),
            &[
                family(&[Some("treatment"), Some("stopped")]),
                family(&[Some("stopped")]),
                family(&[Some("control"), Some("stopped"), Some("control")]),
                family(&[Some("treatment")]),
            ],
        );
        assert_eq!(outcomes.stopped_families, 3);
        assert_eq!(outcomes.mixed_families, 0);
        assert_eq!(arm(&outcomes, "treatment").outcomes.families, 1);
        assert_eq!(arm(&outcomes, "control").outcomes.families, 0);
    }

    #[test]
    fn without_an_experiment_every_family_is_in_one_arm_and_none_is_excluded() {
        let outcomes = build_experiment_outcomes(
            None,
            &[
                family(&[]),
                family(&[Some("treatment"), Some("control")]),
                family(&[Some("stopped")]),
            ],
        );
        assert_eq!(outcomes.experiment, None);
        assert_eq!(outcomes.arms.len(), 1);
        assert_eq!(outcomes.arms[0].arm, ALL_FAMILIES_ARM);
        assert_eq!(outcomes.arms[0].outcomes.families, 3);
        assert_eq!((outcomes.mixed_families, outcomes.stopped_families), (0, 0));
    }

    #[test]
    fn families_split_by_the_driver_of_their_first_launch() {
        let on = |driver: Option<&str>, tokens| {
            let mut record = family(&[Some("treatment")]);
            record.first_driver = driver.map(str::to_owned);
            record.runs = vec![run(Some(tokens))];
            record
        };
        let outcomes = build_experiment_outcomes(
            Some("alpha"),
            &[
                on(Some("claude"), 100),
                on(Some("claude"), 300),
                on(Some("codex"), 50),
                on(None, 7),
            ],
        );
        let treatment = arm(&outcomes, "treatment");
        assert_eq!(treatment.outcomes.families, 4);
        let drivers: Vec<_> = treatment
            .by_driver
            .iter()
            .map(|row| {
                (
                    row.driver.as_str(),
                    row.outcomes.families,
                    row.outcomes.tokens_per_merged_family,
                )
            })
            .collect();
        assert_eq!(
            drivers,
            [
                ("claude", 2, Some(200.0)),
                ("codex", 1, Some(50.0)),
                (UNKNOWN_DRIVER, 1, Some(7.0)),
            ]
        );
        // The arm figure is over every family, whatever its driver.
        assert_eq!(treatment.outcomes.tokens_per_merged_family, Some(114.25));
    }

    #[test]
    fn rates_divide_by_their_own_denominators() {
        let mut open = family(&[Some("treatment")]);
        open.merged = false;
        open.runs = vec![];
        open.review_batches = 3;
        open.findings = 4;
        open.first_ci = Some(false);
        open.revisions = 9; // not counted: the family has not merged
        let mut merged = family(&[Some("treatment")]);
        merged.revisions = 2;
        merged.review_batches = 1;
        merged.findings = 2;
        merged.followups = 3;
        merged.first_ci = Some(true);
        let mut unobserved = family(&[Some("treatment")]);
        unobserved.followups = 1;
        let outcomes = build_experiment_outcomes(Some("alpha"), &[open, merged, unobserved]);
        let treatment = &arm(&outcomes, "treatment").outcomes;
        assert_eq!(treatment.families, 3);
        assert_eq!(treatment.merged_families, 2);
        // CI: two observed, one pass; the unobserved family is not in the rate.
        assert_eq!(
            (treatment.ci_observed_families, treatment.first_attempt_ci_passes),
            (2, 1)
        );
        assert_eq!(treatment.first_attempt_ci_pass_rate, Some(0.5));
        // Revisions and follow-ups: over merged families only.
        assert_eq!(treatment.revisions_before_merge, 2);
        assert_eq!(treatment.revisions_per_merged_pr, Some(1.0));
        assert_eq!(treatment.post_merge_followups, 4);
        assert_eq!(treatment.post_merge_followups_per_merged_pr, Some(2.0));
        // Batches and findings: over every family, merged or not.
        assert_eq!(treatment.review_batches_per_pr, Some(4.0 / 3.0));
        assert_eq!(treatment.review_findings_per_pr, Some(2.0));
    }

    #[test]
    fn an_unmeasured_family_is_counted_and_does_not_hide_the_measured_ones() {
        let with_tokens = |tokens: Option<i64>| {
            let mut record = family(&[Some("control")]);
            record.runs = vec![run(Some(100)), run(tokens)];
            record
        };
        let outcomes = build_experiment_outcomes(
            Some("alpha"),
            &[with_tokens(Some(100)), with_tokens(Some(300)), with_tokens(None)],
        );
        let control = &arm(&outcomes, "control").outcomes;
        assert_eq!(control.merged_families, 3);
        assert_eq!(control.tokens_per_merged_family, Some(300.0));
        assert_eq!(control.merged_families_tokens_unmeasured, 1);
        assert_eq!(control.merged_cost.runs_unmeasured, 1);
    }

    #[test]
    fn a_merged_family_with_no_recorded_run_is_unmeasured_not_zero_tokens() {
        let mut record = family(&[Some("control")]);
        record.runs = vec![];
        let outcomes = build_experiment_outcomes(Some("alpha"), &[record]);
        let control = &arm(&outcomes, "control").outcomes;
        assert_eq!(control.tokens_per_merged_family, None);
        assert_eq!(control.merged_families_tokens_unmeasured, 1);
    }

    #[test]
    fn an_open_family_costs_nothing_to_the_token_figure() {
        let mut open = family(&[Some("control")]);
        open.merged = false;
        open.runs = vec![run(None)];
        let outcomes = build_experiment_outcomes(Some("alpha"), &[open]);
        let control = &arm(&outcomes, "control").outcomes;
        assert_eq!(control.families, 1);
        assert_eq!(control.merged_families, 0);
        assert_eq!(control.merged_families_tokens_unmeasured, 0);
        assert_eq!(control.tokens_per_merged_family, None);
    }
}
