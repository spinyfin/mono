//! Creation-time validation and the upgrade repair of stored decision cards.

use super::*;
use boss_protocol::{
    ChoiceDisposition, OperatorAnswer, OperatorAnswerType, OperatorChoiceOption, OperatorQuestion,
    OperatorQuestionStatus,
};

fn open() -> WorkDb {
    WorkDb::open(temp_db_path("operator-disposition")).unwrap()
}

fn card(options: Vec<OperatorChoiceOption>) -> OperatorQuestion {
    OperatorQuestion {
        text: "What now?".into(),
        explanation: "Pick one.".into(),
        answer_type: OperatorAnswerType::multiple_choice(options),
    }
}

/// A chore waiting on a stored card that is rewritten into the shape the
/// engine stored before dispositions existed: `choices` only, no `options`.
fn chore_with_pre_upgrade_card(db: &WorkDb, label: &str, choices: &[&str]) -> (String, String) {
    let (_product, chore, execution) = make_waiting_human_chore(db, label);
    let question = card(
        choices
            .iter()
            .map(|choice| OperatorChoiceOption::resume(*choice))
            .collect(),
    );
    crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "blocked", &question)
        .unwrap();
    let legacy = serde_json::json!({
        "text": "What now?",
        "explanation": "Pick one.",
        "answer_type": {"kind": "multiple_choice", "choices": choices},
    });
    db.connect()
        .unwrap()
        .execute(
            "UPDATE operator_questions SET question_json = ?2 WHERE work_item_id = ?1",
            params![chore, legacy.to_string()],
        )
        .unwrap();
    let id = db.list_operator_questions(&chore).unwrap()[0].question.id.clone();
    (chore, id)
}

fn task_row(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task or chore, got {other:?}"),
    }
}

#[test]
fn a_question_from_a_project_level_execution_is_still_accepted() {
    for answer_type in [
        OperatorAnswerType::YesNo,
        OperatorAnswerType::multiple_choice(vec![
            OperatorChoiceOption::resume("Gradual"),
            OperatorChoiceOption::resume("All at once"),
        ]),
    ] {
        let question = OperatorQuestion {
            text: "Which way?".into(),
            explanation: "Both fit.".into(),
            answer_type,
        };
        crate::operator_disposition::validate_question_for_creation(None, &question).unwrap();
    }
}

#[test]
fn archive_and_park_options_need_a_task_to_act_on() {
    let question = card(vec![
        OperatorChoiceOption::park("Later"),
        OperatorChoiceOption::resume("Now"),
    ]);
    assert!(crate::operator_disposition::validate_question_for_creation(None, &question).is_err());
}

#[test]
fn engine_reserved_dispositions_are_refused_on_worker_cards() {
    let options = vec![
        OperatorChoiceOption::new("Go", ChoiceDisposition::ForceMerge),
        OperatorChoiceOption::resume("Stay"),
    ];
    assert!(OperatorAnswerType::validate_worker_options(&options).is_err());
    assert!(OperatorAnswerType::validate_options(&options).is_ok());
    assert!("force_merge".parse::<ChoiceDisposition>().is_err());
}

#[test]
fn migration_gives_an_open_pre_upgrade_archive_card_an_archive_option_that_archives() {
    let db = open();
    let (chore, question_id) = chore_with_pre_upgrade_card(&db, "archive", &["Archive this item", "Keep going"]);

    crate::operator_disposition::migrate(&db.connect().unwrap()).unwrap();

    let stored = &db.list_operator_questions(&chore).unwrap()[0];
    assert_eq!(
        stored.question.answer_type.choice_disposition("Archive this item"),
        Some(ChoiceDisposition::Archive)
    );
    assert_eq!(
        stored.question.answer_type.choice_disposition("Keep going"),
        Some(ChoiceDisposition::Resume)
    );
    db.answer_operator_question(
        &question_id,
        OperatorAnswer::MultipleChoice {
            value: "Archive this item".into(),
        },
    )
    .unwrap();
    assert_eq!(task_row(&db, &chore).status, TaskStatus::Archived);
}

#[test]
fn migration_gives_an_open_pre_upgrade_park_card_a_park_option_that_parks() {
    let db = open();
    let (chore, question_id) = chore_with_pre_upgrade_card(&db, "park", &["Park this item", "Keep going"]);

    crate::operator_disposition::migrate(&db.connect().unwrap()).unwrap();

    db.answer_operator_question(
        &question_id,
        OperatorAnswer::MultipleChoice {
            value: "Park this item".into(),
        },
    )
    .unwrap();
    assert!(db.is_parked(&chore).unwrap());
}

#[test]
fn migration_leaves_other_labels_resuming_and_is_idempotent() {
    let db = open();
    let (chore, _) = chore_with_pre_upgrade_card(&db, "other", &["Archive the old logs", "Park"]);
    for _ in 0..2 {
        crate::operator_disposition::migrate(&db.connect().unwrap()).unwrap();
    }
    let stored = &db.list_operator_questions(&chore).unwrap()[0];
    assert_eq!(stored.status, OperatorQuestionStatus::Open);
    for label in ["Archive the old logs", "Park"] {
        assert_eq!(
            stored.question.answer_type.choice_disposition(label),
            Some(ChoiceDisposition::Resume),
            "{label}"
        );
    }
}

#[test]
fn migration_keeps_structured_options_it_did_not_write() {
    let db = open();
    let (_product, chore, execution) = make_waiting_human_chore(&db, "structured");
    let question = card(vec![
        OperatorChoiceOption::park("Wait a week"),
        OperatorChoiceOption::resume("Go"),
    ]);
    crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &question).unwrap();
    crate::operator_disposition::migrate(&db.connect().unwrap()).unwrap();
    let stored = &db.list_operator_questions(&chore).unwrap()[0];
    assert_eq!(stored.question.answer_type, question.answer_type);
}
