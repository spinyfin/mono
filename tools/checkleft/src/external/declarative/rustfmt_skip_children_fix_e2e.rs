//! Hermetic end-to-end regression test for `checkleft fix` on `format/rust`
//! when the target file declares an out-of-line module whose sibling file is
//! absent from the fix sandbox.
//!
//! `WritableSandbox::stage` (`src/fix/safety.rs:91-125`) stages exactly the
//! fixable set — an unmodified sibling declared via `mod foo;` is never
//! staged. Without `--config=skip_children=true` on the fix invocation
//! (`checks/format/rust.yaml`), rustfmt tries to resolve every `mod`
//! declaration from disk and hard-aborts with "failed to resolve mod ...
//! does not exist", leaving the target file unfixed. This test drives the
//! real `run_declarative_fix` entry point — the same one the `checkleft fix`
//! CLI path calls — against the actual committed manifest and a real,
//! pinned-toolchain rustfmt, to prove the fix succeeds and produces bytes
//! governed by the repo's `rustfmt.toml` (not rustfmt defaults).
//!
//! `skip_children` is unstable and only honoured via the CLI `--config=`
//! override loophole on a *stable* toolchain. On a nightly/beta rustfmt,
//! unstable options are accepted unconditionally, so the fix would succeed
//! regardless of whether the stable-channel loophole is open — the guard
//! would become vacuous. `fix_succeeds_on_file_declaring_absent_sibling_module`
//! therefore asserts the staged, hermetic `@rules_rust` toolchain rustfmt
//! (not the host `rustfmt`) actually reports a `-stable` version before
//! trusting the fix outcome, so this test fails loudly — rather than
//! passing vacuously — the moment the pinned toolchain drifts off stable.

use std::path::{Path, PathBuf};

use crate::exclusion_matcher::ExclusionMatcher;
use crate::external::{ExternalCheckPackageImplementation, parse_declarative_check_manifest};
use crate::source_tree::LocalSourceTree;

use super::ExternalCheckDeclarativePackage;
use super::executor::{DeclarativeFixContext, run_declarative_fix};

/// The committed format/rust manifest — the actual thing shipped to CI.
const RUSTFMT_MANIFEST: &str = include_str!("../../../checks/format/rust.yaml");

/// Repo-root `rustfmt.toml` used by the real repo: `edition = "2024"`,
/// `max_width = 120`. Deliberately not the default `max_width = 100` so the
/// test can distinguish "rustfmt ran with this config" from "rustfmt ran
/// with its built-in defaults" (see the max-width probe below).
const RUSTFMT_TOML: &str = "edition = \"2024\"\nmax_width = 120\n";

/// Deliberately unformatted, with a signature line whose length (115 chars)
/// sits strictly between the default `max_width` (100, which would force
/// rustfmt to break the argument list across multiple lines) and the repo's
/// configured `max_width` (120, under which it fits on one line). Declares
/// `mod sibling;` whose target file (`sibling.rs`) is never created anywhere
/// in the source tree — reproducing the absent-module-file bug exactly, since
/// the fix sandbox only ever contains the staged fixable set regardless of
/// whether the sibling exists in the real tree.
const UNFORMATTED_SOURCE_WITH_ABSENT_MODULE: &str = "mod sibling;\nfn probe_configured_max_width_value_for_rustfmt_config_path(alpha:i32,beta:i32,gamma:i32,delta:i32)->i32{alpha+beta+gamma+delta}\n";

/// The one-line form of the signature that only fits under `max_width = 120`.
const ONE_LINE_SIGNATURE: &str = "fn probe_configured_max_width_value_for_rustfmt_config_path(alpha: i32, beta: i32, gamma: i32, delta: i32) -> i32 {";

/// Resolve the real, hermetic-toolchain rustfmt from the test's runfiles, or
/// `None` when not staged (e.g. plain `cargo test`). Outside Bazel this is a
/// no-op, but under `bazel test` a missing `CHECKLEFT_E2E_RUSTFMT` is a broken
/// `data`/`env` wiring, not a silently-skipped check, so it asserts rather
/// than swallows. See [`crate::external::test_support::staged_path_from_runfiles`],
/// which this shares with `parity_e2e::buildifier_from_runfiles`.
fn hermetic_rustfmt_from_runfiles() -> Option<PathBuf> {
    crate::external::test_support::staged_path_from_runfiles(
        "CHECKLEFT_E2E_RUSTFMT",
        "rustfmt wrapper",
        crate::external::test_support::StagedPathKind::File,
    )
}

fn parse_rustfmt_package() -> ExternalCheckDeclarativePackage {
    match parse_declarative_check_manifest(RUSTFMT_MANIFEST)
        .expect("format/rust manifest must parse")
        .implementation
    {
        ExternalCheckPackageImplementation::Declarative(declarative) => declarative,
        other => panic!("expected declarative implementation, got {other:?}"),
    }
}

/// Config table overriding `needs.rustfmt.path` to the resolved hermetic binary,
/// the same override shape `parity_e2e::run_declarative` uses for buildifier.
fn config_with_rustfmt_path(rustfmt: &Path) -> toml::Value {
    let mut path_table = toml::value::Table::new();
    path_table.insert(
        "path".to_owned(),
        toml::Value::String(rustfmt.to_string_lossy().into_owned()),
    );
    let mut needs_table = toml::value::Table::new();
    needs_table.insert("rustfmt".to_owned(), toml::Value::Table(path_table));
    let mut config = toml::value::Table::new();
    config.insert("needs".to_owned(), toml::Value::Table(needs_table));
    toml::Value::Table(config)
}

#[tokio::test]
async fn fix_succeeds_on_file_declaring_absent_sibling_module() {
    let Some(rustfmt) = hermetic_rustfmt_from_runfiles() else {
        return;
    };

    let version_output = std::process::Command::new(&rustfmt)
        .arg("--version")
        .output()
        .expect("staged rustfmt wrapper must run --version");
    let version = String::from_utf8_lossy(&version_output.stdout).trim().to_owned();
    eprintln!("checkleft: staged hermetic rustfmt reports: {version}");
    assert!(
        version.contains("-stable"),
        "the `--config=skip_children=true` loophole this test guards is a stable-channel-only \
         behaviour: an unstable/nightly/beta rustfmt would accept `skip_children` unconditionally, \
         making this test pass regardless of whether the loophole is actually open. Staged rustfmt \
         reported {version:?}, which is not a `-stable` build — the pinned toolchain has drifted off \
         stable and this guard needs re-evaluating, not silently trusting"
    );

    let repo_root = tempfile::tempdir().expect("temp repo root");
    std::fs::write(repo_root.path().join("rustfmt.toml"), RUSTFMT_TOML).expect("write rustfmt.toml");
    std::fs::create_dir(repo_root.path().join("src")).expect("create src dir");
    std::fs::write(
        repo_root.path().join("src").join("lib.rs"),
        UNFORMATTED_SOURCE_WITH_ABSENT_MODULE,
    )
    .expect("write unformatted lib.rs");
    // `sibling.rs` is deliberately never created — this is the bug's precondition.

    let package = parse_rustfmt_package();
    let source_tree = LocalSourceTree::new(repo_root.path()).expect("source tree over temp repo root");
    let config = config_with_rustfmt_path(&rustfmt);
    let exclusion = ExclusionMatcher::default();

    let outcomes = run_declarative_fix(
        DeclarativeFixContext {
            repo_root: repo_root.path(),
            check_id: "format/rust",
        },
        &package,
        &[PathBuf::from("src/lib.rs")],
        &source_tree,
        &config,
        &exclusion,
        |_| {},
    );

    assert_eq!(
        outcomes.len(),
        1,
        "expected one outcome for the `format` invocation; got {outcomes:#?}"
    );
    let outcome = &outcomes[0];
    assert_eq!(outcome.invocation_id, "format");
    assert!(
        outcome.error.is_none(),
        "fix must succeed even though `mod sibling;` has no file on disk anywhere; got error: {:?}",
        outcome.error
    );
    assert!(
        outcome.per_file_errors.is_empty(),
        "expected no per-file errors; got {:?}",
        outcome.per_file_errors
    );
    assert_eq!(
        outcome.applied,
        vec![PathBuf::from("src/lib.rs")],
        "only the staged target file should be written back"
    );

    let fixed = std::fs::read_to_string(repo_root.path().join("src").join("lib.rs")).expect("read fixed lib.rs");
    assert!(
        fixed.contains("mod sibling;"),
        "fixed file must still declare the module; got:\n{fixed}"
    );
    assert!(
        fixed.contains(ONE_LINE_SIGNATURE),
        "signature must stay on one line under the repo's max_width = 120 (would wrap under the \
         default max_width = 100, proving --config-path was honoured); got:\n{fixed}"
    );

    assert!(
        !repo_root.path().join("src").join("sibling.rs").exists(),
        "the absent sibling module must never be materialised by the fix"
    );
}
