//! The owner decides; the engine acts.
//!
//! A progression run that needs the owner asks one question. This module makes
//! sure every answer to it does something, in two places:
//!
//! - **When the question is asked** ([`refuse_question_shape`], from the
//!   `run_done` applier). A question may bind a board change to an answer
//!   ([`boss_protocol::AnswerAction`]); each bound change is checked against the
//!   run's neighbourhood and against the board as it stands. A question worded
//!   so that the owner would have to make the change themselves is refused, and
//!   so is one that asks for an action (criterion 1 or 2) with no answer the
//!   engine carries out, because no answer to it could change anything.
//! - **When the owner answers** ([`apply_bound_in_tx`], from the answer
//!   transaction). The change bound to the chosen answer is made as the
//!   answerer, in the transaction that records the answer. If it cannot be
//!   made, the whole answer is refused with the reason and the question stays
//!   open. An answer is never recorded with its change left undone.
//!
//! Before this, the answer to a progression question changed nothing by
//! itself: it restored the item's stuck state and appended the answer to the
//! item's description for the next progression run, whose verbs were then
//! refused for exactly the reason the first run had asked. The owner was asked
//! again, and finally asked to make the change by hand.
//!
//! The changes are made with the owner's authority, not the agent's. The
//! agent's `archive-revision` needs proof that a revision is dead (its parent
//! PR green, or the revision already delivered); the owner's answer is the
//! decision that it is not wanted, so no such proof is asked for. What still
//! refuses a change is a fact, not a permission: a worker is on the revision,
//! or the engine's own start path declines.

use super::*;
use crate::moot_ci_fix::retire_attempts_of_closed_revision_in_tx;
use crate::progression_scope::check_progression_scope_on;
use crate::progression_verbs::resolve_target;
use crate::task_status_audit::abandon_cause;
use boss_protocol::{
    AnswerAction, OperatorAnswer, OperatorAnswerType, OperatorQuestion, OperatorQuestionError, OperatorQuestionRecord,
    OwnerAction, ProgressionQuestionCriterion, RunDoneProposalPayload,
};

/// `tasks.archived_by` for a revision the owner's answer closed.
pub(crate) const ARCHIVE_MECHANISM_OWNER_ANSWER: &str = "operator_answer_archive_revision";

/// `ci_remediations.failure_reason` for an attempt whose revision the owner's
/// answer closed.
const ATTEMPT_RETIRED_BY_OWNER_ANSWER: &str = "revision_archived_by_owner";

/// How a question must be put instead, appended to every refusal here.
const HOW_TO_ASK: &str = "The owner decides and the engine acts: word each answer as what happens when it is picked, \
     and bind a board change to an answer with `--on-answer '<answer>=<change>'`, where the change is \
     `archive-revision <id>`, `start <id>` or `undepend <dependent-id> <prerequisite-id>`. The options `Park this \
     item` and `Archive this item` are also carried out by the engine.";

/// A bound change resolved against the board: what making it would do.
enum Plan {
    ArchiveRevision(Task),
    Start(Task),
    Undepend {
        dependent: String,
        prerequisite: String,
    },
    /// The board is already as the change would leave it.
    AlreadyDone(String),
}

/// An execution a worker is on, or is being started on, for `work_item_id`.
fn worker_on(conn: &Connection, work_item_id: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, status FROM work_executions
             WHERE work_item_id = ?1 AND status IN ('claimed','running','waiting_human')
             ORDER BY created_at DESC, id DESC LIMIT 1",
            [work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Resolve `action` against the board. `item` is the item the question is
/// about. The inner `Err` says, in words for whoever reads the refusal, why the
/// change cannot be made.
fn plan(conn: &Connection, item: &Task, action: &OwnerAction) -> Result<std::result::Result<Plan, String>> {
    let open_task = |id: &str| -> Result<Option<Task>> { Ok(query_task(conn, id)?.filter(|t| t.deleted_at.is_none())) };
    Ok(match action {
        OwnerAction::ArchiveRevision { revision } => {
            let id = resolve_target(conn, revision)?;
            if id == item.id {
                return Ok(Err(format!(
                    "{revision} is the item itself; offer the option `Archive this item` instead"
                )));
            }
            let Some(task) = query_task(conn, &id)? else {
                return Ok(Err(format!("{revision} is not a known item")));
            };
            if task.kind != TaskKind::Revision || chain_root(conn, &task.id)? != chain_root(conn, &item.id)? {
                return Ok(Err(format!("{revision} is not a revision of this item's pull request")));
            }
            if task.deleted_at.is_some() || task.status.is_terminal() {
                return Ok(Ok(Plan::AlreadyDone(format!("{revision} is already closed"))));
            }
            if let Some((execution, status)) = worker_on(conn, &task.id)? {
                return Ok(Err(format!(
                    "a worker is on {revision} ({execution}, `{status}`); it has to finish or be stopped before \
                     the revision can be closed"
                )));
            }
            Ok(Plan::ArchiveRevision(task))
        }
        OwnerAction::Start { item: target } => {
            let id = resolve_target(conn, target)?;
            let Some(task) = open_task(&id)? else {
                return Ok(Err(format!("{target} is not an open item")));
            };
            if task.status.is_terminal() {
                return Ok(Err(format!("{target} is already `{}`", task.status)));
            }
            let starting: Option<String> = conn
                .query_row(
                    "SELECT status FROM work_executions
                     WHERE work_item_id = ?1 AND kind <> 'progression'
                       AND status IN ('queued','ready','claimed','running','waiting_human')
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    [&task.id],
                    |row| row.get(0),
                )
                .optional()?;
            match starting {
                Some(status) => Ok(Plan::AlreadyDone(format!("{target} already has a `{status}` run"))),
                None => Ok(Plan::Start(task)),
            }
        }
        OwnerAction::Undepend {
            dependent,
            prerequisite,
        } => {
            let dependent_id = resolve_target(conn, dependent)?;
            let prerequisite_id = resolve_target(conn, prerequisite)?;
            if open_task(&dependent_id)?.is_none() {
                return Ok(Err(format!("{dependent} is not an open item")));
            }
            if deps::query_edge(conn, &dependent_id, &prerequisite_id, RELATION_BLOCKS)?.is_none() {
                return Ok(Ok(Plan::AlreadyDone(format!(
                    "{dependent} does not depend on {prerequisite}"
                ))));
            }
            Ok(Plan::Undepend {
                dependent: dependent_id,
                prerequisite: prerequisite_id,
            })
        }
    })
}

/// Why the wording of `question` hands a board change to the owner, or `None`.
/// Checked: the question, each option label, and the parts of the explanation
/// that say what an answer does. `Context:` and `Details:` are free to describe
/// the board.
fn handoff_wording(question: &OperatorQuestion) -> Option<String> {
    let refusal = |place: &str, phrase: String| {
        format!("{place} asks the owner to make a board change themselves (\"{phrase}\"). {HOW_TO_ASK}")
    };
    if let Some(phrase) = boss_protocol::owner_handoff_wording(&question.text) {
        return Some(refusal("the question", phrase));
    }
    if let OperatorAnswerType::MultipleChoice { choices } = &question.answer_type {
        for choice in choices {
            let phrase =
                boss_protocol::owner_voiced_label(choice).or_else(|| boss_protocol::owner_handoff_wording(choice));
            if let Some(phrase) = phrase {
                return Some(refusal(&format!("the option `{choice}`"), phrase));
            }
        }
    }
    boss_protocol::owner_handoff_wording(&boss_protocol::explanation_decision_text(&question.explanation))
        .map(|phrase| refusal("the explanation of what an answer does", phrase))
}

/// Whether the question offers an option the engine carries out without a
/// bound change: the item-local dispositions.
fn offers_item_disposition(question: &OperatorQuestion) -> bool {
    matches!(&question.answer_type, OperatorAnswerType::MultipleChoice { choices }
        if choices.iter().any(|choice| crate::operator_disposition::known_choice(choice).is_some()))
}

/// Why the question a `run_done` declaration carries may not be asked as it
/// stands, in words for the run that asked; `None` when it may. Runs before
/// anything is stamped, so a refusal leaves the run free to ask properly.
pub(crate) fn refuse_question_shape(
    conn: &Connection,
    execution: &WorkExecution,
    payload: &RunDoneProposalPayload,
    payload_json: &str,
) -> Result<Option<String>> {
    let actions = match boss_protocol::run_done_answer_actions(payload_json) {
        Ok(actions) => actions,
        Err(message) => return Ok(Some(message)),
    };
    let Some(question) = &payload.question else {
        return Ok(None);
    };
    if execution.kind != ExecutionKind::Progression {
        // Only a progression run has a neighbourhood to bound a change by.
        return Ok((!actions.is_empty()).then(|| {
            "only a progression run can bind a board change to an answer; ask the decision and let the next run \
             of this task act on it"
                .to_owned()
        }));
    }
    if let Some(reason) = handoff_wording(question) {
        return Ok(Some(reason));
    }
    let Some(item) = query_task(conn, &execution.work_item_id)? else {
        return Ok(None);
    };
    for bound in &actions {
        let spelling = bound.action.spelling();
        for target in bound.action.targets() {
            let id = resolve_target(conn, target)?;
            if check_progression_scope_on(conn, &execution.id, &id).is_err() {
                return Ok(Some(format!(
                    "`{spelling}` is bound to `{}`, but {target} is outside the neighbourhood this run was spawned \
                     for (the item, its revisions, and its direct prerequisites and dependents)",
                    bound.answer
                )));
            }
        }
        match plan(conn, &item, &bound.action)? {
            Ok(Plan::AlreadyDone(why)) => {
                return Ok(Some(format!(
                    "`{spelling}` is bound to `{}`, but it would change nothing: {why}. Bind a change that is \
                     still needed",
                    bound.answer
                )));
            }
            Ok(_) => {}
            Err(why) => {
                return Ok(Some(format!(
                    "`{spelling}` is bound to `{}`, but it cannot be made: {why}",
                    bound.answer
                )));
            }
        }
    }
    let asks_for_an_action = matches!(
        boss_protocol::run_done_question_criterion(payload_json),
        Some(ProgressionQuestionCriterion::HumanOnlyAction | ProgressionQuestionCriterion::VerbsExhausted)
    );
    if asks_for_an_action && actions.is_empty() && !offers_item_disposition(question) {
        return Ok(Some(format!(
            "this question cites criterion 1 or 2, so it asks for an action, but none of its answers is carried \
             out by the engine: whatever the owner picks, nothing on the board changes and the next run is \
             refused the same way. {HOW_TO_ASK} If nothing the engine can do would move the item, do not ask: \
             end the run, and the engine asks the owner whether to park, archive or keep trying once the \
             attempts run out"
        )));
    }
    Ok(None)
}

fn refused(action: &OwnerAction, why: &str) -> anyhow::Error {
    OperatorQuestionError::ValidationFailed {
        message: format!(
            "the engine could not make the change this answer carries (`{}`): {why}. Nothing was changed and \
             the question is still open",
            action.spelling()
        ),
    }
    .into()
}

/// A refusal from an engine path the change routes through becomes the reason
/// the answer is refused; a database fault stays an engine error.
fn or_refusal<T>(action: &OwnerAction, result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
            error
        } else {
            refused(action, &format!("{error:#}"))
        }
    })
}

fn name_of(task: &Task) -> String {
    format!("`{}` ({})", task.id, task.name)
}

/// Make the change bound to `answer`, if the question binds one to it, as
/// `actor`. Returns the other items the change touched, for the board; empty
/// when the answer carries no change. An `Err` carrying an
/// [`OperatorQuestionError`] means the change could not be made; the caller's
/// transaction is then dropped, so the answer is not recorded either.
///
/// Must run after the answer has restored the item's own state, in the same
/// transaction, so a change that starts the item itself sees it restored. The
/// caller's board publication reconciles the product and wakes the scheduler,
/// which is what dispatches work the change released or started.
pub(crate) fn apply_bound_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    task: &Task,
    question: &OperatorQuestionRecord,
    answer: &OperatorAnswer,
    actor: &str,
) -> Result<Vec<String>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT p.payload_json FROM operator_questions q
             JOIN worker_proposals p ON p.id = q.proposal_id
             WHERE q.id = ?1 AND p.kind = 'run_done'",
            [&question.question.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };
    let actions: Vec<AnswerAction> = boss_protocol::run_done_answer_actions(&payload).map_err(|message| {
        anyhow::Error::from(OperatorQuestionError::ValidationFailed {
            message: format!("the change this answer carries cannot be read ({message}); nothing was changed"),
        })
    })?;
    let Some(bound) = actions.iter().find(|bound| bound.applies_to(answer)) else {
        return Ok(Vec::new());
    };
    let action = &bound.action;
    // The scope was checked when the question was asked; the stored snapshot
    // is checked again so a change can never reach outside it.
    for target in action.targets() {
        let id = resolve_target(conn, target)?;
        if check_progression_scope_on(conn, &question.question.execution_id, &id).is_err() {
            return Err(refused(
                action,
                &format!("{target} is outside what the run that asked may touch"),
            ));
        }
    }
    let now = now_string();
    let mut touched = Vec::new();
    let made = match plan(conn, task, action)?.map_err(|why| refused(action, &why))? {
        Plan::AlreadyDone(why) => format!("nothing was left to change: {why}"),
        Plan::ArchiveRevision(revision) => {
            let waiting: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT dependent_id FROM work_item_dependencies WHERE prerequisite_id = ?1 ORDER BY dependent_id",
                )?;
                collect_rows(stmt.query_map([&revision.id], |row| row.get(0))?)?
            };
            crate::chain_helpers::abandon_pending_executions(
                pending,
                conn,
                &revision.id,
                &now,
                abandon_cause::OWNER_ANSWER_ARCHIVED_REVISION.by(actor),
            )?;
            let reason = format!(
                "the owner's answer to a question about {} chose to close this revision (question {})",
                task.id, question.question.id
            );
            let archived = crate::chain_helpers::archive_revision_task_as_actor(
                conn,
                &revision.id,
                &now,
                ArchiveProvenance::new(ARCHIVE_MECHANISM_OWNER_ANSWER, &reason),
                actor,
            )?;
            anyhow::ensure!(archived == 1, "revision {} changed underneath the answer", revision.id);
            // Whatever was queued behind the revision is released.
            cascade_dependents_after_prereq_status_change(pending, conn, &revision.id, "archived", &now)?;
            // The attempt that spawned it ends with it, or it would keep
            // reporting the pull request as failing with nothing left to fix it.
            let retired =
                retire_attempts_of_closed_revision_in_tx(conn, pending, &revision.id, ATTEMPT_RETIRED_BY_OWNER_ANSWER)?;
            stage_task_board_change(pending, conn, &revision.id)?;
            let mut made = format!("closed revision {}", name_of(&revision));
            if !waiting.is_empty() {
                made.push_str(&format!("; {} no longer waits on it", waiting.join(", ")));
            }
            if !retired.is_empty() {
                made.push_str("; the CI attempt that created it was retired");
            }
            touched.push(revision.id);
            touched.extend(waiting);
            made
        }
        Plan::Start(target) => {
            or_refusal(
                action,
                crate::dispatch_helpers::request_execution_in_tx_as_actor(
                    pending,
                    conn,
                    RequestExecutionInput::builder()
                        .work_item_id(target.id.clone())
                        .explicit_start(true)
                        .build(),
                    |_| true,
                    actor,
                ),
            )?;
            stage_task_board_change(pending, conn, &target.id)?;
            let made = format!("started {}", name_of(&target));
            touched.push(target.id);
            made
        }
        Plan::Undepend {
            dependent,
            prerequisite,
        } => {
            deps::delete_edge(conn, &dependent, &prerequisite, RELATION_BLOCKS)?;
            crate::dep_helpers::maybe_engine_unblock_dependent_as_actor(pending, conn, &dependent, &now, actor)?;
            stage_task_board_change(pending, conn, &dependent)?;
            let made = format!("`{dependent}` no longer waits on `{prerequisite}`");
            touched.push(dependent);
            made
        }
    };
    // The next progression run reads the item's description: tell it the
    // change is made, so it neither repeats it nor asks for it again.
    conn.execute(
        "UPDATE tasks SET description = description || ?2, updated_at = ?3 WHERE id = ?1 AND deleted_at IS NULL",
        params![
            task.id,
            format!(
                "\nThe engine made the change bound to this answer (`{}`): {made}. It is done; do not repeat it \
                 or ask for it again.\n",
                action.spelling()
            ),
            now
        ],
    )?;
    stage_task_board_change(pending, conn, &task.id)?;
    tracing::info!(
        work_item_id = %task.id,
        question_id = %question.question.id,
        change = %action.spelling(),
        actor,
        made = %made,
        "operator answer: made the board change bound to the chosen answer",
    );
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use boss_protocol::{
        AnswerOptionLabels, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE, CONFLICT_MISMATCH_CHOICE_REWRITE,
        FileCountBypassSpec, OperatorAnswerType, owner_handoff_wording, owner_voiced_label,
    };

    use crate::operator_disposition::known_choice;
    use crate::progression_ladder::{ARCHIVE_IT, KEEP_TRYING, PARK_IT, QUESTION_TEXT};

    /// Every option label the engine itself defines, by the question it
    /// belongs to. A new engine-defined question adds its labels here.
    fn engine_defined_option_labels() -> Vec<(&'static str, Vec<String>)> {
        let labels = AnswerOptionLabels::split_or_bypass();
        let dispositions = ["item", "chore", "task", "revision", "followup"]
            .iter()
            .flat_map(|noun| [format!("Archive this {noun}"), format!("Park this {noun}")])
            .collect();
        vec![
            (
                "out of attempts",
                vec![PARK_IT.into(), ARCHIVE_IT.into(), KEEP_TRYING.into()],
            ),
            (
                "architectural mismatch",
                vec![
                    CONFLICT_MISMATCH_CHOICE_REWRITE.into(),
                    CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.into(),
                ],
            ),
            ("split or bypass", vec![labels.yes, labels.no]),
            ("item dispositions", dispositions),
        ]
    }

    /// Every question text the engine words itself.
    fn engine_worded_questions() -> Vec<String> {
        vec![
            QUESTION_TEXT.to_owned(),
            OperatorAnswerType::prerequisite_question_text("Fix the fixture race"),
            OperatorAnswerType::link_prerequisite_unresolved_question_text(),
            OperatorAnswerType::link_prerequisite_question_text("Land the wiring", Some("acme/widget#9")),
            OperatorAnswerType::file_count_split_question_text(
                "Storage and migration",
                FileCountBypassSpec { files: 41, limit: 30 },
            ),
        ]
    }

    /// The guard on the option sets the engine defines: none may be worded so
    /// that the owner, not the engine, makes the change. An option such as "I
    /// will delete the cancelled row now", or a question such as "Please
    /// delete that row on the board", fails here.
    #[test]
    fn no_engine_defined_option_or_question_hands_a_board_change_to_the_owner() {
        for (question, labels) in engine_defined_option_labels() {
            assert!(!labels.is_empty(), "{question}");
            for label in labels {
                assert_eq!(owner_voiced_label(&label), None, "{question}: `{label}`");
                assert_eq!(owner_handoff_wording(&label), None, "{question}: `{label}`");
                assert_eq!(
                    OperatorAnswerType::validate_choices(&[label.clone(), "Other".into()]),
                    Ok(()),
                    "{question}: `{label}` must be a valid button label"
                );
            }
        }
        for text in engine_worded_questions() {
            assert_eq!(owner_handoff_wording(&text), None, "{text}");
        }
    }

    /// The guard has teeth: the shapes it exists to keep out are named.
    #[test]
    fn the_guard_names_an_option_that_leaves_the_change_to_the_owner() {
        assert!(owner_voiced_label("I will delete the cancelled row now").is_some());
        assert!(owner_handoff_wording("Please delete that row on the board so conflict work can start").is_some());
        assert!(owner_handoff_wording("You archive the cancelled build-fix.").is_some());
    }

    /// Every item-disposition label the guard lists is one the engine acts on.
    #[test]
    fn every_listed_item_disposition_is_carried_out_by_the_engine() {
        let (_, dispositions) = engine_defined_option_labels()
            .into_iter()
            .find(|(question, _)| *question == "item dispositions")
            .unwrap();
        for label in dispositions {
            assert!(known_choice(&label).is_some(), "`{label}` has no engine handler");
        }
    }
}
