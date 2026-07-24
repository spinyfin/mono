//! Top-level entry point: parses nothing itself, but routes an already-parsed
//! [`Cli`] to the per-area command handlers.

use std::path::Path;

use crate::cli::{Cli, Command, DoctorArgs, GraphArgs};
use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::config;

use crate::app::change::{run_change, run_pr, run_stack};
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::repo::{RepoEnsureDefaults, run_repo};
use crate::app::workspace::run_workspace;

pub fn run(cli: Cli) -> Result<RunResult> {
    let runner = RealCommandRunner;
    run_with_dependencies(cli, None, &runner)
}

pub(super) fn run_with_dependencies(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    run_with_context(cli, database_path, runner, None, None)
}

pub(super) fn run_with_context(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
    cube_config: Option<config::CubeConfig>,
) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => run_repo(command, database_path, runner, repo_ensure_defaults, cube_config),
        Command::Workspace { command } => run_workspace(command, database_path, runner),
        Command::Change { command } => run_change(command, database_path, runner),
        Command::Stack { command } => run_stack(command),
        Command::Pr { command } => run_pr(command, runner),
        Command::Graph(args) => run_graph(args),
        Command::Doctor(args) => run_doctor(args),
    }
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
