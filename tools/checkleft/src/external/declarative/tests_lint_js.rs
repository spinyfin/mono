//! Tests for the lint/js declarative check (ESLint via npm/npx binding).

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode, ToolInvocation};

const LINT_JS_MANIFEST: &str = include_str!("../../../checks/lint/js.yaml");

fn parse_lint_js_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(LINT_JS_MANIFEST).expect("lint/js manifest must parse");
    assert_eq!(package.id, "lint/js");
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

#[test]
fn lint_js_manifest_parses_correctly() {
    let package = parse_lint_js_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "lint");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    // ESLint: 0 = no errors (findings), 1 = errors (findings), default = error.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn lint_js_needs_npm_default_pinned_to_10_5_0_with_path_fallback() {
    let package = parse_lint_js_package();
    let req = package.needs.get("eslint").expect("eslint binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "eslint");
            assert_eq!(version, "10.5.0", "default ESLint version must be 10.5.0");
        }
        other => panic!("default binding must be an npm version-pinned binding; got: {other:?}"),
    }
    assert!(
        matches!(req.fallback, Some(super::BinaryBinding::Path(_))),
        "fallback binding must be a PATH binary for environments without npx; got: {:?}",
        req.fallback
    );
}

#[test]
fn lint_js_applies_to_covers_js_ts_variants() {
    use globset::{Glob, GlobSetBuilder};

    let package = parse_lint_js_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    for path in ["a.js", "b.jsx", "c.mjs", "d.cjs", "e.ts", "f.tsx", "g.mts", "h.cts"] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by lint/js applies_to"
        );
    }

    for path in ["x.rs", "y.py", "z.json", "w.css"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by lint/js applies_to"
        );
    }
}

#[test]
fn lint_js_args_include_no_config_lookup_and_config_ref() {
    let package = parse_lint_js_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--no-config-lookup"),
        "lint/js must pass --no-config-lookup to prevent config auto-discovery; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{config.config_file}}"),
        "lint/js must pass {{{{config.config_file}}}} so the required config is always specified; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--format" || a == "-f"),
        "lint/js must pass --format / -f for JSON output; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "json"),
        "lint/js must specify json as the output format; got: {args:?}"
    );
}

#[test]
fn lint_js_npm_default_resolves_to_npx_with_pinned_version() {
    let package = parse_lint_js_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/bin/npx")),
    )
    .expect("resolve");
    let eslint = resolved.get("eslint").expect("eslint resolved");
    assert_eq!(eslint.program, Path::new("/fake/bin/npx"));
    assert_eq!(
        eslint.prefix_args,
        vec!["--yes".to_owned(), "eslint@10.5.0".to_owned()],
        "default pin must produce `npx --yes eslint@10.5.0`"
    );
}

#[test]
fn lint_js_npm_version_override_repins() {
    let package = parse_lint_js_package();
    let config: toml::Value = toml::from_str("[needs.eslint.npm]\nversion = \"10.6.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["eslint"].prefix_args,
        vec!["--yes".to_owned(), "eslint@10.6.0".to_owned()],
        "version override must re-pin to 10.6.0 while inheriting the package name"
    );
}

// Real ESLint JSON output (eslint --format json) for a file with one error and one warning.
const ESLINT_JSON_VIOLATIONS: &[u8] = br#"[
  {
    "filePath": "src/app.js",
    "messages": [
      {
        "ruleId": "no-unused-vars",
        "severity": 2,
        "message": "'x' is defined but never used.",
        "line": 1,
        "column": 7,
        "endLine": 1,
        "endColumn": 8,
        "nodeType": "Identifier"
      },
      {
        "ruleId": "no-console",
        "severity": 1,
        "message": "Unexpected console statement.",
        "line": 3,
        "column": 1,
        "endLine": 3,
        "endColumn": 14,
        "nodeType": "MemberExpression"
      }
    ],
    "errorCount": 1,
    "warningCount": 1
  }
]"#;

// ESLint JSON for a clean file (no messages).
const ESLINT_JSON_CLEAN: &[u8] = br#"[
  {
    "filePath": "src/clean.js",
    "messages": [],
    "errorCount": 0,
    "warningCount": 0
  }
]"#;

// ESLint JSON for a parse error (ruleId is null).
const ESLINT_JSON_PARSE_ERROR: &[u8] = br#"[
  {
    "filePath": "src/broken.js",
    "messages": [
      {
        "ruleId": null,
        "severity": 2,
        "message": "Parsing error: Unexpected token",
        "line": 5,
        "column": 1,
        "nodeType": null
      }
    ],
    "errorCount": 1,
    "warningCount": 0
  }
]"#;

fn lint_js_json_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_lint_js_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(1), None, None)
        .expect("lint/js transform")
}

#[test]
fn lint_js_transform_maps_error_severity_to_error() {
    let findings = lint_js_json_findings(ESLINT_JSON_VIOLATIONS);
    let error_findings: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Error).collect();
    assert_eq!(error_findings.len(), 1, "one error finding expected; got: {findings:?}");
    let f = error_findings[0];
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/app.js"));
    assert_eq!(loc.line, Some(1));
    assert_eq!(loc.column, Some(7));
    assert!(
        f.message.contains("no-unused-vars"),
        "message must include ruleId; got: {}",
        f.message
    );
    assert!(
        f.message.contains("defined but never used"),
        "message must include violation text; got: {}",
        f.message
    );
}

#[test]
fn lint_js_transform_maps_warning_severity_to_warning() {
    let findings = lint_js_json_findings(ESLINT_JSON_VIOLATIONS);
    let warn_findings: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Warning).collect();
    assert_eq!(
        warn_findings.len(),
        1,
        "one warning finding expected; got: {findings:?}"
    );
    let f = warn_findings[0];
    assert!(
        f.message.contains("no-console"),
        "message must include ruleId; got: {}",
        f.message
    );
    assert_eq!(f.severity, Severity::Warning);
}

#[test]
fn lint_js_transform_no_findings_on_clean_file() {
    let findings = lint_js_json_findings(ESLINT_JSON_CLEAN);
    assert!(
        findings.is_empty(),
        "clean file must produce no findings; got: {findings:?}"
    );
}

#[test]
fn lint_js_transform_handles_null_rule_id_as_parse_error() {
    let findings = lint_js_json_findings(ESLINT_JSON_PARSE_ERROR);
    assert_eq!(
        findings.len(),
        1,
        "parse error must produce one finding; got: {findings:?}"
    );
    let f = &findings[0];
    assert_eq!(f.severity, Severity::Error);
    // No ruleId — message text should appear without a "null: " prefix.
    assert!(
        f.message.contains("Parsing error"),
        "parse error message must appear in finding; got: {}",
        f.message
    );
    assert!(
        !f.message.contains("null:"),
        "null ruleId must not produce a 'null:' prefix in the message; got: {}",
        f.message
    );
}

// ── {{config.KEY}} arg expansion ──────────────────────────────────────────────

/// Build a lint/js manifest with the eslint binary replaced by a fake script path
/// so the full executor pipeline runs without npx.
fn lint_js_manifest_with_path_default(script: &Path) -> String {
    LINT_JS_MANIFEST.replace(
        "needs:\n  eslint:\n    default:\n      npm:\n        package: \"eslint\"\n        version: \"10.5.0\"\n    fallback:\n      path: \"eslint\"",
        &format!("needs:\n  eslint:\n    default:\n      path: \"{}\"", script.display()),
    )
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
fn lint_js_config_file_arg_is_expanded_from_check_config() {
    // Fake eslint script that prints its --config arg to stdout as ESLint JSON.
    // This verifies that {{config.config_file}} is expanded to the value the
    // operator set in their CHECKS.yaml `config:` block.
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_eslint.sh");
    write_executable_script(
        &script_path,
        r#"#!/bin/sh
# Extract --config <path> and echo it as an ESLint JSON finding message.
config_file=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--config" ]; then config_file="$2"; shift 2; else shift; fi
done
printf '[{"filePath":"src/x.js","messages":[{"ruleId":"test","severity":2,"message":"config=%s","line":1,"column":1,"nodeType":"x"}],"errorCount":1,"warningCount":0}]' "$config_file"
exit 1
"#,
    );

    let manifest = lint_js_manifest_with_path_default(&script_path);
    assert_ne!(
        manifest, LINT_JS_MANIFEST,
        "path-default replacement must change the manifest"
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/x.js"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    // config block with config_file set
    let config: toml::Value = toml::from_str("config_file = \"my-eslint.config.js\"\n").expect("config");
    let result = super::run_declarative_check(temp.path(), "lint/js", &declarative, &changeset, &config, None)
        .expect("run succeeds");

    assert_eq!(
        result.findings.len(),
        1,
        "expected one finding; got: {:#?}",
        result.findings
    );
    assert!(
        result.findings[0].message.contains("config=my-eslint.config.js"),
        "finding message must contain the expanded config_file path; got: {}",
        result.findings[0].message
    );
}

#[cfg(unix)]
#[test]
fn lint_js_missing_config_file_produces_clear_error() {
    // When config_file is absent from the CHECKS config, the invocation must fail
    // with a clear error naming the missing key — not silently use a default.
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_eslint.sh");
    write_executable_script(&script_path, "#!/bin/sh\nexit 0\n");

    let manifest = lint_js_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/x.js"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    // No config_file in config — must error.
    let config = toml::Value::Table(Default::default());
    let err = super::run_declarative_check(temp.path(), "lint/js", &declarative, &changeset, &config, None)
        .expect_err("missing config_file must produce an error");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("config_file") && (msg.contains("not set") || msg.contains("required")),
        "error must name the missing key `config_file`; got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn lint_js_clean_file_produces_no_findings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_eslint.sh");
    // Fake eslint that always exits 0 with an empty JSON array.
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '[{\"filePath\":\"src/x.js\",\"messages\":[],\"errorCount\":0,\"warningCount\":0}]'\nexit 0\n",
    );

    let manifest = lint_js_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/x.js"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let config: toml::Value = toml::from_str("config_file = \"eslint.config.js\"\n").expect("config");
    let result = super::run_declarative_check(temp.path(), "lint/js", &declarative, &changeset, &config, None)
        .expect("run succeeds");
    assert!(
        result.findings.is_empty(),
        "clean file must produce no findings; got: {:#?}",
        result.findings
    );
}

#[test]
fn config_ref_in_args_rejected_with_dot_in_key() {
    // {{config.a.b}} has a dot in the key name — the framework only supports
    // single-level config keys to keep the CHECKS config flat and readable.
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.js"]
needs:
  eslint:
    default: {path: "eslint"}
invocations:
  - id: lint
    run: eslint
    mode: batch
    args: ["{{config.a.b}}", "{{files}}"]
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("dot in config key must be rejected");
    assert!(
        err.to_string().contains("config") || err.to_string().contains("dot"),
        "error must explain the problem; got: {err:#}"
    );
}

#[test]
fn config_ref_in_args_rejected_with_empty_key() {
    let manifest = r#"
id: bad
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.js"]
needs:
  eslint:
    default: {path: "eslint"}
invocations:
  - id: lint
    run: eslint
    mode: batch
    args: ["{{config.}}", "{{files}}"]
    exit: {"0": findings, default: error}
    transform: {kind: passthrough}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("empty config key must be rejected");
    assert!(
        err.to_string().contains("config"),
        "error must explain the problem; got: {err:#}"
    );
}

#[test]
fn lint_js_pinned_version_honored_via_npm_version_override() {
    // Verify the npm version-pin mechanism works for eslint identically to prettier:
    // a version override in CHECKS config produces the correct npx invocation.
    let package = parse_lint_js_package();
    let config: toml::Value = toml::from_str("[needs.eslint.npm]\nversion = \"10.6.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/usr/bin/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["eslint"].display_invocation, "npx --yes eslint@10.6.0",
        "version override must update display_invocation to `npx --yes eslint@10.6.0`"
    );
}
