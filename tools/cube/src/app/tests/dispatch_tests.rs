use super::support::with_database_path;
use clap::Parser;

use crate::cli::{Cli, Command};

use crate::app::dispatch::run_with_context;
use crate::app::errors::CubeError;
use crate::app::repo::RepoEnsureDefaults;

#[test]
fn graph_arguments_parse_from_docs_shape() {
    let cli = Cli::parse_from(["cube", "graph", "--workspace", "/tmp/mono-agent-004"]);

    match cli.command {
        Command::Graph(graph) => {
            assert_eq!(graph.workspace.as_deref(), Some("/tmp/mono-agent-004"))
        }
        _ => panic!("expected graph command"),
    }
}

#[test]
fn workspace_dir_create_error_has_specific_variant() {
    // Ensure that when workspace directory creation fails, the error surfaces
    // as WorkspaceDirCreate (not the generic Io variant). This guards against
    // regressions to the old #[from] io::Error pattern that reported every
    // io error as "failed to prepare Cube data directory".
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");

    // Create a *file* at the workspace_root path so create_dir_all fails.
    std::fs::write(&workspace_root, b"not a dir").expect("write sentinel file");

    let defaults = RepoEnsureDefaults {
        repo_root: tempdir.path().join("repos"),
        workspace_root: workspace_root.clone(),
    };

    let cli = Cli::parse_from(["cube", "repo", "ensure", "--origin", "https://github.com/example/repo"]);
    let runner = crate::command_runner::RealCommandRunner;
    let err = run_with_context(cli, Some(&database_path), &runner, Some(&defaults), None)
        .expect_err("should fail because workspace_root is a file");

    assert!(
        matches!(err, CubeError::WorkspaceDirCreate { ref path, .. } if path == &workspace_root),
        "expected WorkspaceDirCreate, got: {err:?}"
    );
}

// --- resolve_body_file / stdin materialization tests ---
