//! Validate the labelled explanation before submitting a new question.

const HEADINGS: [&str; 5] = ["Context:", "Yes:", "No:", "Recommendation:", "Details:"];
const EXPECTED: &str = "--explanation requires nonempty sections with labels on separate lines, in order:\nContext:\nWork and blocker\nYes:\nWhat approval authorizes\nNo:\nWhat declining means\nRecommendation:\nRecommended answer and reason\nDetails:\nTechnical evidence (omit this section if unnecessary)";

pub(super) fn validate(raw: &str) -> Result<String, String> {
    let mut next = 0;
    let mut has_content = false;
    for line in raw.lines().map(str::trim) {
        // Details is verbatim evidence, including any label-like lines.
        if next < HEADINGS.len() && HEADINGS.contains(&line) {
            if line != HEADINGS[next] || (next > 0 && !has_content) {
                return Err(EXPECTED.into());
            }
            next += 1;
            has_content = false;
        } else if !line.is_empty() {
            if next == 0 {
                return Err(EXPECTED.into());
            }
            has_content = true;
        }
    }
    if next < 4 || !has_content {
        return Err(EXPECTED.into());
    }
    Ok(raw.into())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;

    pub(crate) const VALID: &str = "Context:\nWork is blocked.\nYes:\nApprove a small fix.\nNo:\nWork stays blocked.\nRecommendation:\nYes, to finish the work.";

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
}
