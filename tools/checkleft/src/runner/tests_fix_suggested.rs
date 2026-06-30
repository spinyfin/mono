// Tests for the suggested-fix positional edit applier.
//
// Covers:
// - line_start_offset helper
// - apply_positioned_edit helper (unit tests)
// - apply_suggested_fixes end-to-end: positional targeting of non-first occurrences,
//   uniqueness guard when no position is available, and multi-edit-per-file ordering.

use super::{apply_positioned_edit, line_start_offset};

// ── line_start_offset ─────────────────────────────────────────────────────────

#[test]
fn line_start_offset_line_one_is_zero() {
    assert_eq!(line_start_offset("hello\nworld\n", 1), Some(0));
}

#[test]
fn line_start_offset_line_two() {
    // "hello\n" is 6 bytes; line 2 starts at byte 6.
    assert_eq!(line_start_offset("hello\nworld\n", 2), Some(6));
}

#[test]
fn line_start_offset_line_three() {
    // "hello\nworld\n" is 12 bytes; line 3 starts at byte 12.
    assert_eq!(line_start_offset("hello\nworld\n", 3), Some(12));
}

#[test]
fn line_start_offset_line_zero_is_none() {
    assert_eq!(line_start_offset("hello\n", 0), None);
}

#[test]
fn line_start_offset_beyond_file_is_none() {
    assert_eq!(line_start_offset("hello\n", 3), None);
}

#[test]
fn line_start_offset_single_line_no_trailing_newline() {
    assert_eq!(line_start_offset("hello", 1), Some(0));
    assert_eq!(line_start_offset("hello", 2), None);
}

// ── apply_positioned_edit unit tests ──────────────────────────────────────────

#[test]
fn apply_positioned_edit_sole_occurrence_no_line() {
    let result = apply_positioned_edit("hello world", "hello", "goodbye", None);
    assert_eq!(result, Ok(Some("goodbye world".to_owned())));
}

#[test]
fn apply_positioned_edit_absent_returns_none() {
    let result = apply_positioned_edit("hello world", "xyz", "abc", None);
    assert_eq!(result, Ok(None));
}

#[test]
fn apply_positioned_edit_refuses_non_unique_without_line() {
    let content = "foo bar foo";
    let result = apply_positioned_edit(content, "foo", "baz", None);
    assert!(result.is_err(), "must refuse non-unique old_text with no position");
    let msg = result.unwrap_err();
    assert!(msg.contains("2"), "error message must mention the count");
}

#[test]
fn apply_positioned_edit_with_line_targets_correct_occurrence() {
    // File has "foo" on line 1 and line 3; finding is at line 3.
    let content = "foo\nbar\nfoo\n";
    // line 3 starts at byte 8 ("foo\nbar\n" = 8 bytes).
    let result = apply_positioned_edit(content, "foo", "baz", Some(3));
    assert_eq!(result, Ok(Some("foo\nbar\nbaz\n".to_owned())));
}

#[test]
fn apply_positioned_edit_with_line_targets_first_occurrence_on_that_line() {
    // "foo" appears on line 1 only; finding is at line 1.
    let content = "foo\nbar\n";
    let result = apply_positioned_edit(content, "foo", "baz", Some(1));
    assert_eq!(result, Ok(Some("baz\nbar\n".to_owned())));
}

#[test]
fn apply_positioned_edit_line_out_of_range_falls_back_to_uniqueness() {
    // Only one occurrence → uniqueness fallback applies the edit.
    let content = "foo\n";
    let result = apply_positioned_edit(content, "foo", "baz", Some(99));
    assert_eq!(result, Ok(Some("baz\n".to_owned())));
}

#[test]
fn apply_positioned_edit_line_out_of_range_refuses_if_non_unique() {
    let content = "foo\nfoo\n";
    let result = apply_positioned_edit(content, "foo", "baz", Some(99));
    assert!(
        result.is_err(),
        "must refuse when line is out of range and text is non-unique"
    );
}

#[test]
fn apply_positioned_edit_empty_old_text_is_refused() {
    let result = apply_positioned_edit("hello", "", "x", None);
    assert!(result.is_err(), "empty old_text must be refused");
}

#[test]
fn apply_positioned_edit_absent_with_line_returns_none() {
    let content = "hello\nworld\n";
    let result = apply_positioned_edit(content, "missing", "x", Some(1));
    assert_eq!(result, Ok(None));
}

// ── apply_suggested_fixes integration tests ───────────────────────────────────

fn make_runner_with_files(dir: &tempfile::TempDir, files: &[(&str, &[u8])]) -> Runner {
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create dirs");
        }
        fs::write(&path, content).expect("write file");
    }
    make_runner_for_tree(dir)
}

fn finding_with_fix(file: &str, line: Option<u32>, old_text: &str, new_text: &str) -> Finding {
    Finding {
        severity: Severity::Error,
        message: "test finding".to_owned(),
        location: Some(Location {
            path: PathBuf::from(file),
            line,
            column: None,
        }),
        remediations: vec![],
        suggested_fix: Some(SuggestedFix {
            description: "auto-fix".to_owned(),
            edits: vec![FileEdit {
                path: PathBuf::from(file),
                old_text: old_text.to_owned(),
                new_text: new_text.to_owned(),
            }],
        }),
    }
}

/// When old_text appears ≥2 times and the finding's line points to the non-first
/// occurrence, the applier must edit the correct (non-first) occurrence.
#[test]
fn apply_suggested_fixes_positional_targets_non_first_occurrence() {
    let dir = tempdir().expect("temp dir");
    // "foo" on line 1 and line 3; finding is at line 3 → must replace line 3's "foo".
    let content = b"foo\nbar\nfoo\n";
    let runner = make_runner_with_files(&dir, &[("a.txt", content)]);

    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![finding_with_fix("a.txt", Some(3), "foo", "baz")],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert!(outcomes["my-check"][0].error.is_none());
    assert!(outcomes["my-check"][0].per_file_errors.is_empty());
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"foo\nbar\nbaz\n",
        "must replace the occurrence at line 3, not line 1"
    );
}

/// When old_text appears ≥2 times and no line position is available, the fix
/// must be refused (per_file_errors) and the file must be left unchanged.
#[test]
fn apply_suggested_fixes_refuses_non_unique_without_position() {
    let dir = tempdir().expect("temp dir");
    let content = b"foo\nbar\nfoo\n";
    let runner = make_runner_with_files(&dir, &[("a.txt", content)]);

    // Finding with no line info.
    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![Finding {
            severity: Severity::Error,
            message: "test".to_owned(),
            location: Some(Location {
                path: PathBuf::from("a.txt"),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: Some(SuggestedFix {
                description: "fix".to_owned(),
                edits: vec![FileEdit {
                    path: PathBuf::from("a.txt"),
                    old_text: "foo".to_owned(),
                    new_text: "baz".to_owned(),
                }],
            }),
        }],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert_eq!(
        outcomes["my-check"][0].per_file_errors.len(),
        1,
        "must record exactly one refusal"
    );
    let (ref err_path, ref err_msg) = outcomes["my-check"][0].per_file_errors[0];
    assert_eq!(err_path, &PathBuf::from("a.txt"));
    assert!(
        err_msg.contains("2"),
        "error message should mention occurrence count; got: {err_msg}"
    );
    // File must be unchanged.
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        content,
        "ambiguous fix must leave file unchanged"
    );
}

/// Two findings on the same file at different lines, both with unique-at-position
/// old_text. Both edits must be applied and target the correct lines.
#[test]
fn apply_suggested_fixes_multiple_edits_both_applied_by_position() {
    let dir = tempdir().expect("temp dir");
    // "foo" on line 1 and line 3; each finding edits a different occurrence.
    let content = b"foo\nbar\nfoo\n";
    let runner = make_runner_with_files(&dir, &[("a.txt", content)]);

    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![
            finding_with_fix("a.txt", Some(1), "foo", "qux"),
            finding_with_fix("a.txt", Some(3), "foo", "baz"),
        ],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert!(outcomes["my-check"][0].error.is_none());
    assert!(
        outcomes["my-check"][0].per_file_errors.is_empty(),
        "both edits must be applied without refusals"
    );
    assert_eq!(
        fs::read(dir.path().join("a.txt")).unwrap(),
        b"qux\nbar\nbaz\n",
        "both positional edits must apply to their respective lines"
    );
}

/// A refused edit (non-unique, no position) on one file must not block a valid
/// edit on a different file in the same check.
#[test]
fn apply_suggested_fixes_refused_edit_does_not_block_other_files() {
    let dir = tempdir().expect("temp dir");
    let runner = make_runner_with_files(&dir, &[("a.txt", b"foo foo"), ("b.txt", b"hello")]);

    let result = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![
            // Edit for a.txt: ambiguous (no position, 2 occurrences) — will be refused.
            Finding {
                severity: Severity::Error,
                message: "a".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("a.txt"),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: Some(SuggestedFix {
                    description: "fix a".to_owned(),
                    edits: vec![FileEdit {
                        path: PathBuf::from("a.txt"),
                        old_text: "foo".to_owned(),
                        new_text: "bar".to_owned(),
                    }],
                }),
            },
            // Edit for b.txt: unique — must succeed.
            Finding {
                severity: Severity::Error,
                message: "b".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("b.txt"),
                    line: None,
                    column: None,
                }),
                remediations: vec![],
                suggested_fix: Some(SuggestedFix {
                    description: "fix b".to_owned(),
                    edits: vec![FileEdit {
                        path: PathBuf::from("b.txt"),
                        old_text: "hello".to_owned(),
                        new_text: "goodbye".to_owned(),
                    }],
                }),
            },
        ],
    };
    let fix_plan = BTreeMap::from([(
        "my-check".to_owned(),
        vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
    )]);

    let outcomes = runner.apply_suggested_fixes(&[result], &fix_plan, dir.path());

    assert_eq!(
        outcomes["my-check"][0].per_file_errors.len(),
        1,
        "one refusal for a.txt"
    );
    assert!(outcomes["my-check"][0].error.is_none());
    // a.txt unchanged (edit refused)
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"foo foo");
    // b.txt fixed
    assert_eq!(fs::read(dir.path().join("b.txt")).unwrap(), b"goodbye");
}

/// Confirms the multi-pass fix path: after the first pass fixes one occurrence,
/// a second call with a finding for the remaining occurrence applies it correctly.
#[test]
fn apply_suggested_fixes_second_pass_applies_remaining_after_first_pass() {
    let dir = tempdir().expect("temp dir");
    // Two occurrences of "foo"; first pass fixes the one at line 3.
    let content = b"foo\nbar\nfoo\n";
    let runner = make_runner_with_files(&dir, &[("a.txt", content)]);

    // First pass: fix line 3 only.
    let result1 = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![finding_with_fix("a.txt", Some(3), "foo", "baz")],
    };
    let fix_plan = BTreeMap::from([("my-check".to_owned(), vec![PathBuf::from("a.txt")])]);
    let outcomes1 = runner.apply_suggested_fixes(&[result1], &fix_plan, dir.path());
    assert!(outcomes1["my-check"][0].error.is_none());
    assert!(outcomes1["my-check"][0].per_file_errors.is_empty());
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"foo\nbar\nbaz\n");

    // Second pass: remaining "foo" at line 1 is now unique → safe to fix without position.
    let result2 = CheckResult {
        check_id: "my-check".to_owned(),
        findings: vec![finding_with_fix("a.txt", None, "foo", "qux")],
    };
    let outcomes2 = runner.apply_suggested_fixes(&[result2], &fix_plan, dir.path());
    assert!(outcomes2["my-check"][0].error.is_none());
    assert!(outcomes2["my-check"][0].per_file_errors.is_empty());
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"qux\nbar\nbaz\n");
}
