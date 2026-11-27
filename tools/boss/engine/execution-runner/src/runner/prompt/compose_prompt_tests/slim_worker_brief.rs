//! Tests for the `slim_worker_brief` treated worker rules and brief variants
//! (deduplication, single-occurrence rule preservation across drivers and
//! execution kinds, kind gating of the file-count protocol and done
//! declaration, and prose/invocation hygiene).

use super::{
    ExecutionPromptParams, base_execution, bazel_workspace, chore_with_pr, chore_without_pr, ci_remediation_execution,
    compose_execution_prompt, design_task, revision_execution, revision_task_with_created_via, sample_ci_attempt,
    sample_conflict_attempt,
};
use crate::driver::DriverRegistry;
use crate::prose_vocabulary::assert_no_forbidden_prose;
use crate::work::{CiRemediation, ConflictResolution, WorkExecution, WorkItem};
use crate::worker_setup::{WorkerKind, WorkerSetupInput, render_claude_md, worker_kind_for_execution};
use boss_protocol::{ExecutionKind, TaskKind};

struct Case {
    label: &'static str,
    execution: WorkExecution,
    work_item: WorkItem,
    conflict: Option<ConflictResolution>,
    ci: Option<CiRemediation>,
}

fn with_kind(mut execution: WorkExecution, kind: ExecutionKind) -> WorkExecution {
    execution.kind = kind;
    execution
}

fn postmortem_task() -> WorkItem {
    let mut item = design_task();
    if let WorkItem::Task(task) = &mut item {
        task.kind = TaskKind::DesignPostmortem;
    }
    item
}

fn queue_ci(failure_kind: &str) -> CiRemediation {
    let mut attempt = sample_ci_attempt();
    attempt.failure_kind = Some(failure_kind.into());
    attempt
}

fn standard_cases() -> Vec<Case> {
    const PR: &str = "https://github.com/org/repo/pull/42";
    let case = |label, execution, work_item, conflict, ci| Case {
        label,
        execution,
        work_item,
        conflict,
        ci,
    };
    vec![
        case(
            "task (fresh)",
            with_kind(base_execution(), ExecutionKind::TaskImplementation),
            chore_without_pr(),
            None,
            None,
        ),
        case(
            "task (resumed PR)",
            with_kind(base_execution(), ExecutionKind::TaskImplementation),
            chore_with_pr(PR),
            None,
            None,
        ),
        case("chore (fresh)", base_execution(), chore_without_pr(), None, None),
        case(
            "investigation",
            with_kind(base_execution(), ExecutionKind::InvestigationImplementation),
            chore_without_pr(),
            None,
            None,
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
            "revision (plain)",
            revision_execution(PR),
            chore_with_pr(PR),
            None,
            None,
        ),
        case(
            "revision (conflict)",
            revision_execution(PR),
            revision_task_with_created_via(None, "merge-conflict:crz_frag_01"),
            Some(sample_conflict_attempt()),
            None,
        ),
        case(
            "revision (ci fix)",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            Some(sample_ci_attempt()),
        ),
        case(
            "revision (queue eviction)",
            revision_execution(PR),
            revision_task_with_created_via(None, "ci-fix:crm_frag_01"),
            None,
            Some(queue_ci("trunk_queue_eviction")),
        ),
        case(
            "conflict resolution",
            with_kind(revision_execution(PR), ExecutionKind::ConflictResolution),
            revision_task_with_created_via(None, "merge-conflict:crz_frag_01"),
            Some(sample_conflict_attempt()),
            None,
        ),
        case(
            "ci remediation (retrigger)",
            ci_remediation_execution(PR),
            chore_with_pr(PR),
            None,
            Some(sample_ci_attempt()),
        ),
        case(
            "ci remediation (queue eviction)",
            ci_remediation_execution(PR),
            chore_with_pr(PR),
            None,
            Some(queue_ci("trunk_queue_eviction")),
        ),
    ]
}

fn render_pair(
    case: &Case,
    driver_slug: &str,
    workspace: &std::path::Path,
    run_done_seam: bool,
    worker_signal_seam: bool,
    slim: bool,
) -> (String, String) {
    let registry = DriverRegistry::default();
    let driver = registry.require(driver_slug).expect("driver registered");
    let setup_input = WorkerSetupInput::builder()
        .run_id(case.execution.id.clone())
        .lease_id("lease-slim-test")
        .workspace_path(workspace.to_path_buf())
        .events_socket_path(std::path::PathBuf::from("/tmp/events.sock"))
        .boss_event_path(std::path::PathBuf::from("/tmp/boss-event"))
        .execution_kind(case.execution.kind.as_str())
        .worker_kind(worker_kind_for_execution(&case.execution.kind))
        .run_done_proposals_seam_enabled(run_done_seam)
        .worker_signal_proposals_seam_enabled(worker_signal_seam)
        .slim_worker_brief(slim)
        .build();
    let rules = render_claude_md(
        &setup_input,
        driver.agent_rules_preamble(),
        driver.descriptor().config_dir,
    );
    let brief = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&case.execution)
            .work_item(&case.work_item)
            .workspace_path(workspace)
            .maybe_conflict_attempt(case.conflict.as_ref())
            .maybe_ci_attempt(case.ci.as_ref())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .worker_signal_proposals_seam_enabled(worker_signal_seam)
            .deferred_scope_proposals_seam_enabled(worker_signal_seam)
            .followup_proposals_seam_enabled(worker_signal_seam)
            .run_done_proposals_seam_enabled(run_done_seam)
            .slim_worker_brief(slim)
            .build(),
    );
    (rules, brief)
}

#[test]
fn default_and_false_slim_worker_brief_are_byte_identical_to_control() {
    let ws = bazel_workspace();
    for case in standard_cases() {
        let brief_default = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .review_signal_checks(|_| &[])
                .execution(&case.execution)
                .work_item(&case.work_item)
                .workspace_path(ws.path())
                .maybe_conflict_attempt(case.conflict.as_ref())
                .maybe_ci_attempt(case.ci.as_ref())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .worker_signal_proposals_seam_enabled(true)
                .deferred_scope_proposals_seam_enabled(true)
                .followup_proposals_seam_enabled(true)
                .run_done_proposals_seam_enabled(true)
                .build(),
        );
        let (_, brief_false) = render_pair(&case, "claude", ws.path(), true, true, false);
        assert_eq!(
            brief_default, brief_false,
            "[{}] explicit slim_worker_brief=false must match omitted default",
            case.label
        );
    }
}

#[test]
fn non_standard_worker_kinds_are_unchanged_by_slim_worker_brief() {
    let ws = bazel_workspace();
    let registry = DriverRegistry::default();
    for kind in [
        ExecutionKind::PrReview,
        ExecutionKind::PrReviewGuide,
        ExecutionKind::AutomationTriage,
        ExecutionKind::AnswerAgent,
        ExecutionKind::Progression,
    ] {
        assert_ne!(worker_kind_for_execution(&kind), WorkerKind::Standard);
        let case = Case {
            label: "non-standard",
            execution: with_kind(base_execution(), kind.clone()),
            work_item: chore_without_pr(),
            conflict: None,
            ci: None,
        };
        for slug in registry.slugs() {
            let (ctrl_rules, ctrl_brief) = render_pair(&case, slug, ws.path(), true, true, false);
            let (slim_rules, slim_brief) = render_pair(&case, slug, ws.path(), true, true, true);
            assert_eq!(
                slim_rules, ctrl_rules,
                "non-Standard kind {kind:?} on {slug} must keep control worker rules byte-for-byte"
            );
            assert_eq!(
                slim_brief, ctrl_brief,
                "non-Standard kind {kind:?} on {slug} must keep control brief byte-for-byte"
            );
        }
    }
}

/// Every rule that the control text states must appear **exactly once** in the
/// treated `rules + brief` for each Standard execution kind and driver, with
/// the long form in the worker rules and a concise pointer in the brief.
#[test]
fn every_control_rule_appears_exactly_once_in_treated_rules_plus_brief_across_kinds_and_drivers() {
    let ws = bazel_workspace();
    let registry = DriverRegistry::default();
    let drivers: Vec<&str> = registry.slugs().collect();
    assert!(
        ["claude", "codex", "grok", "agy"].iter().all(|d| drivers.contains(d)),
        "expected all four drivers in registry, got {drivers:?}"
    );

    for case in standard_cases() {
        let kind = &case.execution.kind;
        let is_ci_templated = *kind == ExecutionKind::CiRemediation && case.ci.is_some();
        let is_queue_ci_templated = is_ci_templated
            && case
                .ci
                .as_ref()
                .is_some_and(crate::work::queue_ci_evidence::queue_ci_verification_required);
        let is_pr_opening = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::ProjectDesign
                | ExecutionKind::InvestigationImplementation
        );
        let has_build_gate = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::RevisionImplementation
                | ExecutionKind::ConflictResolution
        );
        let has_check_bypass_in_brief = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::InvestigationImplementation
                | ExecutionKind::RevisionImplementation
                | ExecutionKind::ConflictResolution
        ) || is_queue_ci_templated;
        let has_bound_pr = matches!(
            kind,
            ExecutionKind::RevisionImplementation | ExecutionKind::ConflictResolution | ExecutionKind::CiRemediation
        ) || crate::runner::work_item::work_item_pr_url(&case.work_item).is_some();
        let supports_no_op = *kind == ExecutionKind::RevisionImplementation
            || (!has_bound_pr
                && matches!(
                    kind,
                    ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
                ));
        let supports_file_count = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::RevisionImplementation
        );

        for slug in &drivers {
            let (ctrl_rules, ctrl_brief) = render_pair(&case, slug, ws.path(), true, true, false);
            let (slim_rules, slim_brief) = render_pair(&case, slug, ws.path(), true, true, true);
            let combined = format!("{slim_rules}\n{slim_brief}");
            let ctx = format!("[{} / driver={slug}]", case.label);

            // Net combined size must shrink for every Standard execution kind.
            assert!(
                slim_rules.len() + slim_brief.len() < ctrl_rules.len() + ctrl_brief.len(),
                "{ctx} expected treated rules+brief ({} + {} = {}) < control ({} + {} = {})",
                slim_rules.len(),
                slim_brief.len(),
                slim_rules.len() + slim_brief.len(),
                ctrl_rules.len(),
                ctrl_brief.len(),
                ctrl_rules.len() + ctrl_brief.len(),
            );

            // 1. Pre-push rebase block: once in rules, pointer in brief.
            if is_pr_opening {
                assert_eq!(
                    slim_rules.matches("### Before every PR branch push").count(),
                    1,
                    "{ctx} long-form pre-push rebase must be in rules"
                );
                assert_eq!(
                    slim_brief.matches("### Before every PR branch push").count(),
                    0,
                    "{ctx} long-form pre-push rebase must not repeat in brief"
                );
                assert_eq!(
                    combined.matches("Stacked changes are not supported.").count(),
                    1,
                    "{ctx} stacked-changes rule must appear exactly once"
                );
                assert!(
                    slim_brief.contains("\"Before every PR branch push\" section in the worker rules"),
                    "{ctx} brief must carry a one-line pointer to pre-push rebase rules"
                );
            }

            // 2. Check-bypass prohibition: once in rules, pointer in brief where applicable.
            assert_eq!(
                slim_rules
                    .matches("**Hard constraint — fix failing checks at the root cause; never bypass them.**")
                    .count(),
                1,
                "{ctx} check-bypass rule must appear once in rules"
            );
            assert_eq!(
                slim_brief
                    .matches("**Hard constraint — fix failing checks at the root cause; never bypass them.**")
                    .count(),
                0,
                "{ctx} check-bypass long form must not repeat in brief"
            );
            assert_eq!(
                combined
                    .matches("exclusions/allowlists (`CHECKS.yaml`, checkleft)")
                    .count(),
                1,
                "{ctx} check-bypass detail must appear exactly once"
            );
            if has_check_bypass_in_brief {
                assert!(
                    slim_brief.contains("\"Fix failing checks at the root cause\" section in the worker rules"),
                    "{ctx} brief must carry a one-line pointer to check-bypass rules"
                );
            }

            // 3. Long-running commands / no-`timeout` details: once in rules for build-gate kinds, never repeated in brief.
            assert_eq!(
                slim_brief
                    .matches(
                        "Do not wrap `bazel build`, `bazel test`, `bazel run`, or `checkleft run` in a shell `timeout`"
                    )
                    .count(),
                0,
                "{ctx} no-timeout long sentence must not appear in treated brief"
            );
            assert_eq!(
                slim_brief
                    .matches("Do not wrap `bazel build` in a shell `timeout`")
                    .count(),
                0,
                "{ctx} conflict no-timeout sentence must not repeat in treated brief"
            );
            if has_build_gate {
                assert_eq!(
                    slim_rules.matches("## Long-running build and test commands").count(),
                    1,
                    "{ctx} long-running command rules must appear once in rules"
                );
                assert_eq!(
                    combined.matches("Bazel's own per-test timeouts").count(),
                    1,
                    "{ctx} Bazel timeout explanation must appear exactly once"
                );
                assert_eq!(
                    combined
                        .matches("inspect only this invocation's output base and logs")
                        .count(),
                    1,
                    "{ctx} invocation-scoped log diagnosis must appear exactly once"
                );
                if *kind != ExecutionKind::ConflictResolution {
                    assert!(
                        slim_brief.contains("\"Long-running build and test commands\" section in the worker rules"),
                        "{ctx} build-gate brief must point to long-running command rules"
                    );
                }
            } else {
                assert_eq!(
                    slim_rules.matches("## Long-running build and test commands").count(),
                    0,
                    "{ctx} non-build-gate kind must omit long-running build rules"
                );
            }

            // 4. PR `--body-file` / no inline `--body` rule: once in rules, pointer in revision step 5c.
            assert_eq!(
                slim_rules
                    .matches("**NEVER pass a PR body or comment as `--body \"<inline text>\"`**")
                    .count(),
                1,
                "{ctx} inline --body prohibition must appear once in rules"
            );
            assert_eq!(
                slim_brief.matches("the shell evaluates backticks and `$(...)`").count(),
                0,
                "{ctx} inline --body long explanation must not repeat in brief"
            );
            assert_eq!(
                combined.matches("### Canonical PR creation recipe").count(),
                1,
                "{ctx} PR body-file recipe must appear exactly once"
            );

            // 5. Empty-diff and `no-changes-needed` rule: once in rules, pointer in brief.
            assert_eq!(
                slim_rules.matches("Before pushing, run `jj diff -r @`.").count(),
                1,
                "{ctx} empty-diff rule must appear once in rules"
            );
            if supports_no_op {
                assert_eq!(
                    slim_rules.matches("propose done --outcome no-changes-needed").count(),
                    1,
                    "{ctx} no-changes-needed command must appear once in rules"
                );
                assert_eq!(
                    slim_brief.matches("propose done --outcome no-changes-needed").count(),
                    0,
                    "{ctx} no-changes-needed command must not repeat in brief"
                );
                assert!(
                    slim_brief.contains("\"Empty diff and no-changes-needed\" in the worker rules"),
                    "{ctx} brief must point to empty-diff / no-changes-needed rules"
                );
            }

            // 6. Inline `-m` commit message rule: once in rules, pointer in conflict/CI fragments.
            assert_eq!(
                slim_rules.matches("### Commit messages must be inline").count(),
                1,
                "{ctx} inline -m rule must appear once in rules"
            );
            assert_eq!(
                slim_brief
                    .matches("The worker environment has no usable editor")
                    .count(),
                0,
                "{ctx} conflict fragment must not repeat inline -m explanation"
            );
            assert_eq!(
                slim_brief
                    .matches("The worker environment has no usable `$EDITOR`")
                    .count(),
                0,
                "{ctx} CI fragment must not repeat inline -m explanation"
            );

            // 7. Revisions section: only for RevisionImplementation, never duplicated in Constraints.
            if *kind == ExecutionKind::RevisionImplementation {
                assert_eq!(
                    slim_rules.matches("## Revisions").count(),
                    1,
                    "{ctx} revision rules must appear once in worker rules"
                );
                assert_eq!(
                    combined.matches("do NOT run `gh pr create` or").count(),
                    1,
                    "{ctx} revision no-new-PR rule must appear exactly once"
                );
                assert!(
                    slim_brief.contains("Follow \"Revisions\" and \"Empty diff and no-changes-needed\""),
                    "{ctx} revision brief Constraints must point to worker rules"
                );
            } else {
                assert_eq!(
                    slim_rules.matches("## Revisions").count(),
                    0,
                    "{ctx} non-revision kind must not receive ## Revisions"
                );
            }

            // 8. Question-writing contract (when run_done seam is on): once in rules, pointer in brief.
            assert_eq!(
                slim_rules
                    .matches("## Write questions for someone arriving cold")
                    .count(),
                1,
                "{ctx} question contract must appear once in rules"
            );
            assert_eq!(
                combined
                    .matches("\"No = remain blocked\" is not a real choice.")
                    .count(),
                1,
                "{ctx} actionable-No rule must appear exactly once"
            );
            if !is_ci_templated {
                assert!(
                    slim_brief.contains("\"Write questions for someone arriving cold\" section in the worker rules"),
                    "{ctx} brief must point to question contract in rules"
                );
            }

            // 9. File-count split-vs-bypass protocol: once in rules + pointer in brief for multi-file kinds only.
            if supports_file_count {
                assert_eq!(
                    slim_rules.matches("BYPASS_CHANGE_FILE_COUNT=").count(),
                    1,
                    "{ctx} file-count bypass formula must appear once in rules"
                );
                assert_eq!(
                    slim_brief.matches("BYPASS_CHANGE_FILE_COUNT=").count(),
                    0,
                    "{ctx} file-count bypass formula must not repeat in brief"
                );
                assert_eq!(
                    combined.matches("--file-count <n> --file-count-limit <limit>").count(),
                    1,
                    "{ctx} file-count command template must appear exactly once"
                );
                assert!(
                    slim_brief.contains("Split-vs-Bypass-limit protocol in the worker rules"),
                    "{ctx} multi-file brief must point to Split-vs-Bypass-limit protocol in rules"
                );
            }

            // 10. Universal rules from control remain present exactly once.
            for needle in [
                "Classify a CI failure by cause, then act",
                "## Always use absolute paths",
                "## Reuse before you build",
                "## Boundaries",
                "## Capturing a screenshot for your own verification",
                "## Probes",
                "**Trust Bazel's caching.**",
                "**Write in project terms.**",
            ] {
                assert_eq!(
                    combined.matches(needle).count(),
                    1,
                    "{ctx} universal rule `{needle}` must appear exactly once"
                );
            }
        }
    }
}

#[test]
fn file_count_protocol_is_gated_to_multi_file_implementation_kinds_only() {
    let ws = bazel_workspace();
    for case in standard_cases() {
        let (rules, brief) = render_pair(&case, "claude", ws.path(), true, true, true);
        let combined = format!("{rules}\n{brief}");
        let expects_protocol = matches!(
            case.execution.kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::RevisionImplementation
        );
        for marker in [
            "--file-count",
            "--file-count-limit",
            "Bypass limit",
            "BYPASS_CHANGE_FILE_COUNT",
            "Split:",
            "Split-vs-Bypass-limit",
        ] {
            assert_eq!(
                combined.contains(marker),
                expects_protocol,
                "[{}] file-count marker `{marker}` expected={expects_protocol}\n--- RULES ---\n{rules}\n--- BRIEF ---\n{brief}",
                case.label,
            );
        }
        // Only revisions receive the revision-specific footprint re-check paragraph.
        let expects_revision_recheck = case.execution.kind == ExecutionKind::RevisionImplementation;
        assert_eq!(
            rules.contains("Revisions re-run `checkleft run` before every push."),
            expects_revision_recheck,
            "[{}] revision re-check expected={expects_revision_recheck}",
            case.label,
        );
        // Single-file / non-design non-multi-file kinds should not mention `change/file-count` at all.
        if !expects_protocol && case.execution.kind != ExecutionKind::ProjectDesign {
            assert!(
                !combined.contains("change/file-count"),
                "[{}] non-multi-file kind must not mention change/file-count",
                case.label,
            );
        }
    }
}

#[test]
fn done_declaration_is_kind_gated_for_outcomes_question_types_and_build_gate_exception() {
    let ws = bazel_workspace();
    for case in standard_cases() {
        let kind = &case.execution.kind;
        // Templated CI remediation (with ci_attempt) returns before run_done; still test rules gating.
        let is_ci_templated = *kind == ExecutionKind::CiRemediation && case.ci.is_some();
        let (rules, brief) = render_pair(&case, "claude", ws.path(), true, true, true);

        let has_bound_pr = matches!(
            kind,
            ExecutionKind::RevisionImplementation | ExecutionKind::ConflictResolution | ExecutionKind::CiRemediation
        ) || crate::runner::work_item::work_item_pr_url(&case.work_item).is_some();
        let supports_no_op = *kind == ExecutionKind::RevisionImplementation
            || (!has_bound_pr
                && matches!(
                    kind,
                    ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
                ));
        let supports_multiple_choice = *kind != ExecutionKind::CiRemediation;
        let supports_prerequisites = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::RevisionImplementation
                | ExecutionKind::ConflictResolution
        );
        let has_build_gate = matches!(
            kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::RevisionImplementation
                | ExecutionKind::ConflictResolution
        );

        // Worker rules question-type gating.
        assert_eq!(
            rules.contains("--answer-type multiple-choice"),
            supports_multiple_choice,
            "[{}] multiple-choice in rules expected={supports_multiple_choice}",
            case.label,
        );
        assert_eq!(
            rules.contains("### Waiting on another PR (prerequisite tasks)"),
            supports_prerequisites,
            "[{}] prerequisites section in rules expected={supports_prerequisites}",
            case.label,
        );
        assert_eq!(
            rules.contains("## Empty diff and no-changes-needed"),
            matches!(
                kind,
                ExecutionKind::TaskImplementation
                    | ExecutionKind::ChoreImplementation
                    | ExecutionKind::RevisionImplementation
            ),
            "[{}] empty-diff no-op rules heading mismatch",
            case.label,
        );

        if is_ci_templated {
            continue;
        }

        let run_done = brief
            .split("## Declaring your run finished")
            .nth(1)
            .unwrap_or_else(|| panic!("[{}] missing run_done section", case.label));
        let run_done = run_done
            .split("\nRespond with concise markdown")
            .next()
            .unwrap_or(run_done);

        // Outcome syntax & bullet gating.
        assert_eq!(
            run_done.contains("no-changes-needed"),
            supports_no_op,
            "[{}] no-changes-needed in run_done expected={supports_no_op}\n{run_done}",
            case.label,
        );
        assert_eq!(
            run_done.contains("--superseded-by"),
            has_bound_pr,
            "[{}] --superseded-by in run_done expected={has_bound_pr}\n{run_done}",
            case.label,
        );

        // Question-type pointers in run_done.
        assert_eq!(
            run_done.contains("--answer-type multiple-choice"),
            supports_multiple_choice,
            "[{}] multiple-choice pointer in run_done expected={supports_multiple_choice}\n{run_done}",
            case.label,
        );
        assert_eq!(
            run_done.contains("create-prerequisite-task"),
            supports_prerequisites,
            "[{}] prerequisite pointer in run_done expected={supports_prerequisites}\n{run_done}",
            case.label,
        );

        // Build-gate exception gating.
        let has_ordinary_exception = run_done.contains("For validation failures, this is a narrow exception");
        let has_conflict_exception =
            run_done.contains("The merge-correctness pre-push gate has no unattributable-failure exception");
        if has_build_gate {
            if case.conflict.is_some() {
                assert!(
                    has_conflict_exception,
                    "[{}] expected conflict exception:\n{run_done}",
                    case.label
                );
                assert!(
                    !has_ordinary_exception && !run_done.contains("You MAY push despite a still-red target"),
                    "[{}] conflict run must not allow pushing a still-red compile:\n{run_done}",
                    case.label,
                );
            } else {
                assert!(
                    has_ordinary_exception,
                    "[{}] expected ordinary build-gate exception:\n{run_done}",
                    case.label
                );
                assert!(
                    !has_conflict_exception,
                    "[{}] unexpected conflict exception:\n{run_done}",
                    case.label
                );
            }
        } else {
            assert!(
                !has_ordinary_exception && !has_conflict_exception,
                "[{}] non-build-gate kind must omit validation-failure gate exception:\n{run_done}",
                case.label,
            );
        }
    }
}

#[test]
fn slim_non_bazel_and_unresolved_gates_keep_pointers_without_bazel_leakage() {
    let non_bazel = tempfile::tempdir().unwrap();
    std::fs::write(non_bazel.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();

    let non_bazel_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(non_bazel.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .worker_signal_proposals_seam_enabled(true)
            .slim_worker_brief(true)
            .build(),
    );
    assert!(non_bazel_prompt.contains("\"Long-running build and test commands\" section in the worker rules"));
    assert!(non_bazel_prompt.contains("The repo's own checks remain the source of truth"));
    assert!(!non_bazel_prompt.contains("pre-push Bazel gate"));
    assert!(!non_bazel_prompt.contains("Bazel remains the source of truth"));

    let missing_path = std::path::Path::new("/nonexistent/remote-workspace-path");
    let unresolved_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(missing_path)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .worker_signal_proposals_seam_enabled(true)
            .slim_worker_brief(true)
            .build(),
    );
    assert!(unresolved_prompt.contains("Pre-push build gate (build system unresolved)"));
    assert!(unresolved_prompt.contains("\"Long-running build and test commands\" section in the worker rules"));
    assert!(unresolved_prompt.contains("The checks you discovered on this execution host remain the source of truth"));
}

#[test]
fn treated_rules_and_prompts_pass_vocabulary_and_invocation_hygiene() {
    let ws = bazel_workspace();
    for case in standard_cases() {
        for seams in [false, true] {
            let (rules, brief) = render_pair(&case, "claude", ws.path(), seams, seams, true);
            assert_no_forbidden_prose(&format!("slim rules [{} seams={seams}]", case.label), &rules);
            assert_no_forbidden_prose(&format!("slim brief [{} seams={seams}]", case.label), &brief);
            for (surface, text) in [("rules", &rules), ("brief", &brief)] {
                for bad_invocation in ["`boss ", "`cube "] {
                    assert!(
                        !text.contains(bad_invocation),
                        "[{} seams={seams}] slim {surface} must not teach bare invocation {bad_invocation}",
                        case.label,
                    );
                }
                let lower = text.to_ascii_lowercase();
                for leak in [
                    "slim_worker_brief",
                    "control arm",
                    "treatment arm",
                    "a/b test",
                    "a/b split",
                ] {
                    assert!(
                        !lower.contains(leak),
                        "[{} seams={seams}] slim {surface} leaked experiment vocabulary `{leak}`",
                        case.label,
                    );
                }
            }
        }
    }
}

#[test]
fn print_slim_worker_brief_byte_comparison_by_kind() {
    let ws = bazel_workspace();
    for case in standard_cases() {
        let (ctrl_rules, ctrl_brief) = render_pair(&case, "claude", ws.path(), true, true, false);
        let (slim_rules, slim_brief) = render_pair(&case, "claude", ws.path(), true, true, true);
        let ctrl_total = ctrl_rules.len() + ctrl_brief.len();
        let slim_total = slim_rules.len() + slim_brief.len();
        let saved = ctrl_total.saturating_sub(slim_total);
        println!(
            "SLIM_BYTES {:32} ctrl={} ({}+{}) slim={} ({}+{}) saved={} ({:.1}%)",
            case.label,
            ctrl_total,
            ctrl_rules.len(),
            ctrl_brief.len(),
            slim_total,
            slim_rules.len(),
            slim_brief.len(),
            saved,
            (saved as f64) * 100.0 / (ctrl_total as f64),
        );
        assert!(slim_total < ctrl_total, "[{}] expected net byte reduction", case.label);
    }
}
