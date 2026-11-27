//! The `batched_search` treatment: one worker rule, for Standard workers only.

use super::{
    ExecutionPromptParams, base_execution, chore_with_pr, chore_without_pr, compose_execution_prompt, design_task,
    sample_ci_attempt,
};
use crate::prose_vocabulary::assert_no_forbidden_prose;
use crate::work::{WorkExecution, WorkItem};
use crate::worker_setup::{WorkerKind, worker_kind_for_execution};
use boss_protocol::ExecutionKind;

const RULE: &str = "**Batch independent tool calls.**";
const PR: &str = "https://github.com/org/repo/pull/42";

fn compose(execution: &WorkExecution, work_item: &WorkItem, batched_search: bool) -> String {
    let attempt = sample_ci_attempt();
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(execution)
            .work_item(work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .batched_search(batched_search)
            .build(),
    )
}

fn cases() -> Vec<(ExecutionKind, WorkItem)> {
    vec![
        (ExecutionKind::TaskImplementation, chore_without_pr()),
        (ExecutionKind::ChoreImplementation, chore_without_pr()),
        (ExecutionKind::InvestigationImplementation, chore_without_pr()),
        (ExecutionKind::RevisionImplementation, chore_with_pr(PR)),
        (ExecutionKind::ConflictResolution, chore_with_pr(PR)),
        (ExecutionKind::CiRemediation, chore_with_pr(PR)),
        (ExecutionKind::ProjectDesign, design_task()),
        (ExecutionKind::PrReview, chore_with_pr(PR)),
        (ExecutionKind::PrReviewGuide, chore_with_pr(PR)),
        (ExecutionKind::AnswerAgent, chore_without_pr()),
        (ExecutionKind::AutomationTriage, chore_without_pr()),
        (ExecutionKind::Progression, chore_without_pr()),
    ]
}

#[test]
fn treated_standard_workers_get_the_rule_once_and_every_other_kind_never() {
    for (kind, work_item) in cases() {
        let mut execution = base_execution();
        execution.kind = kind.clone();
        let standard = worker_kind_for_execution(&kind) == WorkerKind::Standard;
        let treated = compose(&execution, &work_item, true);
        assert_eq!(
            treated.matches(RULE).count(),
            usize::from(standard),
            "{kind:?}: the rule is for Standard workers only"
        );
        assert_eq!(
            compose(&execution, &work_item, false).matches(RULE).count(),
            0,
            "{kind:?}"
        );
    }
}

#[test]
fn the_default_is_the_untreated_prompt() {
    for (kind, work_item) in cases() {
        let mut execution = base_execution();
        execution.kind = kind.clone();
        let attempt = sample_ci_attempt();
        let default = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .review_signal_checks(|_| &[])
                .execution(&execution)
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .ci_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert_eq!(default, compose(&execution, &work_item, false), "{kind:?}");
    }
}

#[test]
fn the_rule_is_the_only_difference_and_sits_under_the_opening_line() {
    let execution = base_execution();
    let work_item = chore_without_pr();
    let untreated = compose(&execution, &work_item, false);
    let treated = compose(&execution, &work_item, true);
    assert_eq!(
        treated,
        super::super::tool_use::with_batched_search_directive(untreated.clone())
    );
    let (opening, _) = untreated.split_once('\n').unwrap();
    assert!(
        treated.starts_with(&format!("{opening}\n\n{RULE}")),
        "the rule follows the opening line, got: {}",
        &treated[..treated.len().min(400)]
    );
}

#[test]
fn the_rule_names_the_habit_and_its_exception_and_nothing_about_why_it_is_there() {
    let treated = compose(&base_execution(), &chore_without_pr(), true);
    for wanted in [
        "issue them together as parallel tool calls in a single turn",
        "Sequence calls only when a later call needs the result of an earlier one",
    ] {
        assert!(treated.contains(wanted), "missing {wanted}");
    }
    let lowered = treated.to_lowercase();
    for hidden in ["experiment", "treatment", "control arm", "batched_search", "a/b"] {
        assert!(!lowered.contains(hidden), "the worker must not be told about {hidden}");
    }
    assert_no_forbidden_prose("treated task prompt", &treated);
}
