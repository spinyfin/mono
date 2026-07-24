//! Worker Stop-boundary deferred-scope marker detection.
//!
//! Root cause (T222/PR #765, recovered weeks later as Flunge T254): a worker
//! legitimately narrowed its task's scope mid-execution — it wired part of
//! the brief and deferred the rest because it needed new data plumbing, not
//! just wiring. The only record of the deferred remainder was a prose
//! sentence in the PR body ending with a false "I've filed it as a
//! followup" — workers have no ability to write the taxonomy. Task
//! completion is binary (PR merged => done) and nothing reconciles delivered
//! scope against brief scope, so the remainder silently died until an
//! operator happened to notice weeks later.
//!
//! This module gives deferred scope a first-class, parseable channel,
//! mirroring [`crate::worker_escalation`]'s `[effort-escalation]`/`[blocked]`
//! marker protocol: a worker that deliberately delivers less than the brief
//! asks emits one `[deferred-scope]` line per deferred item on its Stop
//! boundary (see
//! [`crate::runner::prompt::deferred_scope_directive`] for the prompt text taught to
//! workers). [`crate::completion::WorkerCompletionHandler::detect_and_record_deferred_scope`]
//! detects it at the same Stop-boundary surface `[effort-escalation]`/
//! `[blocked]` travel on, appends a durable audit line to the work item's
//! description, and files a coordinator-visible attention item — so a
//! followup gets created or the deferral is consciously accepted, instead of
//! silently vanishing.
//!
//! Detection is best-effort and permissive, matching
//! [`crate::worker_escalation`]'s discipline: a marker missing a required
//! field is still reported (with a parse warning) rather than dropped, and
//! matching requires a line whose trimmed content STARTS WITH the marker
//! prefix — not a substring scan — so prose that merely mentions the
//! protocol never trips it.

use crate::worker_escalation::extract_quoted;

/// `[deferred-scope]` marker prefix.
pub const DEFERRED_SCOPE_MARKER: &str = "[deferred-scope]";

/// Engine-owned `work_attention_items.kind` for a filed `[deferred-scope]` signal.
pub const DEFERRED_SCOPE_ATTENTION_KIND: &str = "deferred_scope";

/// One `[deferred-scope]` marker detected in a worker's Stop-boundary text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredScopeItem {
    /// The marker line verbatim (trimmed), e.g.
    /// `[deferred-scope] summary="…" reason="…"`.
    pub marker_line: String,
    /// `None` when both fields parsed cleanly. `Some(problem)` when
    /// `summary=`/`reason=` is missing or malformed — the item is still
    /// reported (never silently dropped), just flagged so the coordinator
    /// knows to read it by hand.
    pub parse_warning: Option<String>,
}

impl DeferredScopeItem {
    pub fn is_well_formed(&self) -> bool {
        self.parse_warning.is_none()
    }
}

/// Scan `text` (a worker's Stop-boundary assistant prose) for every line
/// beginning with [`DEFERRED_SCOPE_MARKER`]. Returns one [`DeferredScopeItem`]
/// per matching line, in document order — a worker deferring several distinct
/// pieces of scope emits one line each, and all are reported.
pub fn detect_deferred_scope_items(text: &str) -> Vec<DeferredScopeItem> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            line.strip_prefix(DEFERRED_SCOPE_MARKER).map(|rest| DeferredScopeItem {
                marker_line: line.to_owned(),
                parse_warning: validate_deferred_scope_fields(rest),
            })
        })
        .collect()
}

/// `[deferred-scope]` is well-formed when the line carries both a
/// double-quoted `summary="…"` (what was not delivered) and `reason="…"`
/// (why it was deferred).
fn validate_deferred_scope_fields(rest: &str) -> Option<String> {
    let summary_ok = extract_quoted(rest, "summary").is_some();
    let reason_ok = extract_quoted(rest, "reason").is_some();
    if summary_ok && reason_ok {
        return None;
    }
    let mut problems = Vec::new();
    if !summary_ok {
        problems.push("summary= missing or not a double-quoted string");
    }
    if !reason_ok {
        problems.push("reason= missing or not a double-quoted string");
    }
    Some(problems.join("; "))
}

/// Render the `[deferred-scope]` audit line appended to a work item's
/// description by
/// [`crate::completion::WorkerCompletionHandler::record_deferred_scope_item`].
/// `epoch` is unix seconds. `item.marker_line`'s own `[deferred-scope]`
/// prefix is stripped before formatting so the resulting line carries the
/// tag exactly once (e.g.
/// `[deferred-scope] epoch 1700000000: summary="…" reason="…"`), matching
/// the grep-able `[engine-reconcile] epoch …: …` convention already used for
/// audit lines on work item descriptions.
/// Extract `(summary, reason)` from a `[deferred-scope]` marker line's
/// quoted fields, e.g. for prefilling the followup task created by
/// [`crate::app::attentions::handle_create_task_from_deferred_scope_attention`].
/// `None` for a field that's missing or malformed — mirrors
/// [`validate_deferred_scope_fields`]'s leniency: a malformed marker still
/// yields whatever DID parse instead of nothing. Accepts the line with or
/// without its `[deferred-scope]` prefix.
pub fn summary_and_reason(marker_line: &str) -> (Option<String>, Option<String>) {
    let rest = marker_line.strip_prefix(DEFERRED_SCOPE_MARKER).unwrap_or(marker_line);
    (
        extract_quoted(rest, "summary").map(str::to_owned),
        extract_quoted(rest, "reason").map(str::to_owned),
    )
}

pub fn render_audit_line(epoch: i64, item: &DeferredScopeItem) -> String {
    let fields = item
        .marker_line
        .strip_prefix(DEFERRED_SCOPE_MARKER)
        .unwrap_or(item.marker_line.as_str())
        .trim();
    format!("\n{DEFERRED_SCOPE_MARKER} epoch {epoch}: {fields}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_well_formed_deferred_scope() {
        let text = "Some prose.\n\n\
                    [deferred-scope] summary=\"T11 data plumbing\" reason=\"needs a new ingestion pipeline\"\n";
        let items = detect_deferred_scope_items(text);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_well_formed(), "warning: {:?}", items[0].parse_warning);
        assert_eq!(
            items[0].marker_line,
            "[deferred-scope] summary=\"T11 data plumbing\" reason=\"needs a new ingestion pipeline\""
        );
    }

    #[test]
    fn detects_malformed_deferred_scope_as_an_item_with_warning() {
        let text = "I'm done for now.\n\n[deferred-scope]\n";
        let items = detect_deferred_scope_items(text);
        assert_eq!(items.len(), 1);
        assert!(!items[0].is_well_formed());
        let warning = items[0].parse_warning.as_deref().unwrap();
        assert!(warning.contains("summary"), "warning: {warning}");
        assert!(warning.contains("reason"), "warning: {warning}");
    }

    #[test]
    fn missing_reason_only_flags_reason() {
        let text = "[deferred-scope] summary=\"foo\"\n";
        let items = detect_deferred_scope_items(text);
        assert_eq!(items.len(), 1);
        assert!(!items[0].is_well_formed());
        let warning = items[0].parse_warning.as_deref().unwrap();
        assert!(warning.contains("reason"));
        assert!(!warning.contains("summary= missing"));
    }

    #[test]
    fn detects_multiple_items_in_order() {
        let text =
            "[deferred-scope] summary=\"a\" reason=\"ra\"\nmore text\n[deferred-scope] summary=\"b\" reason=\"rb\"\n";
        let items = detect_deferred_scope_items(text);
        assert_eq!(items.len(), 2);
        assert!(items[0].marker_line.contains("\"a\""));
        assert!(items[1].marker_line.contains("\"b\""));
    }

    #[test]
    fn prose_mentioning_the_marker_does_not_trip_detection() {
        let text = "I considered emitting [deferred-scope] but decided to deliver everything.";
        assert!(detect_deferred_scope_items(text).is_empty());
    }

    #[test]
    fn no_markers_returns_empty() {
        assert!(detect_deferred_scope_items("## Summary\nDelivered everything asked.\n").is_empty());
    }

    #[test]
    fn empty_text_returns_empty() {
        assert!(detect_deferred_scope_items("").is_empty());
    }

    #[test]
    fn renders_audit_line_with_the_tag_exactly_once() {
        let item = DeferredScopeItem {
            marker_line: "[deferred-scope] summary=\"a\" reason=\"b\"".to_owned(),
            parse_warning: None,
        };
        let line = render_audit_line(1_700_000_000, &item);
        assert_eq!(line, "\n[deferred-scope] epoch 1700000000: summary=\"a\" reason=\"b\"");
        assert_eq!(line.matches("[deferred-scope]").count(), 1);
    }

    #[test]
    fn summary_and_reason_extracts_both_fields() {
        let (summary, reason) =
            summary_and_reason("[deferred-scope] summary=\"T11 data plumbing\" reason=\"needs a new pipeline\"");
        assert_eq!(summary.as_deref(), Some("T11 data plumbing"));
        assert_eq!(reason.as_deref(), Some("needs a new pipeline"));
    }

    #[test]
    fn summary_and_reason_tolerates_a_missing_field() {
        let (summary, reason) = summary_and_reason("[deferred-scope] summary=\"only this\"");
        assert_eq!(summary.as_deref(), Some("only this"));
        assert_eq!(reason, None);
    }

    #[test]
    fn summary_and_reason_accepts_the_line_without_its_prefix() {
        let (summary, reason) = summary_and_reason("summary=\"a\" reason=\"b\"");
        assert_eq!(summary.as_deref(), Some("a"));
        assert_eq!(reason.as_deref(), Some("b"));
    }
}
