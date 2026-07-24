//! The Claude driver's **fallback** producer for
//! [`Capability::StructuredOutput`](crate::Capability::StructuredOutput).
//!
//! The primary channel is the driver-agnostic file contract
//! ([`boss_engine_structured_output`]): the engine designates a path per
//! payload kind, the worker writes JSON there, the engine reads it. Nothing in
//! that path needs to know what a Claude transcript looks like.
//!
//! This module is what happens when the file is empty — a remote worker whose
//! artifact was written on the far host, an in-flight run spawned by an older
//! engine, or a worker that simply narrated its result instead of writing it.
//! Every sentinel Boss ever taught a Claude worker lives here, and *only*
//! here: the `FOLLOWUPS:` fenced-JSON block, the `automation: task <id>` /
//! `automation: skip — <reason>` triage marker, the "print the PR URL on its
//! own line" convention, and the reviewer's fenced-JSON `ReviewResult` copy.
//! Each is translated into the same wire form the file contract uses, so the
//! engine parses one shape regardless of which channel delivered it.
//!
//! A driver with no such conventions returns no candidates and rides the
//! file channel alone — which is the point of the whole exercise.

use boss_engine_structured_output::fallback::{FallbackCandidate, json_object_candidates};
use boss_engine_structured_output::{PrUrlPayload, StructuredOutputKind, TriageOutcome, pr_url};
use boss_engine_utils::json_extract::find_first_balanced_array;

/// Recover `kind`'s payload from Claude worker prose, most-preferred first.
///
/// See [`crate::AgentDriver::structured_output_fallback`] for the contract.
pub fn fallback_candidates(kind: StructuredOutputKind, text: &str) -> Vec<FallbackCandidate> {
    match kind {
        StructuredOutputKind::PrUrl => pr_url_candidates(text),
        StructuredOutputKind::ReviewResult => json_object_candidates(text),
        StructuredOutputKind::TriageDecision => triage_candidates(text),
        StructuredOutputKind::Followups => followups_candidates(text),
        // The postmortem manifest has never had a prose sentinel: it is a
        // required file-only artifact, and inventing a scrape for it would
        // undo that. An absent file stays an absent file.
        StructuredOutputKind::PostmortemFollowups => Vec::new(),
    }
}

/// The PR URL from the worker's final message, honouring the prompt's "print
/// the PR URL on its own line as the final thing in your final response"
/// convention: the *last* URL in the message wins, so a message that quotes
/// the parent PR or a related one in its prose still resolves to the PR this
/// run produced.
fn pr_url_candidates(text: &str) -> Vec<FallbackCandidate> {
    let Some(url) = pr_url::find_last_pr_url(text) else {
        return Vec::new();
    };
    payload_candidate(&PrUrlPayload { pr_url: url })
}

/// The triage decision marker (`automation: task <id>` /
/// `automation: skip — <reason>`), translated to the file contract's
/// [`TriageOutcome`] wire form.
///
/// Scans every line and resolves to the **last** marker found. The caller
/// joins every assistant turn in the transcript before handing text here,
/// because the real decision marker can land in a turn after the
/// `boss task create` tool call. That join means a marker-shaped line the
/// agent narrated earlier — quoting the format while explaining what it's
/// about to do, or an "already tracked" preamble example — can appear before
/// the actual decision. Taking the last marker rather than requiring exactly
/// one avoids reading that kind of narration as ambiguity: the agent's real,
/// final decision is always the one closest to the end of its transcript. A
/// single turn that itself contains two contradictory markers is vanishingly
/// rare next to the narration case, and still resolves deterministically
/// (last wins) rather than falling back to the no-marker retry path.
///
/// Matching is lenient on case and on the skip separator (em-dash `—`, hyphen
/// `-`, or colon `:` all accepted) but strict on the `automation:` prefix and
/// on the `task` / `skip` keyword having a word boundary, so prose that merely
/// *mentions* the protocol does not trip it.
fn triage_candidates(text: &str) -> Vec<FallbackCandidate> {
    let Some(outcome) = text.lines().rev().find_map(parse_marker_line) else {
        return Vec::new();
    };
    payload_candidate(&outcome)
}

/// Parse a single line into a triage marker, or `None` if it is not one. A
/// `task` marker with an empty id is rejected (returns `None`) — an explicit
/// skip with an empty reason is still a valid skip.
fn parse_marker_line(line: &str) -> Option<TriageOutcome> {
    let after_prefix = strip_ci_prefix(line.trim(), "automation:")?.trim_start();

    if let Some(rest) = strip_keyword(after_prefix, "task") {
        let id = rest.trim();
        if id.is_empty() {
            return None;
        }
        return Some(TriageOutcome::task(id));
    }
    if let Some(rest) = strip_keyword(after_prefix, "skip") {
        let reason = rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == '-' || c == ':')
            .trim();
        return Some(TriageOutcome::skip(reason));
    }
    None
}

/// Case-insensitively strip `prefix` from the start of `s`, returning the
/// remainder. ASCII-only prefixes keep the byte/char-boundary slice safe.
fn strip_ci_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (sb, pb) = (s.as_bytes(), prefix.as_bytes());
    if sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb) {
        Some(&s[pb.len()..])
    } else {
        None
    }
}

/// Like [`strip_ci_prefix`] but requires a trailing word boundary so `task`
/// matches `task T1` but not `taskforce`.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = strip_ci_prefix(s, keyword)?;
    if rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric() && c != '_') {
        Some(rest)
    } else {
        None
    }
}

/// The `FOLLOWUPS:` block: the last sentinel in the text, followed by the
/// first balanced JSON array after it (fenced or bare). The caller scans only
/// assistant-authored prose, so the `FOLLOWUPS:` instructions in the worker's
/// *prompt* can never be mistaken for an emitted block.
fn followups_candidates(text: &str) -> Vec<FallbackCandidate> {
    let Some(idx) = text.rfind("FOLLOWUPS:") else {
        return Vec::new();
    };
    match find_first_balanced_array(&text[idx..]) {
        Some(array) => vec![FallbackCandidate::explicit(array)],
        None => Vec::new(),
    }
}

/// Serialise a wire payload into a single explicit candidate. Serialisation of
/// these plain structs cannot fail; a failure would mean a broken derive, so
/// it degrades to "no candidate" rather than panicking in the completion path.
fn payload_candidate<T: serde::Serialize>(payload: &T) -> Vec<FallbackCandidate> {
    match serde_json::to_string(payload) {
        Ok(json) => vec![FallbackCandidate::explicit(json)],
        Err(err) => {
            tracing::error!(
                ?err,
                "claude driver: could not serialise a structured-output fallback payload"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triage(text: &str) -> Option<TriageOutcome> {
        let candidates = fallback_candidates(StructuredOutputKind::TriageDecision, text);
        let candidate = candidates.first()?;
        assert!(candidate.explicit, "a decision marker is an explicit payload");
        Some(serde_json::from_str(&candidate.payload).expect("driver emits the file contract's wire form"))
    }

    #[test]
    fn parses_a_clean_task_marker() {
        assert_eq!(
            triage("Work done.\n\nautomation: task T42"),
            Some(TriageOutcome::task("T42"))
        );
    }

    #[test]
    fn parses_canonical_task_id() {
        assert_eq!(
            triage("automation: task task_018abc"),
            Some(TriageOutcome::task("task_018abc")),
        );
    }

    #[test]
    fn parses_skip_with_em_dash() {
        assert_eq!(
            triage("automation: skip — no clippy warnings today"),
            Some(TriageOutcome::skip("no clippy warnings today")),
        );
    }

    #[test]
    fn parses_skip_with_hyphen_or_colon() {
        assert_eq!(
            triage("automation: skip - nothing to do"),
            Some(TriageOutcome::skip("nothing to do")),
        );
        assert_eq!(
            triage("automation: skip: already clean"),
            Some(TriageOutcome::skip("already clean")),
        );
    }

    #[test]
    fn case_insensitive_prefix() {
        assert_eq!(triage("Automation: Task T7"), Some(TriageOutcome::task("T7")));
    }

    #[test]
    fn zero_markers_is_no_candidate() {
        assert_eq!(triage("I looked around but did not finish."), None);
    }

    #[test]
    fn two_markers_resolves_to_the_last_one() {
        let msg = "automation: task T1\nOn reflection:\nautomation: skip — changed my mind";
        assert_eq!(triage(msg), Some(TriageOutcome::skip("changed my mind")));
    }

    #[test]
    fn narrated_marker_example_does_not_shadow_the_real_decision() {
        let msg = "I will emit `automation: skip — example` when done.\n\
                   Created the task.\n\
                   automation: task T9";
        assert_eq!(triage(msg), Some(TriageOutcome::task("T9")));
    }

    #[test]
    fn prose_mentioning_protocol_does_not_match() {
        assert_eq!(triage("automation: taskforce assembled"), None);
        assert_eq!(triage("I will emit automation markers when done."), None);
    }

    #[test]
    fn empty_task_id_is_not_a_marker() {
        assert_eq!(triage("automation: task   "), None);
    }

    #[test]
    fn skip_with_empty_reason_is_still_a_skip() {
        assert_eq!(triage("automation: skip"), Some(TriageOutcome::skip("")));
    }

    #[test]
    fn leading_and_trailing_whitespace_on_marker_line_tolerated() {
        assert_eq!(triage("   automation: task T9   "), Some(TriageOutcome::task("T9")));
    }

    #[test]
    fn pr_url_fallback_takes_the_last_url_in_the_message() {
        let text = "Rebased onto https://github.com/o/r/pull/1.\n\nhttps://github.com/o/r/pull/42";
        let candidates = fallback_candidates(StructuredOutputKind::PrUrl, text);
        let payload: PrUrlPayload = serde_json::from_str(&candidates[0].payload).unwrap();
        assert_eq!(payload.pr_url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn pr_url_fallback_is_empty_without_a_url() {
        assert!(fallback_candidates(StructuredOutputKind::PrUrl, "the PR is open, see above").is_empty());
    }

    #[test]
    fn followups_fallback_extracts_the_fenced_array_after_the_sentinel() {
        let text = "## Open Questions\n\nnone\n\nFOLLOWUPS:\n```json\n[{\"proposed_name\":\"x\"}]\n```\n";
        let candidates = fallback_candidates(StructuredOutputKind::Followups, text);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].payload, "[{\"proposed_name\":\"x\"}]");
        assert!(candidates[0].explicit);
    }

    #[test]
    fn followups_fallback_is_empty_without_the_sentinel() {
        assert!(fallback_candidates(StructuredOutputKind::Followups, "[{\"proposed_name\":\"x\"}]").is_empty());
    }

    #[test]
    fn review_result_fallback_offers_fenced_blocks_first() {
        let text = "prose {\"bare\":1}\n```json\n{\"revision_warranted\":false}\n```";
        let candidates = fallback_candidates(StructuredOutputKind::ReviewResult, text);
        assert_eq!(candidates[0].payload, "{\"revision_warranted\":false}");
        assert!(candidates[0].explicit);
        assert!(
            candidates.iter().any(|c| c.payload == "{\"bare\":1}" && !c.explicit),
            "bare objects remain available as inferred candidates: {candidates:?}",
        );
    }

    #[test]
    fn postmortem_followups_have_no_prose_fallback() {
        let text = "FOLLOWUPS:\n```json\n[{\"name\":\"x\"}]\n```";
        assert!(fallback_candidates(StructuredOutputKind::PostmortemFollowups, text).is_empty());
    }
}
