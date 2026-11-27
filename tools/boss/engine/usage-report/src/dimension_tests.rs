use super::*;

fn experiment_records() -> UsageWindowRecords {
    let mut runs: Vec<_> = [
        ("root", Some(r#"{"alpha":"treatment","beta":"control"}"#)),
        ("revision", Some(r#"{"alpha":"treatment"}"#)),
        ("control", Some(r#"{"alpha":"control"}"#)),
        ("stopped", Some(r#"{"alpha":"stopped"}"#)),
        ("empty", Some("{}")),
        ("old", None),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (id, arms))| {
        let mut r = tokens(run(id, id, id, BASE_EPOCH_S + i as i64), 10, 2, 0, 0);
        r.experiment_arms = arms.map(str::to_owned);
        r
    })
    .collect();
    // Taskless guides share the PR family rather than counting their comparison id.
    let mut guide = runs[0].clone();
    guide.run_id = "guide".into();
    guide.execution_id = "guide".into();
    guide.work_item_id = "comparison".into();
    guide.execution_kind = "pr_review_guide".into();
    guide.experiment_family_id = Some("root".into());
    runs.push(guide);
    records(runs, vec![task("root", "root", 1), task("revision", "root", 1)])
}

#[test]
fn selected_experiment_groups_all_five_states_and_counts_distinct_families() {
    let mut q = window();
    q.experiment = Some("alpha".into());
    q.group_by = vec![UsageDimension::ExperimentArm];
    let report = build_usage_rows_report(&q, &experiment_records(), None);
    let counts: std::collections::BTreeMap<_, _> = report
        .groups
        .iter()
        .map(|g| (g.key[0].value.as_str(), g.measurement.runs_total))
        .collect();
    assert_eq!(
        counts,
        [
            ("control", 1),
            ("not_enrolled", 1),
            ("not_recorded", 1),
            ("stopped", 1),
            ("treatment", 3)
        ]
        .into()
    );
    let observed = &report.observed_experiments[0];
    assert_eq!(observed.experiment, "alpha");
    let treated = observed.arms.iter().find(|a| a.arm == "treatment").unwrap();
    assert_eq!((treated.runs, treated.families), (3, 1));
    assert_eq!(report.observed_experiments[1].experiment, "beta");
    assert_eq!(report.overall.runs_total, 7);
}

#[test]
fn filters_and_cap_do_not_change_observed_experiments_and_capped_comparisons_say_so() {
    let records = experiment_records();
    let mut q = window();
    let observations = build_usage_rows_report(&q, &records, None).observed_experiments;
    q.experiment = Some("alpha".into());
    q.filters = vec![UsageFilter {
        dimension: UsageDimension::ExperimentArm,
        values: vec!["treatment".into()],
    }];
    q.group_by = vec![UsageDimension::ExperimentArm, UsageDimension::PipelineStage];
    q.run_cap = 1;
    let report = build_usage_rows_report(&q, &records, None);
    assert_eq!(report.observed_experiments, observations);
    assert_eq!(report.overall.runs_total, 1);
    assert!(report.truncated);
    assert!(
        report
            .coverage
            .notes
            .iter()
            .any(|n| n.code == "experiment_comparison_capped")
    );
    assert_eq!(report.groups[0].key[0].value, "treatment");
    assert_eq!(report.groups[0].subgroups[0].key[1].value, "filed_work");
    q.run_cap = 50;
    let report = build_usage_rows_report(&q, &records, None);
    assert!(
        !report
            .coverage
            .notes
            .iter()
            .any(|n| n.code == "experiment_comparison_capped")
    );
    q.since_epoch_s = BASE_EPOCH_S + 1;
    let report = build_usage_rows_report(&q, &records, None);
    assert_eq!(
        report.observed_experiments.len(),
        1,
        "beta only appears before the window"
    );
}

#[test]
fn whole_arm_sets_are_canonical_and_empty_is_distinct_from_not_recorded() {
    use crate::dimensions::experiment_value;
    assert_eq!(
        experiment_value(Some(r#"{ "beta": "control", "alpha": "treatment" }"#), None),
        r#"{"alpha":"treatment","beta":"control"}"#
    );
    assert_eq!(experiment_value(Some("{}"), None), "{}");
    assert_eq!(experiment_value(None, None), "not_recorded");
    assert_eq!(experiment_value(Some("broken"), Some("alpha")), "not_recorded");
    assert_eq!(experiment_value(Some("{}"), Some("retired")), "not_enrolled");
}

#[test]
fn pipeline_stages_follow_kind_role_and_provenance() {
    use crate::dimensions::pipeline_stage;
    let mut t = task("revision", "root", 1);
    for (source, expected) in [
        ("cli", "filed_work"),
        ("pr_review:review", "review_finding_revision"),
        ("pr_review:post_merge:proposal", "post_merge_follow_up"),
        ("ci-fix:attempt", "ci_fix_revision"),
        ("merge-conflict:attempt", "conflict_revision"),
        ("attention_conflict_recovery:revision", "conflict_revision"),
        ("progression:execution", "progression_minted_work"),
        ("future:source", "unclassified"),
    ] {
        t.created_via = Some(source.into());
        assert_eq!(
            pipeline_stage("revision_implementation", None, Some(&t)),
            expected,
            "{source}"
        );
        assert_eq!(pipeline_stage("pr_review", None, Some(&t)), "pre_merge_review");
    }
    for (role, expected) in [
        ("supervisor", "review_supervisor"),
        ("post_merge_reviewer", "post_merge_review"),
        ("claude_reviewer", "pre_merge_review"),
        ("codex_reviewer", "pre_merge_review"),
        ("grok_reviewer", "pre_merge_review"),
        ("future_role", "unclassified"),
    ] {
        assert_eq!(pipeline_stage("pr_review", Some(role), Some(&t)), expected);
    }
    for (kind, expected) in [
        ("task_implementation", "filed_work"),
        ("chore_implementation", "filed_work"),
        ("investigation_implementation", "filed_work"),
        ("pr_review_guide", "review_guide"),
        ("progression", "progression"),
        ("ci_remediation", "ci_fix_revision"),
        ("conflict_resolution", "conflict_revision"),
        ("automation_triage", "automation_triage"),
        ("answer_agent", "answer_agent"),
        ("product_design", "product_design"),
        ("project_design", "project_design"),
        ("future_kind", "unclassified"),
    ] {
        assert_eq!(pipeline_stage(kind, None, None), expected, "{kind}");
    }
}

#[test]
fn series_experiment_filter_applies_to_runs_and_runless_completed_executions() {
    let records = experiment_records();
    let q = UsageSeriesQuery::builder()
        .since_epoch_s(BASE_EPOCH_S - 1)
        .until_epoch_s(BASE_EPOCH_S + 100)
        .generated_at_epoch_s(BASE_EPOCH_S + 100)
        .capture_start_epoch_s(0)
        .completed_at_start_epoch_s(0)
        .retention_max_age_secs(RETENTION)
        .experiment("alpha")
        .group_by(UsageDimension::PipelineStage)
        .filters(vec![UsageFilter {
            dimension: UsageDimension::ExperimentArm,
            values: vec!["treatment".into()],
        }])
        .build();
    let finished = |id: &str, arms: &str| {
        UsageExecutionRecord::builder()
            .execution_id(id)
            .work_item_id(id)
            .execution_kind("pr_review")
            .review_member_role("supervisor")
            .execution_status("completed")
            .execution_created_at_epoch_s(BASE_EPOCH_S)
            .execution_finished_at_epoch_s(BASE_EPOCH_S + 1)
            .experiment_arms(arms)
            .build()
    };
    let report = build_usage_series_report(
        &q,
        &UsageSeriesRecords {
            runs: records.runs,
            tasks: records.tasks,
            finished_executions: vec![
                finished("treated", r#"{"alpha":"treatment"}"#),
                finished("control", r#"{"alpha":"control"}"#),
            ],
        },
        None,
    );
    assert_eq!(
        report
            .buckets
            .iter()
            .flat_map(|b| &b.cells)
            .map(|c| c.measurement.runs_total)
            .sum::<u32>(),
        3
    );
    assert_eq!(
        report
            .buckets
            .iter()
            .flat_map(|b| &b.cells)
            .map(|c| c.executions_completed)
            .sum::<u32>(),
        1
    );
    assert!(report.groups.iter().any(|g| g == "review_supervisor"));
    assert_eq!(report.observed_experiments.len(), 2);
}
