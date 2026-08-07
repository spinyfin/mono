//! File-selection tests for the declarative runtime: how a package's
//! definition `applies_to`, the framework's per-instance `PathScope`
//! (check-entry `applies_to` ∩ effective exclude set), and `skip_symlinks`
//! together decide which changed files reach a tool, and how
//! `eligible_file_count` reports that selection for progress display.
//!
//! Per decision 2 of the unify-include-side design, the check-entry
//! `applies_to` INTERSECTS the definition's `applies_to` — it never replaces
//! it. These tests exercise that composition directly via [`PathScope`].

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_external_check_package_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::path_scope::PathScope;

use super::ExternalCheckDeclarativePackage;
use super::tests_common::{changeset_with_files, make_changeset};

// ── select_files: definition ∩ entry composition ───────────────────────────

/// Task 3: `select_files` subtracts the framework exclude set after the
/// positive `applies_to` filter, so an excluded file never reaches the
/// `{{files}}` list.
#[test]
fn select_files_subtracts_excludes_after_applies_to() {
    let changeset = changeset_with_files(&["src/a.rs", "vendor/dep.rs", "src/b.rs"]);
    let path_scope = PathScope::exclude_only(&["vendor/**".to_owned()]).expect("scope");

    let files = super::executor::select_files(Path::new(""), &changeset, &["**/*.rs".to_owned()], false, &path_scope)
        .expect("select_files");

    assert_eq!(files, vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()]);
}

/// Task 3: excludes always win — a file matched by the definition `applies_to`
/// is still removed when it matches an exclude, so the two compose as a
/// second, subtractive stage.
#[test]
fn select_files_excludes_win_over_applies_to_selection() {
    let changeset = changeset_with_files(&["src/keep.rs", "src/generated/out.rs"]);
    let path_scope = PathScope::exclude_only(&["**/generated/**".to_owned()]).expect("scope");

    let files = super::executor::select_files(Path::new(""), &changeset, &["src/**".to_owned()], false, &path_scope)
        .expect("select_files");

    assert_eq!(files, vec!["src/keep.rs".to_owned()]);
}

/// Task 3: an empty (default) path scope subtracts nothing — the definition's
/// positive `applies_to` set is returned unchanged.
#[test]
fn select_files_with_empty_scope_keeps_all_applies_to_matches() {
    let changeset = changeset_with_files(&["src/a.rs", "vendor/dep.rs"]);
    let files = super::executor::select_files(
        Path::new(""),
        &changeset,
        &["**/*.rs".to_owned()],
        false,
        &PathScope::default(),
    )
    .expect("select_files");

    assert_eq!(files, vec!["src/a.rs".to_owned(), "vendor/dep.rs".to_owned()]);
}

#[test]
fn applies_to_override_replaces_definition_glob() {
    // The package applies_to is ["**/*.bzl"]. The config override sets ["**/*.rs"].
    // A changeset with a .rs file should now be selected, while a .bzl file should not.
    let config: toml::Value = toml::from_str(r#"applies_to = ["**/*.rs"]"#).unwrap();
    let globs = super::resolve::override_applies_to(&config)
        .expect("override must be present")
        .expect("override must be valid");
    assert_eq!(globs, vec!["**/*.rs"]);
}

#[test]
fn applies_to_override_absent_falls_back_to_definition() {
    // No `applies_to` key in config → override_applies_to returns None.
    let config: toml::Value = toml::from_str(r#"needs.tool.path = "x""#).unwrap();
    let result = super::resolve::override_applies_to(&config);
    assert!(result.is_none(), "absent override must return None");
}

#[test]
fn applies_to_override_empty_list_is_rejected() {
    let config: toml::Value = toml::from_str("applies_to = []").unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must not be empty"),
        "empty list must be rejected; got: {err:#}"
    );
}

#[test]
fn applies_to_override_non_list_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = "**/*.rs""#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must be a list"),
        "scalar value must be rejected; got: {err:#}"
    );
}

#[test]
fn applies_to_override_empty_string_entry_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = [""]"#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    assert!(
        err.to_string().contains("must not be empty"),
        "empty string entry must be rejected; got: {err:#}"
    );
}

// ── structurally-empty override patterns (case a) ──────────────────────────────
//
// A pattern in this shape can never match any changeset path, in any repo,
// decided from the text alone — distinct from a pattern that is merely
// typo'd/wrong-case (case b, `srcc/**`/`SRC/**` below) which must NOT error.

#[test]
fn applies_to_override_leading_dot_slash_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = ["./src/*.rs"]"#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("./src/*.rs"),
        "must name the pattern verbatim; got: {message}"
    );
    assert!(
        message.contains("applies_to[0]"),
        "must name the key/position; got: {message}"
    );
    assert!(message.contains("./"), "must explain why; got: {message}");
}

#[test]
fn applies_to_override_negation_prefix_is_rejected_and_points_at_exclude() {
    let config: toml::Value = toml::from_str(r#"applies_to = ["!src/**"]"#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("!src/**"),
        "must name the pattern verbatim; got: {message}"
    );
    assert!(
        message.contains("exclude"),
        "must point at the `exclude` key; got: {message}"
    );
}

#[test]
fn applies_to_override_trailing_separator_is_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = ["src/"]"#).unwrap();
    let err = super::resolve::override_applies_to(&config)
        .expect("override present")
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("src/"),
        "must name the pattern verbatim; got: {message}"
    );
    assert!(message.contains("separator"), "must explain why; got: {message}");
}

#[test]
fn applies_to_override_typo_case_is_not_rejected() {
    let config: toml::Value = toml::from_str(r#"applies_to = ["srcc/**"]"#).unwrap();
    let globs = super::resolve::override_applies_to(&config)
        .expect("override must be present")
        .expect("typo'd-but-matchable pattern must not be rejected");
    assert_eq!(globs, vec!["srcc/**"]);
    let config: toml::Value = toml::from_str(r#"applies_to = ["SRC/**"]"#).unwrap();
    super::resolve::override_applies_to(&config)
        .expect("override must be present")
        .expect("wrong-case pattern must not be rejected");
}

/// End-to-end test: config applies_to override restricts file selection so only
/// matching files are checked. The package definition matches `**/*.bzl`; the
/// config override changes it to `**/*.rs`. A .rs file should produce findings;
/// the .bzl file should be skipped (→ empty result, no invocation attempted).
/// Decision 2: the check-entry `applies_to` (here modeled directly as the
/// `path_scope`'s include side) INTERSECTS the definition's `applies_to`
/// rather than replacing it. The definition selects `**/*.bzl`; the entry
/// narrows to `frontend/**`. Only a file matching BOTH is selected.
#[test]
fn entry_applies_to_intersects_definition_applies_to() {
    let changeset = changeset_with_files(&["frontend/BUILD.bzl", "backend/BUILD.bzl", "frontend/app.rs"]);
    let path_scope = PathScope::new(&["frontend/**".to_owned()], &[]).expect("scope");

    let files = super::executor::select_files(Path::new(""), &changeset, &["**/*.bzl".to_owned()], false, &path_scope)
        .expect("select_files");

    // backend/BUILD.bzl matches the definition but not the entry scope; frontend/app.rs
    // matches the entry scope but not the definition. Only the intersection survives.
    assert_eq!(files, vec!["frontend/BUILD.bzl".to_owned()]);
}

/// A check-entry `applies_to` that names a file type the definition does not
/// cover can never widen selection — intersecting with an empty overlap
/// selects nothing, it does not fall back to either side alone.
#[test]
fn entry_applies_to_cannot_widen_beyond_definition() {
    let changeset = changeset_with_files(&["src/main.rs", "backend/lib.rs"]);
    // Entry narrows to frontend/**, but the changeset has no frontend files at all —
    // and even if it did, the definition only selects .bzl, so intersection is empty.
    let path_scope = PathScope::new(&["frontend/**".to_owned()], &[]).expect("scope");

    let files = super::executor::select_files(Path::new(""), &changeset, &["**/*.bzl".to_owned()], false, &path_scope)
        .expect("select_files");

    assert!(
        files.is_empty(),
        "no file satisfies both definition and entry scope; got: {files:?}"
    );
}

/// With no entry-side `applies_to` (a universal `PathScope`), the definition's
/// own `applies_to` is what applies, unnarrowed.
#[test]
fn no_entry_applies_to_uses_definition_glob_unnarrowed() {
    let changeset = changeset_with_files(&["src/main.rs", "a/b/BUILD.bzl"]);
    let files = super::executor::select_files(
        Path::new(""),
        &changeset,
        &["**/*.bzl".to_owned()],
        false,
        &PathScope::default(),
    )
    .expect("select_files");

    assert_eq!(files, vec!["a/b/BUILD.bzl".to_owned()]);
}

/// End-to-end test through the public [`run_declarative_check`] entry point
/// (which always runs with a universal `PathScope`, no entry-side narrowing):
/// the definition's own `applies_to` decides selection.
#[test]
#[cfg(unix)]
fn definition_applies_to_end_to_end_selects_matching_files() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    let script_path = temp.path().join("emit_one.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nprintf '%s' '{\"findings\":[{\"severity\":\"warning\",\"message\":\"selected\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}]}'\n",
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = applies_to_test_package(&script_path.to_string_lossy());
    let changeset = changeset_with_files(&["src/main.rs", "BUILD.bzl"]);

    let result = super::run_declarative_check(
        temp.path(),
        "test-check",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run succeeds");

    assert_eq!(
        result.findings.len(),
        1,
        "definition applies_to must select the .bzl file; got: {:#?}",
        result.findings
    );
    assert_eq!(result.findings[0].message, "selected");
}

/// Build a minimal declarative package that matches only `**/*.bzl` files,
/// wired to a shell script that immediately succeeds and emits one finding.
#[cfg(unix)]
fn applies_to_test_package(script_path: &str) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test-check"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.bzl"]

[needs.tool.default]
path = "{script_path}"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "findings", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#
    );
    let package = parse_external_check_package_manifest(&manifest).expect("test manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

// ── skip_symlinks flag ─────────────────────────────────────────────────────────

/// Build a minimal per_file declarative manifest wired to a fake script, with
/// skip_symlinks controlled by the caller. The script always exits 2 (which maps
/// to `default → error`) when invoked, so the test can tell whether the file was
/// selected (error propagated) or skipped (empty result returned early).
#[cfg(unix)]
fn skip_symlinks_package(script: &Path, skip_symlinks: bool) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test-skip-symlinks"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.md"]
skip_symlinks = {skip_symlinks}

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "run"
run = "tool"
mode = "per_file"
args = ["{{{{file}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "hit"
"#,
        script = script.display(),
    );
    let package = parse_external_check_package_manifest(&manifest).expect("test manifest must parse");
    match package.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn skip_symlinks_true_excludes_symlinked_file() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");

    // Real file.
    std::fs::write(repo_root.path().join("AGENTS.md"), "# Agents\n").expect("write real file");
    // Symlink pointing at the real file (like CLAUDE.md -> AGENTS.md in mono).
    std::os::unix::fs::symlink("AGENTS.md", repo_root.path().join("CLAUDE.md")).expect("create symlink");

    // Script that logs each invocation's file arg, then exits 0 (ok).
    // Verifying CLAUDE.md is absent from the log confirms it was filtered out.
    let script_path2 = repo_root.path().join("count.sh");
    std::fs::write(&script_path2, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms2 = std::fs::metadata(&script_path2).expect("metadata").permissions();
    perms2.set_mode(0o755);
    std::fs::set_permissions(&script_path2, perms2).expect("chmod");

    let package = skip_symlinks_package(&script_path2, true);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("AGENTS.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("CLAUDE.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);
    let result = super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run with skip_symlinks=true must succeed");

    // No findings expected (script exits 0).
    assert!(
        result.findings.is_empty(),
        "skip_symlinks=true with exit-0 script must produce no findings; got: {:#?}",
        result.findings
    );

    // Verify CLAUDE.md was NOT passed to the script by reading the log.
    let log_path = repo_root.path().join("count.sh.log");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("CLAUDE.md"),
        "CLAUDE.md is a symlink and must be skipped with skip_symlinks=true; log: {log}"
    );
    assert!(
        log.contains("AGENTS.md"),
        "AGENTS.md is a real file and must still be checked; log: {log}"
    );
}

#[cfg(unix)]
#[test]
fn skip_symlinks_false_includes_symlinked_file() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");
    std::fs::write(repo_root.path().join("AGENTS.md"), "# Agents\n").expect("write real file");
    std::os::unix::fs::symlink("AGENTS.md", repo_root.path().join("CLAUDE.md")).expect("create symlink");

    let script_path = repo_root.path().join("count.sh");
    std::fs::write(&script_path, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = skip_symlinks_package(&script_path, false);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("AGENTS.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("CLAUDE.md"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run with skip_symlinks=false must succeed");

    assert!(
        result.findings.is_empty(),
        "exit-0 script must produce no findings; got: {:#?}",
        result.findings
    );

    let log = std::fs::read_to_string(repo_root.path().join("count.sh.log")).unwrap_or_default();
    assert!(
        log.contains("CLAUDE.md"),
        "with skip_symlinks=false, CLAUDE.md (symlink) must still be passed to the tool; log: {log}"
    );
    assert!(
        log.contains("AGENTS.md"),
        "AGENTS.md must be passed to the tool; log: {log}"
    );
}

#[cfg(unix)]
#[test]
fn real_non_symlink_file_always_included_regardless_of_flag() {
    use std::os::unix::fs::PermissionsExt;

    let repo_root = tempfile::tempdir().expect("temp repo root");
    std::fs::write(repo_root.path().join("README.md"), "# Hello\n").expect("write file");

    let script_path = repo_root.path().join("count.sh");
    std::fs::write(&script_path, "#!/bin/sh\necho \"$1\" >> \"$0.log\"\nexit 0\n").expect("write count script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    let package = skip_symlinks_package(&script_path, true);
    let changeset = ChangeSet::new(vec![ChangedFile {
        path: std::path::PathBuf::from("README.md"),
        kind: ChangeKind::Modified,
        old_path: None,
    }]);

    super::run_declarative_check(
        repo_root.path(),
        "test-skip-symlinks",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("run must succeed");

    let log = std::fs::read_to_string(repo_root.path().join("count.sh.log")).unwrap_or_default();
    assert!(
        log.contains("README.md"),
        "README.md is a real file and must be included even with skip_symlinks=true; log: {log}"
    );
}

// ── eligible_file_count ──────────────────────────────────────────────────────

fn declarative_package_with_applies_to(applies_to: &[&str]) -> ExternalCheckDeclarativePackage {
    let applies_to_yaml = applies_to
        .iter()
        .map(|p| format!("  - \"{p}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        r#"id: test-check
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to:
{applies_to_yaml}
needs:
  tool:
    default:
      path: "check-tool"
invocations:
  - id: run
    run: tool
    mode: batch
    args: ["{{{{files}}}}"]
    exit:
      "0": ok
      default: error
    transform:
      kind: passthrough
"#
    );
    let pkg = crate::external::parse_declarative_check_manifest(&manifest).expect("valid manifest");
    match pkg.implementation {
        ExternalCheckPackageImplementation::Declarative(d) => d,
        _ => panic!("expected declarative"),
    }
}

#[test]
fn eligible_file_count_filters_by_applies_to_glob() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*.rs"]);
    let changeset = make_changeset(&["a.rs", "b.rs", "c.ts", "BUILD", "d.rs"]);

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset);
    assert_eq!(count, 3, "only .rs files should be counted; got {count}");
}

#[test]
fn eligible_file_count_multi_glob_union() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/BUILD", "**/*.bzl", "**/BUILD.bazel"]);
    let changeset = make_changeset(&[
        "src/main.rs",
        "BUILD",
        "tools/defs.bzl",
        "package/BUILD.bazel",
        "README.md",
    ]);

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset);
    assert_eq!(count, 3, "BUILD + .bzl + BUILD.bazel only; got {count}");
}

#[test]
fn eligible_file_count_all_files_check_returns_full_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*"]);
    let changeset = make_changeset(&["a.rs", "b.ts", "c.md", "BUILD"]);

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset);
    assert_eq!(count, 4, "all-files check must return the full count; got {count}");
}

#[test]
fn eligible_file_count_no_matching_files_returns_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*.java"]);
    let changeset = make_changeset(&["a.rs", "b.ts", "BUILD"]);

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset);
    assert_eq!(count, 0, "no .java files; got {count}");
}

/// `eligible_file_count` assumes the caller has already narrowed `changeset` by
/// the framework `PathScope` (see the runner's scope-filtered seed). Passing an
/// already-narrowed changeset here further narrows only by the definition's own
/// `applies_to`.
#[test]
fn eligible_file_count_counts_only_within_a_pre_scoped_changeset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*.bzl"]);
    // Simulate the runner having already dropped a file outside the entry-side scope.
    let changeset = make_changeset(&["a/b/BUILD.bzl"]);

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset);
    assert_eq!(count, 1, "the one pre-scoped .bzl file should be counted; got {count}");
}
