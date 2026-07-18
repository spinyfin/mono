//! Invocation-execution tests for the declarative runtime: batch chunking under
//! the ARG_MAX guard, per_file error isolation (one file erroring must not
//! suppress other files' findings), and progress-tick emission for per_file and
//! batch checks.

use std::path::Path;

use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Severity;

use super::ExternalCheckDeclarativePackage;
use super::tests_common::{make_changeset, write_executable};

// ── batch chunking (ARG_MAX guard) ────────────────────────────────────────────────

#[test]
fn split_files_into_chunks_single_chunk_when_under_threshold() {
    // All files fit comfortably — must produce exactly one chunk.
    let files: Vec<String> = (0..10).map(|i| format!("src/file_{i}.rs")).collect();
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), 10);
}

#[test]
fn split_files_into_chunks_empty_files_returns_one_empty_chunk() {
    let files: Vec<String> = vec![];
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[test]
fn split_files_into_chunks_splits_when_over_threshold() {
    // Use a very small threshold by making the fixed_cost consume almost all of it.
    // Each file "a.rs" costs 5 bytes (4 chars + 1 null). The available budget is
    // ARG_BYTE_SAFE_THRESHOLD - fixed_cost. By setting fixed_cost to
    // ARG_BYTE_SAFE_THRESHOLD - 8, we leave room for 1 file (5 bytes) in the first
    // chunk but not the second (10 bytes total), forcing a split.
    let available_for_files = 8usize; // room for ~1 file of 5 bytes + overflow at 2
    let fixed_cost = super::executor::ARG_BYTE_SAFE_THRESHOLD - available_for_files;
    let files: Vec<String> = vec!["a.rs".to_owned(), "b.rs".to_owned(), "c.rs".to_owned()];
    // Each "a.rs" / "b.rs" / "c.rs" costs 5 bytes. With 8 bytes available:
    //   chunk 1: a.rs (5 bytes used, 3 left; b.rs would need 5 → exceeds 8) → [a.rs]
    //   chunk 2: b.rs (5 bytes) → [b.rs]
    //   chunk 3: c.rs (5 bytes) → [c.rs]
    let chunks = super::executor::split_files_into_chunks(fixed_cost, &files);
    assert!(
        chunks.len() >= 2,
        "files exceeding threshold must be split into multiple chunks; got {} chunk(s)",
        chunks.len()
    );
    // All files must be present across all chunks.
    let all_files: Vec<&String> = chunks.iter().flat_map(|c| c.iter()).collect();
    assert_eq!(all_files.len(), 3);
    assert_eq!(all_files[0], "a.rs");
    assert_eq!(all_files[1], "b.rs");
    assert_eq!(all_files[2], "c.rs");
}

#[test]
fn split_files_into_chunks_single_oversized_file_stays_in_own_chunk() {
    // When a single file alone exceeds the threshold there is no smaller unit;
    // it must still be processed (placed alone in its chunk).
    let file = "x".repeat(super::executor::ARG_BYTE_SAFE_THRESHOLD + 100);
    let files = vec![file.clone()];
    let chunks = super::executor::split_files_into_chunks(0, &files);
    assert_eq!(chunks.len(), 1, "oversized single file must produce exactly one chunk");
    assert_eq!(chunks[0][0], file);
}

/// End-to-end test: a batch invocation over many small files is chunked and
/// findings from all chunks are concatenated. The fake tool emits one finding per
/// file it receives, so the total finding count equals the file count regardless
/// of how many chunks are used.
#[test]
#[cfg(unix)]
fn batch_invocation_chunks_large_file_list_and_concatenates_findings() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");

    // Script: for each argument that is a known file path, emit one finding.
    // It receives the file list directly as argv entries (no flags before files).
    let script_path = temp.path().join("emit_per_arg.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
findings='{"findings":['
sep=''
for f in "$@"; do
  findings="${findings}${sep}{\"severity\":\"warning\",\"message\":\"found ${f}\",\"location\":null,\"remediations\":[],\"suggested_fix\":null}"
  sep=','
done
findings="${findings}]}"
printf '%s' "$findings"
"#,
    )
    .expect("write script");
    let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).expect("chmod");

    // Generate enough files with long paths so their combined argv cost exceeds the
    // 128 KB threshold. Each path is ~2600 bytes; 50 of them = ~130 KB, which
    // pushes past 128 KB and forces at least two invocations. The files don't need
    // to exist on disk: select_files matches against the changeset (not the
    // filesystem), and the fake script receives the paths purely as arguments.
    let long_name_prefix = "src/".to_owned() + &"a".repeat(2590);
    let file_names: Vec<String> = (0..50u32).map(|i| format!("{long_name_prefix}{i:04}.rs")).collect();

    let manifest = format!(
        r#"
id = "chunking-test"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.rs"]

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{{{{files}}}}"]
exit = {{ "0" = "findings", default = "error" }}

[invocations.transform]
kind = "passthrough"
"#,
        script = script_path.display()
    );

    let package = crate::external::parse_external_check_package_manifest(&manifest).expect("manifest parses");
    let declarative = match package.implementation {
        crate::external::ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    };

    let changeset = crate::input::ChangeSet::new(
        file_names
            .iter()
            .map(|name| crate::input::ChangedFile {
                path: std::path::PathBuf::from(name),
                kind: crate::input::ChangeKind::Modified,
                old_path: None,
            })
            .collect(),
    );

    let result = super::run_declarative_check(
        temp.path(),
        "chunking-test",
        &declarative,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("chunked batch run must succeed");

    assert_eq!(
        result.findings.len(),
        50,
        "findings from all chunks must be concatenated (one per file); got {:#?}",
        result.findings.len()
    );
}

// ── per_file error isolation ────────────────────────────────────────────────────
//
// These tests verify that a single per_file invocation error does NOT suppress
// other files' findings. One file erroring (default → error) must produce an
// error-severity finding for THAT file and let the loop continue.

/// Build a per_file declarative manifest backed by a fake script, with linelist
/// transform and exit semantics: 0=ok, 1=findings, default=error (so exit 2 = error).
#[cfg(unix)]
fn per_file_error_package(script: &Path) -> ExternalCheckDeclarativePackage {
    let manifest = format!(
        r#"
id = "test/per-file-error"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**"]

[needs.tool.default]
path = "{script}"

[[invocations]]
id = "check"
run = "tool"
mode = "per_file"
args = ["{{{{file}}}}"]
exit = {{ "0" = "ok", "1" = "findings", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "needs formatting"
"#,
        script = script.display(),
    );
    let package = crate::external::parse_external_check_package_manifest(&manifest)
        .expect("per_file error test manifest must parse");
    match package.implementation {
        crate::external::ExternalCheckPackageImplementation::Declarative(d) => d,
        other => panic!("expected declarative, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn per_file_error_isolates_to_file_does_not_abort_check() {
    // Three files: A errors (exit 2), B has findings (exit 1 + stdout), C is clean
    // (exit 0). The error on A must NOT suppress B's findings or C's clean result.
    // After the fix: result contains A's error finding AND B's formatting finding.

    let repo_root = tempfile::tempdir().expect("temp repo root");
    // Script: file_a → exit 2 (error); file_b → print filename + exit 1 (finding);
    // file_c → exit 0 (clean).
    let script_path = repo_root.path().join("per_file_tool.sh");
    write_executable(
        &script_path,
        "#!/bin/sh\ncase \"$1\" in\n  *file_a*) exit 2 ;;\n  *file_b*) echo \"$1\"; exit 1 ;;\n  *) exit 0 ;;\nesac\n",
    );

    let package = per_file_error_package(&script_path);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("file_a.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("file_b.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("file_c.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test/per-file-error",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("per_file error must return Ok with findings, not abort");

    // Expect exactly two findings: an error finding for file_a and a warning finding for file_b.
    assert_eq!(
        result.findings.len(),
        2,
        "expected error finding for file_a + formatting finding for file_b; got: {:#?}",
        result.findings
    );

    let error_findings: Vec<_> = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .collect();
    assert_eq!(
        error_findings.len(),
        1,
        "exactly one error finding (for file_a); got: {:#?}",
        error_findings
    );
    let ef = &error_findings[0];
    assert_eq!(
        ef.location.as_ref().map(|l| l.path.as_path()),
        Some(Path::new("file_a.ts")),
        "error finding must be scoped to file_a"
    );
    assert!(
        ef.message.contains("exit") || ef.message.contains("2"),
        "error finding must mention exit code; got: {}",
        ef.message
    );

    let warning_findings: Vec<_> = result
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .collect();
    assert_eq!(
        warning_findings.len(),
        1,
        "exactly one warning finding (for file_b); got: {:#?}",
        warning_findings
    );
    assert_eq!(
        warning_findings[0].location.as_ref().map(|l| l.path.as_path()),
        Some(Path::new("file_b.ts")),
        "warning finding must be for file_b"
    );
}

#[cfg(unix)]
#[test]
fn per_file_single_exit2_does_not_hide_other_files_findings() {
    // Regression for the prettier+symlink case: a single file that exits 2 must not
    // mask the findings from other files. Two files: the first exits 2 (error), the
    // second exits 1 with stdout output (finding). The result must contain the
    // formatting finding from the second file.
    let repo_root = tempfile::tempdir().expect("temp repo root");
    let script_path = repo_root.path().join("tool.sh");
    // First arg that contains "first" → exit 2; anything else → echo filename + exit 1.
    write_executable(
        &script_path,
        "#!/bin/sh\ncase \"$1\" in\n  *first*) exit 2 ;;\n  *) echo \"$1\"; exit 1 ;;\nesac\n",
    );

    let package = per_file_error_package(&script_path);
    let changeset = ChangeSet::new(vec![
        ChangedFile {
            path: std::path::PathBuf::from("first.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
        ChangedFile {
            path: std::path::PathBuf::from("second.ts"),
            kind: ChangeKind::Modified,
            old_path: None,
        },
    ]);

    let result = super::run_declarative_check(
        repo_root.path(),
        "test/per-file-error",
        &package,
        &changeset,
        &toml::Value::Table(Default::default()),
        None,
    )
    .expect("per_file error must not abort the check");

    // Both findings must be present: error for first.ts, warning for second.ts.
    assert_eq!(
        result.findings.len(),
        2,
        "exit-2 on first.ts must not mask second.ts's finding; got: {:#?}",
        result.findings
    );

    let has_error_for_first = result.findings.iter().any(|f| {
        f.severity == Severity::Error && f.location.as_ref().map(|l| l.path.as_path()) == Some(Path::new("first.ts"))
    });
    assert!(
        has_error_for_first,
        "error finding for first.ts must be present; got: {:#?}",
        result.findings
    );

    let has_warning_for_second = result.findings.iter().any(|f| {
        f.severity == Severity::Warning && f.location.as_ref().map(|l| l.path.as_path()) == Some(Path::new("second.ts"))
    });
    assert!(
        has_warning_for_second,
        "formatting finding for second.ts must NOT be suppressed by first.ts's error; got: {:#?}",
        result.findings
    );
}

// ── progress tick tests ──────────────────────────────────────────────────────

/// Build a per_file declarative manifest whose tool just exits 0 (no findings).
#[cfg(unix)]
fn per_file_noop_package(tool_path: &std::path::Path) -> ExternalCheckDeclarativePackage {
    // Use /bin/sh as the binary and pass the script path as the first arg to
    // avoid ETXTBSY (Text file busy) on Linux when directly exec-ing a script
    // that was written to disk in the same test process.
    let manifest = format!(
        r#"
id = "test/per-file-progress"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.ts"]

[needs.tool.default]
path = "/bin/sh"

[[invocations]]
id = "run"
run = "tool"
mode = "per_file"
args = ["{tool}", "{{{{file}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "hit"
"#,
        tool = tool_path.display()
    );
    let pkg = crate::external::parse_external_check_package_manifest(&manifest).expect("valid manifest");
    match pkg.implementation {
        crate::external::ExternalCheckPackageImplementation::Declarative(d) => d,
        _ => panic!("expected declarative"),
    }
}

/// Build a batch declarative manifest whose tool just exits 0 (no findings).
#[cfg(unix)]
fn batch_noop_package(tool_path: &std::path::Path) -> ExternalCheckDeclarativePackage {
    // Use /bin/sh as the binary and pass the script path as the first arg to
    // avoid ETXTBSY (Text file busy) on Linux when directly exec-ing a script
    // that was written to disk in the same test process.
    let manifest = format!(
        r#"
id = "test/batch-progress"
mode = "declarative"
runtime = "declarative-v1"
api_version = "v1"
applies_to = ["**/*.ts"]

[needs.tool.default]
path = "/bin/sh"

[[invocations]]
id = "run"
run = "tool"
mode = "batch"
args = ["{tool}", "{{{{files}}}}"]
exit = {{ "0" = "ok", default = "error" }}

[invocations.transform]
kind = "linelist"
message = "hit"
"#,
        tool = tool_path.display()
    );
    let pkg = crate::external::parse_external_check_package_manifest(&manifest).expect("valid manifest");
    match pkg.implementation {
        crate::external::ExternalCheckPackageImplementation::Declarative(d) => d,
        _ => panic!("expected declarative"),
    }
}

/// per_file check over N eligible files → processed count goes 0 → 1 → 2 → N.
#[cfg(unix)]
#[test]
fn per_file_progress_emits_one_tick_per_file() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    let repo_root = tempfile::tempdir().expect("temp dir");

    let script = repo_root.path().join("noop.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
    let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");

    for f in ["a.ts", "b.ts", "c.ts"] {
        std::fs::write(repo_root.path().join(f), "").expect("write file");
    }

    let package = per_file_noop_package(&script);
    let changeset = make_changeset(&["a.ts", "b.ts", "c.ts"]);
    let config = toml::Value::Table(Default::default());

    let ticks: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let ticks_clone = Arc::clone(&ticks);
    let on_file_processed: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |n| {
        ticks_clone.lock().unwrap().push(n);
    });

    super::run_declarative_check_with_progress(
        repo_root.path(),
        "test/per-file-progress",
        &package,
        &changeset,
        &config,
        None,
        &crate::exclusion_matcher::ExclusionMatcher::default(),
        on_file_processed,
    )
    .expect("check must succeed");

    let ticks = ticks.lock().unwrap().clone();
    assert_eq!(
        ticks,
        vec![1, 2, 3],
        "per_file must emit one cumulative tick per file; got {ticks:?}"
    );
}

/// batch check: all eligible files go into one chunk → one tick equal to N.
#[cfg(unix)]
#[test]
fn batch_progress_emits_one_tick_per_chunk() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    let repo_root = tempfile::tempdir().expect("temp dir");

    let script = repo_root.path().join("noop.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
    let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");

    for f in ["a.ts", "b.ts", "c.ts"] {
        std::fs::write(repo_root.path().join(f), "").expect("write file");
    }

    let package = batch_noop_package(&script);
    let changeset = make_changeset(&["a.ts", "b.ts", "c.ts"]);
    let config = toml::Value::Table(Default::default());

    let ticks: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let ticks_clone = Arc::clone(&ticks);
    let on_file_processed: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |n| {
        ticks_clone.lock().unwrap().push(n);
    });

    super::run_declarative_check_with_progress(
        repo_root.path(),
        "test/batch-progress",
        &package,
        &changeset,
        &config,
        None,
        &crate::exclusion_matcher::ExclusionMatcher::default(),
        on_file_processed,
    )
    .expect("check must succeed");

    let ticks = ticks.lock().unwrap().clone();
    // 3 short paths fit in one chunk, so one tick equal to 3 is emitted.
    assert!(!ticks.is_empty(), "batch must emit at least one tick; got none");
    assert_eq!(
        *ticks.last().unwrap(),
        3,
        "final tick must equal total eligible files; got {ticks:?}"
    );
    // Numerator must never exceed denominator (3 eligible files).
    assert!(
        ticks.iter().all(|&n| n <= 3),
        "numerator must never exceed denominator (3); got {ticks:?}"
    );
}
