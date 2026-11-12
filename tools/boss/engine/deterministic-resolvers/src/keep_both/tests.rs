use super::*;

/// A jj diff-style conflict block with nothing in the base: side #1 as
/// the diff, side #2 as the snapshot.
fn insertion(side1: &str, side2: &str) -> String {
    let mut block = String::from("<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n");
    for line in side1.lines() {
        block.push('+');
        block.push_str(line);
        block.push('\n');
    }
    block.push_str("+++++++ Contents of side #2\n");
    block.push_str(side2);
    block.push_str(">>>>>>> Conflict 1 of 1 ends\n");
    block
}

fn items(content: &str) -> Result<KeepBothResolution, String> {
    keep_both_insertions("src/thing.rs", content, KeepBothScope::ItemsAndImports)
}

fn lines(content: &str) -> Result<KeepBothResolution, String> {
    keep_both_insertions("src/thing.rs", content, KeepBothScope::WholeLines)
}

// ── items ───────────────────────────────────────────────────────────

#[test]
fn keeps_two_tests_appended_at_the_same_spot() {
    let content = format!(
        "mod tests {{\n    #[test]\n    fn existing() {{}}\n{}}}\n",
        insertion(
            "\n    /// Main's new test.\n    #[test]\n    fn from_main() {\n        assert!(true);\n    }\n",
            "\n    #[tokio::test]\n    async fn from_pr() {\n        let v = vec![1];\n        assert_eq!(v.len(), 1);\n    }\n",
        )
    );
    let resolved = items(&content).unwrap();
    assert_eq!(resolved.insertions, [InsertionKind::Items]);
    assert_eq!(
        resolved.content,
        "mod tests {\n    #[test]\n    fn existing() {}\n\n    /// Main's new test.\n    #[test]\n    fn from_main() {\n        assert!(true);\n    }\n\n    #[tokio::test]\n    async fn from_pr() {\n        let v = vec![1];\n        assert_eq!(v.len(), 1);\n    }\n}\n"
    );
}

#[test]
fn separates_two_multi_line_items_that_bring_no_blank_line_of_their_own() {
    let content = format!(
        "fn first() {{}}\n\n{}\nfn last() {{}}\n",
        insertion("fn from_main() {\n    one();\n}\n", "fn from_pr() {\n    two();\n}\n")
    );
    assert_eq!(
        items(&content).unwrap().content,
        "fn first() {}\n\nfn from_main() {\n    one();\n}\n\nfn from_pr() {\n    two();\n}\n\nfn last() {}\n"
    );
}

#[test]
fn a_blank_line_at_the_seam_is_kept_once() {
    let content = insertion("fn from_main() -> u8 { 1 }\n\n", "\nfn from_pr() -> u8 { 2 }\n");
    assert_eq!(
        items(&content).unwrap().content,
        "fn from_main() -> u8 { 1 }\n\nfn from_pr() -> u8 { 2 }\n"
    );
}

#[test]
fn works_in_any_rust_file_not_only_module_roots() {
    let content = insertion("pub const A: u8 = 1;\n", "pub const B: u8 = 2;\n");
    for path in ["src/lib.rs", "src/mod.rs", "src/work/schema.rs", "tests/it.rs"] {
        let resolved = keep_both_insertions(path, &content, KeepBothScope::ItemsAndImports).unwrap();
        assert_eq!(
            resolved.content, "pub const A: u8 = 1;\npub const B: u8 = 2;\n",
            "{path}"
        );
    }
}

#[test]
fn reads_the_conflict_whichever_side_jj_shows_as_the_diff() {
    // jj renders the side closer to the base as the diff; with nothing in
    // the base that is the shorter insertion, so side #1 is a snapshot
    // whenever `main` added more than the PR did.
    let content = "<<<<<<< Conflict 1 of 1\n+++++++ Contents of side #1\nmod main_one;\nmod main_two;\n%%%%%%% Changes from base to side #2\n+mod pr_one;\n>>>>>>> Conflict 1 of 1 ends\n";
    let resolved = items(content).unwrap();
    assert_eq!(resolved.content, "mod main_one;\nmod main_two;\nmod pr_one;\n");
    assert_eq!(resolved.insertions, [InsertionKind::Imports]);
}

#[test]
fn resolves_every_conflict_in_the_file_or_none() {
    let both = format!(
        "{}mod shared;\n{}",
        insertion("mod a1;\n", "mod a2;\n"),
        insertion("fn b1() -> u8 { 1 }\n", "fn b2() -> u8 { 2 }\n")
    );
    let resolved = items(&both).unwrap();
    assert_eq!(
        resolved.content,
        "mod a1;\nmod a2;\nmod shared;\nfn b1() -> u8 { 1 }\nfn b2() -> u8 { 2 }\n"
    );
    assert_eq!(resolved.insertions, [InsertionKind::Imports, InsertionKind::Items]);

    let one_bad = format!(
        "{}mod shared;\n{}",
        insertion("mod a1;\n", "mod a2;\n"),
        insertion("    run_one();\n", "    run_two();\n")
    );
    let err = items(&one_bad).unwrap_err();
    assert!(err.contains("conflict at line 8"), "{err}");
    assert!(err.contains("not a whole item"), "{err}");
}

// ── imports ─────────────────────────────────────────────────────────

#[test]
fn keeps_use_and_mod_declarations_from_both_sides() {
    let content = format!(
        "use std::fmt;\n{}\nfn main() {{}}\n",
        insertion("use std::path::Path;\npub mod alpha;\n", "use std::sync::Arc;\n")
    );
    let resolved = items(&content).unwrap();
    assert_eq!(
        resolved.content,
        "use std::fmt;\nuse std::path::Path;\npub mod alpha;\nuse std::sync::Arc;\n\nfn main() {}\n"
    );
    assert_eq!(resolved.insertions, [InsertionKind::Imports]);
}

#[test]
fn merges_names_both_sides_added_to_a_wrapped_use_list() {
    // rustfmt repacked the list on each side, so the lines differ and the
    // base is not empty, but each side only added a name.
    let content = "use crate::protocol::{\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n-    Alpha, Beta, Delta,\n+    Alpha, Beta, Charlie,\n+    Delta,\n+++++++ Contents of side #2\n    Alpha, Beta, Delta,\n    Echo,\n>>>>>>> Conflict 1 of 1 ends\n    Foxtrot,\n};\n";
    let resolved = items(content).unwrap();
    assert_eq!(
        resolved.content,
        "use crate::protocol::{\n    Alpha, Beta, Charlie, Delta, Echo,\n    Foxtrot,\n};\n"
    );
    assert_eq!(resolved.insertions, [InsertionKind::ImportList]);
}

#[test]
fn refuses_a_use_list_region_where_a_side_dropped_a_name() {
    let content = "use crate::protocol::{\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n-    Alpha, Beta,\n+    Alpha, Charlie,\n+++++++ Contents of side #2\n    Alpha, Beta, Delta,\n>>>>>>> Conflict 1 of 1 ends\n};\n";
    let err = items(content).unwrap_err();
    assert!(err.contains("removed or rewrote the import `Beta`"), "{err}");
}

// ── duplicate additions ─────────────────────────────────────────────

#[test]
fn refuses_when_both_sides_added_an_item_with_the_same_name() {
    let content = insertion(
        "fn helper() -> u8 {\n    1\n}\n",
        "fn helper() -> u8 {\n    2\n}\n\nfn other() {}\n",
    );
    let err = items(&content).unwrap_err();
    assert_eq!(err, "both sides added the same item `helper`");
    // The wider scope must not turn the refusal into a line-by-line union.
    assert_eq!(lines(&content).unwrap_err(), err);
}

#[test]
fn refuses_when_both_sides_added_the_same_declaration() {
    for (side1, side2) in [
        (
            "pub use shared::Thing;\n",
            "pub use shared::Thing;\nuse other::Thing2;\n",
        ),
        ("mod t13;\n", "mod t12;\nmod t13;\n"),
        (
            "#[cfg(test)]\nmod tests {\n    fn a() {}\n}\n",
            "#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n",
        ),
    ] {
        let err = items(&insertion(side1, side2)).unwrap_err();
        assert!(err.starts_with("both sides added the same item"), "{err}");
    }
}

#[test]
fn refuses_when_the_same_item_was_added_in_two_different_conflicts() {
    let content = format!(
        "{}fn between() {{}}\n{}",
        insertion("fn shared() {}\n", "fn only_pr() { pr(); }\n"),
        insertion("fn only_main() { main(); }\n", "fn shared() {}\n")
    );
    assert_eq!(items(&content).unwrap_err(), "both sides added the same item `shared`");
}

#[test]
fn refuses_one_definition_added_under_two_names() {
    let content = insertion(
        "/// Runtime tag for the component tier.\npub const COMPONENT_RUNTIME_V1: &str = \"component-v1\";\n",
        "/// Tag used by the component path.\npub const RUNTIME_COMPONENT_V1: &str = \"component-v1\";\n",
    );
    let err = items(&content).unwrap_err();
    assert_eq!(
        err,
        "both sides added the same definition under two names (`COMPONENT_RUNTIME_V1` and `RUNTIME_COMPONENT_V1`)"
    );
}

#[test]
fn refuses_when_both_sides_added_the_same_name_to_a_use_list() {
    let content = "use std::collections::{\n<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+    HashMap,\n+++++++ Contents of side #2\n    BTreeMap, HashMap,\n>>>>>>> Conflict 1 of 1 ends\n    HashSet,\n};\n";
    let err = items(content).unwrap_err();
    assert!(err.contains("both sides added the same import `HashMap`"), "{err}");
}

#[test]
fn refuses_the_same_name_added_at_two_places_in_one_use_list() {
    // Each conflict on its own is two different additions.
    let content = format!(
        "use std::collections::{{\n{}    BTreeSet,\n{}}};\n",
        insertion("    BTreeMap,\n", "    BinaryHeap,\n"),
        insertion("    HashSet,\n", "    BTreeMap,\n")
    );
    let err = items(&content).unwrap_err();
    assert_eq!(err, "both sides added the same import `std::collections::BTreeMap`");
}

#[test]
fn refuses_the_same_import_written_two_ways() {
    // A declaration of its own against a group that includes the name.
    let grouped = insertion("use crate::model::Task;\n", "use crate::model::{Project, Task};\n");
    assert_eq!(
        items(&grouped).unwrap_err(),
        "both sides added the same import `crate::model::Task`"
    );

    // A name added to an existing list against a declaration of its own.
    let content = format!(
        "use crate::model::{{\n{}    Zone,\n}};\n{}",
        insertion("    Task,\n", "    Project,\n"),
        insertion("use std::fmt;\n", "use crate::model::Task;\n")
    );
    assert_eq!(
        items(&content).unwrap_err(),
        "both sides added the same import `crate::model::Task`"
    );
}

#[test]
fn keeps_different_names_added_to_one_use_list_in_two_conflicts() {
    let content = format!(
        "use std::collections::{{\n{}    BTreeSet,\n{}}};\n",
        insertion("    BTreeMap,\n", "    BinaryHeap,\n"),
        insertion("    HashSet,\n", "    VecDeque,\n")
    );
    let resolved = items(&content).unwrap();
    assert_eq!(
        resolved.content,
        "use std::collections::{\n    BTreeMap, BinaryHeap,\n    BTreeSet,\n    HashSet, VecDeque,\n};\n"
    );
    assert_eq!(
        resolved.insertions,
        [InsertionKind::ImportList, InsertionKind::ImportList]
    );
}

// ── refusals ────────────────────────────────────────────────────────

#[test]
fn refuses_statements_under_the_items_scope_and_keeps_them_under_whole_lines() {
    let content = format!(
        "fn run() {{\n    setup();\n{}}}\n",
        insertion("    migrate_one(&db).unwrap();\n", "    migrate_two(&db).unwrap();\n")
    );
    let err = items(&content).unwrap_err();
    assert!(err.contains("side #1 adds something that is not a whole item"), "{err}");

    let resolved = lines(&content).unwrap();
    assert_eq!(
        resolved.content,
        "fn run() {\n    setup();\n    migrate_one(&db).unwrap();\n    migrate_two(&db).unwrap();\n}\n"
    );
    assert_eq!(resolved.insertions, [InsertionKind::Lines]);
}

#[test]
fn whole_lines_scope_refuses_the_same_line_added_by_both_sides() {
    let content = insertion(
        "    registry.register(alpha());\n    registry.register(shared());\n",
        "    registry.register(shared());\n",
    );
    let err = lines(&content).unwrap_err();
    assert!(
        err.contains("both sides added the same line `registry.register(shared());`"),
        "{err}"
    );
}

#[test]
fn refuses_an_edit_of_existing_lines() {
    let content = "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n-fn old() {}\n+fn new_main() {}\n+++++++ Contents of side #2\nfn new_pr() {}\n>>>>>>> Conflict 1 of 1 ends\n";
    for scope in [KeepBothScope::ItemsAndImports, KeepBothScope::WholeLines] {
        let err = keep_both_insertions("a.rs", content, scope).unwrap_err();
        assert!(err.contains("only same-spot insertions are kept"), "{err}");
    }
}

#[test]
fn refuses_an_insertion_that_is_not_self_contained() {
    let unbalanced = insertion("    entry(\n        one,\n", "    entry(\n        two,\n");
    for scope in [KeepBothScope::ItemsAndImports, KeepBothScope::WholeLines] {
        let err = keep_both_insertions("a.rs", &unbalanced, scope).unwrap_err();
        assert!(err.contains("leaves a `(` open"), "{err}");
    }
    let open_string = insertion("const A: &str = \"never closed;\n", "const B: u8 = 1;\n");
    assert!(
        items(&open_string)
            .unwrap_err()
            .contains("ends inside a string literal")
    );
}

#[test]
fn refuses_conflicts_it_cannot_read_as_two_sided() {
    let three_sided = "<<<<<<< Conflict 1 of 1\n%%%%%%% Changes from base to side #1\n+mod a;\n%%%%%%% Changes from base to side #2\n+mod b;\n+++++++ Contents of side #3\nmod c;\n>>>>>>> Conflict 1 of 1 ends\n";
    assert!(items(three_sided).unwrap_err().contains("only two-sided conflicts"));
    assert_eq!(items("mod a;\nmod b;\n").unwrap_err(), "no conflict markers found");
}

#[test]
fn items_scope_is_limited_to_rust_source() {
    let content = insertion("func a() {}\n", "func b() {}\n");
    let err = keep_both_insertions("App/View.swift", &content, KeepBothScope::ItemsAndImports).unwrap_err();
    assert!(err.contains("only recognised in Rust source"), "{err}");
}

// ── whole-lines scope outside Rust ──────────────────────────────────

#[test]
fn whole_lines_scope_keeps_list_entries_in_a_build_file() {
    let content = format!(
        "    deps = [\n        \"//a\",\n{}    ],\n",
        insertion("        \"//from/main\",  # ]\n", "        \"//from/pr\",\n")
    );
    let resolved = keep_both_insertions("pkg/BUILD.bazel", &content, KeepBothScope::WholeLines).unwrap();
    assert_eq!(
        resolved.content,
        "    deps = [\n        \"//a\",\n        \"//from/main\",  # ]\n        \"//from/pr\",\n    ],\n"
    );

    // The same entry on both sides is a duplicate even though it is all
    // string literal, and a comment does not make it a different entry.
    let duplicate = insertion(
        "        \"//shared\",  # from main\n",
        "        \"//shared\",\n        \"//pr\",\n",
    );
    let err = keep_both_insertions("pkg/BUILD.bazel", &duplicate, KeepBothScope::WholeLines).unwrap_err();
    assert!(err.contains("both sides added the same line `\"//shared\",`"), "{err}");
}

// ── misaligned diffs ────────────────────────────────────────────────

#[test]
fn realigns_a_conflict_that_starts_with_the_previous_items_closing_brace() {
    // What a line diff reports when a function is appended after another:
    // the old closing brace is attributed to the insertion and the new
    // function's own closing brace is left as context.
    let content = format!(
        "fn existing() {{\n    body();\n{}}}\n\nfn last() {{}}\n",
        insertion(
            "}\n\nfn from_main() {\n    one();\n",
            "}\n\nfn from_pr() {\n    two();\n"
        )
    );
    let resolved = items(&content).unwrap();
    assert_eq!(
        resolved.content,
        "fn existing() {\n    body();\n}\n\nfn from_main() {\n    one();\n}\n\nfn from_pr() {\n    two();\n}\n\nfn last() {}\n"
    );
}

#[test]
fn realigns_a_conflict_that_ends_with_the_next_items_attribute() {
    let content = format!(
        "#[test]\n{}fn last() {{}}\n",
        insertion(
            "fn from_main() { one(); }\n\n#[test]\n",
            "fn from_pr() { two(); }\n\n#[test]\n"
        )
    );
    let resolved = items(&content).unwrap();
    assert_eq!(
        resolved.content,
        "#[test]\nfn from_main() { one(); }\n\n#[test]\nfn from_pr() { two(); }\n\n#[test]\nfn last() {}\n"
    );
}

#[test]
fn does_not_realign_when_the_shared_lines_are_not_repeated_outside_the_conflict() {
    // Both sides end with an attribute, but the text before the conflict
    // does not, so the attribute really is dangling.
    let content = format!(
        "fn first() {{}}\n{}fn last() {{}}\n",
        insertion(
            "fn from_main() { one(); }\n#[test]\n",
            "fn from_pr() { two(); }\n#[test]\n"
        )
    );
    assert!(items(&content).unwrap_err().contains("attached to no item"));
}

#[test]
fn the_result_keeps_the_files_trailing_newline_state() {
    let with_newline = insertion("mod a;\n", "mod b;\n");
    assert!(items(&with_newline).unwrap().content.ends_with("mod b;\n"));
    let without = with_newline.trim_end_matches('\n').to_owned();
    assert!(items(&without).unwrap().content.ends_with("mod b;"));
}
