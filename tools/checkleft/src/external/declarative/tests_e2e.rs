//! Gated end-to-end tests against a real buildifier binary. These are behind
//! `CHECKLEFT_SPIKE_E2E=1` because they resolve buildifier via `bazel build` /
//! `bazel cquery`, which cannot run inside the hermetic test sandbox. They
//! exercise the production *bazel resolver* path manually — hermetic e2e parity
//! lives in the sibling [`super::parity_e2e`] module.

use std::path::Path;

use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Finding;

use super::tests_common::{parse_package, spike_e2e_enabled, workspace_root};

// Byte-identical to tests/fixtures/buildifier/malformed.bzl.fixture (inlined so the
// lib test stays hermetic under bazel — include_str! of a non-src file would need
// the fixture added to the test target's compile_data). Line 11 = the `def` (module
// docstring + unused `ctx`), line 12 = the no-effect expression.
const FIXTURE: &str = r#"# This file is intentionally malformed for testing the buildifier check.
#
# buildifier --lint=warn flags it for:
#   - module-docstring: no module-level docstring (must be first statement)
#   - function-docstring: _impl has no docstring
#   - no-effect: the string concatenation on line 12 produces a value that is discarded
#
# buildifier --mode=check also flags the formatting issues below
# (e.g. trailing whitespace, argument style).

def _my_rule_impl(ctx):
    "unused" + "string"
    return []

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {},
)
"#;

#[test]
fn e2e_bazel_resolver_resolves_buildifier() {
    if !spike_e2e_enabled() {
        return;
    }
    // Exercises the framework-owned bazel resolver.
    let root = workspace_root();
    let resolved = super::resolve::resolve_bazel_target_executable(&root, "@buildifier_prebuilt//:buildifier")
        .expect("bazel must resolve buildifier");
    assert!(
        resolved.exists(),
        "resolved buildifier path must exist: {}",
        resolved.display()
    );
}

#[test]
fn e2e_declarative_runs_buildifier_end_to_end() {
    if !spike_e2e_enabled() {
        return;
    }
    // Full pipeline: file selection -> binary resolution (path override to the
    // bazel-resolved buildifier) -> invocations -> exit semantics -> transform.
    let root = workspace_root();
    let buildifier = super::resolve::resolve_bazel_target_executable(&root, "@buildifier_prebuilt//:buildifier")
        .expect("resolve buildifier");

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
    std::fs::write(temp.path().join("a/b/malformed.bzl"), FIXTURE).unwrap();
    std::fs::write(temp.path().join("a/b/clean.bzl"), "\"\"\"clean.\"\"\"\n").unwrap();

    let package = parse_package();
    let config: toml::Value =
        toml::from_str(&format!("[needs.buildifier]\npath = \"{}\"\n", buildifier.display())).unwrap();

    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: Path::new("a/b/malformed.bzl").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: Path::new("a/b/clean.bzl").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(temp.path(), "buildifier", &package, &changeset, &config, None)
        .expect("declarative run");

    // The fixture is format-clean but has 3 lint warnings.
    let lint: Vec<&Finding> = result
        .findings
        .iter()
        .filter(|f| f.location.as_ref().map(|l| l.line.is_some()).unwrap_or(false))
        .collect();
    assert_eq!(lint.len(), 3, "expected 3 lint findings, got {:#?}", result.findings);

    let messages: Vec<&str> = lint.iter().map(|f| f.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("module-docstring")),
        "expected module-docstring; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("unused-variable")),
        "expected unused-variable; got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("no-effect")),
        "expected no-effect; got {messages:?}"
    );
}
