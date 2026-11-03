//! The conflict-resolution stop conditions: porting is normal conflict work and
//! `architectural_mismatch` is a high bar that closes once the repo owner has
//! decided.

use super::compose_prompt_tests::{revision_execution, revision_task_with_created_via, sample_conflict_attempt};
use super::*;

fn conflict_prompt(attempt: &crate::work::ConflictResolution) -> String {
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .conflict_attempt(attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    )
}

#[test]
fn conflict_prompt_names_refactors_as_normal_work_and_sets_a_high_bar_for_a_mismatch() {
    let prompt = conflict_prompt(&sample_conflict_attempt());
    assert!(
        prompt.contains("### Porting is part of conflict resolution"),
        "{prompt}"
    );
    for normal_work in [
        "files moved or deleted on `main`",
        "modules or crates split or renamed",
        "helpers that became private or moved behind a new crate boundary",
        "migrations that must move to another crate's sequence",
        "every commit of the PR conflicting",
        "never grounds to stop",
        "Effort is not a reason to stop",
        "effort-escalation marker",
    ] {
        assert!(prompt.contains(normal_work), "missing `{normal_work}`:\n{prompt}");
    }
    for bar in [
        "adds a feature to a subsystem that `main` deleted completely",
        "never meets this bar",
        "--reason architectural_mismatch",
        "--basis <subsystem_deleted|concept_redesigned|behaviour_contradiction>",
        "--attempted-port",
        "--contradiction",
        "--recommendation",
    ] {
        assert!(prompt.contains(bar), "missing `{bar}`:\n{prompt}");
    }
    // A removed abstraction alone is not offered as grounds to stop.
    assert!(
        !prompt.contains("the upstream removed an abstraction this PR"),
        "{prompt}"
    );
}

#[test]
fn conflict_prompt_does_not_offer_a_mismatch_exit_once_the_repo_owner_has_decided() {
    let mut attempt = sample_conflict_attempt();
    attempt.mismatch_exit_barred = true;
    let prompt = conflict_prompt(&attempt);
    assert!(
        prompt.contains("`architectural_mismatch` is **not** an allowed exit for this attempt"),
        "{prompt}",
    );
    assert!(!prompt.contains("--basis <subsystem_deleted"), "{prompt}");
    // The other stop conditions and the porting guidance remain.
    assert!(prompt.contains("`product_decision_required`"));
    assert!(prompt.contains("### Porting is part of conflict resolution"));
}
