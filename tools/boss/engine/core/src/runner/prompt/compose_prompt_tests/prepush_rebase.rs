use super::*;
use crate::driver::DriverRegistry;
use crate::worker_setup::{WorkerSetupInput, render_claude_md, worker_kind_for_execution};

fn assert_prepush_rebase(text: &str, label: &str) {
    for instruction in [
        "Before the first push and every later push",
        "fetch and compare the branch's base with its current upstream tip",
        "workspace local-rebase --pr <bound-pr-number> --json",
        "workspace local-rebase --base <intended-pr-base-branch> --json",
        "including a parent PR branch for stacked PRs",
        "If the base has advanced, the branch must include the latest base before pushing",
        "integrating both sides' intent; never discard upstream changes",
        "re-run the builds and tests affected by the upstream changes and conflict resolutions before pushing",
        "even when the rebase looked clean",
        "no commit to be pushed has unresolved conflicts",
        "stop and report the conflicting files and reason; never push a half-resolved branch",
    ] {
        assert!(text.contains(instruction), "{label}: missing {instruction}");
    }
    assert_eq!(text.matches("### Before every PR branch push").count(), 1, "{label}");
}

#[test]
fn every_pr_producing_brief_and_driver_rules_require_fresh_base_before_each_push() {
    let registry = DriverRegistry::default();
    let mut postmortem = design_task();
    if let WorkItem::Task(task) = &mut postmortem {
        task.kind = TaskKind::DesignPostmortem;
    }
    let pr = "https://github.com/org/repo/pull/77";
    for (kind, item) in [
        (ExecutionKind::TaskImplementation, chore_without_pr()),
        (ExecutionKind::ChoreImplementation, chore_without_pr()),
        (ExecutionKind::ChoreImplementation, chore_with_pr(pr)),
        (ExecutionKind::ProjectDesign, design_task()),
        (ExecutionKind::ProjectDesign, postmortem),
        (ExecutionKind::InvestigationImplementation, chore_without_pr()),
    ] {
        for bazel in [true, false] {
            let mut execution = revision_execution(pr);
            execution.kind = kind.clone();
            let prompt = compose_execution_prompt(
                ExecutionPromptParams::builder()
                    .execution(&execution)
                    .work_item(&item)
                    .workspace_path(Path::new("/tmp/workspace"))
                    .bazel_workspace(bazel)
                    .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                    .build(),
            );
            assert_prepush_rebase(&prompt, kind.as_str());
            for driver_name in ["claude", "codex", "grok"] {
                let driver = registry.require(driver_name).unwrap();
                let input = WorkerSetupInput::builder()
                    .run_id(&execution.id)
                    .lease_id("lease-rebase")
                    .workspace_path(std::path::PathBuf::from("/tmp/workspace"))
                    .events_socket_path(std::path::PathBuf::from("/tmp/events.sock"))
                    .boss_event_path(std::path::PathBuf::from("/tmp/boss-event"))
                    .execution_kind(kind.as_str())
                    .worker_kind(worker_kind_for_execution(&kind))
                    .build();
                let rules = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
                assert_prepush_rebase(&rules, driver_name);
            }
        }
    }
}

#[test]
fn read_only_briefs_do_not_receive_pr_push_instructions() {
    for kind in [
        ExecutionKind::AnswerAgent,
        ExecutionKind::PrReview,
        ExecutionKind::AutomationTriage,
    ] {
        let mut execution = base_execution();
        execution.kind = kind.clone();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&execution)
                .work_item(&chore_without_pr())
                .workspace_path(Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(!prompt.contains("### Before every PR branch push"), "{kind:?}");
    }
}

#[test]
fn revision_ci_and_conflict_workers_keep_the_existing_rebase_flow_only() {
    let pr = "https://github.com/org/repo/pull/77";
    let conflict = sample_conflict_attempt();
    let ci = sample_ci_attempt();
    for (kind, conflict_attempt, ci_attempt) in [
        (ExecutionKind::RevisionImplementation, None, None),
        (ExecutionKind::RevisionImplementation, Some(&conflict), None),
        (ExecutionKind::RevisionImplementation, None, Some(&ci)),
        (ExecutionKind::CiRemediation, None, Some(&ci)),
        (ExecutionKind::ConflictResolution, Some(&conflict), None),
    ] {
        let mut execution = revision_execution(pr);
        execution.kind = kind.clone();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&execution)
                .work_item(&chore_with_pr(pr))
                .workspace_path(Path::new("/tmp/workspace"))
                .maybe_conflict_attempt(conflict_attempt)
                .maybe_ci_attempt(ci_attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(!prompt.contains("### Before every PR branch push"));
        assert!(!prompt.contains("local-rebase --base"));
        if kind == ExecutionKind::RevisionImplementation {
            assert_eq!(prompt.matches(super::super::revision_rebase::REBASE_FIRST).count(), 1);
            assert_eq!(
                prompt.matches(super::super::revision_rebase::VERIFY_MERGEABLE).count(),
                1
            );
        }
        let registry = DriverRegistry::default();
        for name in ["claude", "codex", "grok"] {
            let driver = registry.require(name).unwrap();
            let input = WorkerSetupInput::builder()
                .run_id(&execution.id)
                .lease_id("lease-rebase")
                .workspace_path(std::path::PathBuf::from("/tmp/workspace"))
                .events_socket_path(std::path::PathBuf::from("/tmp/events.sock"))
                .boss_event_path(std::path::PathBuf::from("/tmp/boss-event"))
                .execution_kind(kind.as_str())
                .worker_kind(worker_kind_for_execution(&kind))
                .build();
            let rules = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
            assert!(!rules.contains("### Before every PR branch push"));
            assert!(!rules.contains("local-rebase --base"));
        }
    }
}
