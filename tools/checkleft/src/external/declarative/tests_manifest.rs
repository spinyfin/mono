//! Manifest parsing and validation tests for the declarative external-check tier:
//! the shipped format/bazel and lint/bazel manifests parse into the expected
//! shape, and malformed manifests (unknown transform kind, unknown binary,
//! missing exit default, declarative fields in component mode) are rejected.

use crate::external::{parse_declarative_check_manifest, parse_external_check_package_manifest};

use super::tests_common::{BUILDIFIER_MANIFEST, parse_lint_bazel_package, parse_package, tool};
use super::{ExitOutcome, InvocationMode};

#[test]
fn format_bazel_manifest_parses_single_format_invocation() {
    let package = parse_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "format");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::Batch);
    assert!(package.needs.contains_key("buildifier"));
    // exit `0 -> findings`, everything else -> error.
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Error);
    assert_eq!(package.invocations[0].exit.classify(None), ExitOutcome::Error);
}

#[test]
fn lint_bazel_manifest_parses_single_lint_invocation() {
    let package = parse_lint_bazel_package();
    assert_eq!(package.invocations.len(), 1);
    assert_eq!(package.invocations[0].id, "lint");
    assert_eq!(tool(&package.invocations[0]).mode, InvocationMode::PerFile);
    assert!(package.needs.contains_key("buildifier"));
    assert_eq!(package.invocations[0].exit.classify(Some(0)), ExitOutcome::Findings);
    assert_eq!(package.invocations[0].exit.classify(Some(1)), ExitOutcome::Error);
}

#[test]
fn manifest_rejects_unknown_transform_kind() {
    let manifest = BUILDIFIER_MANIFEST.replace("kind: json", "kind: regex");
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("reserved for a future spike"),
        "unexpected: {err:#}"
    );
}

#[test]
fn manifest_rejects_invocation_with_unknown_binary() {
    let manifest = BUILDIFIER_MANIFEST.replace("run: buildifier", "run: nonexistent");
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(format!("{err:#}").contains("unknown binary"), "unexpected: {err:#}");
}

#[test]
fn manifest_requires_default_exit_outcome() {
    // Remove the `default: error` line from the first invocation's exit block.
    let manifest = BUILDIFIER_MANIFEST.replacen("      default: error\n", "", 1);
    let err = parse_declarative_check_manifest(&manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("default"),
        "exit semantics must require a default so crashes surface as errors: {err:#}"
    );
}

#[test]
fn declarative_fields_rejected_in_component_mode() {
    let manifest = r#"
id = "x"
mode = "component"
runtime = "component-v1"
api_version = "v1"
artifact_path = "bin/x.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
applies_to = ["**/*.bzl"]
"#;
    let err = parse_external_check_package_manifest(manifest).unwrap_err();
    assert!(
        format!("{err:#}").contains("only allowed in `declarative` mode"),
        "unexpected: {err:#}"
    );
}
