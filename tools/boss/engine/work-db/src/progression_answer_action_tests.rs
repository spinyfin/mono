//! A progression question whose answers the engine carries out.
//!
//! The fixture is the board as it stood when a pull request sat stuck for
//! days: a merge-queue build had failed, the CI-fix revision created for it
//! was left unstarted after its worker was cancelled, the PR then conflicted
//! with main, and the conflict work a progression run created was queued
//! behind the dead revision. The owner answered Yes twice to "skip the
//! build-fix" and nothing moved, because an answer changed nothing by itself
//! and the next run's `archive-revision` was refused for the same reason as
//! the first. The third question asked the owner to delete the row by hand.

use super::*;
use boss_protocol::{OperatorAnswer, OperatorQuestionError, OperatorQuestionStatus};

struct Stuck {
    item: String,
    ci_fix: String,
    conflict_work: String,
    attempt: String,
}

fn stuck(fx: &Fx) -> Stuck {
    // Every check on the head passes, but the pending merge-queue attempt
    // keeps the stored CI state at `fail`.
    let item = fx.pr_item("Show a banner", "fail");
    let ci_fix = fx.revision_of(&item, "Fix failing CI: build", "todo");
    fx.sql(&format!(
        "UPDATE tasks SET autostart = 0, created_via = 'ci-fix:cir_1' WHERE id = '{ci_fix}'"
    ));
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&ci_fix)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Cancelled)
                .build(),
        )
        .unwrap();
    let attempt = fx
        .db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: fx.product.clone(),
            work_item_id: item.clone(),
            pr_url: fx.task(&item).pr_url.unwrap(),
            pr_number: 100,
            head_branch: "feature".into(),
            head_sha_at_trigger: "mq:head-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "merge_queue_rebounce".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap()
        .id;
    let conflict_work = fx.revision_of(&item, "Unstick: Show a banner", "blocked");
    fx.sql(&format!(
        "UPDATE ci_remediations SET revision_task_id = '{ci_fix}' WHERE id = '{attempt}';
         UPDATE tasks SET blocked_reason = 'dependency', autostart = 1 WHERE id = '{conflict_work}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at)
         VALUES ('{conflict_work}', '{ci_fix}', 'now')"
    ));
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&conflict_work)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::WaitingDependency)
                .build(),
        )
        .unwrap();
    Stuck {
        item,
        ci_fix,
        conflict_work,
        attempt,
    }
}

const EXPLANATION: &str = "Context:\nThe pull request conflicts with main and a cancelled build-fix blocks the conflict \
     work.\nYes:\nThe build-fix is skipped and the conflict work starts.\nNo:\nThe build-fix runs first, then the \
     conflict work.\nRecommendation:\nI recommend Yes because the pull request head already passes its checks.";

/// The first question that was asked, with the change each answer means bound to it.
fn skip_or_run(stuck: &Stuck) -> serde_json::Value {
    json!({
        "outcome": "blocked", "summary": "A cancelled build-fix blocks the conflict work", "criterion": "2",
        "question": {"text": "Skip that build-fix so we can resolve the conflicts with main?",
                     "answer_type": {"kind": "yes_no"}, "explanation": EXPLANATION},
        "answer_actions": [
            {"answer": "yes", "action": {"kind": "archive_revision", "revision": stuck.ci_fix}},
            {"answer": "no", "action": {"kind": "start", "item": stuck.ci_fix}},
        ],
    })
}

/// Ask `payload` from a fresh run on the stuck item and park the item on it.
fn ask(fx: &Fx, stuck: &Stuck, payload: serde_json::Value) -> String {
    let run = fx.run(&stuck.item, FINGERPRINT, "S13_revision_chain");
    let asked = done(fx, &run, payload, "ask");
    assert_eq!(
        asked.proposal.state,
        ProposalState::Applied,
        "{:?}",
        asked.proposal.decision_reason
    );
    fx.db.finish_progression_declared(&run).unwrap();
    assert_eq!(
        fx.task(&stuck.item).blocked_reason.as_deref(),
        Some("awaiting_operator_answer")
    );
    run
}

fn rejection(fx: &Fx, run: &str, payload: serde_json::Value, key: &str) -> String {
    let outcome = done(fx, run, payload, key);
    assert_eq!(outcome.proposal.state, ProposalState::Rejected);
    outcome.proposal.decision_reason.expect("a rejection says why")
}

fn attempt_row(fx: &Fx, id: &str) -> CiRemediation {
    query_ci_remediation(&fx.db.connect().unwrap(), id).unwrap().unwrap()
}

fn answer(fx: &Fx, item: &str, answer: OperatorAnswer) -> Result<OperatorAnswerOutcome> {
    fx.db.answer_operator_question_with(item, answer, &fx.checker)
}

/// What made the run ask in the first place, and ask again after each Yes: its
/// own verb needs the parent PR green, and the dead revision's own pending
/// attempt is what keeps the stored state red.
#[test]
fn the_agents_own_archive_is_refused_while_the_dead_fix_keeps_ci_reported_red() {
    let fx = fx();
    let stuck = stuck(&fx);
    let run = fx.run(&stuck.item, FINGERPRINT, "S13_revision_chain");
    let refusal = fx
        .submit(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": stuck.ci_fix, "reason": "cancelled and moot"}),
        )
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("is not green"), "{refusal}");
    assert_eq!(fx.task(&stuck.ci_fix).status, TaskStatus::Todo);
}

#[test]
fn the_owners_yes_closes_the_dead_revision_and_releases_what_waited_on_it() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(&fx, &stuck, skip_or_run(&stuck));

    let outcome = answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: true }).expect("the answer is applied");

    // The revision is closed, as the owner, by the answer.
    let closed = fx.task(&stuck.ci_fix);
    assert_eq!(closed.status, TaskStatus::Archived);
    assert!(closed.deleted_at.is_some());
    assert_eq!(
        closed.archived_by.as_deref(),
        Some(crate::answer_actions::ARCHIVE_MECHANISM_OWNER_ANSWER)
    );
    assert_eq!(fx.actor(&stuck.ci_fix), "human");
    // The attempt that created it ends with it, so it stops reporting the PR red.
    let ended = attempt_row(&fx, &stuck.attempt);
    assert_eq!(ended.status, "abandoned");
    assert_eq!(ended.failure_reason.as_deref(), Some("revision_archived_by_owner"));
    assert!(
        fx.db
            .active_ci_remediation_for_work_item(&stuck.item)
            .unwrap()
            .is_none()
    );
    // The conflict work is released.
    assert!(fx.db.gating_prereqs_for(&stuck.conflict_work).unwrap().is_empty());
    assert_ne!(fx.task(&stuck.conflict_work).status, TaskStatus::Blocked);
    // The answer is recorded, the item is back in review, and the change is confirmed where the next run reads it.
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Answered
    );
    let item = fx.task(&stuck.item);
    assert_eq!(item.status, TaskStatus::InReview);
    assert!(
        item.description
            .contains("The engine made the change bound to this answer (`archive-revision"),
        "{}",
        item.description
    );
    assert!(item.description.contains("closed revision"), "{}", item.description);
    assert!(
        item.description.contains("the CI attempt that created it was retired"),
        "{}",
        item.description
    );
    // The board is told about every row that moved.
    for moved in [&stuck.ci_fix, &stuck.conflict_work] {
        assert!(outcome.touched_work_item_ids.contains(moved), "{moved}");
    }
    assert_eq!(outcome.retired_ci_attempts.len(), 1);
    assert_eq!(outcome.retired_ci_attempts[0].id, stuck.attempt);
    assert_eq!(outcome.retired_ci_attempts[0].status, "abandoned");
    // Answering again is the idempotent repeat it always was.
    answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: true }).expect("idempotent repeat");
}

#[test]
fn the_owners_no_starts_the_revision_instead() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(&fx, &stuck, skip_or_run(&stuck));

    answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: false }).expect("the answer is applied");

    let started = fx.task(&stuck.ci_fix);
    assert!(!started.status.is_terminal(), "{:?}", started.status);
    assert!(
        fx.executions(&stuck.ci_fix)
            .iter()
            .any(|execution| execution.status == ExecutionStatus::Ready),
        "the build-fix must have a run queued: {:?}",
        fx.executions(&stuck.ci_fix)
            .iter()
            .map(|execution| execution.status.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(attempt_row(&fx, &stuck.attempt).status, "pending");
    assert!(
        fx.task(&stuck.item)
            .description
            .contains("The engine made the change bound to this answer (`start")
    );
    // The conflict work still waits behind it: this is the order the owner chose.
    assert_eq!(
        fx.db.gating_prereqs_for(&stuck.conflict_work).unwrap(),
        vec![stuck.ci_fix.clone()]
    );
}

#[test]
fn a_change_that_cannot_be_made_refuses_the_answer_and_leaves_the_question_open() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(&fx, &stuck, skip_or_run(&stuck));
    // A worker got onto the revision after the question was asked.
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&stuck.ci_fix)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let before = fx.task(&stuck.item);

    let error = answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: true }).unwrap_err();

    let Some(OperatorQuestionError::ValidationFailed { message }) = error.downcast_ref::<OperatorQuestionError>()
    else {
        panic!("expected a typed refusal the card can show, got {error:#}");
    };
    assert!(message.contains("archive-revision"), "{message}");
    assert!(message.contains("a worker is on"), "{message}");
    assert!(message.contains("the question is still open"), "{message}");
    // Nothing was recorded: not the answer, not the change, not the note.
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Open
    );
    assert_eq!(fx.task(&stuck.ci_fix).status, TaskStatus::Todo);
    assert_eq!(attempt_row(&fx, &stuck.attempt).status, "pending");
    let after = fx.task(&stuck.item);
    assert_eq!(after.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    assert_eq!(after.description, before.description);
    // The other answer is still there to pick, and is already true: the build-fix is running.
    answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: false }).expect("the other answer applies");
    assert!(fx.task(&stuck.item).description.contains("nothing was left to change"));
}

#[test]
fn a_change_the_engine_already_made_is_not_an_error_when_the_answer_arrives() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(&fx, &stuck, skip_or_run(&stuck));
    // The merge poller's reconciliation closed the revision in the meantime.
    assert_eq!(fx.db.retire_moot_pending_ci_fixes(&stuck.item, true).unwrap().len(), 1);
    assert_eq!(fx.task(&stuck.ci_fix).status, TaskStatus::Archived);

    answer(&fx, &stuck.item, OperatorAnswer::YesNo { value: true }).expect("nothing left to do is not a failure");

    let item = fx.task(&stuck.item);
    assert!(
        item.description.contains("nothing was left to change"),
        "{}",
        item.description
    );
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Answered
    );
}

#[test]
fn a_dependency_can_be_dropped_by_the_owners_choice() {
    let fx = fx();
    let stuck = stuck(&fx);
    let choices = ["Start the conflict work now", "Keep waiting for the build-fix"];
    ask(
        &fx,
        &stuck,
        json!({
            "outcome": "blocked", "summary": "The conflict work waits on a cancelled build-fix", "criterion": "2",
            "question": {"text": "Start the conflict work without waiting for the cancelled build-fix?",
                         "answer_type": {"kind": "multiple_choice", "choices": choices},
                         "explanation": "Context:\nEither the conflict work starts now, or it keeps waiting.\n\
                                         Recommendation:\nStart it now, because the head already passes its checks."},
            "answer_actions": [{"answer": choices[0], "action": {
                "kind": "undepend", "dependent": stuck.conflict_work, "prerequisite": stuck.ci_fix}}],
        }),
    );

    answer(
        &fx,
        &stuck.item,
        OperatorAnswer::MultipleChoice {
            value: choices[0].into(),
        },
    )
    .expect("the answer is applied");

    assert!(fx.db.gating_prereqs_for(&stuck.conflict_work).unwrap().is_empty());
    assert_ne!(fx.task(&stuck.conflict_work).status, TaskStatus::Blocked);
    // Only the edge went: the build-fix row is as it was.
    assert_eq!(fx.task(&stuck.ci_fix).status, TaskStatus::Todo);
    assert_eq!(attempt_row(&fx, &stuck.attempt).status, "pending");
    assert!(fx.task(&stuck.item).description.contains("no longer waits on"));
}

#[test]
fn an_option_with_no_bound_change_still_only_carries_the_decision() {
    let fx = fx();
    let stuck = stuck(&fx);
    let choices = ["Start the conflict work now", "Keep waiting for the build-fix"];
    ask(
        &fx,
        &stuck,
        json!({
            "outcome": "blocked", "summary": "Waits on a cancelled build-fix", "criterion": "2",
            "question": {"text": "Start the conflict work without waiting for the cancelled build-fix?",
                         "answer_type": {"kind": "multiple_choice", "choices": choices},
                         "explanation": "Context:\nEither it starts now, or it keeps waiting.\nRecommendation:\nStart it."},
            "answer_actions": [{"answer": choices[0], "action": {
                "kind": "undepend", "dependent": stuck.conflict_work, "prerequisite": stuck.ci_fix}}],
        }),
    );

    answer(
        &fx,
        &stuck.item,
        OperatorAnswer::MultipleChoice {
            value: choices[1].into(),
        },
    )
    .expect("the answer is recorded");

    assert_eq!(
        fx.db.gating_prereqs_for(&stuck.conflict_work).unwrap(),
        vec![stuck.ci_fix.clone()]
    );
    assert!(!fx.task(&stuck.item).description.contains("The engine made the change"));
}

/// The three questions that reached the owner. None could have changed
/// anything, and the last two told the owner to make the change.
#[test]
fn a_question_no_answer_to_which_could_change_anything_is_refused() {
    let fx = fx();
    let stuck = stuck(&fx);
    let run = fx.run(&stuck.item, FINGERPRINT, "S13_revision_chain");
    let question = |text: &str, answer_type: serde_json::Value, explanation: &str| json!({"text": text, "answer_type": answer_type, "explanation": explanation});
    let asking = |criterion: &str, question: &serde_json::Value| {
        json!({"outcome": "blocked", "summary": "Blocked on a cancelled build-fix", "criterion": criterion,
               "question": question})
    };

    // 1. Asked for an action, bound none: whatever the owner picks, nothing moves.
    let first = question(
        "Skip that build-fix so we can resolve the conflicts with main?",
        json!({"kind": "yes_no"}),
        EXPLANATION,
    );
    for (criterion, key) in [("1", "q1a"), ("2", "q1b")] {
        let reason = rejection(&fx, &run, asking(criterion, &first), key);
        assert!(reason.contains("none of its answers is carried out"), "{reason}");
        assert!(reason.contains("--on-answer"), "{reason}");
    }

    // 2. "Yes: You archive the cancelled build-fix."
    let second = question(
        "Skipping the cancelled build-fix did not unblock the conflict work. Archive it so conflict resolution \
         can start?",
        json!({"kind": "yes_no"}),
        "Context:\nThe conflict work cannot start.\nYes:\nYou archive the cancelled build-fix. The conflict work \
         then starts.\nNo:\nThe build-fix runs after all.\nRecommendation:\nArchive the cancelled build-fix.",
    );
    let reason = rejection(&fx, &run, asking("2", &second), "q2");
    assert!(reason.contains("you archive"), "{reason}");
    assert!(reason.contains("the explanation of what an answer does"), "{reason}");

    // 3. "Please delete that row on the board", with "I will delete the cancelled row now" as an option.
    let third_text = "Answering yes did not remove the cancelled build-fix. Please delete that row on the board so \
                      conflict work can start, or start that row to run the build-fix first?";
    let third_explanation = "Context:\nDeleting the cancelled row on the board lets the conflict work \
                             start.\nRecommendation:\nDelete the cancelled row on the board, because the pull \
                             request already passes its checks.";
    let third = question(
        third_text,
        json!({"kind": "multiple_choice",
               "choices": ["I will delete the cancelled row now", "Start the cancelled build-fix"]}),
        third_explanation,
    );
    // The option is refused before the question reaches the engine, for every kind of run.
    let invalid = boss_engine_proposal_validation::validate_payload(ProposalKind::RunDone, &asking("2", &third))
        .expect_err("an option that speaks as the owner is not a valid payload");
    assert!(
        invalid.iter().any(|error| error.field == "question.answer_type.choices"
            && error.message.contains("speaks as the person answering")),
        "{invalid:?}"
    );
    // With the option reworded, the question still tells the owner to do it.
    let neutral_options = json!({"kind": "multiple_choice",
                                 "choices": ["Delete the cancelled row", "Start the cancelled build-fix"]});
    let reason = rejection(
        &fx,
        &run,
        asking("2", &question(third_text, neutral_options.clone(), third_explanation)),
        "q3",
    );
    assert!(reason.contains("please delete"), "{reason}");
    // And with the question reworded too, the recommendation still does.
    let reason = rejection(
        &fx,
        &run,
        asking("2", &question("Which first?", neutral_options, third_explanation)),
        "q3b",
    );
    assert!(reason.contains("delete … on the board"), "{reason}");

    // Binding the change does not excuse the wording.
    let mut bound = asking("2", &second);
    bound["answer_actions"] =
        json!([{"answer": "yes", "action": {"kind": "archive_revision", "revision": stuck.ci_fix}}]);
    assert!(rejection(&fx, &run, bound, "q2-bound").contains("you archive"));

    assert!(
        fx.db.list_operator_questions(&stuck.item).unwrap().is_empty(),
        "a refused question never reaches the owner"
    );
    assert_eq!(fx.db.progression_disposition(&run).unwrap(), None);
}

#[test]
fn an_action_question_may_offer_the_item_dispositions_the_engine_carries_out() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(
        &fx,
        &stuck,
        json!({
            "outcome": "blocked", "summary": "Nothing left to try", "criterion": "1",
            "question": {"text": "This change has stalled. Archive it, or keep it?",
                         "answer_type": {"kind": "multiple_choice", "choices": ["Archive this item", "Keep it"]},
                         "explanation": "Context:\nNothing is working it.\nRecommendation:\nKeep it."},
        }),
    );
}

#[test]
fn a_bound_change_must_be_in_reach_possible_and_still_needed() {
    let fx = fx();
    let stuck = stuck(&fx);
    let elsewhere = fx.chore("Unrelated work");
    let closed = fx.revision_of(&stuck.item, "Already closed", "archived");
    let run = fx.run(&stuck.item, FINGERPRINT, "S13_revision_chain");
    let with = |action: serde_json::Value| {
        let mut payload = skip_or_run(&stuck);
        payload["answer_actions"] = json!([{"answer": "yes", "action": action}]);
        payload
    };

    let outside = rejection(&fx, &run, with(json!({"kind": "start", "item": elsewhere})), "k1");
    assert!(outside.contains("outside the neighbourhood"), "{outside}");

    let itself = rejection(
        &fx,
        &run,
        with(json!({"kind": "archive_revision", "revision": stuck.item})),
        "k2",
    );
    assert!(itself.contains("Archive this item"), "{itself}");

    let nothing = rejection(
        &fx,
        &run,
        with(json!({"kind": "archive_revision", "revision": closed})),
        "k3",
    );
    assert!(nothing.contains("would change nothing"), "{nothing}");

    let no_edge = rejection(
        &fx,
        &run,
        with(json!({"kind": "undepend", "dependent": stuck.ci_fix, "prerequisite": stuck.conflict_work})),
        "k4",
    );
    assert!(no_edge.contains("would change nothing"), "{no_edge}");

    // An ordinary worker has no neighbourhood to bound a change by.
    let worker = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&stuck.item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let from_worker = rejection(&fx, &worker.id, skip_or_run(&stuck), "k5");
    assert!(from_worker.contains("only a progression run"), "{from_worker}");
}

#[test]
fn a_coordinator_answer_does_not_make_the_bound_change() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(&fx, &stuck, skip_or_run(&stuck));

    let error = fx
        .db
        .answer_operator_question_from(
            &stuck.item,
            OperatorAnswer::YesNo { value: true },
            &fx.checker,
            boss_protocol::LAST_STATUS_ACTOR_BOSS,
            None,
            false,
        )
        .unwrap_err();
    let Some(OperatorQuestionError::ValidationFailed { message }) = error.downcast_ref::<OperatorQuestionError>()
    else {
        panic!("expected a typed refusal, got {error:#}");
    };
    assert!(message.contains("only the owner"), "{message}");
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Open
    );
    assert_eq!(fx.task(&stuck.ci_fix).status, TaskStatus::Todo);
    assert_eq!(attempt_row(&fx, &stuck.attempt).status, "pending");
}

#[test]
fn a_multiple_choice_handoff_in_context_is_refused_even_when_another_option_is_bound() {
    let fx = fx();
    let stuck = stuck(&fx);
    let run = fx.run(&stuck.item, FINGERPRINT, "S13_revision_chain");
    let reason = rejection(
        &fx,
        &run,
        json!({
            "outcome": "blocked", "summary": "A cancelled build-fix blocks the conflict work", "criterion": "2",
            "question": {
                "text": "Skip that build-fix so we can resolve the conflicts with main?",
                "answer_type": {"kind": "multiple_choice", "choices": ["Skip", "Keep"]},
                "explanation": "Context:\nSkip: you delete the cancelled row on the board.\nKeep: the engine \
                                archives it.\nRecommendation:\nKeep it."
            },
            "answer_actions": [{"answer": "Keep", "action": {
                "kind": "archive_revision", "revision": stuck.ci_fix}}]
        }),
        "mc-context",
    );
    assert!(reason.contains("you delete"), "{reason}");
    assert!(reason.contains("the explanation of what an answer does"), "{reason}");
}

#[test]
fn a_prerequisite_question_is_an_executable_answer_under_criterion_2() {
    let fx = fx();
    let stuck = stuck(&fx);
    ask(
        &fx,
        &stuck,
        json!({
            "outcome": "blocked", "summary": "Needs a precursor first", "criterion": "2",
            "question": {
                "text": "Create a prerequisite to land the fixture fix first?",
                "answer_type": {"kind": "create_prerequisite_task", "name": "Fix the fixture race",
                                "brief": "The fixture races retention cleanup."},
                "explanation": "Context:\nThe next run cannot start until that work lands.\nYes:\nThat work is \
                                created and this item waits on it.\nNo:\nThis item continues without it.\n\
                                Recommendation:\nI recommend Yes because the next run cannot start until it lands."
            }
        }),
    );
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Open
    );
}

#[test]
fn a_link_prerequisite_question_is_an_executable_answer_under_criterion_2() {
    let fx = fx();
    let stuck = stuck(&fx);
    fx.keys.set(fx.keys.get() + 1);
    let other = fx.pr_item("Land the wiring", "pass");
    ask(
        &fx,
        &stuck,
        json!({
            "outcome": "blocked", "summary": "Wait for the open wiring PR", "criterion": "2",
            "question": {
                "text": "Wait for the wiring that already has a pull request?",
                "answer_type": {"kind": "link_prerequisite_task", "existing_id": other,
                                "name": "Land the wiring"},
                "explanation": "Context:\nThat pull request is already open.\nYes:\nThis item waits for it.\nNo:\n\
                                This item continues without it.\nRecommendation:\nI recommend Yes because that \
                                work already exists."
            }
        }),
    );
    assert_eq!(
        fx.db.list_operator_questions(&stuck.item).unwrap()[0].status,
        OperatorQuestionStatus::Open
    );
}
