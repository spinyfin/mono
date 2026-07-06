//! Shared string/escape-aware balanced-JSON-object extraction, used by any
//! call site that has to pull a `{...}` object out of an LLM reply that may
//! be wrapped in prose or a markdown fence. Originally lived only in
//! `pr_review.rs`; lifted here so `comment_classifier.rs` (and any future
//! caller) shares the same brace-narrowing pass instead of a second, weaker
//! `find('{')`/`rfind('}')` implementation that mis-bounds on a `}` inside a
//! string value or trailing prose.

/// Given a string starting with `{`, return the slice covering the balanced
/// `{…}` object (handling nested braces and string literals). Returns `None`
/// if the input doesn't start with `{` or the braces are unbalanced.
pub(crate) fn extract_balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape_next = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Scan `s` for the first balanced `{…}` object at any starting offset
/// (string/escape-aware via [`extract_balanced_object`]). Unlike that
/// function, the input need not itself start with `{` — this walks forward
/// looking for the first `{` that yields a balanced object.
pub(crate) fn find_first_balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(obj) = extract_balanced_object(&s[i..])
        {
            return Some(obj);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_balanced_object_handles_nested_braces_and_strings() {
        let s = r#"{"a": {"b": 1}, "c": "}"}"#;
        assert_eq!(extract_balanced_object(s), Some(s));
    }

    #[test]
    fn extract_balanced_object_ignores_a_brace_inside_a_string() {
        let s = r#"{"note": "${NOTES_FILE}"}"#;
        assert_eq!(extract_balanced_object(s), Some(s));
    }

    #[test]
    fn extract_balanced_object_rejects_input_not_starting_with_brace() {
        assert_eq!(extract_balanced_object("not json"), None);
    }

    #[test]
    fn find_first_balanced_object_skips_leading_prose() {
        let s = r#"Here's the result: {"intent": "question"} thanks"#;
        assert_eq!(find_first_balanced_object(s), Some(r#"{"intent": "question"}"#));
    }

    #[test]
    fn find_first_balanced_object_returns_none_without_braces() {
        assert_eq!(find_first_balanced_object("no braces here"), None);
    }
}
