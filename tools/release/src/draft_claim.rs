//! The claim a release run records on the draft it is working on.
//!
//! A draft release can be left unpublished by a run that failed, or simply be
//! one that another run is still building. Nothing in GitHub's release record
//! tells those apart, so each phase of a run writes a claim into the draft's
//! notes: when a run last worked on it, how many times scheduled runs have
//! picked the draft up again, and which build did so. Later runs read it to
//! decide whether to wait, resume, give up, or delete the draft.
//!
//! The claim is one HTML comment, so it does not render on the release page.
//! `publish` removes it before the release becomes public.

use std::ops::Range;

const MARKER_OPEN: &str = "<!-- release-tool:";
const MARKER_CLOSE: &str = "-->";
const CLAIMED_AT_KEY: &str = "claimed-at";
const SCHEDULED_RESUMES_KEY: &str = "scheduled-resumes";
const BUILD_KEY: &str = "build";

/// What a run recorded when it last worked on a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DraftClaim {
    /// When a run last started a phase on the draft, in seconds since the
    /// Unix epoch.
    pub claimed_at: u64,
    /// How many times scheduled runs have resumed the draft so far.
    pub scheduled_resumes: u32,
    /// The CI build that created or resumed the draft, when the run knew it.
    pub build_url: Option<String>,
}

impl DraftClaim {
    /// Reads the claim out of a draft's notes. Notes without a well-formed
    /// claim, such as those of a draft created before claims existed, yield
    /// `None`.
    pub(crate) fn from_notes(notes: &str) -> Option<Self> {
        let span = claim_spans(notes).pop()?;
        let fields = &notes[span.start + MARKER_OPEN.len()..span.end - MARKER_CLOSE.len()];

        let mut claimed_at = None;
        let mut scheduled_resumes = 0;
        let mut build_url = None;
        for field in fields.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                CLAIMED_AT_KEY => claimed_at = value.parse().ok(),
                SCHEDULED_RESUMES_KEY => scheduled_resumes = value.parse().ok()?,
                BUILD_KEY => build_url = Some(value.to_owned()),
                _ => {}
            }
        }
        Some(Self {
            claimed_at: claimed_at?,
            scheduled_resumes,
            build_url,
        })
    }

    /// The claim as the single comment that is stored in the notes.
    pub(crate) fn marker(&self) -> String {
        let mut marker = format!(
            "{MARKER_OPEN} {CLAIMED_AT_KEY}={} {SCHEDULED_RESUMES_KEY}={}",
            self.claimed_at, self.scheduled_resumes
        );
        if let Some(url) = self.build_url.as_deref().filter(|url| is_storable_url(url)) {
            marker.push_str(&format!(" {BUILD_KEY}={url}"));
        }
        marker.push(' ');
        marker.push_str(MARKER_CLOSE);
        marker
    }

    /// Returns `notes` carrying this claim in place of any earlier one.
    pub(crate) fn write_into(&self, notes: &str) -> String {
        let notes = without_claim(notes);
        if notes.is_empty() {
            format!("{}\n", self.marker())
        } else {
            format!("{notes}\n{}\n", self.marker())
        }
    }
}

/// Returns the notes without their claim, or `None` when they carry none and
/// can be left exactly as they are.
pub(crate) fn strip_claim(notes: &str) -> Option<String> {
    (!claim_spans(notes).is_empty()).then(|| without_claim(notes))
}

/// Where each claim comment sits in `notes`. A comment runs from its opening
/// to the next `-->`, across line breaks, as an HTML comment does. Reading and
/// rewriting both go through this, so they always agree on what the claim is:
/// a claim that could be read but not replaced would never be updated.
fn claim_spans(notes: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(offset) = notes[from..].find(MARKER_OPEN) {
        let start = from + offset;
        let Some(length) = notes[start..].find(MARKER_CLOSE) else {
            break;
        };
        let end = start + length + MARKER_CLOSE.len();
        spans.push(start..end);
        from = end;
    }
    spans
}

/// Removes every claim comment, together with the lines it had to itself, and
/// trims the blank lines this leaves at either end. The result ends with one
/// newline unless it is empty.
fn without_claim(notes: &str) -> String {
    let mut kept = String::with_capacity(notes.len());
    let mut copied_to = 0;
    for span in claim_spans(notes) {
        let line_start = notes[..span.start].rfind('\n').map_or(0, |newline| newline + 1);
        let line_end = notes[span.end..]
            .find('\n')
            .map_or(notes.len(), |newline| span.end + newline + 1);
        let alone_on_its_lines =
            notes[line_start..span.start].trim().is_empty() && notes[span.end..line_end].trim().is_empty();
        let cut = if alone_on_its_lines {
            line_start.max(copied_to)..line_end
        } else {
            span
        };
        kept.push_str(&notes[copied_to..cut.start]);
        copied_to = cut.end;
    }
    kept.push_str(&notes[copied_to..]);

    let lines = kept.lines().collect::<Vec<_>>();
    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => format!("{}\n", lines[first..=last].join("\n")),
        _ => String::new(),
    }
}

/// A build link is stored only when it cannot end the comment early or be
/// split into a second field when the claim is read back.
fn is_storable_url(url: &str) -> bool {
    !url.is_empty()
        && !url.contains(MARKER_CLOSE)
        && url
            .chars()
            .all(|character| character.is_ascii_graphic() && character != '<' && character != '>')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> DraftClaim {
        DraftClaim {
            claimed_at: 1_800_000_000,
            scheduled_resumes: 2,
            build_url: Some("https://buildkite.example/org/pipeline/builds/7".to_owned()),
        }
    }

    #[test]
    fn a_claim_survives_being_written_into_notes_and_read_back() {
        let notes = claim().write_into("## Changes\n\n* one\n");

        assert_eq!(
            notes,
            "## Changes\n\n* one\n\n<!-- release-tool: claimed-at=1800000000 scheduled-resumes=2 build=https://buildkite.example/org/pipeline/builds/7 -->\n"
        );
        assert_eq!(DraftClaim::from_notes(&notes), Some(claim()));
    }

    #[test]
    fn writing_a_claim_replaces_the_previous_one() {
        let first = claim().write_into("notes\n");
        let second = DraftClaim {
            claimed_at: 1_800_000_900,
            scheduled_resumes: 3,
            build_url: None,
        };

        let notes = second.write_into(&first);

        assert_eq!(notes.matches(MARKER_OPEN).count(), 1);
        assert_eq!(DraftClaim::from_notes(&notes), Some(second));
        assert_eq!(strip_claim(&notes).as_deref(), Some("notes\n"));
    }

    #[test]
    fn stripping_restores_the_notes_the_claim_was_added_to() {
        for original in ["## Changes\n\n* one\n", "single line\n", "windows\r\nline endings\r\n"] {
            let stripped = strip_claim(&claim().write_into(original)).expect("claim present");

            assert_eq!(stripped, original.replace("\r\n", "\n"));
        }
    }

    #[test]
    fn stripping_handles_a_claim_placed_ahead_of_generated_notes() {
        // GitHub's notes generator puts caller-supplied notes first.
        let notes = format!("{}\n\n## What's Changed\n* one\n", claim().marker());

        assert_eq!(strip_claim(&notes).as_deref(), Some("## What's Changed\n* one\n"));
        assert_eq!(DraftClaim::from_notes(&notes), Some(claim()));
    }

    #[test]
    fn stripping_keeps_text_that_shares_a_line_with_the_claim() {
        let notes = format!("{}## What's Changed\n", claim().marker());

        assert_eq!(strip_claim(&notes).as_deref(), Some("## What's Changed\n"));
    }

    #[test]
    fn a_claim_reflowed_across_lines_is_still_read_and_replaced() {
        // Reading and rewriting must agree, or the count of scheduled resumes
        // would never advance and the limit would not hold.
        let reflowed = "## Changes\n\n<!-- release-tool: claimed-at=1800000000\n  scheduled-resumes=2\n-->\n";

        let read = DraftClaim::from_notes(reflowed).expect("claim read across lines");
        assert_eq!(read.scheduled_resumes, 2);

        let next = DraftClaim {
            claimed_at: 1_800_000_900,
            scheduled_resumes: read.scheduled_resumes + 1,
            build_url: None,
        };
        let rewritten = next.write_into(reflowed);

        assert_eq!(rewritten.matches(MARKER_OPEN).count(), 1, "{rewritten}");
        assert_eq!(DraftClaim::from_notes(&rewritten), Some(next));
        assert_eq!(strip_claim(&rewritten).as_deref(), Some("## Changes\n"));
    }

    #[test]
    fn the_newest_claim_wins_when_notes_hold_more_than_one() {
        let notes = format!(
            "<!-- release-tool: claimed-at=5 scheduled-resumes=1 -->\nnotes\n{}\n",
            claim().marker()
        );

        assert_eq!(DraftClaim::from_notes(&notes), Some(claim()));
        assert_eq!(strip_claim(&notes).as_deref(), Some("notes\n"));
    }

    #[test]
    fn notes_without_a_claim_are_left_untouched() {
        assert_eq!(strip_claim("## Changes\n\n\n"), None);
        assert_eq!(DraftClaim::from_notes("## Changes\n<!-- another tool -->\n"), None);
    }

    #[test]
    fn notes_holding_only_a_claim_strip_to_nothing() {
        let notes = claim().write_into("");

        assert_eq!(notes, format!("{}\n", claim().marker()));
        assert_eq!(strip_claim(&notes).as_deref(), Some(""));
    }

    #[test]
    fn a_claim_without_a_readable_start_time_is_not_a_claim() {
        assert_eq!(
            DraftClaim::from_notes("<!-- release-tool: scheduled-resumes=1 -->"),
            None
        );
        assert_eq!(
            DraftClaim::from_notes("<!-- release-tool: claimed-at=soon scheduled-resumes=1 -->"),
            None
        );
        assert_eq!(
            DraftClaim::from_notes("<!-- release-tool: claimed-at=5 scheduled-resumes=many -->"),
            None
        );
        assert_eq!(DraftClaim::from_notes("<!-- release-tool: claimed-at=5"), None);
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_ones_default() {
        assert_eq!(
            DraftClaim::from_notes("<!-- release-tool: claimed-at=5 later-addition=x -->"),
            Some(DraftClaim {
                claimed_at: 5,
                scheduled_resumes: 0,
                build_url: None,
            })
        );
    }

    #[test]
    fn a_build_link_that_could_break_the_comment_is_not_stored() {
        for url in [
            "https://example.test/a b",
            "https://example.test/-->",
            "https://example.test/<x>",
            "",
        ] {
            let marker = DraftClaim {
                claimed_at: 5,
                scheduled_resumes: 0,
                build_url: Some(url.to_owned()),
            }
            .marker();

            assert_eq!(
                marker, "<!-- release-tool: claimed-at=5 scheduled-resumes=0 -->",
                "{url:?}"
            );
        }
    }
}
