//! Checkleft check: flag lines in changed files that match configured
//! forbidden regular-expression patterns.
//!
//! Registered under the canonical id `text/forbidden-pattern`. Runs inside the
//! checkleft wasm host and reads files via the WASI filesystem sandbox.
//!
//! ## What the check detects
//!
//! Every non-deleted changed file is scanned line by line against every
//! configured pattern. A regex match on a line produces one finding carrying
//! that pattern's own message and severity, so a single check instance can
//! enforce many unrelated forbidden-text rules at once (e.g. leaked internal
//! identifiers, banned phrases). This check is purely generic and config-driven
//! — it has no built-in knowledge of what patterns to forbid.
//!
//! File exclusion (`exclude` / `exclude_files` / `exclude_globs`) is enforced by
//! the framework host, which subtracts excluded paths from the changeset before
//! it is lowered into this check.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "patterns": [
//!     {
//!       "name": "internal-work-item-id",
//!       "pattern": "\\bT\\d{4,}\\b",
//!       "message": "Internal work-item ids must not appear in PR-visible text.",
//!       "severity": "error"
//!     }
//!   ]
//! }
//! ```
//!
//! Each pattern entry is independent: `name` labels the rule for diagnostics,
//! `pattern` is a Rust `regex` syntax expression evaluated per line, `message`
//! is the finding text shown to the author, and `severity` is an optional
//! per-pattern override (`"error"`, `"warning"`, or `"info"`; defaults to
//! `"error"`; any other value is a config error). Because matching is done
//! per line, a pattern containing a literal newline can never match and is
//! rejected as a config error.

use checkleft_check_sdk::{ChangeKind, CheckInput, Finding, Severity, check};
use regex::Regex;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    patterns: Vec<PatternConfig>,
}

#[derive(Deserialize)]
struct PatternConfig {
    name: String,
    pattern: String,
    message: String,
    #[serde(default)]
    severity: Option<String>,
}

struct CompiledPattern {
    name: String,
    message: String,
    severity: Severity,
    regex: Regex,
}

#[check(
    name = "text/forbidden-pattern",
    description = "flags lines in changed files that match configured forbidden regular-expression patterns",
    severity = error
)]
pub fn forbidden_pattern_check(input: CheckInput) -> Vec<Finding> {
    let cfg: Config = match input.config() {
        Ok(c) => c,
        Err(e) => {
            return vec![Finding::error(format!(
                "invalid text/forbidden-pattern check config: {e}"
            ))];
        }
    };

    if cfg.patterns.is_empty() {
        return vec![Finding::error(
            "invalid text/forbidden-pattern check config: must contain at least one `patterns` entry",
        )];
    }

    let patterns = match compile_patterns(&cfg.patterns) {
        Ok(p) => p,
        Err(e) => {
            return vec![Finding::error(format!(
                "invalid text/forbidden-pattern check config: {e}"
            ))];
        }
    };

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        if file.kind == ChangeKind::Deleted {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };

        for (index, line) in content.lines().enumerate() {
            let line_number = (index + 1) as u32;
            for pattern in &patterns {
                for m in pattern.regex.find_iter(line) {
                    let column = (line[..m.start()].chars().count() + 1) as u32;
                    let finding = match pattern.severity {
                        Severity::Error => Finding::error(pattern.message.clone()),
                        Severity::Warning => Finding::warning(pattern.message.clone()),
                        Severity::Info => Finding::info(pattern.message.clone()),
                    }
                    .at_column(file.path.clone(), line_number, column)
                    .with_remediation(format!("matched forbidden pattern `{}`", pattern.name));
                    findings.push(finding);
                }
            }
        }
    }

    findings
}

fn compile_patterns(patterns: &[PatternConfig]) -> Result<Vec<CompiledPattern>, String> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for (index, pattern) in patterns.iter().enumerate() {
        let field_prefix = format!("patterns[{index}]");
        if pattern.name.trim().is_empty() {
            return Err(format!("`{field_prefix}.name` must not be empty"));
        }
        if pattern.message.trim().is_empty() {
            return Err(format!("`{field_prefix}.message` must not be empty"));
        }
        if pattern.pattern.trim().is_empty() {
            return Err(format!("`{field_prefix}.pattern` must not be empty"));
        }
        if pattern.pattern.contains('\n') {
            return Err(format!(
                "`{field_prefix}.pattern` is evaluated per line and must not span newlines"
            ));
        }

        let regex = Regex::new(&pattern.pattern)
            .map_err(|e| format!("invalid regex `{}` in `{field_prefix}.pattern`: {e}", pattern.pattern))?;

        let severity =
            parse_severity(pattern.severity.as_deref()).map_err(|e| format!("`{field_prefix}.severity` {e}"))?;

        compiled.push(CompiledPattern {
            name: pattern.name.clone(),
            message: pattern.message.clone(),
            severity,
            regex,
        });
    }
    Ok(compiled)
}

fn parse_severity(s: Option<&str>) -> Result<Severity, String> {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        None | Some("error") => Ok(Severity::Error),
        Some("warning") | Some("warn") => Ok(Severity::Warning),
        Some("info") => Ok(Severity::Info),
        Some(other) => Err(format!("must be one of `error`, `warning`, `info`; got `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile, CheckInput};
    use std::io::Write;

    fn write_temp_file(dir: &std::path::Path, name: &str, contents: &str) -> String {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn make_input(changed_files: Vec<ChangedFile>, config_json: &str) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files,
                file_diffs: vec![],
                commit_description: None,
                pr_description: None,
                change_id: None,
                repository: None,
                base_files: vec![],
            },
            config_json.to_owned(),
        )
    }

    #[test]
    fn flags_matching_line_in_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_file(dir.path(), "notes.md", "see task ZZ3124 for context\n");

        let findings = forbidden_pattern_check(make_input(
            vec![ChangedFile {
                path: path.clone(),
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"Internal work-item ids must not leak."}]}"#,
        ));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "Internal work-item ids must not leak.");
        assert_eq!(findings[0].severity, Severity::Error);
        let loc = findings[0].location.as_ref().unwrap();
        assert_eq!(loc.path, path);
        assert_eq!(loc.line, Some(1));
        assert_eq!(loc.column, Some(10));
    }

    #[test]
    fn does_not_flag_non_matching_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_file(dir.path(), "notes.md", "nothing forbidden here\n");

        let findings = forbidden_pattern_check(make_input(
            vec![ChangedFile {
                path,
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"Internal work-item ids must not leak."}]}"#,
        ));

        assert!(findings.is_empty());
    }

    #[test]
    fn skips_deleted_files() {
        let findings = forbidden_pattern_check(make_input(
            vec![ChangedFile {
                path: "/does/not/exist.md".to_owned(),
                kind: ChangeKind::Deleted,
                old_path: None,
            }],
            r#"{"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));

        assert!(findings.is_empty());
    }

    #[test]
    fn emits_one_finding_per_match_per_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_file(dir.path(), "notes.md", "ZZ1111 and ZZ2222 leaked in one line\n");

        let findings = forbidden_pattern_check(make_input(
            vec![ChangedFile {
                path,
                kind: ChangeKind::Added,
                old_path: None,
            }],
            r#"{"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));

        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn per_pattern_severity_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_file(dir.path(), "notes.md", "the operator said so\n");

        let findings = forbidden_pattern_check(make_input(
            vec![ChangedFile {
                path,
                kind: ChangeKind::Modified,
                old_path: None,
            }],
            r#"{"patterns":[{"name":"operator-phrase","pattern":"the operator","message":"avoid this phrase","severity":"warning"}]}"#,
        ));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn requires_at_least_one_pattern() {
        let findings = forbidden_pattern_check(make_input(vec![], r#"{}"#));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("patterns"));
    }

    #[test]
    fn rejects_empty_pattern_name() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"patterns":[{"name":"","pattern":"foo","message":"bar"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("name"));
    }

    #[test]
    fn rejects_empty_pattern_message() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"patterns":[{"name":"foo","pattern":"foo","message":""}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("message"));
    }

    #[test]
    fn rejects_invalid_regex() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"patterns":[{"name":"foo","pattern":"(","message":"bar"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .message
                .contains("invalid text/forbidden-pattern check config")
        );
    }

    #[test]
    fn severity_parsing_is_case_insensitive() {
        for severity_str in &["Warning", "WARNING", "warn", "WARN"] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_temp_file(dir.path(), "notes.md", "ZZ9999 leaked\n");

            let findings = forbidden_pattern_check(make_input(
                vec![ChangedFile {
                    path,
                    kind: ChangeKind::Modified,
                    old_path: None,
                }],
                &format!(
                    r#"{{"patterns":[{{"name":"work-item-id","pattern":"\\bZZ\\d{{4,}}\\b","message":"nope","severity":"{}"}}]}}"#,
                    severity_str
                ),
            ));
            assert_eq!(findings.len(), 1, "severity={}", severity_str);
            assert_eq!(findings[0].severity, Severity::Warning, "severity={}", severity_str);
        }
    }

    #[test]
    fn rejects_unknown_severity() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"patterns":[{"name":"foo","pattern":"bar","message":"baz","severity":"critical"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("severity"));
        assert!(findings[0].message.contains("critical"));
    }

    #[test]
    fn rejects_pattern_spanning_newline() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"patterns":[{"name":"foo","pattern":"foo\nbar","message":"baz"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("newline"));
    }
}
