//! Board changes the engine makes itself when the owner answers a question.
//!
//! A question to the owner is a decision, never a to-do. When an answer means
//! the board should change (close a dead revision, start a row, drop a
//! dependency), the run that asks binds that change to the answer as an
//! [`AnswerAction`], and the engine applies it in the transaction that records
//! the answer. An answer either takes effect or is refused with the reason;
//! it is never recorded and then left for someone to carry out.
//!
//! The bound actions ride beside the typed `run_done` payload under
//! [`ANSWER_ACTIONS_KEY`], the way the question criterion does, so the many
//! existing payload literals are unaffected.
//!
//! [`owner_handoff_wording`] and [`owner_voiced_label`] are the other half of
//! the rule: they name wording that hands a board change to the owner
//! ("please delete that row on the board", "I will delete the row now"). The
//! engine refuses a progression question worded that way, and a test runs every
//! engine-defined option set through the same two functions.

use serde::{Deserialize, Serialize};

use super::proposal::{OperatorAnswer, OperatorAnswerType};

/// Key of the stored `run_done` payload that carries the bound actions.
pub const ANSWER_ACTIONS_KEY: &str = "answer_actions";

/// A board change the engine applies as the owner when the option it is bound
/// to is chosen. Ids are a canonical `task_…` id or a friendly short id; the
/// engine resolves them, and every one must be inside the neighbourhood of the
/// run that asked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerAction {
    /// Close a revision of the item's own PR chain and release what waits on it.
    ArchiveRevision { revision: String },
    /// Start an item, as an explicit start by the owner.
    Start { item: String },
    /// Remove the dependency of `dependent` on `prerequisite`.
    Undepend { dependent: String, prerequisite: String },
}

impl OwnerAction {
    /// The spellings [`Self::parse`] accepts, for help and error text.
    pub const SPELLINGS: &'static str =
        "`archive-revision <id>`, `start <id>` or `undepend <dependent-id> <prerequisite-id>`";

    /// Parse the command-line spelling of a change.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let words: Vec<&str> = spec.split_whitespace().collect();
        match words.as_slice() {
            ["archive-revision", revision] => Ok(Self::ArchiveRevision {
                revision: (*revision).to_owned(),
            }),
            ["start", item] => Ok(Self::Start {
                item: (*item).to_owned(),
            }),
            ["undepend", dependent, prerequisite] => Ok(Self::Undepend {
                dependent: (*dependent).to_owned(),
                prerequisite: (*prerequisite).to_owned(),
            }),
            _ => Err(format!(
                "`{spec}` is not a change the engine can make; expected {}",
                Self::SPELLINGS
            )),
        }
    }

    /// Every work item the change names.
    pub fn targets(&self) -> Vec<&str> {
        match self {
            Self::ArchiveRevision { revision } => vec![revision],
            Self::Start { item } => vec![item],
            Self::Undepend {
                dependent,
                prerequisite,
            } => vec![dependent, prerequisite],
        }
    }

    /// The change in the words of [`Self::parse`], for messages and records.
    pub fn spelling(&self) -> String {
        match self {
            Self::ArchiveRevision { revision } => format!("archive-revision {revision}"),
            Self::Start { item } => format!("start {item}"),
            Self::Undepend {
                dependent,
                prerequisite,
            } => format!("undepend {dependent} {prerequisite}"),
        }
    }
}

/// One answer of a question and the change the engine makes when it is chosen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnswerAction {
    /// `yes` or `no` for a Yes/No question; the option label, verbatim, for a
    /// multiple-choice one.
    pub answer: String,
    pub action: OwnerAction,
}

impl AnswerAction {
    /// Parse `<answer>=<change>`, for example `yes=archive-revision <id>`. The
    /// split is at the last `=`, so an option label may contain one.
    pub fn parse(flag: &str) -> Result<Self, String> {
        let Some((answer, spec)) = flag.rsplit_once('=') else {
            return Err(format!(
                "`{flag}` must be `<answer>=<change>`, for example `yes=archive-revision <id>`"
            ));
        };
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(format!("`{flag}` names no answer before the `=`"));
        }
        Ok(Self {
            answer: answer.to_owned(),
            action: OwnerAction::parse(spec)?,
        })
    }

    /// Whether this is the change bound to `answer`.
    pub fn applies_to(&self, answer: &OperatorAnswer) -> bool {
        match answer {
            OperatorAnswer::YesNo { value } => self.answer.eq_ignore_ascii_case(if *value { "yes" } else { "no" }),
            OperatorAnswer::MultipleChoice { value } => &self.answer == value,
        }
    }
}

/// Check bound actions against the question they belong to: only a plain
/// Yes/No or a multiple-choice question carries them, each names one of the
/// question's answers, and no answer has two.
pub fn validate_answer_actions(answer_type: &OperatorAnswerType, actions: &[AnswerAction]) -> Result<(), String> {
    for (index, bound) in actions.iter().enumerate() {
        match answer_type {
            OperatorAnswerType::YesNo => {
                if !["yes", "no"].iter().any(|name| bound.answer.eq_ignore_ascii_case(name)) {
                    return Err(format!(
                        "`{}` is not an answer of a yes-no question; bind the change to `yes` or `no`",
                        bound.answer
                    ));
                }
            }
            OperatorAnswerType::MultipleChoice { choices } => {
                if !choices.contains(&bound.answer) {
                    return Err(format!("`{}` is not one of the offered choices", bound.answer));
                }
            }
            OperatorAnswerType::CreatePrerequisiteTask { .. } | OperatorAnswerType::LinkPrerequisiteTask { .. } => {
                return Err(
                    "a prerequisite question already says what each answer does; it cannot also bind a change".into(),
                );
            }
        }
        if bound.action.targets().iter().any(|target| target.trim().is_empty()) {
            return Err(format!("the change bound to `{}` names an empty id", bound.answer));
        }
        if actions[..index]
            .iter()
            .any(|earlier| earlier.answer.eq_ignore_ascii_case(&bound.answer))
        {
            return Err(format!("`{}` has more than one change bound to it", bound.answer));
        }
    }
    Ok(())
}

/// The actions a stored `run_done` payload binds to its question's answers.
/// Empty when it binds none. An unreadable value is an error, never an empty
/// list: an answer whose change cannot be read must not be recorded as applied.
pub fn run_done_answer_actions(payload_json: &str) -> Result<Vec<AnswerAction>, String> {
    let payload: serde_json::Value =
        serde_json::from_str(payload_json).map_err(|err| format!("stored declaration is not JSON: {err}"))?;
    match payload.get(ANSWER_ACTIONS_KEY) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|err| format!("stored `{ANSWER_ACTIONS_KEY}` cannot be read: {err}")),
    }
}

/// Verbs that change the board. Matched as whole words.
const BOARD_VERBS: &[&str] = &[
    "archive",
    "cancel",
    "close",
    "delete",
    "drag",
    "drop",
    "move",
    "park",
    "reap",
    "redispatch",
    "remove",
    "reopen",
    "restart",
    "start",
    "stop",
    "unblock",
    "unpark",
];

/// Words that may stand between "you" or "please" and the verb it governs.
const FILLERS: &[&str] = &[
    "also", "can", "could", "first", "go", "have", "just", "ll", "may", "might", "must", "need", "now", "should",
    "then", "to", "will", "would",
];

/// Phrases that put the change in the owner's hands whatever the verb.
const BY_HAND: &[&str] = &["by hand", "manually", "yourself"];

/// Where the owner would have to go to make a change themselves.
const BOARD_PLACES: &[&str] = &[
    "on the board",
    "from the board",
    "off the board",
    "on the kanban",
    "in the app",
];

/// Commands the owner would have to run themselves.
const OWNER_COMMANDS: &[&str] = &["bossctl", "boss task ", "boss chore ", "boss engine "];

fn is_ing_form(word: &str, verb: &str) -> bool {
    let without_e = verb.strip_suffix('e').unwrap_or(verb);
    let doubled = verb.chars().last().map(|last| format!("{verb}{last}ing"));
    word == format!("{verb}ing") || word == format!("{without_e}ing") || doubled.as_deref() == Some(word)
}

/// Why `text` hands a board change to the owner, as the phrase that does it,
/// or `None` when it does not.
///
/// Four shapes are named, each of them wording under which the owner, not the
/// engine, would make the change:
///
/// - "by hand", "manually", "yourself";
/// - "you" or "please" governing a board verb ("you archive", "please delete",
///   "you will need to start");
/// - a sentence that opens with a board verb and says where to do it ("Delete
///   the cancelled row on the board", "Deleting the row on the board lets …");
/// - a command to run (`bossctl …`, `boss task …`).
///
/// A board verb on its own is fine: "Archive the cancelled build-fix" names
/// what the engine does when the change is bound to that answer.
pub fn owner_handoff_wording(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for sentence in lower.split(['.', '?', '!', ';', '\n']) {
        if let Some(phrase) = BY_HAND
            .iter()
            .chain(OWNER_COMMANDS)
            .find(|phrase| sentence.contains(**phrase))
        {
            return Some(phrase.trim().to_owned());
        }
        let words: Vec<&str> = sentence
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        for (index, word) in words.iter().enumerate() {
            if !matches!(*word, "you" | "please") {
                continue;
            }
            let governed = words[index + 1..].iter().find(|next| !FILLERS.contains(next));
            if let Some(verb) = governed.filter(|next| BOARD_VERBS.contains(next)) {
                return Some(format!("{word} {verb}"));
            }
        }
        let opens_with_board_verb = words.first().and_then(|first| {
            BOARD_VERBS
                .iter()
                .find(|verb| first == *verb || is_ing_form(first, verb))
        });
        if let Some(verb) = opens_with_board_verb
            && let Some(place) = BOARD_PLACES.iter().find(|place| sentence.contains(**place))
        {
            return Some(format!("{verb} … {place}"));
        }
    }
    None
}

/// Why a button label speaks as the owner promising to act ("I will delete
/// the cancelled row now"), or `None`. A label names what the engine does when
/// it is chosen, so it is never in the first person.
pub fn owner_voiced_label(label: &str) -> Option<String> {
    let lower = label.trim().to_lowercase().replace('’', "'");
    ["i will ", "i'll ", "i'm ", "i am ", "i can ", "i ", "let me "]
        .iter()
        .find(|opening| lower.starts_with(**opening))
        .map(|opening| opening.trim().to_owned())
}

/// The parts of a labelled explanation that say what an answer does.
///
/// `Details:` is always evidence and is dropped. For a Yes/No question,
/// `Context:` describes the situation and is dropped too; `Yes:`, `No:` and
/// `Recommendation:` are what the owner reads as "what happens if I pick
/// this". A multiple-choice question has no `Yes:` / `No:` sections: it says
/// in `Context:` what each option leads to, so that section is kept.
pub fn explanation_decision_text(explanation: &str, answer_type: &OperatorAnswerType) -> String {
    let keep_context = matches!(answer_type, OperatorAnswerType::MultipleChoice { .. });
    let mut kept = Vec::new();
    let mut in_context = false;
    for line in explanation.lines() {
        match line.trim() {
            "Details:" => break,
            "Context:" => in_context = !keep_context,
            "Yes:" | "No:" | "Recommendation:" | "Split:" | "Bypass limit:" => in_context = false,
            body if !in_context && !body.is_empty() => kept.push(body),
            _ => {}
        }
    }
    kept.join("\n")
}

/// Labels of the question the engine files when progression runs out of attempts.
pub const LADDER_PARK_IT: &str = "Park it";
pub const LADDER_ARCHIVE_IT: &str = "Archive it";
pub const LADDER_KEEP_TRYING: &str = "Keep trying";

/// Nouns the engine puts on item-local Park / Archive options.
pub const ITEM_DISPOSITION_NOUNS: &[&str] = &["item", "chore", "task", "revision", "followup"];

/// Park / Archive labels the engine carries out without a bound change.
pub fn item_disposition_labels() -> Vec<String> {
    ITEM_DISPOSITION_NOUNS
        .iter()
        .flat_map(|noun| [format!("Archive this {noun}"), format!("Park this {noun}")])
        .collect()
}

/// Every option set the engine itself puts on a question. Producers read
/// their labels from here; the wording guard iterates the same list.
pub fn engine_option_sets() -> Vec<(&'static str, Vec<String>)> {
    let labels = super::answer_labels::AnswerOptionLabels::split_or_bypass();
    vec![
        (
            "out of attempts",
            vec![
                LADDER_PARK_IT.to_owned(),
                LADDER_ARCHIVE_IT.to_owned(),
                LADDER_KEEP_TRYING.to_owned(),
            ],
        ),
        (
            "architectural mismatch",
            vec![
                super::conflict_mismatch::CONFLICT_MISMATCH_CHOICE_REWRITE.to_owned(),
                super::conflict_mismatch::CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.to_owned(),
            ],
        ),
        ("split or bypass", vec![labels.yes, labels.no]),
        ("item dispositions", item_disposition_labels()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yes() -> OperatorAnswer {
        OperatorAnswer::YesNo { value: true }
    }

    fn choose(value: &str) -> OperatorAnswer {
        OperatorAnswer::MultipleChoice { value: value.into() }
    }

    fn short_id(n: u32) -> String {
        format!("T{n}")
    }

    #[test]
    fn a_change_is_parsed_from_its_command_line_spelling_and_back() {
        let revision = short_id(12);
        let dependent = short_id(13);
        let archive = format!("archive-revision {revision}");
        let undepend = format!("undepend {dependent} {revision}");
        for spec in [archive.as_str(), "start task_abc", undepend.as_str()] {
            assert_eq!(OwnerAction::parse(spec).unwrap().spelling(), spec);
        }
        assert_eq!(
            OwnerAction::parse(&undepend).unwrap().targets(),
            vec![dependent.as_str(), revision.as_str()]
        );
        for bad in [
            "",
            "archive-revision",
            &format!("delete {revision}"),
            &format!("start {} {}", short_id(1), short_id(2)),
            &format!("undepend {dependent}"),
        ] {
            let error = OwnerAction::parse(bad).unwrap_err();
            assert!(error.contains("archive-revision <id>"), "{error}");
        }
    }

    #[test]
    fn a_bound_action_splits_at_the_last_equals_and_matches_its_answer() {
        let revision = short_id(12);
        let bound = AnswerAction::parse(&format!("yes=archive-revision {revision}")).unwrap();
        assert_eq!(bound.answer, "yes");
        assert!(bound.applies_to(&yes()));
        assert!(!bound.applies_to(&OperatorAnswer::YesNo { value: false }));
        assert!(!bound.applies_to(&choose("Yes, skip it")));

        let labelled = AnswerAction::parse(&format!("Skip a=b build-fix=start {revision}")).unwrap();
        assert_eq!(labelled.answer, "Skip a=b build-fix");
        assert!(labelled.applies_to(&choose("Skip a=b build-fix")));
        assert!(!labelled.applies_to(&choose("skip a=b build-fix")));

        assert!(AnswerAction::parse(&format!("archive-revision {revision}")).is_err());
        assert!(AnswerAction::parse(&format!("=start {revision}")).is_err());
    }

    #[test]
    fn bound_actions_must_name_an_answer_the_question_offers() {
        let archive = |answer: &str| AnswerAction {
            answer: answer.into(),
            action: OwnerAction::ArchiveRevision { revision: short_id(12) },
        };
        assert!(validate_answer_actions(&OperatorAnswerType::YesNo, &[archive("Yes"), archive("no")]).is_ok());
        assert!(validate_answer_actions(&OperatorAnswerType::YesNo, &[archive("maybe")]).is_err());
        assert!(validate_answer_actions(&OperatorAnswerType::YesNo, &[archive("yes"), archive("YES")]).is_err());

        let choices = OperatorAnswerType::MultipleChoice {
            choices: vec!["Skip the build-fix".into(), "Run it first".into()],
        };
        assert!(validate_answer_actions(&choices, &[archive("Skip the build-fix")]).is_ok());
        assert!(validate_answer_actions(&choices, &[archive("yes")]).is_err());

        let prerequisite = OperatorAnswerType::LinkPrerequisiteTask {
            existing_id: Some("task_revision".into()),
            existing_pr: None,
            work_item_id: String::new(),
            name: String::new(),
            pr_url: None,
        };
        assert!(validate_answer_actions(&prerequisite, &[archive("yes")]).is_err());
        assert!(validate_answer_actions(&prerequisite, &[]).is_ok());
    }

    #[test]
    fn stored_actions_are_read_from_beside_the_typed_payload() {
        let revision = short_id(12);
        let stored = format!(
            r#"{{"outcome":"blocked","summary":"s","answer_actions":[
            {{"answer":"yes","action":{{"kind":"archive_revision","revision":"{revision}"}}}},
            {{"answer":"no","action":{{"kind":"start","item":"{revision}"}}}}]}}"#
        );
        let actions = run_done_answer_actions(&stored).unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[1].action, OwnerAction::Start { item: revision });
        assert_eq!(
            run_done_answer_actions(r#"{"outcome":"blocked","summary":"s"}"#).unwrap(),
            vec![]
        );
        // An unreadable value is refused rather than read as "nothing bound".
        assert!(run_done_answer_actions(r#"{"answer_actions":[{"answer":"yes"}]}"#).is_err());
        assert!(
            run_done_answer_actions(r#"{"answer_actions":[{"answer":"yes","action":{"kind":"delete"}}]}"#).is_err()
        );
    }

    /// The wording of the questions that reached the owner when a cancelled
    /// CI-fix revision blocked the conflict work on a pull request.
    #[test]
    fn wording_that_hands_the_change_to_the_owner_is_named() {
        for (text, phrase) in [
            (
                "Answering yes did not remove the cancelled build-fix. Please delete that row on the board so \
                 conflict work can start, or start that row to run the build-fix first?",
                "please delete",
            ),
            ("You archive the cancelled build-fix.", "you archive"),
            (
                "Delete the cancelled row on the board, because the pull request already passes its checks.",
                "delete … on the board",
            ),
            (
                "Deleting the cancelled row on the board lets the conflict work start.",
                "delete … on the board",
            ),
            ("You will need to start the build-fix again.", "you start"),
            ("Remove the dependency by hand, then answer.", "by hand"),
            ("Run `bossctl work start task_revision` to resume.", "bossctl"),
            ("The row has to be closed manually.", "manually"),
        ] {
            assert_eq!(owner_handoff_wording(text).as_deref(), Some(phrase), "{text}");
        }
    }

    #[test]
    fn wording_that_names_what_the_engine_does_is_left_alone() {
        for text in [
            "Skip that build-fix so we can resolve the conflicts with main?",
            "Skip the unstarted build-fix. The already-created conflict-resolution revision then starts.",
            "Archive the cancelled build-fix, because the pull request head already passes its checks.",
            "Start the cancelled build-fix",
            "The cancelled row stays on the board until the conflict work lands.",
            "The engine archives the cancelled build-fix and the conflict work starts.",
            "Do you want the redesign kept?",
            "Progression could not unstick this item. Park it, archive it, or keep trying?",
        ] {
            assert_eq!(owner_handoff_wording(text), None, "{text}");
        }
    }

    #[test]
    fn a_label_in_the_first_person_is_the_owner_promising_to_act() {
        for label in [
            "I will delete the cancelled row now",
            "I’ll archive it",
            "Let me start it",
        ] {
            assert!(owner_voiced_label(label).is_some(), "{label}");
        }
        for label in [
            "Start the cancelled build-fix",
            "Archive this item",
            "Keep trying",
            "Ignore it",
        ] {
            assert_eq!(owner_voiced_label(label), None, "{label}");
        }
    }

    #[test]
    fn decision_text_is_every_section_but_context_and_details() {
        let explanation = "Context:\nPlease delete nothing here.\nYes:\nThe build-fix is skipped.\nNo:\nIt runs \
                           first.\nRecommendation:\nSkip it.\nDetails:\nbossctl output";
        assert_eq!(
            explanation_decision_text(explanation, &OperatorAnswerType::YesNo),
            "The build-fix is skipped.\nIt runs first.\nSkip it."
        );
        assert_eq!(
            explanation_decision_text(
                "Context:\nc\nRecommendation:\nPick the first.",
                &OperatorAnswerType::YesNo
            ),
            "Pick the first."
        );
    }

    #[test]
    fn a_multiple_choice_context_that_hands_a_change_to_the_owner_is_in_the_decision_text() {
        let explanation = "Context:\nSkip: you delete the cancelled row on the board.\nKeep: the engine archives \
                           it.\nRecommendation:\nKeep it.";
        let multiple = OperatorAnswerType::MultipleChoice {
            choices: vec!["Skip".into(), "Keep".into()],
        };
        let decision = explanation_decision_text(explanation, &multiple);
        assert!(
            decision.contains("you delete the cancelled row on the board"),
            "{decision}"
        );
        assert_eq!(
            explanation_decision_text(explanation, &OperatorAnswerType::YesNo),
            "Keep it."
        );
    }
}
