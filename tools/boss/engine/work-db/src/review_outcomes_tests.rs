use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
use boss_protocol::{
    CreateExecutionInput, ExecutionKind, ExecutionStatus, ProposalKind, ProposalState, RunDoneOutcome,
};
use serde_json::json;

const PR: &str = "https://github.com/example/repo/pull/42";

fn fixture(db: &WorkDb, legacy: bool) -> (String, String, String) {
    let product = create_test_product(db);
    let root = create_test_chore(db, &product.id, "Review target");
    let remediation = create_test_chore(db, &product.id, "Remediation");
    let execution = create_ready_chore_execution(db, &remediation.id);
    let reviewer = create_ready_chore_execution(db, &root.id);
    let mut finding = json!({
        "id": "C1", "severity": "high", "category": "correctness", "confidence": "high",
        "file": "src/lib.rs", "title": "Handle the error", "detail": "Return the error", "sources": ["codex-1"]
    });
    if legacy {
        finding.as_object_mut().unwrap().remove("id");
    }
    let payload = json!({"batch_id":"unused", "verdict":{
        "batch_id":"unused", "pr_url":PR, "target_sha":"reviewed", "phase":"pre_merge",
        "summary":"One issue", "revision_warranted":true, "findings":[finding], "contradictions":[]
    }});
    let conn = db.connect().unwrap();
    for task in [&root.id, &remediation.id] {
        conn.execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![task, PR])
            .unwrap();
    }
    conn.execute(
        "UPDATE work_executions SET pr_url = ?2 WHERE id = ?1",
        params![reviewer.id, PR],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO worker_proposals (id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at)
         VALUES ('verdict-proposal', ?1, ?2, 'review_verdict', ?3, 'verdict', 'applied', ?4)",
        params![reviewer.id, root.id, payload.to_string(), now_string()],
    ).unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &conn,
        &reviewer.id,
        &root.id,
        &ReviewVerdictInput {
            head_sha: Some("reviewed".into()),
            findings_count: 1,
            revision_warranted: true,
            gate_outcome: REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE pr_review_verdicts SET proposal_id = 'verdict-proposal', revision_task_id = ?1 WHERE execution_id = ?2",
        params![remediation.id, reviewer.id],
    )
    .unwrap();
    (root.id, remediation.id, execution.id)
}

fn submit(
    db: &WorkDb,
    task: &str,
    execution: &str,
    value: serde_json::Value,
    verify: bool,
) -> std::result::Result<SubmitWorkerProposalOutcome, boss_protocol::ProposalSubmissionError> {
    db.submit_review_outcomes(
        SubmitWorkerProposalInput {
            execution_id: execution,
            work_item_id: task,
            kind: ProposalKind::ReviewOutcomes,
            payload_json: &value.to_string(),
            idempotency_key: &value.to_string(),
        },
        &|url, sha| {
            assert_eq!(url, PR);
            assert_eq!(sha, "a".repeat(40));
            Ok(verify)
        },
    )
    .unwrap()
}

fn fixed() -> serde_json::Value {
    json!({"outcomes":[{"finding_id":"C1", "outcome":"fixed", "commit_sha":"a".repeat(40)}]})
}

fn done(db: &WorkDb, task: &str, execution: &str, outcome: RunDoneOutcome) -> ProposalState {
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id: execution,
        work_item_id: task,
        kind: ProposalKind::RunDone,
        payload_json: &json!({"outcome":outcome, "summary":"Finished"}).to_string(),
        idempotency_key: &format!("done-{outcome}-{}", next_id("key")),
    })
    .unwrap()
    .unwrap()
    .proposal
    .state
}

#[test]
fn complete_outcomes_are_durable_attributed_and_required_to_finish() {
    let (_dir, db) = open_db();
    let (_, task, execution) = fixture(&db, false);
    assert_eq!(
        done(&db, &task, &execution, RunDoneOutcome::Delivered),
        ProposalState::Rejected
    );
    let accepted = submit(&db, &task, &execution, fixed(), true).unwrap();
    assert_eq!(accepted.proposal.state, ProposalState::Applied);
    assert_eq!(accepted.proposal.execution_id, execution);
    let history = db
        .review_finding_outcomes(accepted.proposal.applied_ref.as_deref().unwrap())
        .unwrap();
    assert_eq!(history, vec![accepted.proposal]);
    assert_eq!(
        done(&db, &task, &execution, RunDoneOutcome::Delivered),
        ProposalState::Applied
    );
    let replay = submit(&db, &task, &execution, fixed(), false).unwrap();
    assert!(replay.already_submitted);
    // A successor run must account for the findings itself.
    let next = create_ready_chore_execution(&db, &task);
    assert_eq!(
        done(&db, &task, &next.id, RunDoneOutcome::NoChangesNeeded),
        ProposalState::Rejected
    );
}

#[test]
fn missing_unknown_duplicate_unproven_and_malformed_outcomes_are_rejected() {
    let (_dir, db) = open_db();
    let (_, task, execution) = fixture(&db, false);
    for invalid in [
        json!({"outcomes":[]}),
        json!({"outcomes":[{"finding_id":"other", "outcome":"not_fixed", "reason":"Needs work"}]}),
        json!({"outcomes":[{"finding_id":"C1", "outcome":"fixed"}]}),
        json!({"outcomes":[{"finding_id":"C1", "outcome":"not_fixed", "reason":" "}]}),
        json!({"outcomes":[{"finding_id":"C1", "outcome":"not_applicable"}]}),
        json!({"outcomes":[fixed()["outcomes"][0], fixed()["outcomes"][0]]}),
    ] {
        assert!(submit(&db, &task, &execution, invalid, true).is_err());
    }
    assert!(submit(&db, &task, &execution, fixed(), false).is_err());
    assert!(require_complete(&db.connect().unwrap(), &execution).is_err());
    let direct = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution,
            work_item_id: &task,
            kind: ProposalKind::ReviewOutcomes,
            payload_json: &fixed().to_string(),
            idempotency_key: "unverified",
        })
        .unwrap();
    assert!(direct.is_err(), "generic storage cannot bypass branch verification");
}

#[test]
fn guide_uses_recorded_outcome_instead_of_task_status() {
    let (_dir, db) = open_db();
    let (root, task, execution) = fixture(&db, false);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", params![task])
        .unwrap();
    let guide = db.review_guide_findings(&root, PR).unwrap().unwrap();
    assert!(guide.status_text.contains("0 fixed on PR"));
    submit(&db, &task, &execution, fixed(), true).unwrap();
    let guide = db.review_guide_findings(&root, PR).unwrap().unwrap();
    assert!(guide.status_text.contains("1 fixed on PR"));
    for outcome in ["not_fixed", "not_applicable"] {
        submit(
            &db,
            &task,
            &execution,
            json!({"outcomes":[{
                "finding_id":"C1", "outcome":outcome, "reason":"The branch no longer uses this path"
            }]}),
            true,
        )
        .unwrap();
        let guide = db.review_guide_findings(&root, PR).unwrap().unwrap();
        assert!(guide.status_text.contains("0 fixed on PR"));
        assert!(guide.addendum_markdown.contains("The branch no longer uses this path"));
    }
    let verdict_id = db.latest_review_verdict(&root).unwrap().unwrap().id;
    let history = db.review_finding_outcomes(&verdict_id).unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history
            .iter()
            .filter(|record| record.state == ProposalState::Applied)
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .filter(|record| record.state == ProposalState::Superseded)
            .count(),
        2
    );
}

#[test]
fn legacy_findings_keep_completion_and_guide_behavior() {
    let (_dir, db) = open_db();
    let (root, task, execution) = fixture(&db, true);
    assert_eq!(
        done(&db, &task, &execution, RunDoneOutcome::Delivered),
        ProposalState::Applied
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", params![task])
        .unwrap();
    assert!(
        db.review_guide_findings(&root, PR)
            .unwrap()
            .unwrap()
            .status_text
            .contains("1 fixed on PR")
    );
}

#[test]
fn fixed_outcome_on_followup_pr_is_not_a_fix_on_original_pr() {
    let (_dir, db) = open_db();
    let (root, task, execution) = fixture(&db, false);
    submit(&db, &task, &execution, fixed(), true).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            params![task, "https://github.com/example/repo/pull/43"],
        )
        .unwrap();
    assert!(require_complete(&db.connect().unwrap(), &execution).is_err());
    let value = json!({"outcomes":[{"finding_id":"C1","outcome":"fixed","commit_sha":"a".repeat(40)}]});
    db.submit_review_outcomes(
        SubmitWorkerProposalInput {
            execution_id: &execution,
            work_item_id: &task,
            kind: ProposalKind::ReviewOutcomes,
            payload_json: &value.to_string(),
            idempotency_key: "new-pr",
        },
        &|url, _| Ok(url.ends_with("/43")),
    )
    .unwrap()
    .unwrap();
    assert!(
        db.review_guide_findings(&root, PR)
            .unwrap()
            .unwrap()
            .status_text
            .contains("0 fixed on PR")
    );
}

#[test]
fn legacy_completion_entrypoints_cannot_skip_identified_outcomes() {
    let (_dir, db) = open_db();
    let (_, _, execution) = fixture(&db, false);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            params![execution],
        )
        .unwrap();
    let err = db
        .record_worker_no_op_completion(&execution, "No changes", None)
        .unwrap_err();
    assert!(err.to_string().contains("review remediation incomplete"));
    let err = db
        .record_worker_pr_completion(
            &execution,
            PR,
            Some(&"a".repeat(40)),
            None,
            WorkerPrCompletionTarget::InReview,
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("review remediation incomplete"));
    assert!(!db.get_execution(&execution).unwrap().status.is_terminal());
}

#[test]
fn assignment_is_rechecked_after_network_verification() {
    let (_dir, db) = open_db();
    let (_, task, execution) = fixture(&db, false);
    let outcome = db
        .submit_review_outcomes(
            SubmitWorkerProposalInput {
                execution_id: &execution,
                work_item_id: &task,
                kind: ProposalKind::ReviewOutcomes,
                payload_json: &fixed().to_string(),
                idempotency_key: "racing-binding",
            },
            &|_, _| {
                db.connect().unwrap().execute(
                    "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
                    params![task, "https://github.com/example/repo/pull/43"],
                )?;
                Ok(true)
            },
        )
        .unwrap();
    assert!(outcome.is_err());
    assert!(require_complete(&db.connect().unwrap(), &execution).is_err());
}

#[test]
fn progression_on_a_remediation_task_is_not_gated() {
    let (_dir, db) = open_db();
    let (_, task, execution) = fixture(&db, false);
    let progression = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_eq!(
        done(&db, &task, &progression.id, RunDoneOutcome::Delivered),
        ProposalState::Applied
    );
    assert_eq!(
        done(&db, &task, &execution, RunDoneOutcome::Delivered),
        ProposalState::Rejected
    );
}

#[test]
fn merged_completion_without_outcomes_terminalizes_as_incomplete() {
    let (_dir, db) = open_db();
    let (_, _, execution) = fixture(&db, false);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            params![execution],
        )
        .unwrap();
    let completion = db
        .record_worker_pr_completion(
            &execution,
            PR,
            Some(&"a".repeat(40)),
            None,
            WorkerPrCompletionTarget::Done,
            None,
        )
        .unwrap()
        .expect("merged completion must terminalize");
    assert!(completion.execution.status.is_terminal());
    let items = db.list_attention_items(&execution).unwrap();
    assert!(
        items.iter().any(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::REVIEW_OUTCOMES_NOT_SUBMITTED_ATTENTION_KIND
        }),
        "{items:?}"
    );
    let err = require_complete(&db.connect().unwrap(), &execution).unwrap_err();
    assert!(err.is::<ReviewRemediationIncomplete>(), "{err:#}");
}

#[test]
fn default_idempotency_key_does_not_replay_a_stale_pr_binding() {
    let (_dir, db) = open_db();
    let (_, task, execution) = fixture(&db, false);
    let payload = fixed();
    let key = payload.to_string();
    db.submit_review_outcomes(
        SubmitWorkerProposalInput {
            execution_id: &execution,
            work_item_id: &task,
            kind: ProposalKind::ReviewOutcomes,
            payload_json: &payload.to_string(),
            idempotency_key: &key,
        },
        &|_, _| Ok(true),
    )
    .unwrap()
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            params![task, "https://github.com/example/repo/pull/43"],
        )
        .unwrap();
    assert!(require_complete(&db.connect().unwrap(), &execution).is_err());
    let replay = db
        .submit_review_outcomes(
            SubmitWorkerProposalInput {
                execution_id: &execution,
                work_item_id: &task,
                kind: ProposalKind::ReviewOutcomes,
                payload_json: &payload.to_string(),
                idempotency_key: &key,
            },
            &|_, _| Ok(true),
        )
        .unwrap()
        .expect_err("stale idempotency key must not replay");
    assert!(replay.message.contains("stale outcome set"), "{}", replay.message);
}
