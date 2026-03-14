use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, RepoCommand, StackCommand,
    WorkspaceCommand,
};
use crate::metadata::RepoRecord;
use crate::store::Store;

type Result<T> = std::result::Result<T, CubeError>;

#[derive(Debug, Clone)]
pub struct RunResult {
    pub message: String,
    pub payload: Value,
}

impl RunResult {
    fn new(message: impl Into<String>, payload: impl Serialize) -> Result<Self> {
        Ok(Self {
            message: message.into(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Debug, Error)]
pub enum CubeError {
    #[error("{0}")]
    NotImplemented(String),
    #[error("repo `{0}` is not configured")]
    RepoNotFound(String),
    #[error("failed to access Cube metadata: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("failed to prepare Cube data directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
}

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::NotImplemented(_) => ExitCode::from(2),
            Self::RepoNotFound(_) => ExitCode::from(3),
            Self::Storage(_) | Self::Io(_) | Self::Json(_) => ExitCode::FAILURE,
        }
    }
}

pub fn run(cli: Cli) -> Result<RunResult> {
    run_with_database_path(cli, None)
}

fn run_with_database_path(cli: Cli, database_path: Option<&Path>) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => run_repo(command, database_path),
        Command::Workspace { command } => run_workspace(command),
        Command::Change { command } => run_change(command),
        Command::Stack { command } => run_stack(command),
        Command::Pr { command } => run_pr(command),
        Command::Graph(args) => run_graph(args),
        Command::Doctor(args) => run_doctor(args),
    }
}

fn run_repo(command: RepoCommand, database_path: Option<&Path>) -> Result<RunResult> {
    let store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        RepoCommand::Add {
            repo,
            origin,
            main_branch,
            workspace_root,
            workspace_prefix,
            source,
        } => {
            let config = RepoRecord {
                repo,
                origin,
                main_branch,
                workspace_root: PathBuf::from(workspace_root),
                workspace_prefix,
                source: source.map(PathBuf::from),
            };
            let record = store.upsert_repo(&config)?;
            RunResult::new(
                format!("Registered repo `{}`.", record.repo),
                json!({
                    "repo": record,
                }),
            )
        }
        RepoCommand::List => {
            let repos = store.list_repos()?;
            let message = if repos.is_empty() {
                "No repos configured.".to_string()
            } else {
                repos
                    .iter()
                    .map(human_repo_summary)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            RunResult::new(
                message,
                json!({
                    "repos": repos,
                }),
            )
        }
        RepoCommand::Info { repo } => {
            let record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;
            RunResult::new(
                human_repo_detail(&record),
                json!({
                    "repo": record,
                }),
            )
        }
    }
}

fn run_workspace(command: WorkspaceCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "workspace command `{}` is not implemented yet",
        workspace_command_name(&command)
    )))
}

fn run_change(command: ChangeCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "change command `{}` is not implemented yet",
        change_command_name(&command)
    )))
}

fn run_stack(command: StackCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "stack command `{}` is not implemented yet",
        stack_command_name(&command)
    )))
}

fn run_pr(command: PrCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "pr command `{}` is not implemented yet",
        pr_command_name(&command)
    )))
}

fn run_graph(_args: GraphArgs) -> Result<RunResult> {
    Err(CubeError::NotImplemented(
        "graph command is not implemented yet".to_string(),
    ))
}

fn run_doctor(_args: DoctorArgs) -> Result<RunResult> {
    Err(CubeError::NotImplemented(
        "doctor command is not implemented yet".to_string(),
    ))
}

fn workspace_command_name(command: &WorkspaceCommand) -> &'static str {
    match command {
        WorkspaceCommand::Lease { .. } => "lease",
        WorkspaceCommand::Release { .. } => "release",
        WorkspaceCommand::Status { .. } => "status",
        WorkspaceCommand::Setup { .. } => "setup",
    }
}

fn change_command_name(command: &ChangeCommand) -> &'static str {
    match command {
        ChangeCommand::Create { .. } => "create",
        ChangeCommand::Checkout { .. } => "checkout",
        ChangeCommand::Info { .. } => "info",
    }
}

fn stack_command_name(command: &StackCommand) -> &'static str {
    match command {
        StackCommand::Rebase { .. } => "rebase",
    }
}

fn pr_command_name(command: &PrCommand) -> &'static str {
    match command {
        PrCommand::Sync { .. } => "sync",
        PrCommand::Merge { .. } => "merge",
    }
}

fn human_repo_summary(record: &RepoRecord) -> String {
    format!(
        "{}: {} ({}, prefix `{}`)",
        record.repo,
        record.workspace_root.display(),
        record.main_branch,
        record.workspace_prefix
    )
}

fn human_repo_detail(record: &RepoRecord) -> String {
    let mut lines = vec![
        format!("repo: {}", record.repo),
        format!("origin: {}", record.origin),
        format!("main_branch: {}", record.main_branch),
        format!("workspace_root: {}", record.workspace_root.display()),
        format!("workspace_prefix: {}", record.workspace_prefix),
    ];
    if let Some(source) = &record.source {
        lines.push(format!("source: {}", source.display()));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::cli::{Cli, Command};

    use super::{CubeError, run_with_database_path};

    fn with_database_path() -> (TempDir, std::path::PathBuf) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("state.db");
        (tempdir, database_path)
    }

    #[test]
    fn repo_add_and_info_round_trip() {
        let (_tempdir, database_path) = with_database_path();

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            "/tmp/workspaces",
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            "/tmp/mono",
        ]);
        let add_result =
            run_with_database_path(add, Some(&database_path)).expect("repo add should succeed");
        assert_eq!(add_result.message, "Registered repo `mono`.");
        assert_eq!(add_result.payload["repo"]["repo"], "mono");

        let info = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let info_result =
            run_with_database_path(info, Some(&database_path)).expect("repo info should succeed");
        assert_eq!(info_result.payload["repo"]["workspace_prefix"], "mono-agent-");
        assert_eq!(info_result.payload["repo"]["source"], "/tmp/mono");
    }

    #[test]
    fn repo_list_reports_empty_store() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "list"]);
        let result =
            run_with_database_path(cli, Some(&database_path)).expect("repo list should succeed");

        assert_eq!(result.message, "No repos configured.");
        assert_eq!(result.payload["repos"], json!([]));
    }

    #[test]
    fn repo_commands_report_missing_repo_with_specific_exit_code() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let error = run_with_database_path(cli, Some(&database_path))
            .expect_err("repo info should fail when the repo is unknown");

        assert!(matches!(error, CubeError::RepoNotFound(_)));
        assert_eq!(error.exit_code(), ExitCode::from(3));
    }

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
}
