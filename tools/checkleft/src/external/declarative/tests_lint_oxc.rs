//! Tests for the lint/oxc declarative check (oxlint via npm/npx binding).

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode, ToolInvocation};

const LINT_OXC_MANIFEST: &str = include_str!("../../../checks/lint/oxc.yaml");

fn parse_lint_oxc_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(LINT_OXC_MANIFEST).expect("lint/oxc manifest must parse");
    assert_eq!(package.id, "lint/oxc");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

fn tool(invocation: &Invocation) -> &ToolInvocation {
    match &invocation.kind {
        InvocationKind::Tool(tool) => tool,
        other => panic!("expected tool invocation, got {other:?}"),
    }
}

// Real oxlint 1.70.0 `oxlint --format=json` output: two warning-level diagnostics
// (the second, no-dupe-keys, carries TWO labels — the primary span and a secondary
// "first defined here" note) plus one error-level diagnostic that, like a parse
// error, carries NO `code` field. Exercises: warning vs error severity mapping,
// the `(.code // "oxlint")` fallback, and that only the PRIMARY label is used so
// a multi-label diagnostic still yields exactly one finding.
const OXLINT_JSON: &[u8] = br#"{ "diagnostics": [{"message": "`debugger` statement is not allowed","code": "eslint(no-debugger)","severity": "warning","causes": [],"url": "https://oxc.rs/docs/guide/usage/linter/rules/eslint/no-debugger.html","help": "Remove the debugger statement","filename": "bad.ts","labels": [{"span": {"offset": 0,"length": 9,"line": 1,"column": 1}}],"related": []},
{"message": "Duplicate key 'a'","code": "eslint(no-dupe-keys)","severity": "warning","causes": [],"filename": "bad.ts","labels": [{"label": "Key is first defined here","span": {"offset": 24,"length": 1,"line": 2,"column": 15}},{"label": "and duplicated here","span": {"offset": 30,"length": 1,"line": 2,"column": 21}}],"related": []},
{"message": "Unexpected token","severity": "error","causes": [],"filename": "bad.ts","labels": [{"span": {"offset": 40,"length": 1,"line": 3,"column": 11}}],"related": []}],
              "number_of_files": 1,
              "number_of_rules": 95,
              "threads_count": 12,
              "start_time": 0.007393833
            }"#;

// Real oxlint 1.70.0 output for a clean file: empty diagnostics plus the summary.
const OXLINT_CLEAN_JSON: &[u8] = br#"{ "diagnostics": [],
              "number_of_files": 1,
              "number_of_rules": 95,
              "threads_count": 12,
              "start_time": 0.005884833
            }"#;

// Synthetic fixtures (faithful to the schema) exercising branches the sample above
// does not: an unknown severity string mapping to `info`, and a diagnostic with an
// EMPTY `labels` array so the `(.labels[0].span.line // 1)` guard kicks in.
const OXLINT_ADVICE_JSON: &[u8] = br#"{"diagnostics":[{"message":"prefer a template literal","code":"eslint(prefer-template)","severity":"advice","filename":"x.ts","labels":[{"span":{"offset":3,"length":2,"line":5,"column":7}}],"related":[]}]}"#;
const OXLINT_NO_LABELS_JSON: &[u8] = br#"{"diagnostics":[{"message":"project-level diagnostic","code":"oxc(some-rule)","severity":"warning","filename":"y.ts","labels":[],"related":[]}]}"#;

#[test]
fn lint_oxc_manifest_parses_correctly() {
    let package = parse_lint_oxc_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "lint");
    // batch: oxlint is fast and accepts the whole file set in one process.
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    // oxlint: 0 = no error-level diagnostics (clean or warning/info only) → findings;
    // 1 = at least one error → findings; default = error so a crash never reads as clean.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn lint_oxc_args_use_json_format_and_no_error_on_unmatched() {
    let package = parse_lint_oxc_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--format=json"),
        "lint/oxc must request the json reporter; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--no-error-on-unmatched-pattern"),
        "lint/oxc must pass --no-error-on-unmatched-pattern so an all-ignored set is a no-op, not a parse error; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{files}}"),
        "lint/oxc must pass the matched file set; got: {args:?}"
    );
    // lint/oxc is zero-config: unlike lint/js it must NOT require a config_file.
    assert!(
        !args.iter().any(|a| a.contains("{{config.")),
        "lint/oxc must not require a config key (oxlint ships default rules); got: {args:?}"
    );
    // Lint-CHECK mode only: must NOT mutate files.
    assert!(
        !args
            .iter()
            .any(|a| a == "--fix" || a == "--fix-suggestions" || a == "--fix-dangerously"),
        "lint/oxc check must not write/fix files; got: {args:?}"
    );
}

#[test]
fn lint_oxc_needs_npm_default_pinned_to_1_70_0_with_path_fallback() {
    let package = parse_lint_oxc_package();
    let req = package.needs.get("oxlint").expect("oxlint binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "oxlint");
            assert_eq!(version, "1.70.0", "default oxlint version must be 1.70.0");
        }
        other => panic!("default binding must be an npm version-pinned binding; got: {other:?}"),
    }
    assert!(
        matches!(req.fallback, Some(super::BinaryBinding::Path(_))),
        "fallback binding must be a PATH binary for non-npx environments; got: {:?}",
        req.fallback
    );
}

#[test]
fn lint_oxc_applies_to_covers_js_ts_and_framework_components() {
    use globset::{Glob, GlobSetBuilder};

    let package = parse_lint_oxc_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    // JS/TS family + framework single-file components whose <script> oxlint lints.
    for path in [
        "a.js", "b.jsx", "c.mjs", "d.cjs", "e.ts", "f.tsx", "g.mts", "h.cts", "i.vue", "j.svelte", "k.astro",
    ] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by lint/oxc applies_to"
        );
    }
    // oxlint is JS/TS only — no markdown/css/json/yaml linting.
    for path in ["x.rs", "y.py", "z.json", "w.css", "v.md"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by lint/oxc applies_to"
        );
    }
}

#[test]
fn lint_oxc_npm_default_resolves_to_npx_with_pinned_version() {
    let package = parse_lint_oxc_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/bin/npx")),
    )
    .expect("resolve");
    let oxlint = resolved.get("oxlint").expect("oxlint resolved");
    assert_eq!(oxlint.program, Path::new("/fake/bin/npx"));
    assert_eq!(
        oxlint.prefix_args,
        vec!["--yes".to_owned(), "oxlint@1.70.0".to_owned()],
        "default pin must produce `npx --yes oxlint@1.70.0`"
    );
    assert_eq!(oxlint.display_invocation, "npx --yes oxlint@1.70.0");
}

#[test]
fn lint_oxc_npm_version_override_repins() {
    let package = parse_lint_oxc_package();
    let config: toml::Value = toml::from_str("[needs.oxlint.npm]\nversion = \"1.71.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["oxlint"].prefix_args,
        vec!["--yes".to_owned(), "oxlint@1.71.0".to_owned()],
        "version override must re-pin to 1.71.0 while inheriting the package name"
    );
    assert_eq!(resolved["oxlint"].display_invocation, "npx --yes oxlint@1.71.0");
}

fn lint_oxc_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_lint_oxc_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(1), None, None)
        .expect("lint/oxc transform")
}

#[test]
fn lint_oxc_transform_maps_real_diagnostics() {
    let findings = lint_oxc_findings(OXLINT_JSON);
    assert_eq!(
        findings.len(),
        3,
        "three diagnostics expected (one finding each); got: {findings:?}"
    );

    // The parse-error diagnostic → error at 1-based line 3, column 11, with the
    // `oxlint:` code fallback since it carries no `code` field.
    let err: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Error).collect();
    assert_eq!(err.len(), 1, "one error finding expected; got: {findings:?}");
    let loc = err[0].location.as_ref().expect("location");
    assert_eq!(loc.path, Path::new("bad.ts"));
    assert_eq!(loc.line, Some(3));
    assert_eq!(loc.column, Some(11));
    assert!(
        err[0].message.starts_with("oxlint: "),
        "a code-less diagnostic must fall back to the `oxlint:` prefix; got: {}",
        err[0].message
    );
    assert!(
        err[0].message.contains("Unexpected token"),
        "message must include the diagnostic text; got: {}",
        err[0].message
    );

    // The two warnings carry their rule code in the message.
    let warns: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Warning).collect();
    assert_eq!(warns.len(), 2, "two warning findings expected; got: {findings:?}");
    assert!(
        warns.iter().any(|f| f.message.contains("eslint(no-debugger)")),
        "a warning must carry the no-debugger rule code; got: {warns:?}"
    );

    // no-dupe-keys carries two labels but must yield exactly ONE finding located at
    // the PRIMARY (first) label — line 2, column 15 — not the secondary at col 21.
    let dupe = findings
        .iter()
        .find(|f| f.message.contains("no-dupe-keys"))
        .expect("no-dupe-keys finding");
    let dupe_loc = dupe.location.as_ref().expect("location");
    assert_eq!(dupe_loc.line, Some(2));
    assert_eq!(
        dupe_loc.column,
        Some(15),
        "must use the primary label's span, not a secondary note"
    );

    // Every finding carries the suppression remediation.
    assert!(
        findings
            .iter()
            .all(|f| f.remediations.iter().any(|r| r.contains("oxlint-disable-next-line"))),
        "every finding must include the oxlint-disable remediation; got: {findings:?}"
    );
}

#[test]
fn lint_oxc_transform_maps_unknown_severity_to_info() {
    let findings = lint_oxc_findings(OXLINT_ADVICE_JSON);
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].severity,
        Severity::Info,
        "an oxlint severity outside error/warning must map to Info"
    );
}

#[test]
fn lint_oxc_transform_label_less_diagnostic_falls_back_to_line_one() {
    // A diagnostic with no labelled span must not render a null line/column — the
    // `(.labels[0].span.line // 1)` guard defaults it to 1 so the template parses.
    let findings = lint_oxc_findings(OXLINT_NO_LABELS_JSON);
    assert_eq!(findings.len(), 1);
    let loc = findings[0].location.as_ref().expect("location");
    assert_eq!(loc.path, Path::new("y.ts"));
    assert_eq!(loc.line, Some(1));
    assert_eq!(loc.column, Some(1));
}

#[test]
fn lint_oxc_transform_no_findings_on_clean_file() {
    let findings = lint_oxc_findings(OXLINT_CLEAN_JSON);
    assert!(
        findings.is_empty(),
        "clean output must produce no findings; got: {findings:?}"
    );
}

#[test]
fn lint_oxc_transform_uses_dynamic_severity() {
    let package = parse_lint_oxc_package();
    assert!(
        package.invocations[0].transform.uses_dynamic_severity(),
        "lint/oxc must preserve oxlint's per-finding severity end-to-end"
    );
}

// ── full executor pipeline against a fake oxlint on PATH (zero config required) ──

fn lint_oxc_manifest_with_path_default(script: &Path) -> String {
    let replaced = LINT_OXC_MANIFEST.replace(
        "needs:\n  oxlint:\n    default:\n      npm:\n        package: \"oxlint\"\n        version: \"1.70.0\"\n    fallback:\n      path: \"oxlint\"",
        &format!("needs:\n  oxlint:\n    default:\n      path: \"{}\"", script.display()),
    );
    assert_ne!(
        replaced, LINT_OXC_MANIFEST,
        "path-default replacement must change the manifest"
    );
    replaced
}

#[cfg(unix)]
fn write_executable_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(unix)]
#[test]
fn lint_oxc_end_to_end_requires_no_config() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_oxlint.sh");
    // Fake oxlint emitting one warning diagnostic and exiting 0 (oxlint exits 0
    // when there are no error-level diagnostics).
    write_executable_script(
        &script_path,
        r#"#!/bin/sh
printf '{"diagnostics":[{"message":"`debugger` statement is not allowed","code":"eslint(no-debugger)","severity":"warning","filename":"src/x.ts","labels":[{"span":{"offset":0,"length":9,"line":2,"column":1}}],"related":[]}],"number_of_files":1}'
exit 0
"#,
    );

    let manifest = lint_oxc_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/x.ts"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    // Empty config — no config_file key, no needs override. lint/oxc must work.
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "lint/oxc", &declarative, &changeset, &config, None)
        .expect("run succeeds with no config");

    assert_eq!(
        result.findings.len(),
        1,
        "expected one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.severity, Severity::Warning);
    let loc = f.location.as_ref().expect("location");
    assert_eq!(loc.path, Path::new("src/x.ts"));
    assert_eq!(loc.line, Some(2));
    assert_eq!(loc.column, Some(1));
    assert!(
        f.message.contains("eslint(no-debugger)"),
        "message must carry the rule code; got: {}",
        f.message
    );
}

#[cfg(unix)]
#[test]
fn lint_oxc_end_to_end_clean_file_produces_no_findings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_oxlint.sh");
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '{\"diagnostics\":[],\"number_of_files\":1}'\nexit 0\n",
    );

    let manifest = lint_oxc_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/x.ts"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "lint/oxc", &declarative, &changeset, &config, None)
        .expect("run succeeds");
    assert!(
        result.findings.is_empty(),
        "clean file must produce no findings; got: {:#?}",
        result.findings
    );
}
