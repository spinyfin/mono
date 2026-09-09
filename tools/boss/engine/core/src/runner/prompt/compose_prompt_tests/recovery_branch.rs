use boss_protocol::BranchNaming;

use super::*;

fn prompt_for_prior_branch(prior_branch: Option<PriorBranchProbe>) -> String {
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
            .maybe_prior_branch(prior_branch)
            .build(),
    )
}

#[test]
fn recovery_block_resumes_a_verified_prior_branch() {
    let prompt = prompt_for_prior_branch(Some(PriorBranchProbe {
        exists: true,
        branch: "boss/exec_prior123_09".to_owned(),
    }));
    assert!(prompt.contains("### Prior pushed branch"));
    assert!(prompt.contains("jj edit boss/exec_prior123_09@origin"));
}

#[test]
fn recovery_block_omits_resume_command_when_prior_branch_was_not_pushed() {
    let prompt = prompt_for_prior_branch(Some(PriorBranchProbe {
        exists: false,
        branch: "boss/exec_prior123_09".to_owned(),
    }));
    assert!(prompt.contains("### Prior pushed branch"));
    assert!(prompt.contains("was not pushed to the remote"));
    assert!(!prompt.contains("jj edit boss/exec_prior123_09@origin"));
}

/// The predecessor and successor can have diverged `branch_naming` settings
/// (they are frozen per execution — see `expected_branch_name`'s doc
/// comment). The rendered `jj edit` command must use the branch the probe
/// actually verified against the remote (the predecessor-derived one),
/// never a branch recomputed from the successor's own naming — recomputing
/// from `execution` here is exactly the bug this test pins.
#[test]
fn recovery_block_uses_the_probed_branch_not_the_successors_naming() {
    let successor_derived_branch =
        crate::completion::expected_branch_name("exec_prior123_09", &BranchNaming::BossExecPrefix, None);
    let predecessor_derived_branch = crate::completion::expected_branch_name(
        "exec_prior123_09",
        &BranchNaming::CustomPrefix {
            prefix: "custom".to_owned(),
        },
        None,
    );
    assert_ne!(successor_derived_branch, predecessor_derived_branch);

    let prompt = prompt_for_prior_branch(Some(PriorBranchProbe {
        exists: true,
        branch: predecessor_derived_branch.clone(),
    }));

    assert!(
        prompt.contains(&format!("jj edit {predecessor_derived_branch}@origin")),
        "resume command must use the predecessor-derived branch the probe verified:\n{prompt}",
    );
    assert!(
        !prompt.contains(&format!("jj edit {successor_derived_branch}@origin")),
        "resume command must NOT be recomputed from the successor's own branch naming:\n{prompt}",
    );
}
