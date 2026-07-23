//! Tests for the worker exposure boundary.
//!
//! The policy half is deliberately tested by *behaviour of the decision*, not
//! by reciting the allowlist back — a test that mirrors the table would pass
//! for any table. So the assertions here are the properties that matter: the
//! verbs a live worker prompt actually instructs must be allowed (otherwise
//! this change breaks triage, CI-remediation, conflict and answer-agent runs
//! on the day it lands), the mutating taxonomy verbs must be denied *with a
//! redirect the worker can act on*, and nothing may be denied without a
//! message that says what to do next.

use super::*;
use boss_protocol::{
    BranchNaming, ExecutionKind, ExecutionStatus, FrontendEvent, FrontendRequest, ProposalKind, WorkExecution,
    WorkItemPatch, WorkRun, WorkerTierDenialReason,
};
use policy::variant_name;
use serde_json::json;

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn execution() -> WorkExecution {
    WorkExecution::builder()
        .id("exec_1")
        .work_item_id("chore_1")
        .kind(ExecutionKind::TaskImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:o/r.git")
        .created_at("1700000000")
        .branch_naming(BranchNaming::BossExecPrefix)
        .build()
}

fn run() -> WorkRun {
    WorkRun::builder()
        .id("run_1")
        .agent_id("agent_1")
        .execution_id("exec_1")
        .created_at("1700000000")
        .status("active")
        .transcript_path("/Users/someone/Library/Application Support/Boss/transcripts/exec_1.jsonl")
        .artifacts_path("/Users/someone/Library/Application Support/Boss/artifacts/exec_1")
        .build()
}

/// Assert `request` is allowed, with the verb name in the failure message so
/// a regression says which one broke.
fn assert_allowed(request: FrontendRequest) {
    let decision = worker_verb_decision(&request);
    assert!(
        decision.is_allowed(),
        "{} must be allowed at worker tier, got {decision:?}",
        variant_name(&request),
    );
}

/// Assert `request` is denied, returning the denial for further assertions.
fn assert_denied(request: FrontendRequest) -> boss_protocol::WorkerTierDenial {
    let decision = worker_verb_decision(&request);
    let denial = decision
        .denial()
        .unwrap_or_else(|| panic!("{} must be denied at worker tier", variant_name(&request)))
        .clone();
    assert_eq!(denial.verb, variant_name(&request));
    denial
}

// ── The verbs live worker prompts instruct ───────────────────────────────────
//
// If any of these regress to denied, a whole worker kind stops working. They
// are listed one per prompt site rather than folded into a loop so a failure
// names the broken worker kind directly.

#[test]
fn triage_sanctioned_direct_create_stays_allowed() {
    // `boss task create --automation` (automation_triage.rs prompt). The
    // design keeps this create direct — it is already provenance-checked —
    // and mediates only the outcome declaration.
    assert_allowed(FrontendRequest::CreateAutomationTask {
        automation_id: "auto_1".into(),
        name: "Fix the flake".into(),
        description: None,
        target_files: vec![],
        target_symbols: vec![],
    });
}

#[test]
fn conflict_worker_telemetry_verbs_stay_allowed() {
    // `boss engine conflicts record-producer` — the worker preamble's
    // merge-conflict telemetry section instructs this by name.
    assert_allowed(FrontendRequest::RecordProducerSideConflict {
        execution_id: "exec_1".into(),
        head_branch: "boss/exec_1".into(),
        base_branch: "main".into(),
        conflicted_files: vec!["app.rs".into()],
    });
    // `boss engine conflicts mark-failed` (runner.rs conflict prompt).
    assert_allowed(FrontendRequest::MarkConflictResolutionFailed {
        attempt_id: "cr_1".into(),
        reason: "unresolvable".into(),
    });
}

#[test]
fn ci_remediation_worker_marks_stay_allowed() {
    // Every `boss engine ci …` verb the CI worker prompt instructs.
    assert_allowed(FrontendRequest::ClassifyCiRemediation {
        attempt_id: "ci_1".into(),
        triage_class: "tractable".into(),
    });
    assert_allowed(FrontendRequest::MarkCiRemediationFailed {
        attempt_id: "ci_1".into(),
        reason: "unfixable".into(),
    });
    assert_allowed(FrontendRequest::MarkCiRemediationNoop {
        attempt_id: "ci_1".into(),
        observed_sha: Some("abc123".into()),
        reason: Some("already-green".into()),
    });
    assert_allowed(FrontendRequest::MarkCiRemediationRetriggered {
        attempt_id: "ci_1".into(),
        new_id: "build_2".into(),
    });
    assert_allowed(FrontendRequest::MarkCiRemediationSucceededViaRebase {
        attempt_id: "ci_1".into(),
    });
}

#[test]
fn design_worker_design_doc_pointer_stays_allowed() {
    // `boss project set-design-doc` — named in the design's worker-tier
    // verb policy as a sanctioned telemetry verb for design workers.
    assert_allowed(FrontendRequest::SetProjectDesignDoc {
        input: boss_protocol::SetProjectDesignDocInput {
            project_id: "proj_1".into(),
            unset: false,
            design_doc_branch: None,
            design_doc_path: Some("docs/designs/thing.md".into()),
            design_doc_repo_remote_url: None,
        },
    });
    assert_allowed(FrontendRequest::ResolveProjectDesignDoc {
        project_id: "proj_1".into(),
    });
}

#[test]
fn answer_agent_reply_stays_allowed() {
    // `boss comment reply`. The handler resolves comment and run from the
    // caller's own BOSS_RUN_ID, so it cannot target another thread.
    assert_allowed(FrontendRequest::CommentsPostAnswer {
        run_id: "exec_1".into(),
        body: "Here is the answer.".into(),
    });
}

#[test]
fn proposal_verbs_are_allowed() {
    assert_allowed(FrontendRequest::SubmitProposal {
        run_id: "exec_1".into(),
        kind: ProposalKind::Blocked,
        payload: json!({"reason": "stuck"}),
        idempotency_key: None,
    });
    assert_allowed(FrontendRequest::ListProposals {
        run_id: "exec_1".into(),
        kind: None,
        state: None,
    });
}

#[test]
fn taxonomy_reads_are_allowed() {
    assert_allowed(FrontendRequest::ListProducts);
    assert_allowed(FrontendRequest::GetWorkItem { id: "task_1".into() });
    assert_allowed(FrontendRequest::GetWorkItemByShortId {
        product_id: "prod_1".into(),
        short_id: 42,
    });
    assert_allowed(FrontendRequest::ListExecutions {
        work_item_id: Some("task_1".into()),
        include_revision_chain: false,
    });
    assert_allowed(FrontendRequest::ListRuns {
        execution_id: "exec_1".into(),
    });
}

// ── The mediation invariant ──────────────────────────────────────────────────

#[test]
fn mutating_taxonomy_verbs_are_denied_and_name_a_proposal_verb() {
    // The gap this task closes: `boss task create` from a worker shell used
    // to execute at RpcTier::User, unconditionally allowed.
    let denial = assert_denied(FrontendRequest::CreateTask {
        input: boss_protocol::CreateTaskInput::builder()
            .product_id("prod_1")
            .project_id("proj_1")
            .name("Something")
            .build(),
    });
    assert_eq!(denial.reason, WorkerTierDenialReason::MutatingTaxonomy);
    assert_eq!(denial.use_instead.as_deref(), Some("boss propose followup-task"));
    assert!(
        denial.message.contains("boss propose followup-task"),
        "the message must name the verb to use instead, got: {}",
        denial.message,
    );

    let denial = assert_denied(FrontendRequest::UpdateWorkItem {
        id: "task_1".into(),
        patch: WorkItemPatch::default(),
    });
    assert_eq!(denial.reason, WorkerTierDenialReason::MutatingTaxonomy);
    assert!(denial.use_instead.is_some());
}

#[test]
fn runtime_half_stays_closed() {
    // The sharpest edge: one worker reading another's transcript.
    let denial = assert_denied(FrontendRequest::TailRunTranscript {
        run_id: "exec_other".into(),
        lines: 100,
    });
    assert_eq!(denial.reason, WorkerTierDenialReason::RuntimeIsolation);

    for request in [
        FrontendRequest::GetDispatchState,
        FrontendRequest::ListWorkerLiveStates,
        FrontendRequest::StopRun {
            run_id: "exec_other".into(),
        },
        FrontendRequest::SendInputToWorker {
            run_id: "exec_other".into(),
            text: "hi".into(),
        },
    ] {
        let denial = assert_denied(request);
        assert_eq!(denial.reason, WorkerTierDenialReason::RuntimeIsolation);
    }
}

#[test]
fn coordinator_verbs_stay_closed() {
    for request in [
        FrontendRequest::RegisterBossSession { shell_pid: 1 },
        FrontendRequest::PlanProject {
            project_id: "proj_1".into(),
            dry_run: false,
            force: false,
        },
        FrontendRequest::MergeWhenReady {
            work_item_id: "task_1".into(),
        },
    ] {
        let denial = assert_denied(request);
        assert_eq!(denial.reason, WorkerTierDenialReason::CoordinatorOnly);
    }
}

#[test]
fn every_denial_tells_the_worker_what_to_do_next() {
    // A bare "denied" is the failure mode this whole project exists to kill:
    // the worker on the other end has to decide what to do next, and prose
    // it cannot act on sends it back to guessing.
    for request in [
        FrontendRequest::CreateTask {
            input: boss_protocol::CreateTaskInput::builder()
                .product_id("prod_1")
                .project_id("proj_1")
                .name("x")
                .build(),
        },
        FrontendRequest::DeleteWorkItem { id: "task_1".into() },
        FrontendRequest::GetDispatchState,
        FrontendRequest::ListHosts,
        FrontendRequest::TrunkStatus,
    ] {
        let verb = variant_name(&request);
        let denial = assert_denied(request);
        assert!(
            denial.message.contains(&verb),
            "denial must name the refused verb, got: {}",
            denial.message,
        );
        let actionable = denial.use_instead.is_some() || denial.message.contains("boss propose blocked");
        assert!(
            actionable,
            "{verb}'s denial must either redirect or point at the blocked escape hatch, got: {}",
            denial.message,
        );
    }
}

// ── Verb naming ──────────────────────────────────────────────────────────────

#[test]
fn variant_name_recovers_the_rust_variant_from_the_wire_tag() {
    // Tricky cases: consecutive capitals (`CiRemediation`), an embedded
    // acronym serde splits oddly (`GitHubAuthStart` → `git_hub_auth_start`),
    // and a short-id suffix.
    assert_eq!(variant_name(&FrontendRequest::ListProducts), "ListProducts");
    assert_eq!(variant_name(&FrontendRequest::GitHubAuthStart), "GitHubAuthStart");
    assert_eq!(
        variant_name(&FrontendRequest::MarkCiRemediationNoop {
            attempt_id: "ci_1".into(),
            observed_sha: Some("abc".into()),
            reason: Some("already-green".into()),
        }),
        "MarkCiRemediationNoop",
    );
    assert_eq!(
        variant_name(&FrontendRequest::GetWorkItemByShortId {
            product_id: "prod_1".into(),
            short_id: 42,
        }),
        "GetWorkItemByShortId",
    );
}

// ── Sanitization ─────────────────────────────────────────────────────────────

/// Every non-null value a forbidden key holds anywhere in the serialized
/// event, nesting included, as `(key, rendered value)` pairs.
///
/// Presence of the key is not itself a leak: [`WorkRun::transcript_path`] is
/// a plain `Option<String>` with no `skip_serializing_if`, so a sanitized row
/// still serializes `"transcript_path": null`. What must never survive is a
/// *value* — so that is what this looks for.
fn forbidden_values(event: &FrontendEvent) -> Vec<(String, String)> {
    fn walk(value: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if SANITIZED_EXECUTION_FIELDS.contains(&key.as_str()) && !child.is_null() {
                        out.push((key.clone(), child.to_string()));
                    }
                    walk(child, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&serde_json::to_value(event).expect("event must serialize"), &mut out);
    out
}

fn assert_nothing_leaked(event: &FrontendEvent) {
    let leaked = forbidden_values(event);
    assert!(
        leaked.is_empty(),
        "sanitized event still carries runtime-half values {leaked:?}; extend `sanitize.rs` to strip them",
    );
}

#[test]
fn run_transcript_and_artifacts_paths_are_stripped() {
    let before = FrontendEvent::RunResult { run: run() };
    // Guard the guard: the fixture must actually carry both paths, or this
    // test would pass against a sanitizer that does nothing.
    let mut before_keys = forbidden_values(&before)
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    before_keys.sort();
    assert_eq!(before_keys, vec!["artifacts_path", "transcript_path"]);

    let after = sanitize_event_for_worker(before);
    assert_nothing_leaked(&after);
    let FrontendEvent::RunResult { run } = after else {
        panic!("sanitizing must not change the variant");
    };
    assert!(run.transcript_path.is_none());
    assert!(run.artifacts_path.is_none());
    // Everything else survives — a sanitizer that blanked the row would
    // satisfy the forbidden-key assertion while making the read useless.
    assert_eq!(run.id, "run_1");
    assert_eq!(run.execution_id, "exec_1");
    assert_eq!(run.status, "active");
}

#[test]
fn every_run_carrying_event_is_sanitized() {
    for event in [
        FrontendEvent::RunResult { run: run() },
        FrontendEvent::RunCreated { run: run() },
        FrontendEvent::RunsList {
            execution_id: "exec_1".into(),
            runs: vec![run(), run()],
        },
    ] {
        assert_nothing_leaked(&sanitize_event_for_worker(event));
    }
}

#[test]
fn execution_rows_carry_no_runtime_half_fields() {
    // Today this passes because `WorkExecution` never had `host_id` /
    // `remote_pid` / `shell_pid` on the wire — they are DB columns
    // `mappers.rs` does not map. The assertion is here so that stays true:
    // adding one of them to the struct fails this test until `sanitize.rs`
    // strips it.
    for event in [
        FrontendEvent::ExecutionResult { execution: execution() },
        FrontendEvent::ExecutionsList {
            work_item_id: Some("chore_1".into()),
            executions: vec![execution()],
        },
        FrontendEvent::ExecutionCreated { execution: execution() },
        FrontendEvent::ExecutionRequested { execution: execution() },
        FrontendEvent::ExecutionCancelled { execution: execution() },
        FrontendEvent::RunReaped {
            run_id: "run_1".into(),
            execution: execution(),
        },
        FrontendEvent::PrReviewTriggered {
            execution: execution(),
            work_item_id: "chore_1".into(),
            pr_url: "https://github.com/o/r/pull/1".into(),
        },
    ] {
        assert_nothing_leaked(&sanitize_event_for_worker(event));
    }
}

#[test]
fn unrelated_events_pass_through_untouched() {
    let before = FrontendEvent::ProductsList { products: vec![] };
    let after = sanitize_event_for_worker(before);
    assert!(matches!(after, FrontendEvent::ProductsList { .. }));
}
