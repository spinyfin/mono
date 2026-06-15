//! Checkleft check: flag changed lines that match forbidden regex patterns.
//!
//! This is the Component Model wasm port of the former built-in
//! `forbidden-imports-deps` check, registered under the canonical id
//! `file/forbidden-patterns`. It runs inside the checkleft wasm host and reads
//! files via the WASI filesystem sandbox.
//!
//! ## What the check detects
//!
//! A *generic*, line-by-line regex scanner scoped to path globs. The original
//! name (`forbidden-imports-deps`) encoded one use case — import/dependency
//! enforcement — but the implementation has no knowledge of import syntax: it
//! matches any regex against any line of any text file. `file/forbidden-patterns`
//! names the mechanism, not a use case, so authors find it instead of writing
//! another single-purpose regex check.
//!
//! For each changed (non-deleted) file, every line is scanned against every
//! configured rule whose `include_globs`/`exclude_globs` path filters select the
//! file. A finding is emitted per matching line, carrying the rule's `message`,
//! `severity`, and `remediation`.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "rules": [
//!     {
//!       "pattern": "\\bfetch\\(url\\(",
//!       "message": "Use frontend api/* modules for backend calls.",
//!       "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
//!       "exclude_globs": ["frontend/src/api/**"],
//!       "severity": "error",
//!       "remediation": "Import from frontend/src/api/ instead."
//!     }
//!   ],
//!   "severity": "error",
//!   "remediation": "Default remediation applied to rules that omit their own."
//! }
//! ```
//!
//! ### Instance-per-policy convention
//!
//! `file/forbidden-patterns` follows checkleft's instance-per-policy idiom: each
//! logical prohibition is its own `- id:` entry in the CHECKS file, so findings,
//! bypasses, and severity are keyed to that policy. Rules listed under a single
//! `- id:` are sub-clauses of the *same* prohibition; distinct prohibitions —
//! different owners, bypass lifecycles, or remediation — each get their own
//! `- id:` pointing at `check: file/forbidden-patterns`.
//!
//! ### Glob coordinates
//!
//! `include_globs` and `exclude_globs` (alias `exclude_files`) are matched
//! against repo-root-relative paths. The host rewrites top-level
//! `exclude_files`/`exclude_globs` keys in the config table but does NOT recurse
//! into the `rules[]` array, so all rule-level globs reach this check verbatim.
//! They are matched repo-root-relative with no `config_dir` prefix applied.
//! Authors writing a repo-local (subdirectory) `CHECKS` file must express
//! rule-level globs as repo-root-relative paths, not config-dir-relative paths.

use checkleft_check_sdk::{ChangeKind, CheckInput, Finding, Severity, check};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

const DEFAULT_REMEDIATION: &str = "Remove or replace the forbidden pattern with an approved alternative.";

#[derive(Debug, Deserialize, Default)]
struct Config {
    #[serde(default)]
    rules: Vec<RuleConfig>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuleConfig {
    pattern: String,
    message: String,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default, alias = "exclude_globs")]
    exclude_files: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledRule {
    pattern: regex::Regex,
    include_globs: Option<GlobSet>,
    exclude_files: Option<GlobSet>,
    message: String,
    remediation: String,
    severity: Severity,
}

impl CompiledRule {
    /// Whether this rule applies to `path` (repo-root-relative). An excluded
    /// path never applies; otherwise it applies when there are no `include_globs`
    /// or the path matches one. Mirrors the native check exactly.
    fn applies_to(&self, path: &str) -> bool {
        if self.exclude_files.as_ref().is_some_and(|globs| globs.is_match(path)) {
            return false;
        }
        match &self.include_globs {
            Some(globs) => globs.is_match(path),
            None => true,
        }
    }
}

#[check(
    name = "file/forbidden-patterns",
    description = "flags changed lines matching forbidden regex patterns, scoped to path globs",
    severity = error
)]
pub fn forbidden_patterns_check(input: CheckInput) -> Vec<Finding> {
    // Fail loudly on a malformed config: silently falling back to an empty rule
    // list would let forbidden content slip through undetected.  A well-formed
    // absent/empty config parses successfully (all Config fields have serde
    // defaults), so this only panics for genuinely invalid JSON or wrong types.
    let cfg: Config = input
        .config()
        .unwrap_or_else(|err| panic!("invalid file/forbidden-patterns config: {err}"));
    if cfg.rules.is_empty() {
        panic!("file/forbidden-patterns config must contain at least one rule");
    }
    let rules = compile_rules(&cfg);

    let mut findings = Vec::new();

    for file in &input.changeset.changed_files {
        if file.kind == ChangeKind::Deleted {
            continue;
        }

        // Pre-select the rules that apply to this file (a per-file, line-invariant
        // test). Iterating them in order inside the line loop preserves the native
        // check's line-major, rule-order finding sequence.
        let applicable: Vec<&CompiledRule> = rules.iter().filter(|rule| rule.applies_to(&file.path)).collect();
        if applicable.is_empty() {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file.path) else {
            continue;
        };

        for (line_index, line) in contents.lines().enumerate() {
            for rule in &applicable {
                if !rule.pattern.is_match(line) {
                    continue;
                }
                findings.push(finding_for(
                    rule.severity,
                    rule.message.clone(),
                    &file.path,
                    (line_index + 1) as u32,
                    rule.remediation.clone(),
                ));
            }
        }
    }

    findings
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check into
// the single multiplexed preinstalled component. That dedups the shared wasm
// runtime baseline (std/alloc/SDK/wit-bindgen/serde) across the preinstalled
// checks instead of duplicating it per component.

/// Compile the configured rules. Invalid regexes are a hard authoring error —
/// the native check bailed at configure time; a silently-skipped rule would let
/// forbidden content slip through, so we panic (surfaced by the host as a check
/// failure) rather than degrade.
fn compile_rules(cfg: &Config) -> Vec<CompiledRule> {
    let default_severity = parse_severity(cfg.severity.as_deref(), Severity::Error);
    let default_remediation = cfg
        .remediation
        .clone()
        .unwrap_or_else(|| DEFAULT_REMEDIATION.to_owned());

    cfg.rules
        .iter()
        .map(|rule| {
            let pattern = regex::Regex::new(&rule.pattern)
                .unwrap_or_else(|err| panic!("invalid file/forbidden-patterns rule regex `{}`: {err}", rule.pattern));
            CompiledRule {
                pattern,
                include_globs: build_globset(&rule.include_globs),
                exclude_files: build_globset(&rule.exclude_files),
                message: rule.message.clone(),
                remediation: rule.remediation.clone().unwrap_or_else(|| default_remediation.clone()),
                severity: parse_severity(rule.severity.as_deref(), default_severity),
            }
        })
        .collect()
}

fn finding_for(severity: Severity, message: String, path: &str, line: u32, remediation: String) -> Finding {
    let finding = match severity {
        Severity::Error => Finding::error(message),
        Severity::Warning => Finding::warning(message),
        Severity::Info => Finding::info(message),
    };
    // Column 1, matching the native check's location.
    finding.at_column(path, line, 1).with_remediation(remediation)
}

/// Parse a severity string, falling back to `default`. Mirrors the host-side
/// `Severity::parse_with_default` (case-insensitive; unknown → default).
fn parse_severity(raw: Option<&str>, default: Severity) -> Severity {
    match raw.unwrap_or("").to_ascii_lowercase().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        _ => default,
    }
}

fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).unwrap_or_else(|err| panic!("invalid file/forbidden-patterns glob `{pattern}`: {err}"));
        builder.add(glob);
    }
    builder
        .build()
        .unwrap_or_else(|err| panic!("failed to compile file/forbidden-patterns globs: {err}"))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeSet, ChangedFile};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Serialize CWD changes so parallel tests don't interfere.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn make_changeset(path: &str, kind: ChangeKind) -> ChangeSet {
        ChangeSet {
            changed_files: vec![ChangedFile {
                path: path.to_owned(),
                kind,
                old_path: None,
            }],
            file_diffs: vec![],
            commit_description: None,
            pr_description: None,
            change_id: None,
            repository: None,
            base_files: vec![],
        }
    }

    /// Run the check against a file written into a temp dir, with the given config.
    fn run(path: &str, kind: ChangeKind, contents: &str, config_json: &str) -> Vec<Finding> {
        // Recover from a poisoned lock: a #[should_panic] test panics while
        // holding the lock, which poisons it.  The CWD is restored by the
        // panicking test's cleanup or left in an indeterminate state, but
        // subsequent tests must still be able to acquire the guard.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        if let Some(parent) = std::path::Path::new(path).parent() {
            fs::create_dir_all(dir.path().join(parent)).unwrap();
        }
        fs::write(dir.path().join(path), contents).unwrap();

        let input = CheckInput::__from_parts(make_changeset(path, kind), config_json.to_owned());
        let findings = forbidden_patterns_check(input);

        std::env::set_current_dir(old_cwd).unwrap();
        findings
    }

    #[test]
    fn flags_forbidden_pattern_in_included_file() {
        let findings = run(
            "frontend/src/components/Foo.tsx",
            ChangeKind::Modified,
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
            r#"{"rules": [{
                "pattern": "\\bfetch\\(url\\(",
                "message": "Use frontend api/* modules for backend calls.",
                "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                "exclude_globs": ["frontend/src/api/**"]
            }]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error, "default severity is error");
        let loc = findings[0].location.as_ref().expect("finding has a location");
        assert_eq!(loc.line, Some(1));
        assert_eq!(loc.column, Some(1));
    }

    #[test]
    fn ignores_excluded_paths() {
        let findings = run(
            "frontend/src/api/http.ts",
            ChangeKind::Modified,
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
            r#"{"rules": [{
                "pattern": "\\bfetch\\(url\\(",
                "message": "Use frontend api/* modules for backend calls.",
                "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                "exclude_files": ["frontend/src/api/**"]
            }]}"#,
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn exclude_globs_alias_still_works() {
        let findings = run(
            "frontend/src/api/http.ts",
            ChangeKind::Modified,
            "const x = fetch(url(\"/api/v2/statusz\"));\n",
            r#"{"rules": [{
                "pattern": "\\bfetch\\(url\\(",
                "message": "Use frontend api/* modules for backend calls.",
                "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
                "exclude_globs": ["frontend/src/api/**"]
            }]}"#,
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn ignores_files_not_matching_include_globs() {
        let findings = run(
            "backend/src/main.rs",
            ChangeKind::Modified,
            "let x = fetch(url(\"/api/v2/statusz\"));\n",
            r#"{"rules": [{
                "pattern": "\\bfetch\\(url\\(",
                "message": "frontend only",
                "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"]
            }]}"#,
        );
        assert!(findings.is_empty(), "include_globs restricts to frontend sources");
    }

    #[test]
    fn deleted_files_are_never_flagged() {
        let _guard = CWD_LOCK.lock().unwrap();
        // No file on disk: a deleted file must be skipped before any read.
        let input = CheckInput::__from_parts(
            make_changeset("frontend/src/gone.ts", ChangeKind::Deleted),
            r#"{"rules": [{"pattern": "anything", "message": "m", "include_globs": ["frontend/src/**"]}]}"#.to_owned(),
        );
        assert!(forbidden_patterns_check(input).is_empty());
    }

    /// Call the check with config only — no file on disk needed because the
    /// panic occurs before any file I/O (during config parsing or rule
    /// validation).  Bypasses the CWD-mutating `run()` helper so that these
    /// `#[should_panic]` tests cannot poison CWD_LOCK for sibling tests.
    fn check_with_config(config_json: &str) {
        let input = CheckInput::__from_parts(make_changeset("a.ts", ChangeKind::Modified), config_json.to_owned());
        forbidden_patterns_check(input);
    }

    #[test]
    #[should_panic(expected = "must contain at least one rule")]
    fn empty_rules_panic() {
        check_with_config("{}");
    }

    #[test]
    #[should_panic(expected = "invalid file/forbidden-patterns config")]
    fn malformed_config_panics() {
        // A rule missing the required `pattern` field is a config error; the
        // check must fail loudly rather than silently enforcing nothing.
        check_with_config(r#"{"rules": [{"message": "m"}]}"#);
    }

    #[test]
    #[should_panic(expected = "invalid file/forbidden-patterns config")]
    fn invalid_json_config_panics() {
        check_with_config("not json at all");
    }

    #[test]
    fn per_rule_severity_overrides_default() {
        let findings = run(
            "a.ts",
            ChangeKind::Added,
            "DANGER\n",
            r#"{"severity": "info", "rules": [
                {"pattern": "DANGER", "message": "default-sev"},
                {"pattern": "DANGER", "message": "warn-sev", "severity": "warning"}
            ]}"#,
        );
        assert_eq!(findings.len(), 2);
        // Line-major, rule-order: first rule uses the top-level default (info),
        // second overrides to warning.
        assert_eq!(findings[0].severity, Severity::Info, "top-level default applies");
        assert_eq!(findings[1].severity, Severity::Warning, "per-rule override applies");
    }

    #[test]
    fn custom_remediation_is_attached() {
        let findings = run(
            "a.ts",
            ChangeKind::Added,
            "BAD\n",
            r#"{"rules": [{"pattern": "BAD", "message": "m", "remediation": "do the right thing"}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].remediations, vec!["do the right thing".to_owned()]);
    }

    #[test]
    fn default_remediation_applies_when_unset() {
        let findings = run(
            "a.ts",
            ChangeKind::Added,
            "BAD\n",
            r#"{"rules": [{"pattern": "BAD", "message": "m"}]}"#,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].remediations, vec![DEFAULT_REMEDIATION.to_owned()]);
    }

    /// Proves the former bundled `frontend-no-legacy-api` check is fully
    /// expressible as config of this generic check: an ES import statement from
    /// any path ending in a legacy module name, scoped to TS/TSX under
    /// `frontend/src/`.
    #[test]
    fn subsumes_frontend_no_legacy_api() {
        let config = r#"{"rules": [{
            "pattern": "^\\s*import\\b[^;]*\\bfrom\\s+[\"'][^\"']*api/fencingtracker[\"']",
            "message": "import from deprecated frontend API module api/fencingtracker",
            "severity": "error",
            "include_globs": ["frontend/src/**/*.ts", "frontend/src/**/*.tsx"],
            "remediation": "Use supported frontend API modules under frontend/src/api/."
        }]}"#;

        // Relative import (../api/fencingtracker) is flagged.
        let flagged = run(
            "frontend/src/components/Foo.tsx",
            ChangeKind::Modified,
            "import { x } from \"../api/fencingtracker\";\n",
            config,
        );
        assert_eq!(flagged.len(), 1, "legacy relative import must be flagged");
        assert_eq!(flagged[0].severity, Severity::Error);

        // A supported module import is left alone.
        let ok = run(
            "frontend/src/components/Foo.tsx",
            ChangeKind::Modified,
            "import { getStatusz } from \"../api/statusz\";\n",
            config,
        );
        assert!(ok.is_empty(), "non-legacy import must not be flagged");

        // The same legacy import outside frontend/src is not scanned.
        let outside = run(
            "backend/src/foo.ts",
            ChangeKind::Modified,
            "import { x } from \"../api/fencingtracker\";\n",
            config,
        );
        assert!(outside.is_empty(), "include_globs scopes to frontend/src");
    }
}
