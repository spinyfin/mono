pub const DEFERRED_SCOPE_MARKER: &str = "[deferred-scope]";

pub const PROCESSED_MARKER: &str = "[postmortem-followups]";

/// `[engine-reconcile]` audit-line marker, shared with
/// [`ENGINE_AUDIT_LINE_PREFIXES`].
pub const ENGINE_RECONCILE_MARKER: &str = "[engine-reconcile]";

/// `[doc-detector]` audit-line marker, shared with
/// [`ENGINE_AUDIT_LINE_PREFIXES`].
pub const DOC_DETECTOR_MARKER: &str = "[doc-detector]";

/// `[pr-review-skip]` audit-line marker appended by
/// `completion::finalize_passes`'s pure-rebase skip gate, shared
/// with [`ENGINE_AUDIT_LINE_PREFIXES`] so that gate's audit line is also
/// stripped before brief / emptiness checks see it.
pub const PR_REVIEW_SKIP_MARKER: &str = "[pr-review-skip]";

/// Prefixes of engine-appended description lines. Shared by
/// `WorkDb::append_work_item_description_line` writers and [`strip_engine_audit_lines`] so the
/// vocabulary of audit lines lives in one place: a line whose trimmed text
/// starts with one of these is engine-owned, not human-authored brief.
///
/// Every `append_description_line` writer's marker must appear here — this
/// list is the single source of truth for what counts as an engine audit
/// line rather than human-authored brief text. See
/// `engine_audit_line_prefixes_cover_every_known_writer` below, which pins
/// each writer's marker constant against this list so a new writer that
/// forgets to register here fails a test instead of silently poisoning
/// empty-brief detection.
pub const ENGINE_AUDIT_LINE_PREFIXES: &[&str] = &[
    DOC_DETECTOR_MARKER,
    ENGINE_RECONCILE_MARKER,
    DEFERRED_SCOPE_MARKER,
    PR_REVIEW_SKIP_MARKER,
    PROCESSED_MARKER,
];

/// True when `line` is an engine-appended audit line rather than
/// human-authored brief text.
pub fn is_engine_audit_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    ENGINE_AUDIT_LINE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// Strip engine-appended audit lines from a work-item description so
/// emptiness checks and reviewer-brief rendering see only the
/// human-authored body.
///
/// `[deferred-scope]` lines are still collected into
/// `deferred_scope_declarations` from the original description; this helper
/// only removes them from the brief / revision-ask text.
pub fn strip_engine_audit_lines(description: &str) -> String {
    description
        .lines()
        .filter(|line| !is_engine_audit_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}
