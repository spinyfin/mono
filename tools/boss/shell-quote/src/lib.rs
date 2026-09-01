//! POSIX-shell quoting for operator-facing commands and remote argv strings.

/// Single-quote `value` for a POSIX shell, escaping embedded single quotes
/// with the standard close-quote, escaped-quote, reopen-quote idiom.
///
/// Always quotes each value, including values without shell metacharacters,
/// so each caller gets a literal token that remains one argument when pasted
/// or re-parsed by a shell.
pub fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
