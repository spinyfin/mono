use super::*;

fn prompt_for_prior_branch(prior_branch_exists: bool) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    boss_engine_recovery::recovery_apply::RecoveryReport {
        for_execution_id: "exec_abc123_01".to_owned(),
        from_execution_id: "exec_prior123_09".to_owned(),
        source: boss_engine_recovery::recovery_apply::RecoverySource::CubeInPlace,
        applied: None,
        patch_error: None,
    }
    .write(ws.path())
    .unwrap();
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .prior_branch_exists(prior_branch_exists)
            .build(),
    )
}

#[test]
fn recovery_block_resumes_a_verified_prior_branch() {
    let prompt = prompt_for_prior_branch(true);
    assert!(prompt.contains("### Prior pushed branch"));
    assert!(prompt.contains("jj edit boss/exec_prior123_09@origin"));
}

#[test]
fn recovery_block_omits_resume_command_when_prior_branch_was_not_pushed() {
    let prompt = prompt_for_prior_branch(false);
    assert!(prompt.contains("### Prior pushed branch"));
    assert!(prompt.contains("was not pushed to the remote"));
    assert!(!prompt.contains("jj edit boss/exec_prior123_09@origin"));
}
