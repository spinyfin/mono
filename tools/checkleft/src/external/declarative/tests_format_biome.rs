//! Tests for the format/biome declarative check (Biome formatter via npm/npx binding).

use std::collections::BTreeMap;
use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationKind, InvocationMode, ToolInvocation};

const FORMAT_BIOME_MANIFEST: &str = include_str!("../../../checks/format/biome.yaml");

fn parse_format_biome_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(FORMAT_BIOME_MANIFEST).expect("format/biome manifest must parse");
    assert_eq!(package.id, "format/biome");
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

/// Resolved-invocation map for `{{needs.biome.invocation}}` rendering in transform
/// unit tests (the executor builds this from resolved binaries at run time).
fn biome_needs() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("biome".to_owned(), "npx --yes @biomejs/biome@2.5.0".to_owned());
    map
}

// Real Biome 2.5.0 `biome format --reporter=json` output for one unformatted file.
const BIOME_FORMAT_JSON: &[u8] = br#"{"summary":{"changed":0,"unchanged":1,"matches":0,"duration":7523708,"errors":1,"warnings":0,"infos":0,"skipped":0,"suggestedFixesSkipped":0,"diagnosticsNotPrinted":0,"scannerDuration":184083},"diagnostics":[{"severity":"error","message":"Formatter would have printed the following content:","category":"format","location":{"path":"bad.js","start":{"line":0,"column":0},"end":{"line":0,"column":0}},"advices":[]}],"command":"format"}"#;

// Real Biome 2.5.0 output for a clean (already-formatted) file: no diagnostics.
const BIOME_FORMAT_CLEAN_JSON: &[u8] = br#"{"summary":{"changed":0,"unchanged":1,"matches":0,"duration":2745166,"errors":0,"warnings":0,"infos":0,"skipped":0,"suggestedFixesSkipped":0,"diagnosticsNotPrinted":0,"scannerDuration":193083},"diagnostics":[],"command":"format"}"#;

#[test]
fn format_biome_manifest_parses_correctly() {
    let package = parse_format_biome_package();
    assert!(package.skip_symlinks, "format/biome must skip symlinks like prettier");
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    // batch: Biome is fast and accepts the whole file set in one process.
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    // exit 0 = already formatted (ok); exit 1 = needs formatting (findings via the
    // json transform); anything else = error so a crash never reads as clean.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn format_biome_args_use_format_ignore_unknown_and_json_reporter() {
    let package = parse_format_biome_package();
    let args = &tool(&package.invocations[0]).args;
    assert_eq!(
        args.first().map(String::as_str),
        Some("format"),
        "first arg must be the `format` subcommand; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--files-ignore-unknown=true"),
        "format/biome must pass --files-ignore-unknown=true so unsupported files are skipped, not errors; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--reporter=json"),
        "format/biome must request the json reporter; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{files}}"),
        "format/biome must pass the matched file set; got: {args:?}"
    );
    // Format-CHECK mode only: must NOT mutate files in the check invocation.
    assert!(
        !args.iter().any(|a| a == "--write" || a == "--fix"),
        "format/biome check must not write/fix files; got: {args:?}"
    );
}

#[test]
fn format_biome_needs_npm_default_pinned_to_2_5_0_with_path_fallback() {
    let package = parse_format_biome_package();
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
fn format_biome_applies_to_covers_formattable_types_only() {
    use globset::{Glob, GlobSetBuilder};

    let package = parse_format_biome_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    for path in [
        "a.js",
        "b.jsx",
        "c.mjs",
        "d.cjs",
        "e.ts",
        "f.tsx",
        "g.mts",
        "h.cts",
        "i.json",
        "j.jsonc",
        "k.css",
        "l.graphql",
        "m.gql",
    ] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by format/biome applies_to"
        );
    }

    // Biome 2.5.0 does not format these — keep them out of applies_to.
    for path in ["x.md", "y.markdown", "z.html", "w.rs", "v.yaml", "u.scss"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by format/biome applies_to"
        );
    }
}

#[test]
fn format_biome_npm_default_resolves_to_npx_with_pinned_version() {
    let package = parse_format_biome_package();
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
    assert_eq!(biome.display_invocation, "npx --yes @biomejs/biome@2.5.0");
}

#[test]
fn format_biome_npm_version_override_repins() {
    let package = parse_format_biome_package();
    let config: toml::Value = toml::from_str("[needs.biome.npm]\nversion = \"2.6.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["biome"].prefix_args,
        vec!["--yes".to_owned(), "@biomejs/biome@2.6.0".to_owned()],
        "version override must re-pin to 2.6.0 while inheriting the package name"
    );
    assert_eq!(resolved["biome"].display_invocation, "npx --yes @biomejs/biome@2.6.0");
}

fn format_biome_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_format_biome_package();
    let needs = biome_needs();
    package.invocations[0]
        .transform
        .apply(stdout, Some(1), None, Some(&needs))
        .expect("format/biome transform")
}

#[test]
fn format_biome_transform_emits_one_finding_per_unformatted_file() {
    let findings = format_biome_findings(BIOME_FORMAT_JSON);
    assert_eq!(findings.len(), 1, "one finding expected; got: {findings:?}");
    let f = &findings[0];
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("bad.js"));
    // Formatting is a whole-file property: no single offending line/column.
    assert_eq!(loc.line, None);
    assert_eq!(loc.column, None);
    assert_eq!(f.severity, Severity::Warning);
    assert!(
        f.message.contains("biome formatting"),
        "message must mention biome formatting; got: {}",
        f.message
    );
    assert_eq!(f.remediations.len(), 1);
    assert!(
        f.remediations[0].contains("npx --yes @biomejs/biome@2.5.0 format --write bad.js"),
        "remediation must render the dynamic invocation + per-file write command; got: {}",
        f.remediations[0]
    );
}

#[test]
fn format_biome_transform_no_findings_on_clean_output() {
    // Belt-and-suspenders: the executor short-circuits exit 0 to `ok` so the
    // transform is not even called, but an empty diagnostics array must still
    // project to zero findings.
    let findings = format_biome_findings(BIOME_FORMAT_CLEAN_JSON);
    assert!(
        findings.is_empty(),
        "clean output must produce no findings; got: {findings:?}"
    );
}

// ── full executor pipeline against a fake biome on PATH ─────────────────────────

fn format_biome_manifest_with_path_default(script: &Path) -> String {
    FORMAT_BIOME_MANIFEST.replace(
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
fn format_biome_end_to_end_reports_unformatted_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_biome.sh");
    // Fake biome that emits a format diagnostic for bad.js and exits 1.
    write_executable_script(
        &script_path,
        r#"#!/bin/sh
printf '{"summary":{"errors":1},"diagnostics":[{"severity":"error","message":"Formatter would have printed the following content:","category":"format","location":{"path":"bad.js","start":{"line":0,"column":0},"end":{"line":0,"column":0}},"advices":[]}],"command":"format"}'
exit 1
"#,
    );

    let manifest = format_biome_manifest_with_path_default(&script_path);
    assert_ne!(
        manifest, FORMAT_BIOME_MANIFEST,
        "path-default replacement must change the manifest"
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("bad.js"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "format/biome", &declarative, &changeset, &config, None)
        .expect("run succeeds");

    assert_eq!(
        result.findings.len(),
        1,
        "expected one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.location.as_ref().expect("location").path, Path::new("bad.js"));
    // The dynamic remediation renders the resolved invocation (here the fake path).
    assert!(
        f.remediations[0].contains(&format!("{} format --write bad.js", script_path.display())),
        "remediation must render the resolved invocation; got: {}",
        f.remediations[0]
    );
}

#[cfg(unix)]
#[test]
fn format_biome_end_to_end_clean_file_produces_no_findings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("fake_biome.sh");
    // Fake biome that reports nothing to format and exits 0.
    write_executable_script(
        &script_path,
        "#!/bin/sh\nprintf '{\"summary\":{\"errors\":0},\"diagnostics\":[],\"command\":\"format\"}'\nexit 0\n",
    );

    let manifest = format_biome_manifest_with_path_default(&script_path);
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("clean.js"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let config = toml::Value::Table(Default::default());
    let result = super::run_declarative_check(temp.path(), "format/biome", &declarative, &changeset, &config, None)
        .expect("run succeeds");
    assert!(
        result.findings.is_empty(),
        "clean file must produce no findings; got: {:#?}",
        result.findings
    );
}
