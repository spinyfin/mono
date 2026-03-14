use std::process::ExitCode;

use serde::Serialize;
use thiserror::Error;

use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, RepoCommand, StackCommand,
    WorkspaceCommand,
};

type Result<T> = std::result::Result<T, CubeError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunResult {
    pub message: String,
}

#[derive(Debug, Error)]
pub enum CubeError {
    #[error("{0}")]
    NotImplemented(String),
}

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::NotImplemented(_) => ExitCode::from(2),
        }
    }
}

pub fn run(cli: Cli) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => run_repo(command),
        Command::Workspace { command } => run_workspace(command),
        Command::Change { command } => run_change(command),
        Command::Stack { command } => run_stack(command),
        Command::Pr { command } => run_pr(command),
        Command::Graph(args) => run_graph(args),
        Command::Doctor(args) => run_doctor(args),
    }
}

fn run_repo(command: RepoCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "repo command `{}` is not implemented yet",
        repo_command_name(&command)
    )))
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

fn repo_command_name(command: &RepoCommand) -> &'static str {
    match command {
        RepoCommand::Add { .. } => "add",
        RepoCommand::List => "list",
        RepoCommand::Info { .. } => "info",
    }
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

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use clap::Parser;

    use crate::cli::{Cli, Command};

    use super::{CubeError, run};

    #[test]
    fn repo_commands_report_not_implemented() {
        let cli = Cli::parse_from([
            "cube",
            "--json",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            "/tmp/workspaces",
            "--workspace-prefix",
            "mono-agent-",
        ]);

        let error = run(cli).expect_err("repo add should not be implemented in the scaffold");
        assert!(matches!(error, CubeError::NotImplemented(_)));
        assert_eq!(error.exit_code(), ExitCode::from(2));
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
