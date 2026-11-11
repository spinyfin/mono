//! Validate the labelled explanation before submitting a new question.

use boss_protocol::AnswerOptionLabels;

const HEADINGS: [&str; 5] = ["Context:", "Yes:", "No:", "Recommendation:", "Details:"];
/// A multiple-choice question has option buttons rather than Yes/No, so its
/// explanation carries no Yes/No sections.
const CHOICE_HEADINGS: [&str; 3] = ["Context:", "Recommendation:", "Details:"];
const EXPECTED: &str = "--explanation requires nonempty sections with labels on separate lines, in order:\nContext:\nWork and blocker\nYes:\nWhat approval authorizes\nNo:\nWhat declining means\nRecommendation:\nRecommended answer and reason\nDetails:\nTechnical evidence (omit this section if unnecessary)";
const EXPECTED_SPLIT: &str = "--explanation for a --file-count question requires nonempty sections with labels on separate lines, in order, named for the two buttons (not Yes:/No:):\nContext:\nWork and blocker\nSplit:\nWhat Split does\nBypass limit:\nWhat Bypass limit does\nRecommendation:\nI recommend Split (or Bypass limit) because <reason>\nDetails:\nTechnical evidence (omit this section if unnecessary)";
const EXPECTED_CHOICE: &str = "--explanation for --answer-type multiple-choice requires nonempty sections with labels on separate lines, in order, and no Yes:/No: sections:\nContext:\nWork and blocker, and what each option leads to\nRecommendation:\nRecommended option and reason\nDetails:\nTechnical evidence (omit this section if unnecessary)";
const REMAIN_BLOCKED: &str = "--explanation No: must name an actionable path the engine or the next worker will take; \"No = remain blocked\" is not a real choice. If there is only one viable path, do not ask a question.";

/// Clap-time check: accepts any of the shapes. The shape that must hold depends
/// on `--answer-type` and `--file-count`, which a value parser cannot see;
/// `validate_for` enforces it.
pub(super) fn validate(raw: &str) -> Result<String, String> {
    let labels = AnswerOptionLabels::split_or_bypass();
    let (yes, no) = labels.headings();
    // Details is verbatim evidence: only the structural lines before it pick the shape.
    let mut structural = raw.lines().take_while(|line| line.trim() != "Details:");
    if structural.any(|line| line.trim() == yes || line.trim() == no) {
        return check(raw, &labelled_headings(&labels)).map_err(|_| EXPECTED_SPLIT.into());
    }
    check(raw, &HEADINGS)
        .or_else(|_| check(raw, &CHOICE_HEADINGS))
        .map_err(|_| EXPECTED.into())
}

/// The explanation shape a question requires.
pub(super) enum Shape<'a> {
    /// `Yes:` / `No:` sections, for a genuine Yes/No question.
    YesNo,
    /// No option sections: the buttons are the options.
    MultipleChoice,
    /// A section named for each of the two options, replacing `Yes:` / `No:`.
    Labelled(&'a AnswerOptionLabels),
}

fn labelled_headings(labels: &AnswerOptionLabels) -> Vec<String> {
    let (yes, no) = labels.headings();
    ["Context:", &yes, &no, "Recommendation:", "Details:"]
        .map(str::to_owned)
        .to_vec()
}

/// The explanation shape required by the question's answer type.
pub(super) fn validate_for(raw: &str, shape: Shape<'_>) -> Result<String, String> {
    match shape {
        Shape::MultipleChoice => check(raw, &CHOICE_HEADINGS).map_err(|()| EXPECTED_CHOICE.into()),
        Shape::Labelled(labels) => {
            let text = check(raw, &labelled_headings(labels)).map_err(|()| EXPECTED_SPLIT.to_owned())?;
            // The decline option must be a second real path, as No must be.
            let (_, no_heading) = labels.headings();
            let section = boss_protocol::explanation_section(&text, &no_heading).unwrap_or_default();
            if remain_blocked_language(&section.to_ascii_lowercase()) {
                return Err(REMAIN_BLOCKED.replace("--explanation No:", &format!("--explanation {no_heading}")));
            }
            Ok(text)
        }
        Shape::YesNo => {
            let text = check(raw, &HEADINGS).map_err(|()| EXPECTED.to_owned())?;
            if no_section_names_remain_blocked(&text) {
                return Err(REMAIN_BLOCKED.into());
            }
            Ok(text)
        }
    }
}

/// Phrases that describe No as "stay blocked" rather than a second path.
const REMAIN_BLOCKED_PHRASES: &[&str] = &[
    "stay blocked",
    "stays blocked",
    "staying blocked",
    "remain blocked",
    "remains blocked",
    "remaining blocked",
    "leaves the work blocked",
    "leave the work blocked",
    "leaves the task blocked",
    "leave the task blocked",
    "a different approach would be needed",
];
const STAY_WORDS: &[&str] = &["stay", "stays", "staying", "remain", "remains", "leave", "leaves"];
const BLOCKED_WORDS: &[&str] = &["blocked", "parked"];

fn no_section_names_remain_blocked(raw: &str) -> bool {
    remain_blocked_language(
        &boss_protocol::explanation_no_section(raw)
            .unwrap_or_default()
            .to_ascii_lowercase(),
    )
}

fn remain_blocked_language(body: &str) -> bool {
    if REMAIN_BLOCKED_PHRASES.iter().any(|phrase| body.contains(phrase)) {
        return true;
    }
    let tokens: Vec<&str> = body
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        if !STAY_WORDS.contains(token) {
            continue;
        }
        let lo = index.saturating_sub(6);
        let hi = (index + 6).min(tokens.len().saturating_sub(1));
        let window = &tokens[lo..=hi];
        if window.iter().any(|word| BLOCKED_WORDS.contains(word)) {
            return true;
        }
        if window.windows(2).any(|pair| pair == ["on", "hold"]) {
            return true;
        }
    }
    false
}

fn check<H: AsRef<str>>(raw: &str, headings: &[H]) -> Result<String, ()> {
    // Details is the last heading; every heading before it is required.
    let required = headings.len() - 1;
    let mut next = 0;
    let mut has_content = false;
    for line in raw.lines().map(str::trim) {
        // Details is verbatim evidence, including any label-like lines.
        if next < headings.len() && headings.iter().any(|heading| heading.as_ref() == line) {
            if line != headings[next].as_ref() || (next > 0 && !has_content) {
                return Err(());
            }
            next += 1;
            has_content = false;
        } else if !line.is_empty() {
            if next == 0 || (next < headings.len() && matches!(line, "Yes:" | "No:")) {
                return Err(());
            }
            has_content = true;
        }
    }
    if next < required || !has_content {
        return Err(());
    }
    Ok(raw.into())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;

    pub(crate) const VALID: &str = "Context:\nWork is blocked.\nYes:\nApprove a small fix.\nNo:\nShip the smaller change instead.\nRecommendation:\nYes, to finish the work.";
    pub(crate) const REMAIN_BLOCKED: &str = "Context:\nWork is blocked.\nYes:\nApprove a small fix.\nNo:\nWork stays blocked.\nRecommendation:\nYes, to finish the work.";

    #[test]
    fn accepts_optional_details_and_preserves_evidence() {
        for text in [
            VALID.to_owned(),
            format!("{VALID}\nDetails:\nEvidence\nContext:\nverbatim"),
        ] {
            assert_eq!(validate(&text).unwrap(), text);
        }
        assert!(validate(&VALID.replace('\n', "\r\n")).is_ok());
    }

    pub(crate) const VALID_SPLIT: &str = "Context:\nThe change is too big for one PR.\nSplit:\nLand the first capability as its own PR, then finish the rest.\nBypass limit:\nKeep one PR with a one-time exception.\nRecommendation:\nI recommend Bypass limit because the change is one unit.";

    #[test]
    fn split_questions_use_their_option_names_not_yes_no() {
        let labels = AnswerOptionLabels::split_or_bypass();
        assert_eq!(
            validate_for(VALID_SPLIT, Shape::Labelled(&labels)).unwrap(),
            VALID_SPLIT
        );
        assert!(
            validate(VALID_SPLIT).is_ok(),
            "the clap-time check accepts the split shape"
        );
        let with_details = format!("{VALID_SPLIT}\nDetails:\nEvidence\nYes:\nverbatim");
        assert!(validate_for(&with_details, Shape::Labelled(&labels)).is_ok());
        for text in [
            VALID.to_owned(),
            VALID_CHOICE.to_owned(),
            VALID_SPLIT.replace("Split:", "Yes:"),
            VALID_SPLIT.replace("Bypass limit:\nKeep one PR with a one-time exception.\n", ""),
            VALID_SPLIT.replace("Land the first capability as its own PR, then finish the rest.", ""),
            format!("{VALID_SPLIT}\nDetails:\n "),
            VALID_SPLIT.replace("Context:", "Preamble\nContext:"),
        ] {
            let error = validate_for(&text, Shape::Labelled(&labels)).unwrap_err();
            assert!(
                error.contains("Split:") && error.contains("Bypass limit:"),
                "{text}: {error}"
            );
        }
        // The other shapes still refuse the split headings.
        assert!(validate_for(VALID_SPLIT, Shape::YesNo).is_err());
        // Declining must still be a real second path.
        let blocked = VALID_SPLIT.replace("Keep one PR with a one-time exception.", "The work stays blocked.");
        let error = validate_for(&blocked, Shape::Labelled(&labels)).unwrap_err();
        assert!(
            error.contains("--explanation Bypass limit:") && error.contains("remain blocked"),
            "{error}"
        );
    }

    pub(crate) const VALID_CHOICE: &str =
        "Context:\nThe rollout needs a strategy.\nRecommendation:\nGradual, to limit risk.";

    #[test]
    fn clap_reports_split_headings_for_empty_split_sections() {
        for text in [
            VALID_SPLIT.replace("Land the first capability as its own PR, then finish the rest.", ""),
            VALID_SPLIT.replace("Keep one PR with a one-time exception.", ""),
        ] {
            let error = Cli::try_parse_from([
                "boss",
                "propose",
                "done",
                "--outcome",
                "blocked",
                "--summary",
                "Split needed",
                "--answer-type",
                "create-prerequisite-task",
                "--prerequisite-name",
                "First capability",
                "--prerequisite-brief",
                "Land the first capability",
                "--file-count",
                "48",
                "--file-count-limit",
                "30",
                "--explanation",
                &text,
            ])
            .unwrap_err()
            .to_string();
            assert!(error.contains(EXPECTED_SPLIT), "{error}");
        }
    }

    #[test]
    fn details_quoting_split_headings_do_not_change_the_shape() {
        let quoted = "\nDetails:\nQuoted question:\nSplit:\nLand the first PR.\nBypass limit:\nKeep one PR.";
        for (text, extra) in [
            (format!("{VALID}{quoted}"), vec!["--question", "Approve the fix?"]),
            (
                format!("{VALID_CHOICE}{quoted}"),
                vec!["--choices", "Gradual", "All at once", "--question", "Which rollout?"],
            ),
        ] {
            assert_eq!(validate(&text).unwrap(), text);
            let kind = if extra.contains(&"--choices") {
                "multiple-choice"
            } else {
                "yes-no"
            };
            let mut args = vec![
                "boss",
                "propose",
                "done",
                "--outcome",
                "blocked",
                "--summary",
                "Blocked",
                "--answer-type",
                kind,
                "--explanation",
                &text,
            ];
            args.extend(extra);
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn multiple_choice_requires_the_short_shape_and_rejects_yes_no() {
        assert!(validate_for(VALID_CHOICE, Shape::MultipleChoice).is_ok());
        assert!(
            validate_for(
                &format!("{VALID_CHOICE}\nDetails:\nEvidence\nYes:\nverbatim"),
                Shape::MultipleChoice
            )
            .is_ok()
        );
        assert!(validate_for(VALID, Shape::MultipleChoice).is_err());
        assert!(validate_for(VALID_CHOICE, Shape::YesNo).is_err());
        for text in [
            VALID_CHOICE.replace("Gradual, to limit risk.", ""),
            VALID_CHOICE.replace("Recommendation:", "Yes:\nA\nNo:\nB\nRecommendation:"),
            VALID_CHOICE.replace("Context:", "Preamble\nContext:"),
        ] {
            assert!(validate_for(&text, Shape::MultipleChoice).is_err(), "{text}");
        }
        assert!(validate(VALID_CHOICE).is_ok());
    }

    #[test]
    fn both_question_types_reject_malformed_explanations() {
        let invalid = [
            "Why".to_owned(),
            format!("Preamble\n{VALID}"),
            VALID.replace("Context:", "Context: inline"),
            VALID.replace("Yes:\nApprove a small fix.\n", ""),
            VALID.replace("Yes:", "No:"),
            VALID.replace("Approve a small fix.", "  "),
            VALID.replace("Yes, to finish the work.", ""),
            format!("{VALID}\nDetails:\n  "),
        ];
        for kind in ["yes-no", "create-prerequisite-task"] {
            for text in &invalid {
                let mut args = vec![
                    "boss",
                    "propose",
                    "done",
                    "--outcome",
                    "blocked",
                    "--summary",
                    "Blocked",
                    "--answer-type",
                    kind,
                    "--explanation",
                    text,
                ];
                if kind == "yes-no" {
                    args.extend(["--question", "Approve the fix?"]);
                } else {
                    args.extend([
                        "--prerequisite-name",
                        "Fix fixture",
                        "--prerequisite-brief",
                        "Repair fixture",
                    ]);
                }
                let error = Cli::try_parse_from(args).expect_err("must reject malformed explanation");
                assert!(error.to_string().contains(EXPECTED), "{error}");
            }
        }
    }

    #[test]
    fn rejects_no_equals_remain_blocked() {
        assert_eq!(
            validate_for(REMAIN_BLOCKED, Shape::YesNo).unwrap_err(),
            super::REMAIN_BLOCKED
        );
        assert!(validate_for(VALID, Shape::YesNo).is_ok());
        assert!(validate_for(VALID_CHOICE, Shape::MultipleChoice).is_ok());
        for no in [
            "The work remains blocked.",
            "No leaves the task blocked; a different approach would be needed.",
            "Stay blocked until a split is approved.",
            "The work stays parked.",
            "Leave this on hold until a split is approved.",
        ] {
            let text = VALID.replace("Ship the smaller change instead.", no);
            assert!(
                validate_for(&text, Shape::YesNo)
                    .unwrap_err()
                    .contains("remain blocked"),
                "{no}"
            );
        }
        for no in [
            "Fix the blocked import here and drop the remaining refactor.",
            "Fix the blocked import here and drop the files left over.",
            "Ship the 12 remaining files; the blocked job is fixed separately.",
            "Ship the files left; the blocked job is fixed separately.",
        ] {
            assert!(validate_for(&VALID.replace("Ship the smaller change instead.", no), Shape::YesNo).is_ok());
        }
        let details_no = format!("{VALID}\nDetails:\nEvidence\nNo:\nWork stays blocked.\nverbatim");
        assert!(
            validate_for(&details_no, Shape::YesNo).is_ok(),
            "a No: line inside Details must not re-enter the remain-blocked scan"
        );
    }
}
