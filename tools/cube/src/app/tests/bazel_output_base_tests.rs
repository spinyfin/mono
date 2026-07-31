//! Tests for bazel output-base discovery, root resolution via a live
//! workspace probe, and the orphan sweep — the parts of
//! `crate::app::bazel_output_base` that need the shared `Store`/`FakeRunner`
//! test harness. Pure-logic coverage (hashing, permission-fixing removal)
//! lives inline in that module instead.

use std::path::PathBuf;

use super::support::{ExpectedCommand, FakeRunner, with_database_path};

use crate::app::bazel_output_base::{discover_bazel_user_root, hash_workspace_path, sweep_orphaned_output_bases};
use crate::metadata::{RepoRecord, WorkspaceCandidate};
use crate::store::Store;

#[test]
fn discover_bazel_user_root_tries_each_candidate_and_reports_none_on_total_failure() {
    let one = PathBuf::from("/nonexistent/one");
    let two = PathBuf::from("/nonexistent/two");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::failing(one.clone(), "bazel", &["info", "output_base"], "no such workspace"),
        ExpectedCommand::failing(two.clone(), "bazel", &["info", "output_base"], "no such workspace"),
    ]);
    let candidates = vec![one, two];
    assert!(discover_bazel_user_root(&runner, &candidates, None).is_none());
    runner.assert_exhausted();
}

#[test]
fn sweep_skips_non_hash_entries_and_removes_orphans() {
    let (_tempdir_db, database_path) = with_database_path();
    let store = Store::open_at(&database_path).expect("open store");

    let root = tempfile::tempdir().expect("root");
    let orphan = root.path().join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    std::fs::create_dir_all(&orphan).expect("orphan dir");
    std::fs::create_dir_all(root.path().join("install")).expect("install dir");
    std::fs::write(root.path().join("install").join("marker"), b"do-not-touch").expect("marker");

    let report = sweep_orphaned_output_bases(&store, root.path(), false);

    assert_eq!(report.candidates_examined, 1);
    assert_eq!(report.removed, vec![orphan.clone()]);
    assert!(!orphan.exists());
    assert!(root.path().join("install").join("marker").exists());
}

#[test]
fn sweep_never_removes_a_hash_matching_a_live_workspace() {
    let (_tempdir_db, database_path) = with_database_path();
    let mut store = Store::open_at(&database_path).expect("open store");

    let repo_workspace_root = tempfile::tempdir().expect("repo workspace root");
    let live_path = repo_workspace_root.path().join("mono-agent-001");
    std::fs::create_dir_all(&live_path).expect("live workspace dir");

    store
        .upsert_repo(&RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: repo_workspace_root.path().to_path_buf(),
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
            clone_command: None,
        })
        .expect("upsert repo");
    store
        .sync_workspaces(
            "mono",
            &[WorkspaceCandidate {
                workspace_id: "mono-agent-001".to_string(),
                workspace_path: live_path.clone(),
            }],
        )
        .expect("register live workspace");
    let store = store;

    let root = tempfile::tempdir().expect("root");
    let live_hash = hash_workspace_path(&live_path);
    std::fs::create_dir_all(root.path().join(&live_hash)).expect("live-hashed dir");

    let report = sweep_orphaned_output_bases(&store, root.path(), false);

    assert!(report.removed.is_empty());
    assert_eq!(report.skipped_now_live, 1);
    assert!(root.path().join(&live_hash).exists());
}
