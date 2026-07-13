/// Asserts that `FrontendRequest` variants are in alphabetical order.
///
/// This test fails when a variant is inserted out of order. If you are
/// adding a new variant, insert it in the correct alphabetical position
/// in the enum — do NOT append it to the end.  Keeping variants sorted
/// spreads concurrent additions across the file and cuts merge conflicts.
#[test]
fn frontend_request_variants_are_alphabetically_sorted() {
    let src = include_str!("../wire.rs");
    let variants: Vec<&str> = src
        .lines()
        .skip_while(|l| !l.contains("pub enum FrontendRequest {"))
        .skip(1)
        .take_while(|l| *l != "}")
        .filter_map(|l| {
            let t = l.trim();
            if t.chars().next().is_some_and(|c| c.is_uppercase()) {
                // Extract just the variant name (up to the first
                // non-alphanumeric character: space, `{`, or `,`).
                t.split_once(|c: char| !c.is_alphanumeric())
                    .map(|(name, _)| name)
                    .or(Some(t))
            } else {
                None
            }
        })
        .collect();

    let mut expected = variants.clone();
    expected.sort_by_key(|s| s.to_ascii_lowercase());

    assert_eq!(
        variants, expected,
        "FrontendRequest variants are not in alphabetical order. \
         Insert new variants in sorted position (do not append to the end)."
    );
}
