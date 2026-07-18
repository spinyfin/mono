//! Tests for the format/rust (rustfmt) declarative check: manifest shape and
//! flags (`--check`, `-l`, `--config-path`, no nightly-only flags), the linelist
//! transform (unformatted detection, multi-file, parse-error handling),
//! bazel/path binding fallback, absolute-path normalisation, duplicate-finding
//! dedup, `{{repo_root}}` arg expansion, and template-ref validation.

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::{Finding, Severity};

use super::tests_common::tool;
use super::{ExitOutcome, ExternalCheckDeclarativePackage, InvocationMode};

const RUSTFMT_MANIFEST: &str = include_str!("../../../checks/format/rust.yaml");

fn parse_rustfmt_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(RUSTFMT_MANIFEST).expect("format/rust manifest must parse");
    assert_eq!(package.id, "format/rust");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// Apply the rustfmt linelist transform directly for unit testing.
fn rustfmt_linelist_findings(stdout: &[u8], exit_code: i32) -> Vec<Finding> {
    let package = parse_rustfmt_package();
    assert_eq!(package.invocations.len(), 1);
    package.invocations[0]
        .transform
        .apply(stdout, Some(exit_code), Some("src/lib.rs"), None)
        .expect("rustfmt transform")
}

#[test]
fn rustfmt_manifest_parses_correctly() {
    let package = parse_rustfmt_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::PerFile);
    assert!(package.needs.contains_key("rustfmt"));
    // With --check mode: exit 0 = ok (already formatted), exit 1 = findings
    // (needs formatting, filename on stdout) or operational error (no stdout).
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Ok);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn rustfmt_config_path_arg_is_present() {
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--config-path={{repo_root}}"),
        "expected --config-path={{{{repo_root}}}} in rustfmt args to pin config to repo root regardless of cwd; got: {args:?}"
    );
}

#[test]
fn rustfmt_check_flag_is_present() {
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "--check"),
        "expected --check flag for stable-compatible invocation; got: {args:?}"
    );
}

#[test]
fn rustfmt_list_flag_is_present() {
    // -l prints filenames needing formatting to stdout — required by the linelist
    // transform to distinguish violations (stdout non-empty) from parse errors (empty).
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        args.iter().any(|a| a == "-l"),
        "expected -l flag so violated filenames appear on stdout; got: {args:?}"
    );
}

#[test]
fn rustfmt_no_unstable_features_flag() {
    // --unstable-features only exists on nightly rustfmt; stable rejects it.
    // The check must not pass this flag.
    let package = parse_rustfmt_package();
    let args = &tool(&package.invocations[0]).args;
    assert!(
        !args.iter().any(|a| a == "--unstable-features"),
        "--unstable-features must not be in rustfmt args (stable rustfmt rejects it); got: {args:?}"
    );
}

#[test]
fn rustfmt_exit_one_is_findings_not_error() {
    // With --check, exit 1 means the file needs formatting (or a parse error when
    // stdout is empty — the linelist transform handles the distinction).
    let package = parse_rustfmt_package();
    assert_eq!(
        package.invocations[0].exit.classify(Some(1)),
        ExitOutcome::Findings,
        "exit 1 must be classified as findings so the linelist transform runs"
    );
}

#[test]
fn rustfmt_linelist_unformatted_file_produces_finding() {
    // `rustfmt --check -l src/lib.rs` prints the filename when it needs formatting.
    let findings = rustfmt_linelist_findings(b"src/lib.rs\n", 1);
    assert_eq!(findings.len(), 1, "one unformatted file should produce one finding");
    let f = &findings[0];
    assert_eq!(f.severity, Severity::Warning);
    let loc = f.location.as_ref().expect("finding must have a location");
    assert_eq!(loc.path, Path::new("src/lib.rs"));
    assert!(loc.line.is_none(), "linelist findings are file-level (no line number)");
    assert!(
        f.message.contains("formatting"),
        "message should mention formatting; got: {}",
        f.message
    );
    assert!(
        f.remediations.iter().any(|r| r.contains("cargo fmt")),
        "remediation should mention cargo fmt; got: {:?}",
        f.remediations
    );
}

#[test]
fn rustfmt_linelist_multiple_files_produce_multiple_findings() {
    let stdout = b"src/a.rs\nsrc/b.rs\n";
    let findings = rustfmt_linelist_findings(stdout, 1);
    assert_eq!(findings.len(), 2, "two unformatted files should produce two findings");
    let paths: Vec<&Path> = findings
        .iter()
        .map(|f| f.location.as_ref().unwrap().path.as_path())
        .collect();
    assert!(paths.contains(&Path::new("src/a.rs")));
    assert!(paths.contains(&Path::new("src/b.rs")));
}

#[test]
fn rustfmt_linelist_nonzero_with_no_output_is_error() {
    // Exit 1 with no stdout = rustfmt parse error (not a formatting violation).
    // The transform must surface this as an error rather than silently returning clean.
    let package = parse_rustfmt_package();
    let err = package.invocations[0]
        .transform
        .apply(b"", Some(1), Some("src/lib.rs"), None)
        .expect_err("empty stdout + exit 1 must be an error, not clean");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("operational error") || msg.contains("parse") || msg.contains("exit"),
        "error must explain the cause; got: {msg}"
    );
}

#[test]
fn rustfmt_has_bazel_default_and_path_fallback() {
    // The manifest must declare a bazel default (hermetic CI toolchain) AND a path
    // fallback (standalone / non-Bazel use). The framework warns loudly when it
    // uses the fallback so the operator knows hermetic resolution was skipped.
    let package = parse_rustfmt_package();
    let req = package.needs.get("rustfmt").expect("rustfmt binary must be declared");
    assert!(
        matches!(req.default, super::BinaryBinding::Bazel(_)),
        "default binding must be bazel for hermetic CI use; got: {:?}",
        req.default
    );
    assert!(
        matches!(req.fallback, Some(super::BinaryBinding::Path(_))),
        "fallback binding must be a path for non-Bazel use; got: {:?}",
        req.fallback
    );
}

#[test]
fn rustfmt_missing_binary_degrades_to_check_error() {
    // When the rustfmt binary cannot be found, run_declarative_check returns
    // Err — the runner converts this to an error-severity finding rather than
    // panicking, so checkleft degrades gracefully.
    use std::path::PathBuf;

    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("write file");

    // Replace the bazel default and remove the fallback so there's no valid binary.
    let manifest = RUSTFMT_MANIFEST.replace(
        "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
        "path: \"nonexistent_rustfmt_binary_xyz\"",
    );
    let package = parse_declarative_check_manifest(&manifest).expect("modified manifest must parse");
    let declarative = match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = ChangeSet::new(vec![ChangedFile {
        path: PathBuf::from("main.rs"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    let err = super::run_declarative_check(
        temp.path(),
        "rustfmt",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect_err("missing binary must produce an error, not a successful result");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("nonexistent_rustfmt_binary_xyz") || msg.contains("spawn") || msg.contains("format"),
        "error message should reference the binary or spawn failure; got: {msg}"
    );
}

// ── path normalisation (issue: absolute paths from hermetic toolchain) ──────────

#[test]
fn linelist_absolute_paths_are_normalised_to_repo_relative() {
    // When the hermetic Bazel rustfmt wrapper canonicalises the input path, it
    // echoes back an absolute path. The framework must strip the repo-root prefix
    // before emitting the finding.
    let findings = rustfmt_linelist_findings(b"/repo/root/tools/src/lib.rs\n", 1);
    // Without normalization the path stays absolute — this is what the transform
    // itself returns; normalization is the executor's responsibility.
    assert_eq!(
        findings[0].location.as_ref().unwrap().path,
        Path::new("/repo/root/tools/src/lib.rs"),
        "transform should preserve the path as-is; normalization happens in the executor"
    );

    // Normalization happens inside run_invocation. Verify it via run_declarative_check
    // by wiring up a tiny shell script that prints an absolute path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Fake rustfmt that always exits 1 and prints an absolute path
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\necho '{}/tools/src/lib.rs'\nexit 1\n",
                repo_root.path().display()
            ),
        )
        .expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        let changeset = ChangeSet::new(vec![ChangedFile {
            path: std::path::PathBuf::from("tools/src/lib.rs"),
            kind: crate::input::ChangeKind::Modified,
            old_path: None,
        }]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        assert_eq!(
            result.findings.len(),
            1,
            "expected one finding; got: {:#?}",
            result.findings
        );
        assert_eq!(
            result.findings[0].location.as_ref().unwrap().path,
            Path::new("tools/src/lib.rs"),
            "absolute path must be normalised to repo-relative"
        );
    }
}

// ── duplicate findings (issue: module-tree recursion causes double-reporting) ───

#[test]
fn duplicate_findings_across_module_tree_are_deduplicated() {
    // Simulate rustfmt being invoked per-file for both mod.rs and one of its
    // declared submodule files. When mod.rs recurses into the submodule, both
    // invocations emit a finding for the submodule file — the dedup pass in the
    // executor must collapse them to one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Fake rustfmt: always prints "src/lib.rs" and exits 1, regardless of
        // which file was passed. This simulates rustfmt recursing into a submodule
        // from both the parent and the child invocations.
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'src/lib.rs'\nexit 1\n").expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        // Two files in the changeset — simulating mod.rs + child both in the PR.
        let changeset = ChangeSet::new(vec![
            ChangedFile {
                path: std::path::PathBuf::from("src/mod.rs"),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: std::path::PathBuf::from("src/lib.rs"),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            },
        ]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        assert_eq!(
            result.findings.len(),
            1,
            "duplicate findings for src/lib.rs must be deduplicated to one; got: {:#?}",
            result.findings
        );
        assert_eq!(
            result.findings[0].location.as_ref().unwrap().path,
            Path::new("src/lib.rs")
        );
    }
}

// ── {{repo_root}} arg expansion ───────────────────────────────────────────────────

#[test]
fn rustfmt_repo_root_arg_expands_to_absolute_path() {
    // When run_declarative_check is called, --config-path={{repo_root}} must be
    // expanded to the absolute repo root before rustfmt is invoked. A fake rustfmt
    // script prints its --config-path arg to stdout and exits 1 (→ "findings"); the
    // linelist transform puts each stdout line into a finding's location.path, so we
    // can assert the expanded path was passed to the tool.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let repo_root = tempfile::tempdir().expect("temp repo root");
        std::fs::write(repo_root.path().join("rustfmt.toml"), "edition = \"2021\"\n").expect("write rustfmt.toml");

        // Print the --config-path=... arg to stdout and exit 1 (→ findings).
        let script_path = repo_root.path().join("fake_rustfmt.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --config-path=*) echo \"$arg\"; exit 1;;\n  esac\ndone\nexit 0\n",
        )
        .expect("write script");
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");

        let manifest = RUSTFMT_MANIFEST.replace(
            "bazel: \"@rules_rust//tools/upstream_wrapper:rustfmt\"\n    fallback:\n      path: \"rustfmt\"",
            &format!("path: \"{}\"", script_path.display()),
        );
        let package = parse_declarative_check_manifest(&manifest).expect("manifest parses");
        let declarative = match package.implementation {
            ExternalCheckPackageImplementation::Declarative(d) => d,
            other => panic!("expected declarative, got {other:?}"),
        };

        let changeset = crate::input::ChangeSet::new(vec![crate::input::ChangedFile {
            path: std::path::PathBuf::from("src.rs"),
            kind: crate::input::ChangeKind::Modified,
            old_path: None,
        }]);

        let result = super::run_declarative_check(
            repo_root.path(),
            "rustfmt",
            &declarative,
            &changeset,
            &toml::Value::Table(Default::default()),
            None,
        )
        .expect("run succeeds");

        // The fake script prints exactly one line: --config-path=<path>. The linelist
        // transform records it as finding.location.path (one finding, no line number).
        assert_eq!(
            result.findings.len(),
            1,
            "expected one finding from --config-path echo; got: {:#?}",
            result.findings
        );
        let path_str = result.findings[0].location.as_ref().unwrap().path.to_string_lossy();
        let expected_config_arg = format!("--config-path={}", repo_root.path().display());
        assert_eq!(
            path_str.as_ref(),
            expected_config_arg.as_str(),
            "{{{{repo_root}}}} must expand to the absolute repo root; got: {path_str}"
        );
    }
}

#[test]
fn args_rejects_unknown_template_ref_at_load_time() {
    // An unrecognised {{...}} token in invocation args must be caught at manifest-load
    // time, not silently passed through to the tool.
    let manifest = RUSTFMT_MANIFEST.replace("{{repo_root}}", "{{unknown_var}}");
    let err = parse_declarative_check_manifest(&manifest).expect_err("manifest with {{unknown_var}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown template ref") || msg.contains("unknown_var"),
        "error must name the bad ref; got: {msg}"
    );
}

// ── template validation (issue: {{file}} silently rendered raw) ──────────────────

#[test]
fn linelist_rejects_unknown_template_var_in_remediations_at_parse_time() {
    // {{file}} is not a valid template ref; the check should be rejected at
    // manifest-load time so operators see the error immediately.
    let manifest = RUSTFMT_MANIFEST.replace("{{input.file}}", "{{file}}");
    let err = parse_declarative_check_manifest(&manifest).expect_err("manifest with {{file}} must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown template ref") || msg.contains("{{file}}"),
        "error must name the bad ref; got: {msg}"
    );
}

#[test]
fn linelist_remediations_substitute_input_file() {
    // {{input.file}} in the remediation text should expand to the file passed to
    // the per-file invocation.
    let findings = rustfmt_linelist_findings(b"src/lib.rs\n", 1);
    assert_eq!(findings.len(), 1);
    let remediation = &findings[0].remediations[0];
    assert!(
        remediation.contains("src/lib.rs"),
        "remediation must contain the input file path `src/lib.rs`; got: {remediation}"
    );
    assert!(
        !remediation.contains("{{"),
        "remediation must not contain unsubstituted template vars; got: {remediation}"
    );
}
