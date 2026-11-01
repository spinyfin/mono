use crate::work::CiRemediationInsertInput;
use crate::work::test_support::{is_lint_check, validate_queue_ci_evidence as validate};
use boss_protocol::{QueueCiEvidence, QueueCiRun};
const URL: &str = "https://buildkite.com/flunge/mono/builds/20619#failed-job";
fn evidence() -> QueueCiEvidence {
    QueueCiEvidence::builder()
        .main_sha("a".repeat(40))
        .verified_head_sha("b".repeat(40))
        .queue_sha("c".repeat(40))
        .queue_base_sha("d".repeat(40))
        .runs(vec![QueueCiRun {
            check_name: "bazel-build-test".into(),
            source_url: URL.into(),
            command: "bazel test //tools/boss/engine/core:engine_lib_test".into(),
            exit_code: 0,
            output: "11 shards passed".into(),
            coverage: "All failed targets from bazel-build-test, all shards".into(),
        }])
        .build()
}

/// Trunk captures name each failed job `Trunk merge queue: <pipeline>/<label>`
/// (see `ci_watch::trunk_eviction_failures_from_build`), differing by the
/// `#job` fragment. A lint job and a non-Bazel job must be evidenceable.
#[test]
fn trunk_shaped_lint_and_non_bazel_jobs_are_evidenceable() {
    let build = boss_ci_log_reader::TrunkEvictionBuild {
        pipeline_slug: "flunge-ci".into(),
        build_number: "7".into(),
        web_url: "https://buildkite.com/flunge/flunge-ci/builds/7".into(),
        failed_job_ids: vec!["job-lint".into(), "job-fe".into()],
        failed_job_details: vec![":mag: checks-framework", "frontend-tests"]
            .into_iter()
            .map(|label| boss_ci_log_reader::TrunkJobDetails {
                label: label.into(),
                step_key: None,
            })
            .collect(),
    };
    let failures = crate::ci_watch::trunk_eviction_failures_from_build(&build);
    let checks: Vec<_> = failures
        .iter()
        .map(|f| serde_json::json!({"name": f.name, "target_url": f.target_url}))
        .collect();
    assert!(is_lint_check(&failures[0].name));
    assert!(!is_lint_check(&failures[1].name));
    let (_dir, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "Trunk CI");
    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id,
            work_item_id: revision.id.clone(),
            pr_url: "https://github.com/spinyfin/mono/pull/3150".into(),
            pr_number: 3150,
            head_branch: "feature".into(),
            head_sha_at_trigger: "trunk:entry_2@2026-07-27T23:07:13.000Z".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: serde_json::Value::Array(checks).to_string(),
            failure_kind: "trunk_queue_eviction".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    let run = |i: usize, command: &str| QueueCiRun {
        check_name: failures[i].name.clone(),
        source_url: failures[i].target_url.clone(),
        command: command.into(),
        exit_code: 0,
        output: "ok".into(),
        coverage: "whole job".into(),
    };
    let mut proof = evidence();
    proof.runs = vec![
        run(0, "checkleft run"),
        run(1, "npm ci && npm run build && npm run test"),
    ];
    assert!(validate(&attempt, Some(&proof)).is_ok());
    // Each job still needs its own successful, matching verification.
    proof.runs[1] = run(1, "checkleft run");
    assert!(validate(&attempt, Some(&proof)).is_err());
    proof.runs[1] = run(1, "npm ci && make test");
    assert!(validate(&attempt, Some(&proof)).is_err());
    proof.runs = vec![run(0, "checkleft run")];
    assert!(validate(&attempt, Some(&proof)).is_err());
    // A GitHub-style `checks-framework` lint name is also a lint job.
    assert!(is_lint_check("checks-framework"));
    assert!(!is_lint_check("bazel-build-test"));
}
