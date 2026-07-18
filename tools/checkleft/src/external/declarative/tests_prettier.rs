//! Tests for the format/prettier declarative check and the npm version-pinned
//! `needs` binding: manifest shape and flags, applies_to coverage, the
//! `npx --yes <pkg>@<ver>` resolution (with version / package / path overrides
//! and npx-missing fallback), binding validation, end-to-end runs through a fake
//! prettier, the `{{needs.<name>.invocation}}` remediation template, and the
//! skip_symlinks manifest flag.

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{CheckResult, Severity};

use super::tests_common::{tool, write_executable};
use super::{ExitOutcome, ExternalCheckDeclarativePackage, InvocationMode};

const PRETTIER_MANIFEST: &str = include_str!("../../../checks/format/prettier.yaml");

fn parse_prettier_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(PRETTIER_MANIFEST).expect("format/prettier manifest must parse");
    assert_eq!(package.id, "format/prettier");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

#[test]
fn prettier_manifest_parses_correctly() {
    let package = parse_prettier_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    // batch: one Prettier process over all files, paying npx startup once.
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--list-different"),
        "expected --list-different so violated paths appear on stdout; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--ignore-unknown"),
        "expected --ignore-unknown so unsupported files are skipped, not errors; got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "{{files}}"),
        "expected {{{{files}}}} for batch mode; got: {args:?}"
    );
    // exit 0 = all formatted (ok); exit 1 = some need formatting (findings);
    // exit 2 = parse/syntax error but Prettier continues and reports other files
    // (findings); anything else = error so a crash never reads as clean.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(2)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn prettier_applies_to_covers_js_ts_and_friends() {
    // Behavioral test: compile the same globset select_files builds and verify
    // representative files match (including tsx/mjs which exercise brace
    // alternation) and that non-prettier file types do not.
    use globset::{Glob, GlobSetBuilder};

    let package = parse_prettier_package();
    let mut builder = GlobSetBuilder::new();
    for pattern in &package.applies_to {
        builder.add(Glob::new(pattern).unwrap_or_else(|e| panic!("invalid applies_to glob `{pattern}`: {e}")));
    }
    let globset = builder.build().expect("applies_to globset must build");

    for path in [
        "a.js",
        "b.tsx",
        "c.mjs",
        "d.css",
        "e.json",
        "f.md",
        "g.yaml",
        "h.jsx",
        "i.ts",
        "j.cjs",
        "k.mts",
        "l.cts",
        "m.scss",
        "n.less",
        "o.html",
        "p.vue",
        "q.markdown",
        "r.yml",
        "s.graphql",
        "t.gql",
    ] {
        assert!(
            globset.is_match(path),
            "`{path}` should be matched by prettier's applies_to"
        );
    }

    for path in ["x.rs", "y.png"] {
        assert!(
            !globset.is_match(path),
            "`{path}` should NOT be matched by prettier's applies_to"
        );
    }
}

#[test]
fn prettier_needs_npm_default_pinned_to_3_8_4_with_path_fallback() {
    // The version pin lives in the manifest as the per-check default (3.8.4), with a
    // PATH fallback for environments without npx — mirroring rustfmt's bazel+path shape.
    let package = parse_prettier_package();
    let req = package.needs.get("prettier").expect("prettier binary must be declared");
    match &req.default {
        super::BinaryBinding::Npm { package, version } => {
            assert_eq!(package, "prettier");
            assert_eq!(version, "3.8.4", "default Prettier version must be 3.8.4");
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
fn npm_default_resolves_to_npx_with_pinned_version_spec() {
    // With npx present, the npm binding resolves to `npx --yes prettier@3.8.4`: the
    // pinned version rides ahead of the check's own args as prefix args.
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let npx = Path::new("/fake/bin/npx");
    let resolved =
        super::resolve::resolve_all_with_npx(Path::new("/repo"), &package.needs, &config, Some(npx)).expect("resolve");
    let prettier = resolved.get("prettier").expect("prettier resolved");
    assert_eq!(prettier.program, npx);
    assert_eq!(
        prettier.prefix_args,
        vec!["--yes".to_owned(), "prettier@3.8.4".to_owned()],
        "default pin must produce `npx --yes prettier@3.8.4`"
    );
}

#[test]
fn npm_version_override_repins_the_pinned_version() {
    // A repo overrides just the version via CHECKS config; the package is inherited
    // from the default npm binding.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier.npm]\nversion = \"3.9.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].prefix_args,
        vec!["--yes".to_owned(), "prettier@3.9.0".to_owned()],
        "version override must re-pin to 3.9.0 while inheriting the package name"
    );
}

#[test]
fn npm_full_override_replaces_package_and_version() {
    let package = parse_prettier_package();
    let config: toml::Value =
        toml::from_str("[needs.prettier.npm]\npackage = \"@scope/prettier\"\nversion = \"4.0.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].prefix_args,
        vec!["--yes".to_owned(), "@scope/prettier@4.0.0".to_owned()]
    );
}

#[test]
fn npm_path_override_swaps_binding_and_drops_npx() {
    // A `path` override fully replaces the npm binding even when npx is available.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier]\npath = \"/opt/prettier\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/fake/npx")),
    )
    .expect("resolve");
    assert_eq!(resolved["prettier"].program, Path::new("/opt/prettier"));
    assert!(
        resolved["prettier"].prefix_args.is_empty(),
        "a path binding carries no prefix args"
    );
}

#[test]
fn npm_missing_npx_falls_back_to_path_binary() {
    // No npx on PATH: the npm default fails to resolve and the declared `fallback.path`
    // takes over (a loud warning is emitted to stderr).
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let resolved =
        super::resolve::resolve_all_with_npx(Path::new("/repo"), &package.needs, &config, None).expect("resolve");
    assert_eq!(
        resolved["prettier"].program,
        Path::new("prettier"),
        "fallback should use the PATH `prettier` binary"
    );
    assert!(resolved["prettier"].prefix_args.is_empty());
}

#[test]
fn npm_binding_requires_both_package_and_version() {
    let missing_version = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      npm:
        package: "prettier"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(missing_version).expect_err("npm without version must be rejected");
    assert!(err.to_string().contains("must set `version`"), "got: {err:#}");

    let missing_package = missing_version.replace("package: \"prettier\"", "version: \"3.8.4\"");
    let err = parse_declarative_check_manifest(&missing_package).expect_err("npm without package must be rejected");
    assert!(err.to_string().contains("must set `package`"), "got: {err:#}");
}

#[test]
fn binding_rejects_more_than_one_kind() {
    let manifest = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      path: "prettier"
      npm:
        package: "prettier"
        version: "3.8.4"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("two binding kinds must be rejected");
    assert!(
        err.to_string().contains("exactly one of `bazel`, `path`, or `npm`"),
        "got: {err:#}"
    );
}

#[test]
fn path_default_with_fallback_is_rejected() {
    // A `path` default always resolves, so a fallback would be unreachable.
    let manifest = r#"
id: format/prettier
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to: ["**/*.ts"]
needs:
  prettier:
    default:
      path: "prettier"
    fallback:
      path: "prettier2"
invocations:
  - id: format
    run: prettier
    mode: per_file
    args: ["--list-different", "{{file}}"]
    exit: {"0": ok, "1": findings, default: error}
    transform: {kind: linelist, message: "x"}
"#;
    let err = parse_declarative_check_manifest(manifest).expect_err("path default + fallback must be rejected");
    assert!(
        err.to_string()
            .contains("fallback is only meaningful when `default` is `bazel` or `npm`"),
        "got: {err:#}"
    );
}

/// Build a prettier manifest whose default binding is a path to a fake script, so
/// the full executor pipeline runs without npx/Node.
fn prettier_manifest_with_path_default(script: &Path) -> String {
    PRETTIER_MANIFEST.replace(
        "needs:\n  prettier:\n    default:\n      npm:\n        package: \"prettier\"\n        version: \"3.8.4\"\n    fallback:\n      path: \"prettier\"",
        &format!("needs:\n  prettier:\n    default:\n      path: \"{}\"", script.display()),
    )
}

#[cfg(unix)]
fn run_prettier_e2e(script_body: &str, file: &str) -> CheckResult {
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("fake_prettier.sh");
    write_executable(&script_path, script_body);

    let manifest = prettier_manifest_with_path_default(&script_path);
    // The replacement must actually change the manifest, else the test would silently
    // exercise the npm binding instead of the fake script.
    assert_ne!(
        manifest, PRETTIER_MANIFEST,
        "path-default replacement did not match the manifest"
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from(file),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    super::run_declarative_check(
        repo_root.path(),
        "format/prettier",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run succeeds")
}

#[cfg(unix)]
#[test]
fn prettier_unformatted_file_produces_finding_with_remediation() {
    // Fake `prettier --list-different ... <file>`: echo the last arg (the file) and
    // exit 1, exactly like prettier listing a file that needs reformatting.
    let result = run_prettier_e2e(
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\"\nexit 1\n",
        "src/app.ts",
    );
    assert_eq!(
        result.findings.len(),
        1,
        "one unformatted file should produce one finding; got: {:#?}",
        result.findings
    );
    let f = &result.findings[0];
    assert_eq!(f.severity, Severity::Warning);
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/app.ts"));
    assert!(loc.line.is_none(), "linelist findings are file-level");
    assert!(
        f.message.contains("prettier formatting"),
        "message should mention prettier; got: {}",
        f.message
    );
    // The e2e helper uses a path-default binding (a fake script), so
    // {{needs.prettier.invocation}} expands to the fake script path rather than
    // `npx --yes prettier@<version>`. Assert the file arg is present; the
    // version-specific assertion is in the dedicated template tests below.
    assert!(
        f.remediations
            .iter()
            .any(|r| r.contains("--write") && r.contains("src/app.ts")),
        "remediation should contain `--write src/app.ts`; got: {:?}",
        f.remediations
    );
    // No unsubstituted template vars must remain.
    assert!(
        f.remediations.iter().all(|r| !r.contains("{{")),
        "remediation must not contain unsubstituted template vars; got: {:?}",
        f.remediations
    );
}

#[cfg(unix)]
#[test]
fn prettier_clean_file_produces_no_finding() {
    // Fake prettier exits 0 (file already formatted) — no findings.
    let result = run_prettier_e2e("#!/bin/sh\nexit 0\n", "src/app.ts");
    assert!(
        result.findings.is_empty(),
        "formatted file should produce no findings; got: {:#?}",
        result.findings
    );
}

#[cfg(unix)]
#[test]
fn prettier_skips_files_outside_applies_to() {
    // A non-prettier file (e.g. a .rs file) must not be selected, so the fake script
    // never runs and there are no findings even though it would exit 1.
    let result = run_prettier_e2e("#!/bin/sh\necho should-not-run\nexit 1\n", "src/lib.rs");
    assert!(
        result.findings.is_empty(),
        "a .rs file is outside prettier's applies_to and must be skipped; got: {:#?}",
        result.findings
    );
}

#[cfg(unix)]
#[test]
fn prettier_exit_two_with_no_output_causes_check_error() {
    // In batch mode, prettier exit 2 maps to `findings` and the linelist runs.
    // With empty stdout (all files errored), the linelist bails as an operational
    // error — the check returns Err rather than a spurious "zero findings" result.
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("fake_prettier.sh");
    write_executable(&script_path, "#!/bin/sh\necho 'tool error' >&2\nexit 2\n");
    let manifest = prettier_manifest_with_path_default(&script_path);
    assert_ne!(
        manifest, PRETTIER_MANIFEST,
        "path-default replacement did not match the manifest"
    );
    let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("src/app.ts"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);
    let err = super::run_declarative_check(
        repo_root.path(),
        "format/prettier",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect_err("batch mode: exit 2 + empty stdout must return Err (operational error)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("operational error") || msg.contains("exit") || msg.contains("no output"),
        "error must explain the cause; got: {msg}"
    );
}

// ── {{needs.<name>.invocation}} template variable ──────────────────────────────

#[test]
fn npm_default_resolved_binary_has_correct_display_invocation() {
    // ResolvedBinary.display_invocation must use "npx --yes <pkg>@<ver>", not the
    // full npx path, so remediation strings are human-readable on any host.
    let package = parse_prettier_package();
    let config = toml::Value::Table(Default::default());
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/usr/bin/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].display_invocation, "npx --yes prettier@3.8.4",
        "default npm binding must produce display_invocation `npx --yes prettier@3.8.4`"
    );
}

#[test]
fn npm_version_override_updates_display_invocation() {
    // When a repo overrides the version to 3.9.0, display_invocation must reflect
    // 3.9.0, not the hardcoded 3.8.4 default. This is the core invariant: the
    // remediation must stay in lockstep with the actual resolved version.
    let package = parse_prettier_package();
    let config: toml::Value = toml::from_str("[needs.prettier.npm]\nversion = \"3.9.0\"\n").expect("config");
    let resolved = super::resolve::resolve_all_with_npx(
        Path::new("/repo"),
        &package.needs,
        &config,
        Some(Path::new("/usr/bin/npx")),
    )
    .expect("resolve");
    assert_eq!(
        resolved["prettier"].display_invocation, "npx --yes prettier@3.9.0",
        "version override to 3.9.0 must update display_invocation to `npx --yes prettier@3.9.0`"
    );
}

#[test]
fn prettier_linelist_remediation_renders_default_invocation() {
    // The {{needs.prettier.invocation}} template must expand to the resolved invocation
    // string. This test drives the linelist transform directly with a pre-populated
    // needs_invocations map, verifying the template expansion without running a binary.
    use std::collections::BTreeMap;
    let package = parse_prettier_package();
    let transform = &package.invocations[0].transform;

    let mut needs_invocations = BTreeMap::new();
    needs_invocations.insert("prettier".to_owned(), "npx --yes prettier@3.8.4".to_owned());

    // Simulate prettier --list-different (batch) printing "src/app.ts" and exiting 1.
    // input_file is None in batch mode; the linelist uses the stdout path per-finding.
    let findings = transform
        .apply(b"src/app.ts\n", Some(1), None, Some(&needs_invocations))
        .expect("linelist transform with needs_invocations");

    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("npx --yes prettier@3.8.4 --write src/app.ts"),
        "remediation must expand to the resolved invocation + file; got: {remediation}"
    );
    assert!(
        !remediation.contains("{{"),
        "remediation must not contain unsubstituted template vars; got: {remediation}"
    );
}

#[test]
fn prettier_linelist_remediation_renders_overridden_version() {
    // When the resolved version is 3.9.0, the remediation must say 3.9.0, not 3.8.4.
    // There must be no hardcoded literal — the template must reference the resolved binary.
    use std::collections::BTreeMap;
    let package = parse_prettier_package();
    let transform = &package.invocations[0].transform;

    let mut needs_invocations = BTreeMap::new();
    needs_invocations.insert("prettier".to_owned(), "npx --yes prettier@3.9.0".to_owned());

    // input_file is None in batch mode; the linelist uses the stdout path per-finding.
    let findings = transform
        .apply(b"src/app.ts\n", Some(1), None, Some(&needs_invocations))
        .expect("linelist transform with version override");

    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("npx --yes prettier@3.9.0 --write src/app.ts"),
        "remediation must reflect the overridden version 3.9.0; got: {remediation}"
    );
    assert!(
        !remediation.contains("3.8.4"),
        "remediation must NOT contain the old hardcoded 3.8.4 when version is overridden to 3.9.0; got: {remediation}"
    );
}

// ── skip_symlinks flag ─────────────────────────────────────────────────────────

#[test]
fn prettier_manifest_has_skip_symlinks_true() {
    let package = parse_prettier_package();
    assert!(
        package.skip_symlinks,
        "format/prettier must set skip_symlinks: true so symlinks (e.g. CLAUDE.md -> AGENTS.md) \
         are not passed to prettier, which exits 2 on symlink paths"
    );
}
