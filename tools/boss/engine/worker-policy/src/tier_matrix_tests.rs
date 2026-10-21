//! The per-tier decision matrix: every sanctioned worker write is denied to
//! progression, and the progression allow-set is pinned to an explicit table.

use super::*;
use boss_protocol::{FrontendRequest, ProposalKind, WorkerTierDenialReason};
use serde_json::json;
use std::collections::BTreeSet;

fn req(value: serde_json::Value) -> FrontendRequest {
    serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("fixture {value} is not a FrontendRequest: {e}"))
}

/// One request per verb in [`PROGRESSION_ALLOWED_VERBS`]. A new `Allow` for
/// progression must add a row here and a name there, so it shows as a diff.
fn progression_allowed_fixtures() -> Vec<FrontendRequest> {
    vec![
        req(json!({"type": "comments_banner_state", "artifact_kind": "task", "artifact_id": "t"})),
        req(json!({"type": "comments_get", "comment_id": "c"})),
        req(json!({"type": "comments_list", "artifact_kind": "task", "artifact_id": "t"})),
        req(json!({"type": "find_work_items_by_pr", "pr_number": 1})),
        req(json!({"type": "get_attention_group", "id": "a"})),
        req(json!({"type": "get_attention_item", "id": "a"})),
        req(json!({"type": "get_decision", "id": "d"})),
        req(json!({"type": "get_engine_version"})),
        req(json!({"type": "get_execution", "id": "e"})),
        req(json!({"type": "get_pr_body", "run_id": "e"})),
        req(json!({"type": "get_pr_status", "run_id": "e"})),
        req(json!({"type": "get_product_design_doc", "repo_remote_url": "u", "path": "p", "git_ref": "main"})),
        req(json!({"type": "get_run", "id": "r"})),
        req(json!({"type": "get_work_item", "id": "t"})),
        req(json!({"type": "get_work_item_by_short_id", "product_id": "p", "short_id": 1})),
        req(json!({"type": "get_work_runtime_patch", "product_id": "p", "item_ids": []})),
        req(json!({"type": "get_work_tree", "product_id": "p"})),
        req(json!({"type": "get_worker_context", "run_id": "e"})),
        req(json!({"type": "list_answer_agent_runs", "comment_id": "c"})),
        req(json!({"type": "list_attachments", "run_id": "e"})),
        req(json!({"type": "list_attention_groups", "product_id": "p"})),
        req(json!({"type": "list_attention_items", "execution_id": "e"})),
        req(json!({"type": "list_attention_items_for_work_item", "work_item_id": "t"})),
        req(json!({"type": "list_chores", "product_id": "p"})),
        req(json!({"type": "list_decisions", "product_id": "p"})),
        req(json!({"type": "list_deferred_scope_attentions", "product_id": "p"})),
        req(json!({"type": "list_dependencies", "work_item": "t"})),
        req(json!({"type": "list_dependencies_detailed", "work_item": "t"})),
        req(json!({"type": "list_executions", "work_item_id": null})),
        req(json!({"type": "list_operator_questions", "id": "t"})),
        req(json!({"type": "list_product_design_docs", "product_id": "p"})),
        req(json!({"type": "list_products"})),
        req(json!({"type": "list_projects", "product_id": "p"})),
        req(json!({"type": "list_proposals", "run_id": "e"})),
        req(json!({"type": "list_revisions", "product_id": "p"})),
        req(json!({"type": "list_runs", "execution_id": "e"})),
        req(json!({"type": "list_tasks", "product_id": "p", "project_id": null})),
        req(json!({"type": "resolve_project_design_doc", "project_id": "p"})),
        req(json!({"type": "submit_proposal", "run_id": "e", "kind": "wait", "payload": {}})),
    ]
}

/// One request per `Allow` arm the worker match marks as a sanctioned write.
fn sanctioned_write_fixtures() -> Vec<FrontendRequest> {
    vec![
        FrontendRequest::SubmitAttachment {
            run_id: "e".into(),
            path: "/tmp/x.png".into(),
            caption: None,
        },
        FrontendRequest::ClassifyCiRemediation {
            queue_ci_evidence: None,
            attempt_id: "ci_1".into(),
            triage_class: "tractable".into(),
        },
        FrontendRequest::CommentsPostAnswer {
            run_id: "e".into(),
            body: "answer".into(),
            proposed_edit: None,
        },
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id: "e".into(),
            comment_id: "c".into(),
            disposition: boss_protocol::GuideCommentDisposition::NoChange,
            body: "done".into(),
            request_regeneration: false,
        },
        FrontendRequest::CreateAutomationTask {
            automation_id: "auto_1".into(),
            name: "Fix".into(),
            description: None,
            target_files: vec![],
            target_symbols: vec![],
        },
        FrontendRequest::MarkCiRemediationFailed {
            queue_ci_evidence: None,
            attempt_id: "ci_1".into(),
            reason: "x".into(),
        },
        FrontendRequest::MarkCiRemediationNoop {
            attempt_id: "ci_1".into(),
            observed_sha: None,
            reason: None,
        },
        FrontendRequest::MarkCiRemediationRetriggered {
            attempt_id: "ci_1".into(),
            new_id: "b".into(),
        },
        FrontendRequest::MarkCiRemediationSucceededViaRebase {
            attempt_id: "ci_1".into(),
        },
        FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: "cr_1".into(),
            reason: "x".into(),
        },
        FrontendRequest::RecordProducerSideConflict {
            execution_id: "e".into(),
            head_branch: "h".into(),
            base_branch: "main".into(),
            conflicted_files: vec![],
        },
        FrontendRequest::SetProjectDesignDoc {
            input: boss_protocol::SetProjectDesignDocInput {
                project_id: "p".into(),
                unset: false,
                design_doc_branch: None,
                design_doc_path: Some("d.md".into()),
                design_doc_repo_remote_url: None,
            },
        },
        FrontendRequest::SetTaskDocPointer {
            input: boss_protocol::SetTaskDocPointerInput {
                task_id: "t".into(),
                unset: false,
                doc_branch: None,
                doc_path: Some("d.md".into()),
                doc_repo_remote_url: None,
            },
        },
    ]
}

/// Representative denials from every other class, denied to both tiers.
fn denied_for_both_fixtures() -> Vec<FrontendRequest> {
    vec![
        req(json!({"type": "delete_work_item", "id": "t"})),
        req(json!({"type": "get_dispatch_state"})),
        req(json!({"type": "set_dispatch_paused", "paused": true})),
        FrontendRequest::AnswerOperatorQuestion {
            id: "oq".into(),
            answer: boss_protocol::OperatorAnswer::YesNo { value: true },
        },
        req(json!({"type": "shutdown", "token": "t"})),
        req(json!({"type": "list_hosts"})),
        req(json!({"type": "list_ideas", "product_id": "p"})),
    ]
}

#[test]
fn sanctioned_writes_are_allowed_to_workers_and_denied_to_progression() {
    for request in sanctioned_write_fixtures() {
        let verb = variant_name(&request);
        assert!(verb_decision(RpcTier::Worker, &request).is_allowed(), "{verb} / Worker");
        assert!(
            !verb_decision(RpcTier::Progression, &request).is_allowed(),
            "{verb} / Progression"
        );
    }
}

#[test]
fn progression_allow_set_is_exactly_the_pinned_table() {
    let fixtures = progression_allowed_fixtures();
    let fixture_verbs: BTreeSet<String> = fixtures.iter().map(variant_name).collect();
    let table: BTreeSet<String> = PROGRESSION_ALLOWED_VERBS.iter().map(|v| (*v).to_owned()).collect();
    assert_eq!(
        fixture_verbs, table,
        "fixtures and PROGRESSION_ALLOWED_VERBS must match"
    );
    assert_eq!(table.len(), PROGRESSION_ALLOWED_VERBS.len(), "no duplicate entries");
    for request in &fixtures {
        let verb = variant_name(request);
        assert!(
            verb_decision(RpcTier::Progression, request).is_allowed(),
            "{verb} / Progression"
        );
        // Everything progression may call is a verb the worker tier allows
        // too, so the table can never smuggle in a verb workers lack.
        assert!(verb_decision(RpcTier::Worker, request).is_allowed(), "{verb} / Worker");
    }
}

/// Candidate JSON values tried, in order, for a field the synthesiser has to
/// invent: whichever one the field's type accepts wins.
fn candidate(index: usize) -> Option<serde_json::Value> {
    [json!("x"), json!(0), json!(false), json!([]), json!({})]
        .get(index)
        .cloned()
}

fn upper_camel_to_snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn object_at<'a>(
    root: &'a mut serde_json::Value,
    path: &[String],
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let mut node = root;
    for key in path {
        node = node.get_mut(key).expect("path exists");
    }
    node.as_object_mut().expect("path names an object")
}

/// The first backticked token after `expected`, for `unknown variant` errors.
fn first_expected_variant(message: &str) -> Option<String> {
    let rest = &message[message.find("expected")?..];
    Some(rest.split('`').nth(1)?.to_owned())
}

/// Builds a minimal valid request for `variant` from nothing but the serde
/// errors: each missing field is filled with the first candidate its type
/// accepts, descending into nested structs it had to open as `{}`. This keeps
/// the matrix one-row-per-variant without a hand-written payload for each of
/// the request vocabulary's ~220 variants.
/// Starting JSON for a variant: its wire tag, plus a value for each field whose
/// type is a tagged enum the synthesiser cannot guess.
fn seed(variant: &str) -> serde_json::Value {
    let tag = upper_camel_to_snake(variant);
    match variant {
        "AnswerOperatorQuestion" => json!({"type": tag, "answer": {"kind": "yes_no", "value": true}}),
        // `input` is flattened into the request, so its fields sit at the root.
        "CreateAutomation" => json!({
            "type": tag, "product_id": "p", "name": "n", "standing_instruction": "s",
            "trigger": {"kind": "schedule", "cron": "0 0 * * *", "timezone": "UTC"},
        }),
        "EngineResponse" => json!({"type": tag, "response": {"kind": "focus_worker_pane", "result": {"Ok": {}}}}),
        _ => json!({ "type": tag }),
    }
}

fn synthesize(variant: &str) -> Result<FrontendRequest, String> {
    let mut value = seed(variant);
    // Objects the synthesiser can add fields to, innermost last: the root, then
    // each `{}` it opens itself.
    let mut objects: Vec<Vec<String>> = vec![vec![]];
    // The field filled last: where it lives, its name, and the candidate used.
    let mut last: Option<(Vec<String>, String, usize)> = None;
    for _ in 0..400 {
        let error = match serde_json::from_value::<FrontendRequest>(value.clone()) {
            Ok(request) => return Ok(request),
            Err(error) => error.to_string(),
        };
        if let Some(rest) = error.strip_prefix("missing field `") {
            let field = rest.split('`').next().expect("field name").to_owned();
            let path = objects
                .iter()
                .rev()
                .find(|path| !object_at(&mut value, path).contains_key(&field))
                .ok_or_else(|| format!("{variant}: cannot place missing field {field}: {error}"))?
                .clone();
            object_at(&mut value, &path).insert(field.clone(), candidate(0).expect("first candidate"));
            last = Some((path, field, 0));
            continue;
        }
        let (path, field, index) = last
            .clone()
            .ok_or_else(|| format!("{variant}: cannot synthesise a request: {error}"))?;
        let next = if error.starts_with("unknown variant") {
            first_expected_variant(&error).map(serde_json::Value::from)
        } else {
            candidate(index + 1)
        };
        let next = next.ok_or_else(|| format!("{variant}: no candidate fits field {field}: {error}"))?;
        if next == json!({}) {
            let mut inner = path.clone();
            inner.push(field.clone());
            objects.push(inner);
        }
        object_at(&mut value, &path).insert(field.clone(), next);
        last = Some((path, field, index + 1));
    }
    Err(format!("{variant}: synthesis did not converge"))
}

/// Every verb the exhaustive worker match names. The match has no wildcard arm,
/// so it lists the whole `FrontendRequest` vocabulary: a new variant fails to
/// compile until classified, and then shows up here and needs no edit.
fn classified_verbs() -> BTreeSet<String> {
    let source = include_str!("policy.rs");
    let body = source
        .split("fn worker_decision(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("worker_decision body");
    body.split("FrontendRequest::")
        .skip(1)
        .filter_map(|rest| rest.split(|c: char| !c.is_alphanumeric()).next())
        .filter(|name| name.chars().next().is_some_and(char::is_uppercase))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_frontend_request_variant_has_an_explicit_decision_for_both_tiers() {
    let classified = classified_verbs();
    assert!(
        classified.len() > 200,
        "vocabulary scan found only {}",
        classified.len()
    );

    // Expected decisions, spelled out per tier. Workers are allowed exactly the
    // progression table plus the sanctioned writes; progression is allowed
    // exactly the table. Everything else is denied to both: deny is the default.
    let progression_allowed: BTreeSet<String> = PROGRESSION_ALLOWED_VERBS.iter().map(|v| (*v).to_owned()).collect();
    let sanctioned: BTreeSet<String> = sanctioned_write_fixtures().iter().map(variant_name).collect();
    let worker_allowed: BTreeSet<String> = progression_allowed.union(&sanctioned).cloned().collect();
    for verb in worker_allowed.iter() {
        assert!(
            classified.contains(verb),
            "{verb} is not a classified FrontendRequest verb"
        );
    }

    let mut fixtures = BTreeSet::new();
    let mut unsynthesised = Vec::new();
    for verb in &classified {
        let request = match synthesize(verb) {
            Ok(request) => request,
            Err(error) => {
                unsynthesised.push(error);
                continue;
            }
        };
        assert_eq!(&variant_name(&request), verb, "fixture is for a different verb");
        let worker = verb_decision(RpcTier::Worker, &request);
        let progression = verb_decision(RpcTier::Progression, &request);
        assert_eq!(worker.is_allowed(), worker_allowed.contains(verb), "{verb} / Worker");
        assert_eq!(
            progression.is_allowed(),
            progression_allowed.contains(verb),
            "{verb} / Progression"
        );
        if !progression.is_allowed() {
            let denial = progression.denial().expect("denied");
            assert_eq!(denial.reason, WorkerTierDenialReason::ProgressionScope, "{verb}");
        }
        for tier in [RpcTier::User, RpcTier::AppOrBoss, RpcTier::BossOnly] {
            assert!(verb_decision(tier, &request).is_allowed(), "{verb} / {tier:?}");
        }
        fixtures.insert(verb.clone());
    }
    assert!(unsynthesised.is_empty(), "{}", unsynthesised.join("\n"));
    // One fixture per classified verb, no more and no fewer.
    assert_eq!(fixtures, classified);
}

#[test]
fn other_classes_are_denied_to_both_agent_tiers() {
    for request in denied_for_both_fixtures() {
        let verb = variant_name(&request);
        for tier in [RpcTier::Worker, RpcTier::Progression] {
            assert!(!verb_decision(tier, &request).is_allowed(), "{verb} / {tier:?}");
        }
    }
}

#[test]
fn trusted_tiers_allow_everything() {
    for tier in [RpcTier::User, RpcTier::AppOrBoss, RpcTier::BossOnly] {
        for request in denied_for_both_fixtures() {
            assert!(verb_decision(tier, &request).is_allowed());
        }
    }
}

#[test]
fn progression_denials_carry_no_worker_guidance() {
    let request = req(json!({"type": "update_work_item", "id": "t", "patch": {}}));
    let verb_denial = verb_decision(RpcTier::Progression, &request).denial().cloned().unwrap();
    assert_eq!(verb_denial.verb, "UpdateWorkItem");
    assert_eq!(verb_denial.reason, WorkerTierDenialReason::ProgressionScope);
    assert_eq!(verb_denial.use_instead, None);
    for denial in [
        verb_denial,
        proposal_kind_decision(RpcTier::Progression, ProposalKind::Wait)
            .denial()
            .cloned()
            .unwrap(),
    ] {
        assert_eq!(denial.reason, WorkerTierDenialReason::ProgressionScope);
        assert!(denial.message.contains("progression sessions"), "{}", denial.message);
        assert!(!denial.message.contains("boss propose blocked"), "{}", denial.message);
        assert!(!denial.message.contains("worker sessions"), "{}", denial.message);
        assert!(!denial.message.contains("coordinator"), "{}", denial.message);
    }
}

#[test]
fn proposal_kinds_are_denied_to_progression_and_allowed_to_workers() {
    for &kind in ProposalKind::ALL {
        assert!(
            !proposal_kind_decision(RpcTier::Progression, kind).is_allowed(),
            "{kind}"
        );
        assert!(proposal_kind_decision(RpcTier::Worker, kind).is_allowed(), "{kind}");
    }
}
