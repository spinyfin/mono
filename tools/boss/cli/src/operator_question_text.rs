//! Human-readable rendering of an operator question for `boss task show`.

use boss_protocol::{OperatorAnswerType, OperatorQuestionRecord};

/// The lines `boss task show` prints for one question, answer or withdrawal.
///
/// A question whose two answers are named actions lists those names as
/// options (answerable with `boss task answer --choice "<name>"`, or with
/// `--yes` / `--no` as before); a genuine Yes/No question lists none.
pub(crate) fn operator_question_lines(record: &OperatorQuestionRecord) -> Vec<String> {
    let q = &record.question;
    let mut lines = vec![format!(
        "\nOperator question {} ({:?}): {}",
        q.id, record.status, q.text
    )];
    let answer_type = match &q.answer_type {
        OperatorAnswerType::YesNo => "yes_no",
        OperatorAnswerType::CreatePrerequisiteTask { .. } => "create_prerequisite_task",
        OperatorAnswerType::MultipleChoice { .. } => "multiple_choice",
    };
    lines.push(format!(
        "  Answer type: {answer_type}; asked: {}; execution: {}",
        q.asked_at, q.execution_id
    ));
    lines.push(format!("  Explanation: {}", q.explanation));
    if let OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } = &q.answer_type {
        lines.push(format!("  Proposed prerequisite task: {name}"));
        lines.push("  Proposed brief:".into());
        lines.extend(brief.lines().map(|line| format!("    {line}")));
        if let Some(spec) = q.answer_type.file_count_bypass() {
            let declining = q.answer_type.option_labels().map_or("No", |labels| labels.no.as_str());
            lines.push(format!(
                "  File-count split: {} files, limit {}. {declining} authorizes a file-count exception for this PR only.",
                spec.files, spec.limit
            ));
        }
    }
    if let Some(labels) = q.answer_type.option_labels() {
        lines.push("  Options:".into());
        lines.push(format!("    {} (--choice \"{}\" or --yes)", labels.yes, labels.yes));
        lines.push(format!("    {} (--choice \"{}\" or --no)", labels.no, labels.no));
    }
    if let OperatorAnswerType::MultipleChoice { choices } = &q.answer_type {
        lines.push("  Options:".into());
        lines.extend(choices.iter().map(|choice| format!("    {choice}")));
    }
    if let Some(answer) = &record.answer {
        lines.push(format!(
            "  Answer: {}; by: {}; at: {}",
            q.answer_type.describe_answer(answer),
            record.answered_by.as_deref().unwrap_or(""),
            record.answered_at.as_deref().unwrap_or("")
        ));
    }
    if let Some(reason) = &record.withdrawn_reason {
        lines.push(format!("  Withdrawn: {reason}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{
        AnswerOptionLabels, FileCountBypassSpec, OperatorAnswer, OperatorQuestionStatus, OperatorQuestionView,
    };

    fn record(answer_type: OperatorAnswerType, answer: Option<OperatorAnswer>) -> OperatorQuestionRecord {
        OperatorQuestionRecord::builder()
            .question(
                OperatorQuestionView::builder()
                    .id("oq_example")
                    .text("Split this 48-file change?")
                    .answer_type(answer_type)
                    .explanation("Context:\nToo big.")
                    .asked_at("1700000000")
                    .execution_id("exec_example")
                    .build(),
            )
            .work_item_id("task_example")
            .status(if answer.is_some() {
                OperatorQuestionStatus::Answered
            } else {
                OperatorQuestionStatus::Open
            })
            .maybe_answer(answer)
            .maybe_answered_by(Some("human".to_owned()))
            .maybe_answered_at(Some("1700000100".to_owned()))
            .build()
    }

    fn split(labels: Option<AnswerOptionLabels>) -> OperatorAnswerType {
        OperatorAnswerType::CreatePrerequisiteTask {
            name: "Extract the first PR".into(),
            brief: "Land the first capability.".into(),
            file_count_bypass: Some(FileCountBypassSpec { files: 48, limit: 30 }),
            option_labels: labels,
        }
    }

    #[test]
    fn a_split_question_lists_its_options_and_the_answer_by_name() {
        let open =
            operator_question_lines(&record(split(Some(AnswerOptionLabels::split_or_bypass())), None)).join("\n");
        assert!(open.contains("Options:\n    Split (--choice \"Split\" or --yes)\n    Bypass limit (--choice \"Bypass limit\" or --no)"), "{open}");
        assert!(
            open.contains("Bypass limit authorizes a file-count exception for this PR only."),
            "{open}"
        );
        let answered = operator_question_lines(&record(
            split(Some(AnswerOptionLabels::split_or_bypass())),
            Some(OperatorAnswer::YesNo { value: false }),
        ))
        .join("\n");
        assert!(answered.contains("Answer: Bypass limit (no); by: human"), "{answered}");
    }

    #[test]
    fn a_question_without_option_names_is_listed_as_yes_no() {
        let old =
            operator_question_lines(&record(split(None), Some(OperatorAnswer::YesNo { value: false }))).join("\n");
        assert!(!old.contains("Options:"), "{old}");
        assert!(old.contains("No authorizes a file-count exception"), "{old}");
        assert!(old.contains("Answer: No; by: human"), "{old}");
        let yes_no = operator_question_lines(&record(OperatorAnswerType::YesNo, None)).join("\n");
        assert!(!yes_no.contains("Options:"), "{yes_no}");
        let choice = operator_question_lines(&record(
            OperatorAnswerType::MultipleChoice {
                choices: vec!["a".into(), "b".into()],
            },
            Some(OperatorAnswer::MultipleChoice { value: "b".into() }),
        ))
        .join("\n");
        assert!(choice.contains("Options:\n    a\n    b"), "{choice}");
        assert!(choice.contains("Answer: b;"), "{choice}");
    }
}
