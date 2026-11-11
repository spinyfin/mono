//! Named options for a two-path operator question.
//!
//! Most questions are genuinely Yes/No. A prerequisite question whose two
//! answers are distinct *actions* (split the change vs. bypass the file-count
//! limit) is still stored and consumed as a [`OperatorAnswer::YesNo`] — the
//! engine's consumers only care which path was taken — but it carries
//! [`AnswerOptionLabels`] so every surface (kanban card, `boss task show`,
//! `boss task answer --choice`, the worker's explanation headings) names the
//! paths instead of saying Yes and No. The engine owns the labels; clients
//! render whatever it sends.

use super::proposal::{OperatorAnswer, OperatorAnswerType};
use serde::{Deserialize, Serialize};

/// What the two answers of a two-path question are called.
///
/// `yes` names the affirmative path (for a prerequisite question: create or
/// link the prerequisite and wait); `no` names the other path. Both are button
/// labels and explanation section headings, so they follow the same rules as a
/// multiple-choice option ([`OperatorAnswerType::validate_choices`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerOptionLabels {
    pub yes: String,
    pub no: String,
}

impl AnswerOptionLabels {
    /// The labels of a split-vs-file-count-bypass question.
    pub fn split_or_bypass() -> Self {
        Self {
            yes: "Split".into(),
            no: "Bypass limit".into(),
        }
    }

    /// Both labels must be valid, distinct option labels.
    pub fn validate(&self) -> Result<(), String> {
        OperatorAnswerType::validate_choices(&[self.yes.clone(), self.no.clone()])
    }

    /// The explanation section headings, in order: `Split:` then `Bypass limit:`.
    pub fn headings(&self) -> (String, String) {
        (format!("{}:", self.yes), format!("{}:", self.no))
    }
}

impl OperatorAnswerType {
    /// Names for the two answers, when this question has them. `None` means a
    /// genuine Yes/No question (and every question stored before labels
    /// existed), which keeps its Yes / No buttons.
    pub fn option_labels(&self) -> Option<&AnswerOptionLabels> {
        match self {
            Self::CreatePrerequisiteTask { option_labels, .. } => option_labels.as_ref(),
            Self::YesNo | Self::MultipleChoice { .. } => None,
        }
    }

    /// Fold an answer given by option name into the stored Yes/No form.
    ///
    /// A labelled question is answered by naming an option (`boss task answer
    /// --choice "Bypass limit"`) or, as before, with Yes/No; both record the
    /// same [`OperatorAnswer::YesNo`], so the stored answer is idempotent under
    /// either spelling and downstream consumers never see a label. Any other
    /// answer is returned unchanged for `check_answer` to judge.
    pub fn canonical_answer(&self, answer: OperatorAnswer) -> OperatorAnswer {
        match (self.option_labels(), answer) {
            (Some(labels), OperatorAnswer::MultipleChoice { value }) if value == labels.yes => {
                OperatorAnswer::YesNo { value: true }
            }
            (Some(labels), OperatorAnswer::MultipleChoice { value }) if value == labels.no => {
                OperatorAnswer::YesNo { value: false }
            }
            (_, answer) => answer,
        }
    }

    /// How to show a recorded answer: the option's name when the question has
    /// one (`Split (yes)`), otherwise `Yes` / `No` or the chosen option.
    pub fn describe_answer(&self, answer: &OperatorAnswer) -> String {
        match (self.option_labels(), answer) {
            (Some(labels), OperatorAnswer::YesNo { value: true }) => format!("{} (yes)", labels.yes),
            (Some(labels), OperatorAnswer::YesNo { value: false }) => format!("{} (no)", labels.no),
            (None, OperatorAnswer::YesNo { value }) => (if *value { "Yes" } else { "No" }).to_owned(),
            (_, OperatorAnswer::MultipleChoice { value }) => value.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileCountBypassSpec;

    fn split_question() -> OperatorAnswerType {
        OperatorAnswerType::CreatePrerequisiteTask {
            name: "Extract the first PR".into(),
            brief: "Land the first capability.".into(),
            file_count_bypass: Some(FileCountBypassSpec { files: 48, limit: 30 }),
            option_labels: Some(AnswerOptionLabels::split_or_bypass()),
        }
    }

    fn choose(value: &str) -> OperatorAnswer {
        OperatorAnswer::MultipleChoice { value: value.into() }
    }

    #[test]
    fn labels_round_trip_and_are_absent_on_questions_stored_before_they_existed() {
        let json = serde_json::to_value(split_question()).unwrap();
        assert_eq!(
            json["option_labels"],
            serde_json::json!({"yes": "Split", "no": "Bypass limit"})
        );
        assert_eq!(
            serde_json::from_value::<OperatorAnswerType>(json).unwrap(),
            split_question()
        );

        // A question stored before labels existed decodes with none and keeps Yes/No.
        let old: OperatorAnswerType = serde_json::from_value(serde_json::json!({
            "kind": "create_prerequisite_task",
            "name": "Extract the first PR",
            "brief": "Land the first capability.",
            "file_count_bypass": {"files": 48, "limit": 30}
        }))
        .unwrap();
        assert_eq!(old.option_labels(), None);
        assert_eq!(old.describe_answer(&OperatorAnswer::YesNo { value: false }), "No");
        assert!(serde_json::to_value(&old).unwrap().get("option_labels").is_none());
    }

    #[test]
    fn naming_an_option_records_the_same_answer_as_yes_or_no() {
        let question = split_question();
        assert_eq!(
            question.canonical_answer(choose("Split")),
            OperatorAnswer::YesNo { value: true }
        );
        assert_eq!(
            question.canonical_answer(choose("Bypass limit")),
            OperatorAnswer::YesNo { value: false }
        );
        // Yes/No stay valid on a labelled question: older clients send them.
        let no = OperatorAnswer::YesNo { value: false };
        assert_eq!(question.canonical_answer(no.clone()), no);
        assert!(question.check_answer(&no).is_ok());
        // Names are matched exactly; anything else is refused with the options listed.
        let unknown = question.canonical_answer(choose("bypass limit"));
        assert_eq!(unknown, choose("bypass limit"));
        let error = question.check_answer(&unknown).unwrap_err();
        assert!(error.contains("Split") && error.contains("Bypass limit"), "{error}");
        // A question without labels never reinterprets a named option.
        let plain = OperatorAnswerType::YesNo;
        assert_eq!(plain.canonical_answer(choose("Split")), choose("Split"));
        assert!(plain.check_answer(&choose("Split")).is_err());
    }

    #[test]
    fn recorded_answers_are_described_by_option_name() {
        let question = split_question();
        assert_eq!(
            question.describe_answer(&OperatorAnswer::YesNo { value: true }),
            "Split (yes)"
        );
        assert_eq!(
            question.describe_answer(&OperatorAnswer::YesNo { value: false }),
            "Bypass limit (no)"
        );
        assert_eq!(
            OperatorAnswerType::YesNo.describe_answer(&OperatorAnswer::YesNo { value: true }),
            "Yes"
        );
    }

    #[test]
    fn labels_must_be_distinct_single_line_options() {
        assert!(AnswerOptionLabels::split_or_bypass().validate().is_ok());
        for (yes, no) in [
            ("Split", "Split"),
            ("", "Bypass limit"),
            ("Split\nnow", "Bypass limit"),
            (" Split", "Bypass"),
        ] {
            let labels = AnswerOptionLabels {
                yes: yes.into(),
                no: no.into(),
            };
            assert!(labels.validate().is_err(), "{yes:?}/{no:?}");
        }
        assert_eq!(
            AnswerOptionLabels::split_or_bypass().headings(),
            ("Split:".to_owned(), "Bypass limit:".to_owned())
        );
    }
}
