//! Worker Stop-boundary escalation/blocker signal detection.
//!
//! Background: incident 2026-07-02 (`exec_18b5243e65ff188_2d`, T2085) — a
//! worker hit a bazel blocker, did exactly the right thing (stopped,
//! emitted an `[effort-escalation]` marker, refused to push unvalidated
//! code, asked for guidance), and the system did nothing with it. Nothing
//! scanned worker Stop payloads for the marker, so the coordinator only
//! found out because a human happened to read the transcript. Meanwhile the
//! engine's "produce a PR" auto-nudge kept firing at the parked worker.
//!
//! This module defines the two sanctioned markers a worker emits on its
//! Stop boundary when it cannot proceed unassisted, and the parser
//! [`crate::completion::WorkerCompletionHandler`] uses to detect them:
//!
//! - **`[effort-escalation]`** — the assigned work is bigger than it was
//!   classified at. Pre-existing convention (design
//!   `effort-and-model-estimation.md`); previously only ever noticed
//!   manually by whichever human/coordinator happened to read the
//!   transcript.
//! - **`[blocked]`** — new convention: the worker needs a human/coordinator
//!   decision before it can continue (a build failure it can't resolve, an
//!   ambiguous requirement, conflicting instructions, a missing credential).
//!
//! Detection is best-effort and permissive: a marker that is missing or
//! malforms its fields is still a signal (surfaced to the coordinator with
//! a parse warning) rather than being silently dropped — an operator
//! reading a garbled escalation is much better than the engine pretending
//! nothing happened, which is exactly the failure mode this module fixes.
//! The matching discipline mirrors [`crate::no_op_signal`] and
//! [`crate::automation_triage`]'s marker parsers: a line whose trimmed
//! content starts with the marker prefix (case-sensitive, brackets
//! included), not a substring scan — prose that merely *mentions* the
//! protocol must not trip it.

/// `[effort-escalation]` marker prefix (design `effort-and-model-estimation.md`).
pub const EFFORT_ESCALATION_MARKER: &str = "[effort-escalation]";

/// `[blocked]` marker prefix — the new blocker convention this module
/// introduces. Documented for workers in
/// [`crate::runner::worker_escalation_protocol_directive`].
pub const BLOCKED_MARKER: &str = "[blocked]";

/// Engine-owned `work_attention_items.kind` for a filed `[effort-escalation]` signal.
pub const WORKER_ESCALATION_ATTENTION_KIND: &str = "worker_escalation";

/// Engine-owned `work_attention_items.kind` for a filed `[blocked]` signal.
pub const WORKER_BLOCKED_ATTENTION_KIND: &str = "worker_blocked";

/// Which of the two sanctioned markers a [`WorkerSignal`] was parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSignalKind {
    EffortEscalation,
    Blocked,
}

impl WorkerSignalKind {
    /// The `work_attention_items.kind` this signal files as.
    pub fn attention_kind(self) -> &'static str {
        match self {
            WorkerSignalKind::EffortEscalation => WORKER_ESCALATION_ATTENTION_KIND,
            WorkerSignalKind::Blocked => WORKER_BLOCKED_ATTENTION_KIND,
        }
    }
}

/// One escalation/blocker marker detected in a worker's Stop-boundary text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSignal {
    pub kind: WorkerSignalKind,
    /// The marker line verbatim (trimmed), e.g.
    /// `[effort-escalation] requested_level=large reason="…"`.
    pub marker_line: String,
    /// `None` when the marker's fields parsed cleanly. `Some(problem)` when
    /// a required field is missing or malformed — the marker is still
    /// reported as a signal (never silently dropped), just flagged so the
    /// coordinator knows to read it by hand rather than trusting an
    /// automated field extraction.
    pub parse_warning: Option<String>,
}

impl WorkerSignal {
    pub fn is_well_formed(&self) -> bool {
        self.parse_warning.is_none()
    }
}

/// Scan `text` (a worker's Stop-boundary assistant prose, possibly spanning
/// many turns) for every line beginning with [`EFFORT_ESCALATION_MARKER`] or
/// [`BLOCKED_MARKER`]. Returns one [`WorkerSignal`] per matching line, in
/// document order; multiple markers in one response are all reported (the
/// coordinator processes them in order per its existing protocol).
pub fn detect_worker_signals(text: &str) -> Vec<WorkerSignal> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix(EFFORT_ESCALATION_MARKER) {
                Some(WorkerSignal {
                    kind: WorkerSignalKind::EffortEscalation,
                    marker_line: line.to_owned(),
                    parse_warning: validate_effort_escalation_fields(rest),
                })
            } else {
                line.strip_prefix(BLOCKED_MARKER).map(|rest| WorkerSignal {
                    kind: WorkerSignalKind::Blocked,
                    marker_line: line.to_owned(),
                    parse_warning: validate_blocked_fields(rest),
                })
            }
        })
        .collect()
}

/// Low-confidence fallback for a worker that asked for direction in prose
/// without emitting the documented `[blocked]` marker (the marker is the
/// contract; this is a best-effort net under it). Only meaningful when
/// [`detect_worker_signals`] found nothing — callers should not run this
/// when an explicit marker is already present. Matches a small, deliberately
/// narrow set of phrases lifted from real incident transcripts, to keep the
/// false-positive rate low; this is NOT a general sentiment/urgency
/// classifier.
pub fn detect_heuristic_blocker(text: &str) -> Option<WorkerSignal> {
    const PHRASES: &[&str] = &[
        "i need guidance or explicit direction before proceeding",
        "i need explicit guidance before proceeding",
        "need explicit direction before proceeding",
    ];
    let lower = text.to_lowercase();
    let matched = PHRASES.iter().find(|phrase| lower.contains(*phrase))?;
    Some(WorkerSignal {
        kind: WorkerSignalKind::Blocked,
        marker_line: format!("(heuristic match, no [blocked] marker present) …{matched}…"),
        parse_warning: Some(
            "heuristic guidance-ask match, not the documented [blocked] marker — verify by reading the \
             transcript"
                .to_owned(),
        ),
    })
}

/// `[effort-escalation]` is well-formed when the same line carries
/// `requested_level=<trivial|small|medium|large|max>` (bareword) and a
/// double-quoted `reason="…"`. Mirrors the coordinator's documented parsing
/// contract (`BossPaneModel.swift`, "Worker effort escalation" § Parsing).
fn validate_effort_escalation_fields(rest: &str) -> Option<String> {
    let level_ok = extract_bareword(rest, "requested_level")
        .is_some_and(|v| matches!(v, "trivial" | "small" | "medium" | "large" | "max"));
    let reason_ok = extract_quoted(rest, "reason").is_some();
    if level_ok && reason_ok {
        return None;
    }
    let mut problems = Vec::new();
    if !level_ok {
        problems.push("requested_level missing or not one of trivial|small|medium|large|max");
    }
    if !reason_ok {
        problems.push("reason= missing or not a double-quoted string");
    }
    Some(problems.join("; "))
}

/// `[blocked]` is well-formed when the line carries a double-quoted
/// `reason="…"`. A bare `[blocked]` with no reason is still a real signal
/// (the worker clearly meant to stop the world) — just flagged so the
/// coordinator knows there's no machine-readable reason to relay.
fn validate_blocked_fields(rest: &str) -> Option<String> {
    if extract_quoted(rest, "reason").is_some() {
        None
    } else {
        Some("reason= missing or not a double-quoted string".to_owned())
    }
}

/// Extract `key=bareword` (unquoted, whitespace-terminated) from `text`.
fn extract_bareword<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let value = &rest[..end];
    (!value.is_empty()).then_some(value)
}

/// Extract `key="quoted value"` from `text`. Requires a matching closing
/// quote on the same line; an unterminated quote yields `None` (malformed).
///
/// `pub(crate)` so [`crate::deferred_scope`] can reuse it for its own
/// `summary=`/`reason=` field extraction rather than reimplementing the
/// same quoted-value scan.
pub(crate) fn extract_quoted<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_well_formed_effort_escalation() {
        let text = "Some prose.\n\n[effort-escalation] requested_level=large reason=\"multi-subsystem race\"\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, WorkerSignalKind::EffortEscalation);
        assert!(signals[0].is_well_formed(), "warning: {:?}", signals[0].parse_warning);
        assert_eq!(
            signals[0].marker_line,
            "[effort-escalation] requested_level=large reason=\"multi-subsystem race\""
        );
    }

    #[test]
    fn detects_malformed_effort_escalation_as_a_signal_with_warning() {
        // O'Brien's incident marker: bare, no requested_level, no reason.
        let text = "I'm blocked.\n\n[effort-escalation]\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, WorkerSignalKind::EffortEscalation);
        assert!(!signals[0].is_well_formed());
        let warning = signals[0].parse_warning.as_deref().unwrap();
        assert!(warning.contains("requested_level"), "warning: {warning}");
        assert!(warning.contains("reason"), "warning: {warning}");
    }

    #[test]
    fn detects_effort_escalation_with_invalid_level_value() {
        let text = "[effort-escalation] requested_level=huge reason=\"big\"\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 1);
        assert!(!signals[0].is_well_formed());
        assert!(signals[0].parse_warning.as_deref().unwrap().contains("requested_level"));
    }

    #[test]
    fn detects_well_formed_blocked_marker() {
        let text = "[blocked] reason=\"bazel build fails with E0583, survives clean --expunge; need guidance\"\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, WorkerSignalKind::Blocked);
        assert!(signals[0].is_well_formed());
    }

    #[test]
    fn detects_bare_blocked_marker_as_malformed_signal() {
        let text = "[blocked]\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, WorkerSignalKind::Blocked);
        assert!(!signals[0].is_well_formed());
    }

    #[test]
    fn detects_multiple_markers_in_order() {
        let text = "[effort-escalation] requested_level=medium reason=\"a\"\nmore text\n[blocked] reason=\"b\"\n";
        let signals = detect_worker_signals(text);
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].kind, WorkerSignalKind::EffortEscalation);
        assert_eq!(signals[1].kind, WorkerSignalKind::Blocked);
    }

    #[test]
    fn prose_mentioning_the_marker_does_not_trip_detection() {
        let text = "I considered emitting [effort-escalation] but decided the estimate still holds.";
        assert!(detect_worker_signals(text).is_empty());
    }

    #[test]
    fn effort_escalation_ack_marker_does_not_collide() {
        // The coordinator's own reply marker must never be mistaken for a
        // fresh worker escalation.
        let text = "[effort-escalation-ack] level=large next_dispatch=true\n";
        assert!(detect_worker_signals(text).is_empty());
    }

    #[test]
    fn no_markers_returns_empty() {
        assert!(detect_worker_signals("## Summary\nOpened the PR.\n").is_empty());
    }

    #[test]
    fn empty_text_returns_empty() {
        assert!(detect_worker_signals("").is_empty());
    }

    #[test]
    fn heuristic_blocker_matches_incident_phrase() {
        let text = "Stopping here — I need guidance or explicit direction before proceeding on this bazel issue.";
        let sig = detect_heuristic_blocker(text).expect("heuristic match");
        assert_eq!(sig.kind, WorkerSignalKind::Blocked);
        assert!(!sig.is_well_formed());
    }

    #[test]
    fn heuristic_blocker_does_not_match_unrelated_text() {
        assert!(detect_heuristic_blocker("Made the change and opened a PR.").is_none());
    }

    #[test]
    fn attention_kind_maps_correctly() {
        assert_eq!(
            WorkerSignalKind::EffortEscalation.attention_kind(),
            WORKER_ESCALATION_ATTENTION_KIND
        );
        assert_eq!(
            WorkerSignalKind::Blocked.attention_kind(),
            WORKER_BLOCKED_ATTENTION_KIND
        );
    }
}
