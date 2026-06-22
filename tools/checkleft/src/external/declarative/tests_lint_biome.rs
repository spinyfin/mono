//! Tests for the lint/biome declarative check (Biome linter via npm/npx binding).

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode, ToolInvocation};

const LINT_BIOME_MANIFEST: &str = include_str!("../../../checks/lint/biome.yaml");

fn parse_lint_biome_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(LINT_BIOME_MANIFEST).expect("lint/biome manifest must parse");
    assert_eq!(package.id, "lint/biome");
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

// Real Biome 2.5.0 `biome lint --reporter=json` output for a file with two
// warning-level diagnostics and one error-level diagnostic (1-based line/column).
const BIOME_LINT_JSON: &[u8] = br#"{"summary":{"changed":0,"unchanged":1,"matches":0,"duration":16428042,"errors":1,"warnings":2,"infos":0,"skipped":0,"suggestedFixesSkipped":0,"diagnosticsNotPrinted":0,"scannerDuration":186833},"diagnostics":[{"severity":"warning","message":"This let declares a variable that is only assigned once.","category":"lint/style/useConst","location":{"path":"oneline.js","start":{"line":1,"column":1},"end":{"line":1,"column":4}},"advices":[{"start":{"line":1,"column":5},"end":{"line":1,"column":6},"text":"Safe fix: Use const instead."}]},{"severity":"warning","message":"This variable a is unused.","category":"lint/correctness/noUnusedVariables","location":{"path":"oneline.js","start":{"line":1,"column":5},"end":{"line":1,"column":6}},"advices":[]},{"severity":"error","message":"Using == may be unsafe if you are relying on type coercion.","category":"lint/suspicious/noDoubleEquals","location":{"path":"oneline.js","start":{"line":1,"column":11},"end":{"line":1,"column":13}},"advices":[]}],"command":"lint"}"#;

// Real Biome 2.5.0 output for a clean file: no diagnostics.
const BIOME_LINT_CLEAN_JSON: &[u8] = br#"{"summary":{"changed":0,"unchanged":1,"matches":0,"duration":2745166,"errors":0,"warnings":0,"infos":0,"skipped":0,"suggestedFixesSkipped":0,"diagnosticsNotPrinted":0,"scannerDuration":193083},"diagnostics":[],"command":"lint"}"#;

// Synthetic fixture (faithful to the schema) exercising the `information` and
// `fatal` severity branches Biome can emit but the sample above does not.
const BIOME_LINT_INFO_FATAL_JSON: &[u8] = br#"{"diagnostics":[{"severity":"information","message":"prefer a template literal","category":"lint/style/useTemplate","location":{"path":"x.ts","start":{"line":3,"column":2},"end":{"line":3,"column":5}},"advices":[]},{"severity":"fatal","message":"undeclared variable","category":"lint/correctness/noUndeclaredVariables","location":{"path":"x.ts","start":{"line":4,"column":1},"end":{"line":4,"column":2}},"advices":[]}],"command":"lint"}"#;

#[test]
fn lint_biome_manifest_parses_correctly() {
    let package = parse_lint_biome_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "lint");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    // Biome lint: 0 = no errors (clean or warning/info only) → findings; 1 = at
    // least one error → findings; default = error so a crash never reads as clean.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn lint_biome_args_use_lint_ignore_unknown_and_json_reporter() {
    let package = parse_lint_biome_package();
    let args = &tool(&package.invocations[0]).args;
    assert_eq!(
        args.first().map(String::as_str),
        Some("lint"),
        "first arg must be the `lint` subcommand; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--files-ignore-unknown=true"),
        "lint/biome must pass --files-ignore-unknown=true; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--reporter=json"),
        "lint/biome must request the json reporter; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{files}}"),
        "lint/biome must pass the matched file set; got: {args:?}"
    );
    // lint/biome is zero-config: unlike lint/js it must NOT require a config_file.
    assert!(
        !args.iter().any(|a| a.contains("{{config.")),
        "lint/biome must not require a config key (Biome ships recommended defaults); got: {args:?}"
    );
    // Lint-CHECK mode only: must NOT mutate files.
    assert!(
        !args.iter().any(|a| a == "--write" || a == "--fix" || a == "--unsafe"),
        "lint/biome check must not write/fix files; got: {args:?}"
    );
}

#[test]
fn lint_biome_needs_npm_default_pinned_to_2_5_0_with_path_fallback() {
    let package = parse_lint_biome_package();
    let req = package.needs.get("biome").expect("biome binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "@biomejs/biome");
            assert_eq!(version, "2.5.0", "default Biome version must be 2.5.0");
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
fn lint_biome_shares_pinned_version_with_format_biome() {
    // Both checks must declare the SAME `@biomejs/biome` pin so they never diverge
    // (the motivation for building both in one PR).
    let lint = parse_lint_biome_package();
    let format_manifest = include_str!("../../../checks/format/biome.yaml");
    let format = match parse_declarative_check_manifest(format_manifest)
        .expect("format/biome parses")
        .implementation
    {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    assert_eq!(
        lint.needs.get("biome").map(|r| &r.default),
        format.needs.get("biome").map(|r| &r.default),
        "lint/biome and format/biome must share the identical pinned biome binding"
    );
}

#[test]
fn lint_biome_applies_to_covers_js_ts_variants_only() {
    use globset::{Glob, GlobSetBuilder};

    let package = parse_lint_biome_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    for path in ["a.js", "b.jsx", "c.mjs", "d.cjs", "e.ts", "f.tsx", "g.mts", "h.cts"] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by lint/biome applies_to"
        );
    }
    for path in ["x.rs", "y.py", "z.json", "w.css"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by lint/biome applies_to"
        );
    }
}

#[test]
fn lint_biome_npm_default_resolves_to_npx_with_pinned_version() {
    let package = parse_lint_biome_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/bin/npx")),
    )
    .expect("resolve");
    let biome = resolved.get("biome").expect("biome resolved");
    assert_eq!(biome.program, Path::new("/fake/bin/npx"));
    assert_eq!(
        biome.prefix_args,
        vec!["--yes".to_owned(), "@biomejs/biome@2.5.0".to_owned()],
        "default pin must produce `npx --yes @biomejs/biome@2.5.0`"
    );
}

#[test]
fn lint_biome_npm_version_override_repins() {
    let package = parse_lint_biome_package();
    let config: toml::Value = toml::from_str("[needs.biome.npm]\nversion = \"2.6.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["biome"].display_invocation, "npx --yes @biomejs/biome@2.6.0",
        "version override must update display_invocation to `npx --yes @biomejs/biome@2.6.0`"
    );
}

fn lint_biome_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_lint_biome_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(1), None, None)
        .expect("lint/biome transform")
}

#[test]
fn lint_biome_transform_maps_real_diagnostics() {
    let findings = lint_biome_findings(BIOME_LINT_JSON);
    assert_eq!(findings.len(), 3, "three diagnostics expected; got: {findings:?}");

    // noDoubleEquals → error at 1-based line 1, column 11.
    let err: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Error).collect();
    assert_eq!(err.len(), 1, "one error finding expected; got: {findings:?}");
    let loc = err[0].location.as_ref().expect("location");
    assert_eq!(loc.path, Path::new("oneline.js"));
    assert_eq!(loc.line, Some(1));
    assert_eq!(loc.column, Some(11));
    assert!(
        err[0].message.contains("lint/suspicious/noDoubleEquals"),
        "message must include the rule category; got: {}",
        err[0].message
    );
    assert!(
        err[0].message.contains("Using == may be unsafe"),
        "message must include the diagnostic text; got: {}",
        err[0].message
    );

    // useConst + noUnusedVariables → warnings.
    let warns: Vec<&Finding> = findings.iter().filter(|f| f.severity == Severity::Warning).collect();
    assert_eq!(warns.len(), 2, "two warning findings expected; got: {findings:?}");
    assert!(
        warns.iter().any(|f| f.message.contains("lint/style/useConst")),
        "a warning must carry the useConst category; got: {warns:?}"
    );

    // Every finding carries the suppression remediation.
    assert!(
        findings
            .iter()
            .all(|f| f.remediations.iter().any(|r| r.contains("biome-ignore lint"))),
        "every finding must include the biome-ignore remediation; got: {findings:?}"
    );
}

#[test]
fn lint_biome_transform_maps_information_and_fatal_severities() {
    let findings = lint_biome_findings(BIOME_LINT_INFO_FATAL_JSON);
    assert_eq!(findings.len(), 2, "two diagnostics expected; got: {findings:?}");
    let info = findings
        .iter()
        .find(|f| f.message.contains("useTemplate"))
        .expect("info finding");
    assert_eq!(info.severity, Severity::Info, "Biome `information` must map to Info");
    let fatal = findings
        .iter()
        .find(|f| f.message.contains("noUndeclaredVariables"))
        .expect("fatal finding");
    assert_eq!(fatal.severity, Severity::Error, "Biome `fatal` must map to Error");
}

#[test]
fn lint_biome_transform_no_findings_on_clean_file() {
    let findings = lint_biome_findings(BIOME_LINT_CLEAN_JSON);
    assert!(
        findings.is_empty(),
        "clean file must produce no findings; got: {findings:?}"
    );
}

#[test]
fn lint_biome_transform_uses_dynamic_severity() {
    // The transform controls per-finding severity (error vs warning vs info), so
    // the runner must not flatten it to a single default.
    let package = parse_lint_biome_package();
    assert!(
        package.invocations[0].transform.uses_dynamic_severity(),
        "lint/biome must preserve Biome's per-finding severity end-to-end"
    );
}

// ── full executor pipeline against a fake biome on PATH (zero config required) ──

fn lint_biome_manifest_with_path_default(script: &Path) -> String {
    LINT_BIOME_MANIFEST.replace(
        "needs:\n  biome:\n    default:\n      npm:\n        package: \"@biomejs/biome\"\n        version: \"2.5.0\"\n    fallback:\n      path: \"biome\"",
        &format!("needs:\n  biome:\n    default:\n      path: \"{}\"", script.display()),
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
fn lint_biome_end_to_end_requires_no_config() {
    // Contrast with lint/js (which errors without `config_file`): lint/biome runs
    // with NO config block at all, because Biome ships built-in recommended rules.
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_biome.sh");
    write_executable_script(
        &script_path,
        r#"#!/bin/sh
printf '{"diagnostics":[{"severity":"error","message":"Using == may be unsafe if you are relying on type coercion.","category":"lint/suspicious/noDoubleEquals","location":{"path":"src/x.js","start":{"line":1,"column":11},"end":{"line":1,"column":13}},"advices":[]}],"command":"lint"}'
exit 1
"#,
    );

    let manifest = lint_biome_manifest_with_path_default(&script_path);
    assert_ne!(
        manifest, LINT_BIOME_MANIFEST,
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
    // Empty config — no config_file key, no needs override. lint/biome must work.
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "lint/biome", &declarative, &changeset, &config, None)
        .expect("run succeeds with no config");

    assert_eq!(
        result.findings.len(),
        1,
        "expected one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.severity, Severity::Error);
    let loc = f.location.as_ref().expect("location");
    assert_eq!(loc.path, Path::new("src/x.js"));
    assert_eq!(loc.line, Some(1));
    assert_eq!(loc.column, Some(11));
    assert!(
        f.message.contains("noDoubleEquals"),
        "message must carry the rule category; got: {}",
        f.message
    );
}

#[cfg(unix)]
#[test]
fn lint_biome_end_to_end_clean_file_produces_no_findings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_biome.sh");
    // Fake biome that reports no diagnostics and exits 0.
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '{\"diagnostics\":[],\"command\":\"lint\"}'\nexit 0\n",
    );

    let manifest = lint_biome_manifest_with_path_default(&script_path);
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
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "lint/biome", &declarative, &changeset, &config, None)
        .expect("run succeeds");
    assert!(
        result.findings.is_empty(),
        "clean file must produce no findings; got: {:#?}",
        result.findings
    );
}
