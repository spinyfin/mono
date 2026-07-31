//! End-to-end tests for `cube workspace reclaim-bazel-bases`: the standalone
//! sweep for output bases orphaned before per-teardown cleanup existed, or by
//! a crash/kill/interrupted teardown since.

use clap::Parser;

use super::support::{ExpectedCommand, FakeRunner, with_database_path};

use crate::app::bazel_output_base::hash_workspace_path;
use crate::app::dispatch::run_with_dependencies;
use crate::cli::Cli;
use crate::metadata::{RepoRecord, WorkspaceCandidate};
use crate::store::Store;

/// Register a "mono" repo with one live workspace and a canonical source
/// checkout, and return `(store, live_workspace_path, source_path)`.
fn seed_repo_with_one_workspace(
    tempdir: &tempfile::TempDir,
    database_path: &std::path::Path,
) -> (Store, std::path::PathBuf, std::path::PathBuf) {
    let workspace_root = tempdir.path().join("workspaces");
    let source = tempdir.path().join("source/mono");
    let live_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(live_path.join(".jj")).expect("live workspace dir");
    std::fs::create_dir_all(source.join(".jj")).expect("canonical source");

    let mut store = Store::open_at(database_path).expect("store");
    store
        .upsert_repo(&RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: workspace_root.clone(),
            workspace_prefix: "mono-agent-".to_string(),
            source: Some(source.clone()),
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
        .expect("sync workspace");
    (store, live_path, source)
}

fn bazel_root_probe(source: &std::path::Path, output_base_stdout: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
        "bazel",
        &["info", "output_base"],
        output_base_stdout,
    )
}

#[test]
fn reclaim_bazel_bases_removes_orphans_and_spares_live_workspaces() {
    let (tempdir, database_path) = with_database_path();
    let (_store, live_path, source) = seed_repo_with_one_workspace(&tempdir, &database_path);

    let bazel_root = tempdir.path().join("bazel-cache/_bazel_testuser");
    std::fs::create_dir_all(&bazel_root).expect("bazel root");
    let live_hash = hash_workspace_path(&live_path);
    std::fs::create_dir_all(bazel_root.join(&live_hash)).expect("live-hashed base");
    let orphan_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    std::fs::create_dir_all(bazel_root.join(orphan_hash)).expect("orphaned base");
    std::fs::create_dir_all(bazel_root.join("install")).expect("bazel's own install cache");

    let runner = FakeRunner::new(vec![bazel_root_probe(
        &source,
        &bazel_root.join(hash_workspace_path(&source)).display().to_string(),
    )]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reclaim-bazel-bases"]),
        Some(&database_path),
        &runner,
    )
    .expect("reclaim-bazel-bases");
    runner.assert_exhausted();

    assert!(
        bazel_root.join(&live_hash).exists(),
        "the live workspace's base must survive"
    );
    assert!(!bazel_root.join(orphan_hash).exists(), "the orphan must be removed");
    assert!(
        bazel_root.join("install").exists(),
        "bazel's own install cache is never a candidate"
    );

    assert_eq!(result.payload["removed"].as_array().unwrap().len(), 1);
    assert_eq!(result.payload["skipped_now_live"], 1);
    assert!(result.message.contains("Reclaimed 1 of 2"));
}

#[test]
fn reclaim_bazel_bases_dry_run_leaves_everything_on_disk() {
    let (tempdir, database_path) = with_database_path();
    let (_store, _live_path, source) = seed_repo_with_one_workspace(&tempdir, &database_path);

    let bazel_root = tempdir.path().join("bazel-cache/_bazel_testuser");
    std::fs::create_dir_all(&bazel_root).expect("bazel root");
    let orphan_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    std::fs::create_dir_all(bazel_root.join(orphan_hash)).expect("orphaned base");

    let runner = FakeRunner::new(vec![bazel_root_probe(
        &source,
        &bazel_root.join(hash_workspace_path(&source)).display().to_string(),
    )]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reclaim-bazel-bases", "--dry-run"]),
        Some(&database_path),
        &runner,
    )
    .expect("dry-run reclaim-bazel-bases");
    runner.assert_exhausted();

    assert!(
        bazel_root.join(orphan_hash).exists(),
        "dry run must not remove anything"
    );
    assert_eq!(result.payload["removed"].as_array().unwrap().len(), 1);
    assert!(result.message.starts_with("Dry run:"));
}

#[test]
fn reclaim_bazel_bases_errors_when_no_repo_has_a_canonical_source() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let store = Store::open_at(&database_path).expect("store");
    store
        .upsert_repo(&RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root,
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
            clone_command: None,
        })
        .expect("upsert repo without a canonical source");
    drop(store);

    let runner = FakeRunner::new(vec![]);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reclaim-bazel-bases"]),
        Some(&database_path),
        &runner,
    );
    runner.assert_exhausted();

    assert!(
        result.is_err(),
        "no probe candidate exists, so discovery must fail loudly"
    );
}
