//! Extraction of `key="quoted value"` fields from single-line worker markers.

/// Extract `key="quoted value"` from `text`. Requires a matching closing
/// quote on the same line; an unterminated quote yields `None` (malformed).
pub fn extract_quoted<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::extract_quoted;

    #[test]
    fn extracts_the_quoted_value() {
        assert_eq!(extract_quoted(r#"a=1 reason="why not" b=2"#, "reason"), Some("why not"));
    }

    #[test]
    fn unterminated_or_missing_quote_is_none() {
        assert_eq!(extract_quoted(r#"reason="oops"#, "reason"), None);
        assert_eq!(extract_quoted("reason=bare", "reason"), None);
        assert_eq!(extract_quoted("other=\"x\"", "reason"), None);
    }
}
