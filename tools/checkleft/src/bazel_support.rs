//! Shared Bazel cache resolution and rc lexical rules.
use anyhow::{Result, bail};
use std::path::PathBuf;

/// True for a Bazel output-base directory name: an MD5 hex digest, exactly 32
/// lowercase hex characters. Bazel's per-user root also holds non-hash entries
/// (`install/`, `cache/`) that must never be treated as an output base.
pub fn is_output_base_dir_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Bazel cache parent, before Bazel appends its per-user component.
pub fn cache_root(test_tmpdir: Option<&str>, home: Option<&str>, xdg: Option<&str>, macos: bool) -> Option<PathBuf> {
    if let Some(tmp) = test_tmpdir.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(tmp));
    }
    if macos {
        return Some(PathBuf::from(home.filter(|v| !v.is_empty())?).join("Library/Caches/bazel"));
    }
    let cache = xdg
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(home.filter(|v| !v.is_empty())?).join(".cache")))?;
    Some(cache.join("bazel"))
}

pub fn logical_bazelrc_lines(contents: &str) -> Result<Vec<(u32, String)>> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_start_line = 1u32;

    for (index, line) in contents.lines().enumerate() {
        let line_number = (index + 1) as u32;
        if current.is_empty() {
            current_start_line = line_number;
        }

        let continued = line.ends_with('\\');
        if continued {
            current.push_str(&line[..line.len() - 1]);
            continue;
        }

        current.push_str(line);
        result.push((current_start_line, current.clone()));
        current.clear();
    }

    if !current.is_empty() {
        bail!("unterminated bazelrc line continuation");
    }

    Ok(result)
}

pub fn shell_split(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '#' if !in_single && !in_double => break,
            other => current.push(other),
        }
    }

    if in_single || in_double {
        bail!("unterminated quoted string");
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    #[test]
    fn output_base_dir_name_requires_32_lowercase_hex() {
        assert!(super::is_output_base_dir_name("1eb7f20eb66e1385f89492d4801636bf"));
        assert!(!super::is_output_base_dir_name("install"));
        assert!(!super::is_output_base_dir_name("1EB7F20EB66E1385F89492D4801636BF"));
        assert!(!super::is_output_base_dir_name("1eb7f20eb66e1385f89492d4801636b"));
    }

    use super::*;

    #[test]
    fn cache_precedence_on_both_platforms() {
        for macos in [true, false] {
            assert_eq!(
                cache_root(Some("/test"), Some("/home/u"), Some("/xdg"), macos),
                Some("/test".into())
            );
        }
        assert_eq!(
            cache_root(None, Some("/home/u"), Some("/xdg"), false),
            Some("/xdg/bazel".into())
        );
        assert_eq!(
            cache_root(None, Some("/home/u"), Some("/xdg"), true),
            Some("/home/u/Library/Caches/bazel".into())
        );
        assert_eq!(cache_root(None, None, None, false), None);
    }

    #[test]
    fn lexical_rules_preserve_quoted_hashes_and_escaped_spaces() {
        assert_eq!(
            shell_split(r#"startup --output_base="/tmp/a # b" # ignored"#).unwrap(),
            ["startup", "--output_base=/tmp/a # b"]
        );
        assert_eq!(shell_split(r"import /tmp/a\ b").unwrap(), ["import", "/tmp/a b"]);
        assert!(shell_split("import 'unterminated").is_err());
        assert!(logical_bazelrc_lines("startup \\").is_err());
        let lines = logical_bazelrc_lines("startup --output_base=/tmp/ab\\\ncd").unwrap();
        assert_eq!(
            shell_split(&lines[0].1).unwrap(),
            ["startup", "--output_base=/tmp/abcd"]
        );
    }
}
