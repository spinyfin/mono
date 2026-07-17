//! File-selection tests for the declarative runtime: how `applies_to` (and its
//! per-repo CHECKS override) plus the framework exclude set and `skip_symlinks`
//! decide which changed files reach a tool, and how `eligible_file_count`
//! reports that selection for progress display.

use std::path::Path;

use crate::external::{ExternalCheckPackageImplementation, parse_external_check_package_manifest};
use crate::input::{ChangeKind, ChangeSet, ChangedFile};

use super::ExternalCheckDeclarativePackage;
use super::tests_common::{changeset_with_files, make_changeset};

// ── per-repo applies_to override via CHECKS.yaml config blob ──────────────────

/// Build a minimal declarative package that matches only `**/*.bzl` files,
/// wired to a shell script that immediately fails (so we can observe whether
/// `run_declarative_check` selected the file at all — if it short-circuits with
/// an empty result it means the file was NOT selected).
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

/// Task 3: `select_files` subtracts the framework exclude set after the positive
/// `applies_to` filter, so an excluded file never reaches the `{{files}}` list.
#[test]
fn select_files_subtracts_excludes_after_applies_to() {
    let changeset = changeset_with_files(&["src/a.rs", "vendor/dep.rs", "src/b.rs"]);
    let exclusion = crate::exclusion_matcher::ExclusionMatcher::new(&["vendor/**".to_owned()]).expect("matcher");

    let files = super::executor::select_files(Path::new(""), &changeset, &["**/*.rs".to_owned()], false, &exclusion)
        .expect("select_files");

    assert_eq!(files, vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()]);
}

/// Task 3: excludes always win — a file matched by `applies_to` (or its override)
/// is still removed when it matches an exclude, so the two compose as a second,
/// subtractive stage.
#[test]
fn select_files_excludes_win_over_applies_to_selection() {
    let changeset = changeset_with_files(&["src/keep.rs", "src/generated/out.rs"]);
    // `applies_to` (here standing in for a per-repo override) positively selects both
    // files; the exclude then subtracts the generated one.
    let exclusion = crate::exclusion_matcher::ExclusionMatcher::new(&["**/generated/**".to_owned()]).expect("matcher");

    let files = super::executor::select_files(Path::new(""), &changeset, &["src/**".to_owned()], false, &exclusion)
        .expect("select_files");

    assert_eq!(files, vec!["src/keep.rs".to_owned()]);
}

/// Task 3: an empty exclusion matcher subtracts nothing — the positive `applies_to`
/// set is returned unchanged.
#[test]
fn select_files_with_empty_matcher_keeps_all_applies_to_matches() {
    let changeset = changeset_with_files(&["src/a.rs", "vendor/dep.rs"]);
    let files = super::executor::select_files(
        Path::new(""),
        &changeset,
        &["**/*.rs".to_owned()],
        false,
        &crate::exclusion_matcher::ExclusionMatcher::default(),
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

/// End-to-end test: config applies_to override restricts file selection so only
/// matching files are checked. The package definition matches `**/*.bzl`; the
/// config override changes it to `**/*.rs`. A .rs file should produce findings;
/// the .bzl file should be skipped (→ empty result, no invocation attempted).
#[test]
#[cfg(unix)]
fn applies_to_override_end_to_end_restricts_selection() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script that emits one finding for any file passed to it.
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

    // Config override: only match .rs files, not .bzl.
    let config: toml::Value = toml::from_str(r#"applies_to = ["**/*.rs"]"#).unwrap();

    // Changeset has one .rs file and one .bzl file.
    let changeset = changeset_with_files(&["src/main.rs", "BUILD.bzl"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    // The .rs file was selected (→ one finding). The .bzl file was excluded by the override.
    assert_eq!(
        result.findings.len(),
        1,
        "override applies_to must select only .rs file; got: {:#?}",
        result.findings
    );
    assert_eq!(result.findings[0].message, "selected");
}

#[test]
#[cfg(unix)]
fn applies_to_no_override_uses_definition_glob() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script that emits one finding for any file.
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

    // No applies_to override — definition's ["**/*.bzl"] applies.
    let config: toml::Value = toml::Value::Table(Default::default());
    let changeset = changeset_with_files(&["src/main.rs", "a/b/BUILD.bzl"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    // Only the .bzl file matches; .rs is skipped.
    assert_eq!(
        result.findings.len(),
        1,
        "without override, definition applies_to selects only .bzl; got: {:#?}",
        result.findings
    );
}

#[test]
#[cfg(unix)]
fn applies_to_override_all_files_skipped_returns_empty() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script emits one finding.
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

    // Override: only frontend/**. Changeset has no frontend files → nothing selected.
    let config: toml::Value = toml::from_str(r#"applies_to = ["frontend/**"]"#).unwrap();
    let changeset = changeset_with_files(&["src/main.rs", "backend/lib.rs"]);

    let result = super::run_declarative_check(temp.path(), "test-check", &package, &changeset, &config, None)
        .expect("run succeeds");

    assert!(
        result.findings.is_empty(),
        "no files match override glob → no findings; got: {:#?}",
        result.findings
    );
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
    let config = toml::Value::Table(toml::map::Map::new());

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset, &config);
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
    let config = toml::Value::Table(toml::map::Map::new());

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset, &config);
    assert_eq!(count, 3, "BUILD + .bzl + BUILD.bazel only; got {count}");
}

#[test]
fn eligible_file_count_all_files_check_returns_full_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*"]);
    let changeset = make_changeset(&["a.rs", "b.ts", "c.md", "BUILD"]);
    let config = toml::Value::Table(toml::map::Map::new());

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset, &config);
    assert_eq!(count, 4, "all-files check must return the full count; got {count}");
}

#[test]
fn eligible_file_count_no_matching_files_returns_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pkg = declarative_package_with_applies_to(&["**/*.java"]);
    let changeset = make_changeset(&["a.rs", "b.ts", "BUILD"]);
    let config = toml::Value::Table(toml::map::Map::new());

    let count = super::executor::eligible_file_count(temp.path(), &pkg, &changeset, &config);
    assert_eq!(count, 0, "no .java files; got {count}");
}
