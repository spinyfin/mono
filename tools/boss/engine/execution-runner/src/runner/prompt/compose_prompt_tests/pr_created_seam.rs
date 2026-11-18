//! Seam-on prompt coverage for the PR-created declaration. Split out of
//! `compose_prompt_tests.rs` so that file stays under the `file/size` cap.

use super::{ExecutionPromptParams, base_execution, chore_with_pr, chore_without_pr, compose_execution_prompt};

#[test]
fn pr_created_seam_prompt_keeps_declaration_and_printed_url_in_all_pr_paths() {
    // Cover both acceptance-criterion branches plus `pr_terminal_directive`:
    // the declaration is the primary seam channel, while the printed URL is
    // deliberately retained as soak-period redundancy.
    for work_item in [chore_without_pr(), chore_with_pr("https://github.com/org/repo/pull/42")] {
        let execution = base_execution();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .review_signal_checks(|_| &[])
                .execution(&execution)
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .pr_created_proposals_seam_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("propose pr-created --url"),
            "seam-on prompt must teach the declaration in every PR path:\n{prompt}",
        );
        assert!(
            prompt.contains("the final-response URL in the generated rules is its fallback"),
            "seam-on prompt must retain the printed-URL redundancy:\n{prompt}",
        );
        assert!(
            prompt.contains("that declaration IS the terminal action"),
            "seam-on prompt must update the terminal-action directive:\n{prompt}",
        );
    }
}
