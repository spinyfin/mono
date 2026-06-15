//! Shared AST helpers used by both the `rust/giant-structs` (definition) and
//! `rust/giant-structs-create` (instantiation) checkleft checks.

/// Returns true if `attrs` contains `#[cfg(test)]`.
pub fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().ok().is_some_and(|id| id == "test"))
}

/// Scan `source` for the 1-based line number where `struct <name>` is declared.
/// Returns `None` when the declaration cannot be located (e.g. macro-generated).
pub fn struct_declaration_line(source: &str, struct_name: &str) -> Option<u32> {
    let search = format!("struct {struct_name}");
    for (i, line) in source.lines().enumerate() {
        let candidate = strip_visibility(line.trim_start());
        if let Some(after) = candidate.strip_prefix(&search)
            && (after.is_empty() || matches!(after.chars().next(), Some(' ' | '\t' | '<' | '{' | '(')))
        {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Strip a leading `pub` / `pub(...)` visibility modifier from a trimmed line.
pub fn strip_visibility(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    match rest.chars().next() {
        Some('(') => match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => line,
        },
        Some(c) if c.is_whitespace() => rest.trim_start(),
        _ => line,
    }
}

/// Returns true if `pattern` is a literal file path (no glob metacharacters).
pub fn is_literal_path(pattern: &str) -> bool {
    !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') && !pattern.contains('{')
}
