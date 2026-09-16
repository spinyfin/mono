//! Recovery prompts use the coordinator's durable result, never scratch state.
use super::{
    ExecutionPromptParams, base_execution, chore_with_pr, chore_without_pr, compose_execution_prompt,
    revision_execution,
};

fn render(
    execution: &crate::work::WorkExecution,
    item: &crate::work::WorkItem,
    recovery: Option<&(String, bool)>,
    path: &std::path::Path,
) -> String {
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(execution)
            .work_item(item)
            .workspace_path(path)
            .maybe_bookmark_recovery(recovery)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    )
}

#[test]
fn recovery_preserves_existing_pr_and_requires_revalidation() {
    for has_work in [true, false] {
        let result = ("exec_prior".into(), has_work);
        let prompt = render(
            &base_execution(),
            &chore_with_pr("https://github.com/org/repo/pull/42"),
            Some(&result),
            std::path::Path::new("/nonexistent/scratch"),
        );
        assert!(prompt.contains("## RESUME EXISTING PR"));
        assert!(prompt.contains("## EXECUTION BOOKMARK RECOVERY"));
        assert!(prompt.contains("exec_prior"));
        assert!(prompt.contains("Do NOT run `jj new main`"));
        assert!(prompt.contains("Re-run the required build and tests in your own leased workspace"));
        assert!(!prompt.contains("lands you on the PR branch"));
        assert!(prompt.find("## EXECUTION BOOKMARK RECOVERY").unwrap() < prompt.find("Execution context:").unwrap());
        if has_work {
            assert!(prompt.contains("Unpublished changes and their history were recovered"));
        } else {
            assert!(prompt.contains("no unpublished changes to recover"));
        }
    }
}

#[test]
fn revision_recovery_never_offers_checkout_that_discards_inherited_work() {
    let execution = revision_execution("https://github.com/org/repo/pull/77");
    let task = super::revision_task_with_created_via(None, "operator");
    let result = ("exec_prior_revision".into(), true);
    let prompt = render(
        &execution,
        &task,
        Some(&result),
        std::path::Path::new("/nonexistent/scratch"),
    );
    assert!(prompt.contains("recorded execution bookmark"));
    assert!(prompt.contains("Stay at `@`"));
    assert!(!prompt.contains("The engine pre-positioned this workspace via"));
    assert!(!prompt.contains("**Fallback**"));
    assert!(prompt.contains("keep it advanced locally"));
    assert!(prompt.contains("pr update --branch"));
    assert!(prompt.contains("--json headRefName --jq .headRefName"));
    assert!(!prompt.contains("jj log -r 'parents(@)'"));
    let empty = ("exec_empty_revision".into(), false);
    let prompt = render(
        &execution,
        &task,
        Some(&empty),
        std::path::Path::new("/nonexistent/scratch"),
    );
    assert!(prompt.contains("The engine pre-positioned this workspace via"));
    assert!(prompt.contains("workspace goto --pr 77"));
}

#[test]
fn merge_cancelled_review_followup_reports_bookmark_recovery_without_workspace_affinity() {
    let mut execution = base_execution();
    execution.allow_dirty = true;
    execution.prefer_is_soft = true;
    execution.preferred_workspace_id = Some("released".into());
    execution.cube_workspace_id = Some("fresh".into());
    let recovery = ("exec_cancelled_revision".into(), true);
    let prompt = render(
        &execution,
        &super::review_followup(),
        Some(&recovery),
        std::path::Path::new("/nonexistent/scratch"),
    );
    assert!(prompt.contains("exec_cancelled_revision"));
    assert!(prompt.contains("Unpublished changes and their history were recovered"));
    assert!(prompt.contains("The old workspace was not used"));
    assert!(!prompt.contains("fresh-workspace fallback"));
}

#[test]
fn workspace_identity_and_legacy_markers_cannot_claim_recovery() {
    use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};
    let workspace = tempfile::tempdir().unwrap();
    let mut execution = base_execution();
    execution.allow_dirty = true;
    execution.prefer_is_soft = true;
    execution.preferred_workspace_id = Some("reused".into());
    execution.cube_workspace_id = Some("reused".into());
    for source in [
        RecoverySource::BlockedInPlace,
        RecoverySource::BlockedFresh,
        RecoverySource::CubeInPlace,
        RecoverySource::Patch,
    ] {
        RecoveryReport {
            for_execution_id: execution.id.clone(),
            from_execution_id: "foreign".into(),
            source,
            applied: None,
            patch_error: Some("untrusted legacy patch".into()),
        }
        .write(workspace.path())
        .unwrap();
        let prompt = render(&execution, &chore_without_pr(), None, workspace.path());
        assert!(!prompt.contains("foreign"));
        assert!(!prompt.contains("untrusted legacy patch"));
        assert!(!prompt.contains("re-leased without a reset"));
        assert!(!prompt.contains("## EXECUTION BOOKMARK RECOVERY"));
        assert!(!prompt.contains("## STARTUP RECOVERY"));
        assert!(!prompt.contains("@origin"));
    }
}
