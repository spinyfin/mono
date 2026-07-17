//! Shared fixtures and helper functions for the declarative external-check tests.
//!
//! The declarative test surface is split across several sibling `tests_*` files
//! grouped by concern (manifest parsing, passthrough, selector/template,
//! format/lint transforms, rustfmt, prettier, file selection, execution, gated
//! e2e). The constants and helpers those files share live here so a single
//! source of truth backs them all and the individual files stay well under the
//! repo's file-size cap.

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Finding;

use super::{ExternalCheckDeclarativePackage, Invocation, InvocationKind, ToolInvocation};

// The committed manifests — source of truth for the declarative bazel checks.
// Tests source from these files so the tests and shipped definitions cannot drift.
pub(super) const BUILDIFIER_MANIFEST: &str = include_str!("../../../checks/format/bazel.yaml");
pub(super) const LINT_BAZEL_MANIFEST: &str = include_str!("../../../checks/lint/bazel.yaml");

// Real buildifier 7.3.1 `--mode=check --format=json` output for an unformatted file.
pub(super) const REAL_FORMAT_UNFORMATTED: &[u8] =
    br#"{"success":false,"files":[{"filename":"a/b/unformatted.bzl","formatted":false,"valid":true,"warnings":[]}]}"#;

// Real buildifier output for an already-clean file (no format finding).
pub(super) const REAL_FORMAT_CLEAN: &[u8] =
    br#"{"success":true,"files":[{"filename":"a/b/clean.bzl","formatted":true,"valid":true,"warnings":[]}]}"#;

// Real buildifier `--mode=check --lint=warn --format=json` output for the spike
// fixture (tests/fixtures/buildifier/malformed.bzl.fixture). Note: warnings carry
// NO `filename` — the finding path must come from invocation context.
pub(super) const REAL_LINT_WARNINGS: &[u8] = br##"{"success":false,"files":[{"filename":"a/b/malformed.bzl","formatted":true,"valid":true,"warnings":[{"start":{"line":11,"column":1},"end":{"line":11,"column":2},"category":"module-docstring","actionable":true,"autoFixable":false,"message":"The file has no module docstring.\nA module docstring is a string literal (not a comment) which should be the first statement of a file (it may follow comment lines).","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#module-docstring"},{"start":{"line":11,"column":19},"end":{"line":11,"column":22},"category":"unused-variable","actionable":true,"autoFixable":false,"message":"Variable \"ctx\" is unused. Please remove it.","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#unused-variable"},{"start":{"line":12,"column":5},"end":{"line":12,"column":24},"category":"no-effect","actionable":true,"autoFixable":false,"message":"Expression result is not used.","url":"https://github.com/bazelbuild/buildtools/blob/main/WARNINGS.md#no-effect"}]}]}"##;

pub(super) const REAL_LINT_CLEAN: &[u8] =
    br#"{"success":true,"files":[{"filename":"a/b/clean.bzl","formatted":true,"valid":true,"warnings":[]}]}"#;

pub(super) fn parse_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(BUILDIFIER_MANIFEST).expect("format/bazel manifest must parse");
    assert_eq!(package.id, "format/bazel");
    assert_eq!(package.runtime, "declarative-v1");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

pub(super) fn parse_lint_bazel_package() -> ExternalCheckDeclarativePackage {
    let package = parse_declarative_check_manifest(LINT_BAZEL_MANIFEST).expect("lint/bazel manifest must parse");
    assert_eq!(package.id, "lint/bazel");
    assert_eq!(package.runtime, "declarative-v1");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// Unwrap a tool-kind invocation's fields for assertion convenience.
pub(super) fn tool(invocation: &Invocation) -> &ToolInvocation {
    match &invocation.kind {
        InvocationKind::Tool(tool) => tool,
        other => panic!("expected tool invocation, got {other:?}"),
    }
}

pub(super) fn declarative_format_findings(stdout: &[u8]) -> Vec<Finding> {
    let package = parse_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(0), None, None)
        .expect("format transform")
}

pub(super) fn declarative_lint_findings(stdout: &[u8], input_file: &str) -> Vec<Finding> {
    let package = parse_lint_bazel_package();
    package.invocations[0]
        .transform
        .apply(stdout, Some(0), Some(input_file), None)
        .expect("lint transform")
}

pub(super) fn workspace_root() -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("MODULE.bazel").exists() {
            return dir;
        }
        if !dir.pop() {
            panic!("could not locate MODULE.bazel above CARGO_MANIFEST_DIR");
        }
    }
}

pub(super) fn spike_e2e_enabled() -> bool {
    std::env::var("CHECKLEFT_SPIKE_E2E").is_ok()
}

/// A changeset with one file of each given path so tests can verify glob selection.
pub(super) fn changeset_with_files(paths: &[&str]) -> ChangeSet {
    ChangeSet::new(
        paths
            .iter()
            .map(|p| ChangedFile {
                path: std::path::PathBuf::from(p),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    )
}

pub(super) fn make_changeset(paths: &[&str]) -> ChangeSet {
    ChangeSet::new(
        paths
            .iter()
            .map(|p| ChangedFile {
                path: Path::new(p).to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    )
}

#[cfg(unix)]
pub(super) fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}
