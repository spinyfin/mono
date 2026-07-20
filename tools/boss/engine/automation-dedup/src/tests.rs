//! The cases here are the audited duplicate clusters (2026-07-20), plus
//! the near-misses that must NOT be suppressed. A false suppression
//! silently loses real work, so the "distinct" half of this file is the
//! half that matters.

use super::*;

/// `a` and `b` are the same finding, matched on `expected`.
#[track_caller]
fn assert_duplicate(a: (&str, Option<&str>), b: (&str, Option<&str>), expected: MatchKind) {
    let left = fingerprint(a.0, a.1);
    let right = fingerprint(b.0, b.1);
    let forward = left
        .duplicate_of(&right)
        .unwrap_or_else(|| panic!("expected duplicate:\n  {:?}\n  {:?}", a.0, b.0));
    assert_eq!(forward.kind, expected, "matched on the wrong signal");

    // Order must never change the verdict: the gate compares a candidate
    // against siblings in arbitrary DB order.
    let backward = right
        .duplicate_of(&left)
        .unwrap_or_else(|| panic!("asymmetric verdict:\n  {:?}\n  {:?}", b.0, a.0));
    assert_eq!(backward.kind, forward.kind, "asymmetric match kind");
}

/// `a` and `b` are genuinely different findings.
#[track_caller]
fn assert_distinct(a: (&str, Option<&str>), b: (&str, Option<&str>)) {
    let left = fingerprint(a.0, a.1);
    let right = fingerprint(b.0, b.1);
    assert_eq!(
        left.duplicate_of(&right),
        None,
        "false suppression:\n  {:?}\n  {:?}",
        a.0,
        b.0
    );
    assert_eq!(
        right.duplicate_of(&left),
        None,
        "false suppression (reversed):\n  {:?}\n  {:?}",
        b.0,
        a.0
    );
}

// ── The audited clusters ─────────────────────────────────────────────────

/// The engine/core `app.rs` split, filed twice with both workers
/// dispatched simultaneously. The paraphrase the brief calls out.
#[test]
fn paraphrased_file_split_is_a_duplicate() {
    assert_duplicate(
        ("Split engine core app.rs (~2548 lines)", None),
        ("Split engine/core src/app.rs (nearing 3000-line limit)", None),
        MatchKind::FileTarget,
    );
}

/// `blocking.rs` test coverage — filed nine times over two weeks, the
/// single worst cluster.
#[test]
fn blocking_rs_test_coverage_cluster_collapses() {
    let filings = [
        "Add test coverage for work/blocking.rs",
        "Improve blocking.rs test coverage",
        "blocking.rs: add unit tests for the dependency gate",
        "Increase coverage of engine/core/src/work/blocking.rs (currently 34%)",
    ];
    let first = fingerprint(filings[0], None);
    for later in &filings[1..] {
        let candidate = fingerprint(later, None);
        assert!(
            candidate.duplicate_of(&first).is_some(),
            "should be suppressed against the surviving row: {later:?}"
        );
    }
}

/// The `pr_review` crate extraction, filed three times with two competing
/// PRs. No file is named at all — this is the module-target path.
#[test]
fn crate_extraction_cluster_matches_on_module_target() {
    assert_duplicate(
        ("Extract pr_review into its own crate", None),
        ("Move the pr_review reviewer out of engine/core into a crate", None),
        MatchKind::ModuleTarget,
    );
}

/// `metrics` carries no underscore and no backticks in one of its
/// filings — it is recovered from its position beside "crate".
#[test]
fn anchor_word_recovers_a_plain_crate_name() {
    assert_duplicate(
        ("Extract metrics crate from engine core", None),
        ("Pull the `metrics` module out into a separate crate", None),
        MatchKind::ModuleTarget,
    );
}

/// A title that names no file and no module still collapses when the
/// content words match as a set — the secondary signal.
#[test]
fn reordered_title_matches_on_normalized_title() {
    assert_duplicate(
        ("Fix flaky live-event dedup in FTL", None),
        ("Fix FTL live-event dedup flaky", None),
        MatchKind::NormalizedTitle,
    );
}

/// Sizes and counts are the noise that made exact-name matching useless:
/// the same finding is re-measured on every fire.
#[test]
fn sizes_and_counts_do_not_defeat_the_title_signal() {
    assert_duplicate(
        ("Reduce coordinator fan-out below 12 handlers", None),
        ("Reduce coordinator fan-out below 9 handlers", None),
        MatchKind::NormalizedTitle,
    );
}

// ── Must NOT be suppressed ───────────────────────────────────────────────

/// The brief's explicit requirement: one automation legitimately produces
/// findings about different files.
#[test]
fn same_automation_different_files_are_distinct() {
    assert_distinct(
        ("Split engine/core/src/app.rs (~2548 lines)", None),
        ("Split engine/core/src/runner.rs (~3100 lines)", None),
    );
}

/// Same basename, contradictory paths. This is what the qualifier
/// subsequence rule buys: without it, every `app.rs` in the repo would
/// collapse into one finding.
#[test]
fn same_basename_under_different_crates_is_distinct() {
    assert_distinct(
        ("Split engine/core/src/app.rs", None),
        ("Split tools/cube/src/app.rs", None),
    );
}

/// A missing path is "unspecified", so it must stay compatible — the
/// paraphrase case depends on it.
#[test]
fn unqualified_basename_still_matches_a_qualified_path() {
    assert_duplicate(
        ("Split app.rs, it is over the size limit", None),
        ("Split engine/core/src/app.rs", None),
        MatchKind::FileTarget,
    );
}

/// Different crates being extracted are different work.
#[test]
fn different_crate_extractions_are_distinct() {
    assert_distinct(
        ("Extract pr_review into its own crate", None),
        ("Extract automation_schedule into its own crate", None),
    );
}

/// Regression: "out" is an anchor-adjacent neighbour of "module" in both
/// titles, but neither occurrence is a backtick/`::`/`snake_case` shape —
/// it is weak on both sides. A shared weak identifier must not suppress
/// two titles that are about different code.
#[test]
fn weak_anchor_neighbour_alone_does_not_match() {
    let metrics = fingerprint("Pull the `metrics` module out into a separate crate", None);
    assert_eq!(
        metrics.module_targets.get("out"),
        Some(&false),
        "'out' should be a weak target only: {:?}",
        metrics.module_targets
    );
    assert_distinct(
        ("Pull the `metrics` module out into a separate crate", None),
        ("Move the bar module out of engine/core", None),
    );
}

/// A file-targeted finding and a module-targeted one are not comparable.
/// Rather than guess, the gate declines to match — a missed duplicate is
/// the cheap error.
#[test]
fn a_file_target_never_matches_a_bare_title() {
    assert_distinct(
        ("Split engine/core/src/app.rs", None),
        ("Tidy up the engine core", None),
    );
}

/// Two findings about the same file but genuinely different work still
/// collapse — deliberately. One automation, one file, one open task is
/// the invariant the operator asked for; the triage agent can widen the
/// surviving task's scope instead of filing beside it.
#[test]
fn different_work_on_the_same_file_collapses_by_design() {
    assert_duplicate(
        ("Split engine/core/src/app.rs", None),
        ("Add rustdoc to engine/core/src/app.rs", None),
        MatchKind::FileTarget,
    );
}

// ── Extraction details ───────────────────────────────────────────────────

/// The title wins when both title and description name a file: the
/// description's paths are usually incidental context.
#[test]
fn title_file_target_takes_precedence_over_description() {
    assert_duplicate(
        ("Split app.rs", Some("It trips CHECKS.yaml at 3000 lines")),
        (
            "Split engine/core/src/app.rs",
            Some("See tools/checkleft/checks/file/size"),
        ),
        MatchKind::FileTarget,
    );
}

/// A title with no path falls back to the description, which is where a
/// terse title ("Fix the size check failure") hides its subject.
#[test]
fn description_supplies_the_file_target_when_the_title_omits_it() {
    assert_duplicate(
        (
            "Address the file-size check failure",
            Some("engine/core/src/app.rs is 2548 lines"),
        ),
        ("Split engine/core/src/app.rs", None),
        MatchKind::FileTarget,
    );
}

/// Version-like and measurement tokens must never be mistaken for files.
#[test]
fn non_source_extensions_are_not_file_targets() {
    for title in ["Bump the pin to v1.2", "Cut latency by 3.5x", "Handle the 0.1 case"] {
        let printed = fingerprint(title, None);
        assert!(printed.file_target.is_none(), "not a file target: {title:?}");
    }
}

/// Boilerplate path segments are dropped so equivalent spellings of the
/// same path converge.
#[test]
fn noise_segments_are_dropped_from_qualifiers() {
    assert_duplicate(
        ("Split tools/boss/engine/core/src/app.rs", None),
        ("Split boss/engine/core/app.rs", None),
        MatchKind::FileTarget,
    );
}

/// Grammar beside an anchor word is not an identifier.
#[test]
fn anchor_words_do_not_harvest_grammar() {
    let printed = fingerprint("Extract it into its own crate", None);
    assert!(
        printed.module_targets.is_empty(),
        "harvested grammar as a module target: {:?}",
        printed.module_targets
    );
}

/// An empty or punctuation-only title has nothing to compare and must
/// never match — otherwise two content-free rows would suppress each other.
#[test]
fn empty_titles_never_match() {
    assert_distinct(("", None), ("", None));
    assert_distinct(("---", None), ("***", None));
}

/// The recorded key names the shared subject, preferring the spelling
/// that carries the path so the operator-facing trace is navigable.
#[test]
fn match_key_prefers_the_qualified_spelling() {
    let printed = fingerprint("Split app.rs", None);
    let sibling = fingerprint("Split engine/core/src/app.rs", None);
    assert_eq!(printed.duplicate_of(&sibling).unwrap().key, "engine/core/app.rs");
    assert_eq!(sibling.duplicate_of(&printed).unwrap().key, "engine/core/app.rs");
}

/// A fingerprint is always a duplicate of itself — the degenerate case
/// the gate hits when the triage agent re-runs the identical create.
#[test]
fn a_fingerprint_duplicates_itself() {
    for title in [
        "Split engine/core/src/app.rs",
        "Extract pr_review into its own crate",
        "Reduce coordinator fan-out",
    ] {
        let printed = fingerprint(title, None);
        assert!(printed.duplicate_of(&printed).is_some(), "self-match failed: {title:?}");
    }
}

#[test]
fn match_kind_strings_are_stable() {
    assert_eq!(MatchKind::FileTarget.as_str(), "file_target");
    assert_eq!(MatchKind::ModuleTarget.as_str(), "module_target");
    assert_eq!(MatchKind::NormalizedTitle.as_str(), "normalized_title");
}
