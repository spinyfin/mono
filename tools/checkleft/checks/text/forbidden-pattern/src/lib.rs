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
//!   "surfaces": ["files", "changeset"],
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
//!
//! `surfaces` controls which text sources every configured pattern is
//! evaluated against. It is a list containing one or both of:
//!
//! * `"files"` — scan the content of changed, non-deleted files (the
//!   long-standing behaviour). This is the default when `surfaces` is
//!   omitted, so existing configs are unaffected.
//! * `"changeset"` — additionally scan the PR description and the
//!   joined-range commit-description string carried on
//!   [`checkleft_check_sdk::ChangeSet`]. Either, both, or neither may be
//!   present on a given run: `commit_description` is set whenever the VCS can
//!   supply one, while `pr_description` depends on the host having resolved
//!   the PR (it is reliably absent, for example, when running from a jj
//!   workspace with no `.git` at its root — see
//!   `tools/checkleft/docs/investigations/locationless-and-virtual-path-finding-rendering.md`).
//!   A check run must not treat an absent PR description as "clean"; it
//!   simply means that surface was not scanned this run.
//!
//! `surfaces` entries are additive — `["files", "changeset"]` scans both;
//! `["changeset"]` scans only the PR/commit text and skips file content
//! entirely. An unrecognized surfaces value is a config error.
//!
//! `surfaces` is deliberately named differently from the framework-level,
//! per-check `scope` key that CHECKS files use to control *scheduling*
//! (`files` | `changeset`, see `CheckScope` in `tools/checkleft/src/config.rs`
//! and `Runner::schedule_changeset_scope_runs` in
//! `tools/checkleft/src/runner.rs`). The two are unrelated and must not be
//! confused: the framework `scope` decides *whether this check runs at all*
//! for a given changeset, while this check's `surfaces` decides *which text*
//! the check reads once it does run. In particular, if this check's config
//! sets `"surfaces": ["changeset"]` (to scan PR/commit text) but the CHECKS
//! entry leaves the framework `scope` at its default of `files`, the check is
//! only scheduled when some changed file matches its file selector — on a
//! changeset that touches no matching file, the PR/commit text is never
//! scanned and the run reports no findings. A CHECKS entry that wants
//! changeset-text scanning to run unconditionally must set the framework
//! `scope: changeset` as well.
//!
//! Matches found in the PR description or commit description have no
//! changed-file location to point at, so they are reported as bare
//! locationless findings (`Finding::location == None`) rather than being
//! attached to a synthetic path. This is a deliberate choice, not an
//! oversight: `checkleft`'s changeset-scoping filter silently discards any
//! finding whose location path is not one of the run's changed files, so a
//! synthetic path (e.g. `<pr-description>`) would be dropped before it ever
//! reached an output surface. Locationless findings are the one shape the
//! framework explicitly preserves for a non-file subject; see the
//! investigation doc above for the full analysis. Each locationless finding
//! from this check also carries a `surface` (`Surface::PrDescription` /
//! `Surface::CommitMessage` from the check SDK, aliased here as
//! `FindingSurface` to avoid colliding with this module's own `Surface`
//! config enum) naming which changeset text it came from, so
//! human-readable output renders `<pr description>` / `<commit message>`
//! instead of `<unknown>`.

use checkleft_check_sdk::{ChangeKind, ChangeSet, CheckInput, Finding, Severity, Surface as FindingSurface, check};
use regex::Regex;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    patterns: Vec<PatternConfig>,
    #[serde(default)]
    surfaces: Option<Vec<String>>,
}

/// Which text surfaces the configured patterns are evaluated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Files,
    Changeset,
}

/// Where a changeset-scoped match was found, for use in the finding's
/// remediation text (the finding itself carries no location).
#[derive(Clone, Copy)]
enum ChangesetSource {
    PrDescription,
    CommitDescription,
}

impl ChangesetSource {
    fn label(self) -> &'static str {
        match self {
            ChangesetSource::PrDescription => "PR description",
            ChangesetSource::CommitDescription => "commit message",
        }
    }

    fn surface(self) -> FindingSurface {
        match self {
            ChangesetSource::PrDescription => FindingSurface::PrDescription,
            ChangesetSource::CommitDescription => FindingSurface::CommitMessage,
        }
    }
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

impl CompiledPattern {
    fn finding(&self) -> Finding {
        match self.severity {
            Severity::Error => Finding::error(self.message.clone()),
            Severity::Warning => Finding::warning(self.message.clone()),
            Severity::Info => Finding::info(self.message.clone()),
        }
    }
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

    let surfaces = match parse_surfaces(cfg.surfaces.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            return vec![Finding::error(format!(
                "invalid text/forbidden-pattern check config: {e}"
            ))];
        }
    };

    let mut findings = Vec::new();

    if surfaces.contains(&Surface::Files) {
        scan_files(&input.changeset, &patterns, &mut findings);
    }

    if surfaces.contains(&Surface::Changeset) {
        if let Some(pr_description) = &input.changeset.pr_description {
            scan_changeset_text(pr_description, ChangesetSource::PrDescription, &patterns, &mut findings);
        }
        if let Some(commit_description) = &input.changeset.commit_description {
            scan_changeset_text(
                commit_description,
                ChangesetSource::CommitDescription,
                &patterns,
                &mut findings,
            );
        }
    }

    findings
}

fn scan_files(changeset: &ChangeSet, patterns: &[CompiledPattern], findings: &mut Vec<Finding>) {
    for file in &changeset.changed_files {
        if file.kind == ChangeKind::Deleted {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&file.path) else {
            continue;
        };

        for (index, line) in content.lines().enumerate() {
            let line_number = (index + 1) as u32;
            for pattern in patterns {
                for m in pattern.regex.find_iter(line) {
                    let column = (line[..m.start()].chars().count() + 1) as u32;
                    let finding = pattern
                        .finding()
                        .at_column(file.path.clone(), line_number, column)
                        .with_remediation(format!("matched forbidden pattern `{}`", pattern.name));
                    findings.push(finding);
                }
            }
        }
    }
}

/// Scan a changeset-level text blob (PR description or joined commit
/// description) against every pattern, emitting bare locationless findings.
///
/// These findings intentionally carry no `Location`: `checkleft`'s
/// changeset-scoping filter drops any finding whose location path is not one
/// of the run's changed files, so a synthetic path would be silently
/// discarded before reaching any output surface. See the module docs for the
/// full rationale.
///
/// Like `scan_files`, this uses `find_iter` rather than `is_match`, so
/// multiple distinct matches of the same pattern on one line each produce
/// their own finding, and each finding's remediation names the matched text
/// (there is no column to report for a locationless finding, so naming the
/// match is what lets a reviewer tell two same-line matches apart).
fn scan_changeset_text(text: &str, source: ChangesetSource, patterns: &[CompiledPattern], findings: &mut Vec<Finding>) {
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        for pattern in patterns {
            for m in pattern.regex.find_iter(line) {
                let finding = pattern.finding().on_surface(source.surface()).with_remediation(format!(
                    "matched forbidden pattern `{}` in {} (line {line_number}): `{}`",
                    pattern.name,
                    source.label(),
                    m.as_str(),
                ));
                findings.push(finding);
            }
        }
    }
}

fn parse_surfaces(surfaces: Option<&[String]>) -> Result<Vec<Surface>, String> {
    let Some(surfaces) = surfaces else {
        return Ok(vec![Surface::Files]);
    };

    if surfaces.is_empty() {
        return Err("`surfaces` must not be empty when present".to_owned());
    }

    surfaces
        .iter()
        .map(|s| match s.as_str() {
            "files" => Ok(Surface::Files),
            "changeset" => Ok(Surface::Changeset),
            other => Err(format!(
                "`surfaces` entries must be one of `files`, `changeset`; got `{other}`"
            )),
        })
        .collect()
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

    fn make_changeset_input(
        pr_description: Option<&str>,
        commit_description: Option<&str>,
        config_json: &str,
    ) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files: vec![],
                file_diffs: vec![],
                commit_description: commit_description.map(str::to_owned),
                pr_description: pr_description.map(str::to_owned),
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

    // ── scope: changeset (PR description / commit description) ─────────────

    #[test]
    fn default_scope_does_not_scan_changeset_text() {
        let findings = forbidden_pattern_check(make_changeset_input(
            Some("see task ZZ3124 for context"),
            Some("see task ZZ3124 for context"),
            r#"{"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));
        assert!(findings.is_empty());
    }

    #[test]
    fn changeset_scope_flags_pr_description_with_locationless_finding() {
        let findings = forbidden_pattern_check(make_changeset_input(
            Some("this fixes ZZ3124"),
            None,
            r#"{"surfaces":["changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"Internal work-item ids must not leak."}]}"#,
        ));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "Internal work-item ids must not leak.");
        assert!(findings[0].location.is_none());
        assert_eq!(findings[0].surface, Some(FindingSurface::PrDescription));
        assert!(findings[0].remediations[0].contains("PR description"));
    }

    #[test]
    fn changeset_scope_flags_commit_description_with_locationless_finding() {
        let findings = forbidden_pattern_check(make_changeset_input(
            None,
            Some("fix stuff\n\nsee ZZ4242 for details"),
            r#"{"surfaces":["changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));

        assert_eq!(findings.len(), 1);
        assert!(findings[0].location.is_none());
        assert_eq!(findings[0].surface, Some(FindingSurface::CommitMessage));
        assert!(findings[0].remediations[0].contains("commit message"));
        assert!(findings[0].remediations[0].contains("line 3"));
    }

    #[test]
    fn changeset_scope_with_absent_pr_description_scans_commit_description_only() {
        let findings = forbidden_pattern_check(make_changeset_input(
            None,
            Some("see ZZ5555"),
            r#"{"surfaces":["changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));

        assert_eq!(findings.len(), 1);
        assert!(findings[0].location.is_none());
    }

    #[test]
    fn changeset_scope_with_no_descriptions_present_yields_no_findings() {
        let findings = forbidden_pattern_check(make_changeset_input(
            None,
            None,
            r#"{"surfaces":["changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));
        assert!(findings.is_empty());
    }

    #[test]
    fn surfaces_files_and_changeset_scans_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_file(dir.path(), "notes.md", "see task ZZ3124 for context\n");

        let mut input = make_changeset_input(
            Some("also ZZ9999 here"),
            None,
            r#"{"surfaces":["files","changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        );
        input.changeset.changed_files.push(ChangedFile {
            path: path.clone(),
            kind: ChangeKind::Modified,
            old_path: None,
        });

        let findings = forbidden_pattern_check(input);

        assert_eq!(findings.len(), 2);
        let file_finding = findings.iter().find(|f| f.location.is_some()).unwrap();
        assert_eq!(file_finding.location.as_ref().unwrap().path, path);
        assert!(findings.iter().any(|f| f.location.is_none()));
    }

    #[test]
    fn rejects_empty_surfaces_list() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"surfaces":[],"patterns":[{"name":"foo","pattern":"bar","message":"baz"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("surfaces"));
    }

    #[test]
    fn rejects_unknown_surfaces_value() {
        let findings = forbidden_pattern_check(make_input(
            vec![],
            r#"{"surfaces":["everywhere"],"patterns":[{"name":"foo","pattern":"bar","message":"baz"}]}"#,
        ));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("surfaces"));
        assert!(findings[0].message.contains("everywhere"));
    }

    #[test]
    fn changeset_scope_flags_each_match_on_a_line_separately() {
        let findings = forbidden_pattern_check(make_changeset_input(
            Some("leaked ZZ1111 and ZZ2222 on one line"),
            None,
            r#"{"surfaces":["changeset"],"patterns":[{"name":"work-item-id","pattern":"\\bZZ\\d{4,}\\b","message":"nope"}]}"#,
        ));

        assert_eq!(findings.len(), 2);
        assert!(findings[0].remediations[0].contains("ZZ1111"));
        assert!(findings[1].remediations[0].contains("ZZ2222"));
    }
}
