use super::*;

const ENTRY: &str = "Scope: in-scope
Major deliverables:
1. Add the handler: validate inputs and return a result.
2. Cover the handler with tests.
Likely changed files:
- `src/handler.rs`
- `tests/handler_*.rs` (~2 files)
Estimated size: ~3 files / ~400 changed lines
Effort: medium
Dependencies: none";

#[test]
fn parses_author_footprint_and_renders_brief() {
    let footprint = parse_task_footprint(ENTRY).unwrap();
    assert_eq!(footprint.deliverables.len(), 2);
    assert_eq!(
        footprint.likely_changed_files,
        ["`src/handler.rs`", "`tests/handler_*.rs` (~2 files)"]
    );
    assert_eq!((footprint.files, footprint.changed_lines), (3, 400));
    assert!(footprint.brief().contains("tests/handler_*.rs"));
    assert!(
        footprint
            .brief()
            .contains("Estimated size: ~3 files / ~400 changed lines")
    );
}

#[test]
fn missing_and_invalid_fields_fail_closed() {
    for (doc, reason) in [
        (ENTRY.replace("Estimated size: ~3 files / ~400 changed lines", ""), "missing Estimated size"),
        (ENTRY.replace("Likely changed files:\n- `src/handler.rs`\n- `tests/handler_*.rs` (~2 files)", ""), "missing Likely changed files"),
        (ENTRY.replace("Major deliverables:\n1. Add the handler: validate inputs and return a result.\n2. Cover the handler with tests.", ""), "missing numbered"),
        (ENTRY.replace("~3 files /", "~26 files /"), "exceeds limit"),
        (ENTRY.replace("~400 changed", "~1,501 changed"), "exceeds limit"),
        (ENTRY.replace("2. Cover", "3. Extra capability\n4. Another capability\n2. Cover"), "major deliverables exceeds"),
        (ENTRY.replace("~400", "unknown"), "positive integers"),
        (ENTRY.replace("(~2 files)", ""), "glob needs"),
        (ENTRY.replace("~3 files /", "~2 files /"), "file list covers"),
        (format!("{ENTRY}\nEstimated size: ~99 files / ~9000 changed lines"), "duplicate estimated size"),
    ] {
        let error = parse_task_footprint(&doc).unwrap_err();
        assert!(error.contains(reason), "{reason}: {error}");
    }
}

#[test]
fn accepts_limits_and_markdown_field_labels() {
    let doc = ENTRY
        .replace("~3 files / ~400", "~25 files / ~1,500")
        .replace("Major deliverables:", "**Major deliverables:**")
        .replace("Likely changed files:", "- **Likely changed files:**")
        .replace("Estimated size:", "**Estimated size:**");
    assert_eq!(parse_task_footprint(&doc).unwrap().changed_lines, 1500);
}

#[test]
fn fenced_examples_do_not_supply_fields() {
    assert!(parse_task_footprint(&format!("```markdown\n{ENTRY}\n```")).is_err());
}

#[test]
fn preserves_recursive_inline_globs_and_counts_only_major_deliverables() {
    let doc = ENTRY
        .replace(
            "Likely changed files:\n- `src/handler.rs`\n- `tests/handler_*.rs` (~2 files)",
            "**Likely changed files:** `src/**/*.rs` (~3 files) — mechanical edits",
        )
        .replace(
            "2. Cover the handler with tests.",
            "   1. Nested detail\n   2. Another detail\n2. Cover the handler with tests.",
        );
    let footprint = parse_task_footprint(&doc).unwrap();
    assert_eq!(footprint.deliverables.len(), 2);
    assert!(footprint.brief().contains("`src/**/*.rs`"));
    assert!(parse_task_footprint(&ENTRY.replace("`src/handler.rs`", "`BUILD`")).is_ok());
}

#[test]
fn prose_between_deliverables_cannot_hide_an_oversized_list() {
    let doc = ENTRY.replace(
        "2. Cover",
        "   Note: preserve compatibility.\n3. Extra capability\n4. Another capability\n2. Cover",
    );
    assert!(parse_task_footprint(&doc).unwrap_err().contains("4 major deliverables"));
}
