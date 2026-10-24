//! Worker prompts must not teach workers the engine's internal vocabulary.
//!
//! See [`crate::prose_vocabulary`] for why and for what counts as a hit. This
//! module runs every dispatch kind's assembled prompt and the probe and nudge
//! texts through that scanner; the generated worker rules are covered next to
//! their renderer.

use super::{
    ExecutionPromptParams, base_execution, bazel_workspace, chore_with_pr, chore_without_pr, ci_remediation_execution,
    compose_execution_prompt, design_task, revision_execution, revision_task_with_created_via, sample_ci_attempt,
    sample_conflict_attempt,
};
use crate::prose_vocabulary::{assert_no_forbidden_prose, forbidden_prose};
use crate::work::{CiRemediation, ConflictResolution, WorkExecution, WorkItem};
use boss_protocol::{ExecutionKind, TaskKind};

struct Case {
    label: &'static str,
    execution: WorkExecution,
    work_item: WorkItem,
    conflict: Option<ConflictResolution>,
    ci: Option<CiRemediation>,
}

fn queue_ci(failure_kind: &str, failed_checks: Option<&str>) -> CiRemediation {
    let mut attempt = sample_ci_attempt();
    attempt.failure_kind = Some(failure_kind.into());
    if let Some(checks) = failed_checks {
        attempt.failed_checks = checks.into();
    }
    attempt
}

fn postmortem_task() -> WorkItem {
    let mut item = design_task();
    if let WorkItem::Task(task) = &mut item {
        task.kind = TaskKind::DesignPostmortem;
    }
    item
}

fn with_kind(mut execution: WorkExecution, kind: ExecutionKind) -> WorkExecution {
    execution.kind = kind;
    execution
}

fn cases() -> Vec<Case> {
    const PR: &str = "https://github.com/org/repo/pull/42";
    let conflict = || Some(sample_conflict_attempt());
    let ci = || Some(sample_ci_attempt());
    let case = |label, execution, work_item, conflict, ci| Case {
        label,
        execution,
        work_item,
        conflict,
        ci,
    };
    vec![
        case(
            "task",
            with_kind(base_execution(), ExecutionKind::TaskImplementation),
            chore_without_pr(),
            None,
            None,
        ),
        case(
            "task, resumed PR",
            with_kind(base_execution(), ExecutionKind::TaskImplementation),
            chore_with_pr(PR),
            None,
            None,
        ),
        case("chore", base_execution(), chore_without_pr(), None, None),
        case(
            "investigation",
            with_kind(base_execution(), ExecutionKind::InvestigationImplementation),
            chore_without_pr(),
            None,
            None,
        ),
        case("revision", revision_execution(PR), chore_with_pr(PR), None, None),
        case(
            "revision, conflict",
            revision_execution(PR),
            revision_task_with_created_via(None, "merge-conflict:crz_frag_01"),
            conflict(),
            None,
        ),
        case(
            "revision, ci fix",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            ci(),
        ),
        case(
            "revision, queue eviction",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            Some(queue_ci("trunk_queue_eviction", None)),
        ),
        case(
            "revision, queue eviction without a build",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            Some(queue_ci("trunk_queue_eviction", Some("[]"))),
        ),
        case(
            "revision, queue rebounce",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            Some(queue_ci("merge_queue_rebounce", None)),
        ),
        case(
            "revision, review guide comment",
            revision_execution(PR),
            revision_task_with_created_via(None, "guide-comment:rgs_1"),
            None,
            None,
        ),
        case(
            "conflict resolution",
            with_kind(revision_execution(PR), ExecutionKind::ConflictResolution),
            revision_task_with_created_via(None, "merge-conflict:crz_frag_01"),
            conflict(),
            None,
        ),
        case(
            "ci remediation",
            ci_remediation_execution(PR),
            chore_with_pr(PR),
            None,
            ci(),
        ),
        case(
            "project design",
            with_kind(base_execution(), ExecutionKind::ProjectDesign),
            design_task(),
            None,
            None,
        ),
        case(
            "design postmortem",
            with_kind(base_execution(), ExecutionKind::ProjectDesign),
            postmortem_task(),
            None,
            None,
        ),
        case(
            "product design",
            with_kind(base_execution(), ExecutionKind::ProductDesign),
            chore_without_pr(),
            None,
            None,
        ),
        case(
            "pr review (generic)",
            with_kind(base_execution(), ExecutionKind::PrReview),
            chore_with_pr(PR),
            None,
            None,
        ),
        case(
            "automation triage (generic)",
            with_kind(base_execution(), ExecutionKind::AutomationTriage),
            chore_without_pr(),
            None,
            None,
        ),
    ]
}

#[test]
fn worker_prompts_for_every_dispatch_kind_avoid_internal_vocabulary() {
    let workspace = bazel_workspace();
    let rules = boss_protocol::EditorialRules::default();
    let mut failures = Vec::new();
    for case in cases() {
        // Every gated variant of the prompt: each proposals seam on and off,
        // and editorial rules on and off.
        for seams in [false, true] {
            for editorial in [false, true] {
                let prompt = compose_execution_prompt(
                    ExecutionPromptParams::builder()
                        .execution(&case.execution)
                        .work_item(&case.work_item)
                        .workspace_path(workspace.path())
                        .maybe_conflict_attempt(case.conflict.as_ref())
                        .maybe_ci_attempt(case.ci.as_ref())
                        .editorial_rules(&rules)
                        .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                        .editorial_enabled(editorial)
                        .worker_signal_proposals_seam_enabled(seams)
                        .deferred_scope_proposals_seam_enabled(seams)
                        .followup_proposals_seam_enabled(seams)
                        .run_done_proposals_seam_enabled(seams)
                        .build(),
                );
                // The shared writing rule rides along with every kind exactly once.
                assert_eq!(
                    prompt.matches("**Write in project terms.**").count(),
                    1,
                    "[{} seams={seams} editorial={editorial}] shared writing rule missing or duplicated",
                    case.label,
                );
                for hit in forbidden_prose(&prompt) {
                    failures.push(format!("[{} seams={seams} editorial={editorial}] {hit}", case.label));
                }
            }
        }
    }
    failures.sort();
    failures.dedup();
    assert!(
        failures.is_empty(),
        "worker prompts leak internal vocabulary ({} distinct hits):\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn probe_and_nudge_texts_avoid_internal_vocabulary() {
    use crate::completion::{
        NO_OP_DECLARATION_REFUSED_PROBE, PROBE_EMPTY_PR, PROBE_NO_PR, PROBE_STALE_PR, WORKER_WAIT_NUDGE_DIRECTIVE,
        probe_push_to_existing_pr,
    };
    let pr = "https://github.com/org/repo/pull/42";
    for (label, text) in [
        ("no PR probe", PROBE_NO_PR.to_owned()),
        ("existing PR probe", probe_push_to_existing_pr(pr)),
        ("stale PR probe", PROBE_STALE_PR.to_owned()),
        ("empty PR probe", PROBE_EMPTY_PR.to_owned()),
        ("refused no-op probe", NO_OP_DECLARATION_REFUSED_PROBE.to_owned()),
        ("wait nudge directive", WORKER_WAIT_NUDGE_DIRECTIVE.to_owned()),
        (
            "declare-done probe",
            crate::run_done_backstop::PROBE_DECLARE_DONE.to_owned(),
        ),
        (
            "conflict still present probe",
            crate::conflict_stop_gate::probe_conflict_still_present(pr, "CONFLICTING", "DIRTY"),
        ),
        (
            "conflict mergeability unknown probe",
            crate::conflict_stop_gate::probe_conflict_mergeability_unknown(pr),
        ),
    ] {
        assert_no_forbidden_prose(label, &text);
    }
}
