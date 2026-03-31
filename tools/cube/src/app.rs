use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, RepoCommand, StackCommand,
    WorkspaceCommand,
};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::metadata::RepoRecord;
use crate::paths;
use crate::store::Store;

type Result<T> = std::result::Result<T, CubeError>;

#[derive(Debug, Clone)]
pub struct RunResult {
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
struct RepoEnsureDefaults {
    repo_root: PathBuf,
    workspace_root: PathBuf,
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
    InvalidArgument(String),
    #[error("{0}")]
    NotImplemented(String),
    #[error("repo `{0}` is not configured")]
    RepoNotFound(String),
    #[error("no free workspace is available for repo `{0}`")]
    NoAvailableWorkspace(String),
    #[error("workspace `{0}` is not tracked")]
    WorkspaceNotFound(String),
    #[error("lease `{0}` is not tracked")]
    LeaseNotFound(String),
    #[error("failed to access Cube metadata: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("failed to prepare Cube data directory: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "command `{program} {}` failed{}{}",
        args.join(" "),
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default(),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )]
    CommandFailed {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
}

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidArgument(_) | Self::NotImplemented(_) => ExitCode::from(2),
            Self::RepoNotFound(_) => ExitCode::from(3),
            Self::NoAvailableWorkspace(_) => ExitCode::from(4),
            Self::WorkspaceNotFound(_) | Self::LeaseNotFound(_) => ExitCode::from(5),
            Self::Storage(_) | Self::Io(_) | Self::CommandFailed { .. } | Self::Json(_) => {
                ExitCode::FAILURE
            }
        }
    }
}

pub fn run(cli: Cli) -> Result<RunResult> {
    let runner = RealCommandRunner;
    run_with_dependencies(cli, None, &runner)
}

fn run_with_dependencies(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    run_with_context(cli, database_path, runner, None)
}

fn run_with_context(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => run_repo(command, database_path, runner, repo_ensure_defaults),
        Command::Workspace { command } => run_workspace(command, database_path, runner),
        Command::Change { command } => run_change(command),
        Command::Stack { command } => run_stack(command),
        Command::Pr { command } => run_pr(command),
        Command::Graph(args) => run_graph(args),
        Command::Doctor(args) => run_doctor(args),
    }
}

fn run_repo(
    command: RepoCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
) -> Result<RunResult> {
    let store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        RepoCommand::Ensure { origin } => {
            let origin = normalize_origin(&origin)?;
            let defaults = if let Some(defaults) = repo_ensure_defaults {
                defaults.clone()
            } else {
                default_repo_ensure_defaults()?
            };
            let record = ensure_repo(&store, runner, &origin, &defaults)?;
            let repo_id = record.repo.clone();
            RunResult::new(
                format!("Ensured repo `{repo_id}`."),
                json!({
                    "repo_id": repo_id,
                    "repo": record,
                }),
            )
        }
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

fn ensure_repo(
    store: &Store,
    runner: &dyn CommandRunner,
    origin: &str,
    defaults: &RepoEnsureDefaults,
) -> Result<RepoRecord> {
    if let Some(record) = store.get_repo_by_origin(origin)? {
        return Ok(record);
    }

    let record = infer_repo_record_from_origin(origin, defaults)?;
    if let Some(existing) = store.get_repo(&record.repo)? {
        return Err(CubeError::InvalidArgument(format!(
            "repo `{}` is already configured for origin `{}`; cannot ensure `{origin}`",
            existing.repo, existing.origin
        )));
    }

    fs::create_dir_all(&record.workspace_root)?;
    materialize_repo_source_if_missing(runner, &record)?;
    store.upsert_repo(&record)
}

fn normalize_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err(CubeError::InvalidArgument(
            "origin must not be empty".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn default_repo_ensure_defaults() -> Result<RepoEnsureDefaults> {
    let cube_root = paths::data_dir()?;
    let repo_root = cube_root.join("repos");
    Ok(RepoEnsureDefaults {
        workspace_root: cube_root.join("workspaces"),
        repo_root,
    })
}

fn infer_repo_record_from_origin(
    origin: &str,
    defaults: &RepoEnsureDefaults,
) -> Result<RepoRecord> {
    let repo = repo_id_from_origin(origin)?;
    Ok(RepoRecord {
        repo: repo.clone(),
        origin: origin.to_string(),
        main_branch: "main".to_string(),
        workspace_root: defaults.workspace_root.clone(),
        workspace_prefix: format!("{repo}-agent-"),
        source: Some(defaults.repo_root.join(&repo)),
    })
}

fn materialize_repo_source_if_missing(
    runner: &dyn CommandRunner,
    record: &RepoRecord,
) -> Result<()> {
    let Some(source) = &record.source else {
        return Ok(());
    };

    if source.exists() {
        if source.is_dir() {
            return Ok(());
        }
        return Err(CubeError::InvalidArgument(format!(
            "source path {} exists and is not a directory",
            source.display()
        )));
    }

    let parent = source.parent().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "cannot infer parent directory for source path {}",
            source.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    runner.run(&CommandInvocation {
        cwd: parent.to_path_buf(),
        program: "jj".to_string(),
        args: vec![
            "git".to_string(),
            "clone".to_string(),
            record.origin.clone(),
            source.display().to_string(),
        ],
    })?;
    Ok(())
}

fn repo_id_from_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim().trim_end_matches('/');
    let tail = trimmed
        .rsplit(|ch| ['/', ':'].contains(&ch))
        .next()
        .unwrap_or("");
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let repo = sanitize_repo_id(tail);
    if repo.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "could not infer repo id from origin `{origin}`"
        )));
    }
    Ok(repo)
}

fn sanitize_repo_id(raw: &str) -> String {
    let mut repo = String::new();
    let mut previous_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            repo.push(ch.to_ascii_lowercase());
            previous_dash = false;
            continue;
        }

        if matches!(ch, '-' | '_' | '.') && !previous_dash {
            repo.push('-');
            previous_dash = true;
        }
    }

    repo.trim_matches('-').to_string()
}

fn run_workspace(
    command: WorkspaceCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    let mut store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        WorkspaceCommand::Lease { repo, task } => {
            let repo_record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;
            let candidates = discover_workspaces(&repo_record)?;
            store.sync_workspaces(&repo, &candidates)?;

            let lease_id = Uuid::new_v4().to_string();
            let holder = holder_identity();
            let leased_at_epoch_s = current_epoch_s()?;
            let Some(mut workspace) =
                store.claim_workspace(&repo, &holder, &task, &lease_id, leased_at_epoch_s)?
            else {
                return Err(CubeError::NoAvailableWorkspace(repo));
            };

            if let Err(error) =
                reset_workspace(runner, &workspace.workspace_path, &repo_record.main_branch)
            {
                let _ = store.release_workspace(&lease_id);
                return Err(error);
            }

            let head_commit = current_workspace_commit(runner, &workspace.workspace_path)?;
            store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
            workspace.head_commit = Some(head_commit);

            RunResult::new(
                format!(
                    "Leased {} at {}.",
                    workspace.workspace_id,
                    workspace.workspace_path.display()
                ),
                json!({
                    "workspace": workspace,
                }),
            )
        }
        WorkspaceCommand::Release { lease } => {
            let workspace = store
                .get_workspace_by_lease(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            let repo_record = store
                .get_repo(&workspace.repo)?
                .ok_or_else(|| CubeError::RepoNotFound(workspace.repo.clone()))?;
            reset_workspace(runner, &workspace.workspace_path, &repo_record.main_branch)?;
            let released = store
                .release_workspace(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;

            RunResult::new(
                format!("Released {}.", released.workspace_id),
                json!({
                    "workspace": released,
                }),
            )
        }
        WorkspaceCommand::Status { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let jj_status = runner.run(&RealCommandRunner::invocation(&path, "jj", &["status"]))?;

            RunResult::new(
                human_workspace_detail(&record, &jj_status),
                json!({
                    "workspace": record,
                    "jj_status": jj_status,
                }),
            )
        }
        WorkspaceCommand::Setup { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            RunResult::new(
                format!("No setup steps are configured for {}.", record.workspace_id),
                json!({
                    "workspace": record,
                    "steps": [],
                }),
            )
        }
    }
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

fn discover_workspaces(repo: &RepoRecord) -> Result<Vec<crate::metadata::WorkspaceCandidate>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&repo.workspace_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }

        let workspace_id = entry.file_name();
        let workspace_id = workspace_id.to_string_lossy().to_string();
        if !workspace_id.starts_with(&repo.workspace_prefix) {
            continue;
        }

        candidates.push(crate::metadata::WorkspaceCandidate {
            workspace_id,
            workspace_path: entry.path(),
        });
    }

    candidates.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
    Ok(candidates)
}

fn find_workspace_record(
    store: &mut Store,
    workspace_path: &Path,
) -> Result<Option<crate::metadata::WorkspaceRecord>> {
    if let Some(record) = store.get_workspace_by_path(workspace_path)? {
        return Ok(Some(record));
    }

    for repo in store.list_repos()? {
        if workspace_path.starts_with(&repo.workspace_root) {
            let candidates = discover_workspaces(&repo)?;
            store.sync_workspaces(&repo.repo, &candidates)?;
        }
    }

    store.get_workspace_by_path(workspace_path)
}

fn reset_workspace(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<()> {
    runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["git", "fetch"],
    ))?;
    runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["new", main_branch],
    ))?;
    Ok(())
}

fn current_workspace_commit(runner: &dyn CommandRunner, workspace_path: &Path) -> Result<String> {
    runner.run(&CommandInvocation {
        cwd: workspace_path.to_path_buf(),
        program: "jj".to_string(),
        args: vec![
            "log".to_string(),
            "-r".to_string(),
            "@".to_string(),
            "-T".to_string(),
            "commit_id.short()".to_string(),
        ],
    })
}

fn human_workspace_detail(record: &crate::metadata::WorkspaceRecord, jj_status: &str) -> String {
    let mut lines = vec![
        format!("repo: {}", record.repo),
        format!("workspace_id: {}", record.workspace_id),
        format!("workspace_path: {}", record.workspace_path.display()),
        format!("state: {}", record.state.as_str()),
    ];
    if let Some(lease_id) = &record.lease_id {
        lines.push(format!("lease_id: {lease_id}"));
    }
    if let Some(holder) = &record.holder {
        lines.push(format!("holder: {holder}"));
    }
    if let Some(task) = &record.task {
        lines.push(format!("task: {task}"));
    }
    if let Some(head_commit) = &record.head_commit {
        lines.push(format!("head_commit: {head_commit}"));
    }
    lines.push("jj_status:".to_string());
    lines.push(jj_status.to_string());
    lines.join("\n")
}

fn holder_identity() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("{user}@{host}:{}", std::process::id())
}

fn current_epoch_s() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs() as i64)
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
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::process::ExitCode;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::cli::{Cli, Command};
    use crate::command_runner::{CommandInvocation, CommandRunner};

    use super::{CubeError, RepoEnsureDefaults, Result, run_with_context, run_with_dependencies};

    fn with_database_path() -> (TempDir, std::path::PathBuf) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("state.db");
        (tempdir, database_path)
    }

    fn repo_ensure_defaults(tempdir: &TempDir) -> RepoEnsureDefaults {
        RepoEnsureDefaults {
            repo_root: tempdir.path().join("repos"),
            workspace_root: tempdir.path().join("workspaces"),
        }
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
        let add_result = run_with_dependencies(add, Some(&database_path), &FakeRunner::default())
            .expect("repo add should succeed");
        assert_eq!(add_result.message, "Registered repo `mono`.");
        assert_eq!(add_result.payload["repo"]["repo"], "mono");

        let info = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let info_result = run_with_dependencies(info, Some(&database_path), &FakeRunner::default())
            .expect("repo info should succeed");
        assert_eq!(
            info_result.payload["repo"]["workspace_prefix"],
            "mono-agent-"
        );
        assert_eq!(info_result.payload["repo"]["source"], "/tmp/mono");
    }

    #[test]
    fn repo_list_reports_empty_store() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "list"]);
        let result = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
            .expect("repo list should succeed");

        assert_eq!(result.message, "No repos configured.");
        assert_eq!(result.payload["repos"], json!([]));
    }

    #[test]
    fn repo_commands_report_missing_repo_with_specific_exit_code() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let error = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
            .expect_err("repo info should fail when the repo is unknown");

        assert!(matches!(error, CubeError::RepoNotFound(_)));
        assert_eq!(error.exit_code(), ExitCode::from(3));
    }

    #[test]
    fn repo_ensure_reuses_existing_repo_by_origin() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            &defaults.repo_root.join("mono").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(result.payload["repo_id"], "mono");
        assert_eq!(
            result.payload["repo"]["workspace_root"],
            defaults.workspace_root.display().to_string()
        );
        assert_eq!(
            result.payload["repo"]["source"],
            defaults.repo_root.join("mono").display().to_string()
        );
    }

    #[test]
    fn repo_ensure_infers_repo_and_materializes_missing_source() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("mono");
        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )]);

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);
        let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults))
            .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(result.payload["repo_id"], "mono");
        assert_eq!(result.payload["repo"]["workspace_prefix"], "mono-agent-");
        assert_eq!(
            result.payload["repo"]["workspace_root"],
            defaults.workspace_root.display().to_string()
        );
        assert_eq!(
            result.payload["repo"]["source"],
            source_path.display().to_string()
        );
        assert!(defaults.workspace_root.is_dir());
        runner.assert_exhausted();
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

    #[test]
    fn workspace_lease_claims_first_free_workspace_and_records_head_commit() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let first_path = workspace_root.join("mono-agent-004");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first_path.clone(),
                "jj",
                &["log", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        assert_eq!(
            result.payload["workspace"]["workspace_path"],
            first_path.display().to_string()
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
        runner.assert_exhausted();
    }

    #[test]
    fn workspace_release_resets_and_frees_the_workspace() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        let lease_result =
            run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
        ]);
        let release = Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]);
        let release_result =
            run_with_dependencies(release, Some(&database_path), &release_runner).expect("release");

        assert_eq!(release_result.payload["workspace"]["state"], "free");
        assert_eq!(
            release_result.payload["workspace"]["lease_id"],
            serde_json::Value::Null
        );
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_status_includes_jj_status_output() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let status_runner = FakeRunner::new(vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status"],
            "The working copy is clean",
        )]);
        let status = Cli::parse_from([
            "cube",
            "workspace",
            "status",
            "--workspace",
            &workspace_path.display().to_string(),
        ]);
        let status_result =
            run_with_dependencies(status, Some(&database_path), &status_runner).expect("status");

        assert_eq!(
            status_result.payload["jj_status"],
            "The working copy is clean"
        );
        assert!(status_result.message.contains("jj_status:"));
        status_runner.assert_exhausted();
    }

    #[derive(Default)]
    struct FakeRunner {
        expectations: RefCell<VecDeque<ExpectedCommand>>,
    }

    impl FakeRunner {
        fn new(expectations: Vec<ExpectedCommand>) -> Self {
            Self {
                expectations: RefCell::new(expectations.into()),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.expectations.borrow().is_empty(),
                "unexpected commands remaining: {:?}",
                self.expectations.borrow()
            );
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            let expected = self
                .expectations
                .borrow_mut()
                .pop_front()
                .expect("unexpected command invocation");
            assert_eq!(expected.cwd, invocation.cwd);
            assert_eq!(expected.program, invocation.program);
            assert_eq!(expected.args, invocation.args);
            expected.result
        }
    }

    #[derive(Debug)]
    struct ExpectedCommand {
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        result: Result<String>,
    }

    impl ExpectedCommand {
        fn ok(cwd: PathBuf, program: &str, args: &[&str], stdout: &str) -> Self {
            Self {
                cwd,
                program: program.to_string(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                result: Ok(stdout.to_string()),
            }
        }
    }
}
