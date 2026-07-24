use super::helpers::*;

// ----- ci_revision_description: revision-card title formatting -----

#[test]
fn ci_revision_description_empty_slice_is_generic() {
    assert_eq!(ci_revision_description(&[]), "Fix failing CI");
}

#[test]
fn ci_revision_description_all_blank_names_is_generic() {
    // Empty names are filtered out; with nothing left the title falls back
    // to the generic form rather than listing a run of empty strings.
    let failures = vec![failure("", "FAILURE"), failure("", "FAILURE")];
    assert_eq!(ci_revision_description(&failures), "Fix failing CI");
}

#[test]
fn ci_revision_description_lists_up_to_three_names() {
    let one = vec![failure("ci/test", "FAILURE")];
    assert_eq!(ci_revision_description(&one), "Fix failing CI: ci/test");

    let three = vec![
        failure("ci/test", "FAILURE"),
        failure("ci/lint", "FAILURE"),
        failure("ci/build", "FAILURE"),
    ];
    assert_eq!(
        ci_revision_description(&three),
        "Fix failing CI: ci/test, ci/lint, ci/build",
    );
}

#[test]
fn ci_revision_description_more_than_three_appends_more_tail() {
    let five = vec![
        failure("ci/test", "FAILURE"),
        failure("ci/lint", "FAILURE"),
        failure("ci/build", "FAILURE"),
        failure("ci/fmt", "FAILURE"),
        failure("ci/deploy", "FAILURE"),
    ];
    assert_eq!(
        ci_revision_description(&five),
        "Fix failing CI: ci/test, ci/lint, ci/build (+2 more)",
    );
}

#[test]
fn ci_revision_description_excludes_blanks_from_list_and_count() {
    // Blank names are dropped before both the shown list and the overflow
    // count: four entries but only three non-blank, so no "(+N more)" tail.
    let mixed = vec![
        failure("ci/test", "FAILURE"),
        failure("", "FAILURE"),
        failure("ci/lint", "FAILURE"),
        failure("ci/build", "FAILURE"),
    ];
    assert_eq!(
        ci_revision_description(&mixed),
        "Fix failing CI: ci/test, ci/lint, ci/build",
    );

    // Five entries, one blank -> four non-blank -> "(+1 more)".
    let mixed_overflow = vec![
        failure("ci/test", "FAILURE"),
        failure("", "FAILURE"),
        failure("ci/lint", "FAILURE"),
        failure("ci/build", "FAILURE"),
        failure("ci/fmt", "FAILURE"),
    ];
    assert_eq!(
        ci_revision_description(&mixed_overflow),
        "Fix failing CI: ci/test, ci/lint, ci/build (+1 more)",
    );
}
