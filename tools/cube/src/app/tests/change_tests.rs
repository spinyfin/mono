use super::support::{ExpectedCommand, FakeRunner, seed_mono_repo, with_database_path};
use clap::Parser;

use crate::cli::Cli;

use super::support::ENV_MUTEX;
use crate::app::change::{is_stdin_path, resolve_body_file};
use crate::app::dispatch::run_with_dependencies;

#[test]
fn change_create_records_named_workspace_head() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-004");
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

    let change_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["describe", "-m", "Implement parser"],
            "",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "change_id ++ \"\\n\" ++ commit_id.short()",
            ],
            "zxy123\nabc1234",
        ),
    ]);
    let create = Cli::parse_from([
        "cube",
        "change",
        "create",
        "--workspace",
        &workspace_path.display().to_string(),
        "--title",
        "Implement parser",
    ]);
    let result = run_with_dependencies(create, Some(&database_path), &change_runner).expect("change");

    assert_eq!(result.payload["change"]["repo"], "mono");
    assert_eq!(
        result.payload["change"]["workspace_path"],
        workspace_path.display().to_string()
    );
    assert_eq!(result.payload["change"]["title"], "Implement parser");
    assert_eq!(result.payload["change"]["jj_change_id"], "zxy123");
    assert_eq!(result.payload["change"]["head_commit"], "abc1234");
    change_runner.assert_exhausted();
}

#[test]
fn change_create_from_parent_uses_parent_jj_change_id() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-004");
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

    let root_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["describe", "-m", "Implement parser"],
            "",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "change_id ++ \"\\n\" ++ commit_id.short()",
            ],
            "root123\nabc1234",
        ),
    ]);
    let root = Cli::parse_from([
        "cube",
        "change",
        "create",
        "--workspace",
        &workspace_path.display().to_string(),
        "--title",
        "Implement parser",
    ]);
    let root_result = run_with_dependencies(root, Some(&database_path), &root_runner).expect("root change");
    root_runner.assert_exhausted();
    let parent_change_id = root_result.payload["change"]["change_id"]
        .as_str()
        .expect("parent change id")
        .to_string();

    let child_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "root123", "-m", "Add tests"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "change_id ++ \"\\n\" ++ commit_id.short()",
            ],
            "child456\nbcd2345",
        ),
    ]);
    let child = Cli::parse_from([
        "cube",
        "change",
        "create",
        "--parent",
        &parent_change_id,
        "--title",
        "Add tests",
    ]);
    let child_result = run_with_dependencies(child, Some(&database_path), &child_runner).expect("child");

    assert_eq!(child_result.payload["change"]["parent_change_id"], parent_change_id);
    assert_eq!(child_result.payload["change"]["jj_change_id"], "child456");
    child_runner.assert_exhausted();
}

#[test]
fn change_info_round_trips_record() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-004");
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

    let change_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["describe", "-m", "Implement parser"],
            "",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "change_id ++ \"\\n\" ++ commit_id.short()",
            ],
            "zxy123\nabc1234",
        ),
    ]);
    let create = Cli::parse_from([
        "cube",
        "change",
        "create",
        "--workspace",
        &workspace_path.display().to_string(),
        "--title",
        "Implement parser",
    ]);
    let create_result = run_with_dependencies(create, Some(&database_path), &change_runner).expect("change");
    change_runner.assert_exhausted();

    let change_id = create_result.payload["change"]["change_id"]
        .as_str()
        .expect("change id")
        .to_string();
    let info = Cli::parse_from(["cube", "change", "info", "--change", &change_id]);
    let info_result = run_with_dependencies(info, Some(&database_path), &FakeRunner::default()).expect("info");

    assert_eq!(info_result.payload["change"]["change_id"], change_id);
    assert_eq!(info_result.payload["change"]["title"], "Implement parser");
}

#[test]
fn is_stdin_path_recognises_known_aliases() {
    assert!(is_stdin_path("/dev/stdin"));
    assert!(is_stdin_path("-"));
    assert!(is_stdin_path("/dev/fd/0"));
}

#[test]
fn is_stdin_path_does_not_match_regular_paths() {
    assert!(!is_stdin_path("/tmp/pr-body.md"));
    assert!(!is_stdin_path("/dev/null"));
    assert!(!is_stdin_path(""));
}

#[test]
fn resolve_body_file_errors_on_empty_regular_file() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    // File is created empty by default.
    let result = resolve_body_file(&tmp.path().display().to_string());
    assert!(result.is_err(), "should error on empty file, got {:?}", result);
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("empty"), "error should mention 'empty': {msg}");
}

#[test]
fn resolve_body_file_passes_through_non_empty_regular_file() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(tmp.path(), b"## Summary\n\nBody text.").expect("write");
    let path_str = tmp.path().display().to_string();

    let (resolved, tmpfile) = resolve_body_file(&path_str).expect("resolve regular file");

    // Regular file: path unchanged, no temp file created.
    assert_eq!(resolved, path_str);
    assert!(tmpfile.is_none());
}

#[cfg(unix)]
#[test]
fn resolve_body_file_materialises_fifo_content_to_temp_file() {
    use std::io::Write;

    // `mkfifo` is resolved through PATH, which the checkleft tests rewrite
    // process-wide; hold the env lock so they can't clobber it mid-spawn.
    let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let fifo_path = dir.path().join("test.fifo");

    // Create a FIFO.
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo failed");

    let expected_body = "## PR Body\n\nThis is the materialized body content.";
    let fifo_path_clone = fifo_path.clone();
    let body_clone = expected_body.to_string();

    // Write in a background thread — FIFO open blocks until a reader also opens.
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo_path_clone)
            .expect("open fifo for write");
        f.write_all(body_clone.as_bytes()).expect("write fifo");
    });

    let path_str = fifo_path.display().to_string();
    let (resolved, tmp) = resolve_body_file(&path_str).expect("resolve fifo");

    writer.join().expect("writer thread");

    // resolved path must differ from the FIFO (temp file was created).
    assert_ne!(resolved, path_str, "resolved path should be a temp file, not the FIFO");
    let materialized = std::fs::read_to_string(&resolved).expect("read materialized");
    assert_eq!(materialized, expected_body);

    if let Some(p) = tmp {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(unix)]
#[test]
fn resolve_body_file_errors_on_empty_fifo() {
    // See the note in `resolve_body_file_materialises_fifo_content_to_temp_file`:
    // `mkfifo` needs an unmodified PATH.
    let _env = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let fifo_path = dir.path().join("empty.fifo");

    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo failed");

    let fifo_path_clone = fifo_path.clone();
    // Write empty content to FIFO so the reader gets EOF immediately.
    let writer = std::thread::spawn(move || {
        // Just open and close without writing.
        let _f = std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo_path_clone)
            .expect("open fifo for write");
    });

    let path_str = fifo_path.display().to_string();
    let result = resolve_body_file(&path_str);

    writer.join().expect("writer thread");

    assert!(result.is_err(), "should error on empty FIFO");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("empty"), "error should mention 'empty': {msg}");
}

// --- ensure_pr body-file regression tests ---
