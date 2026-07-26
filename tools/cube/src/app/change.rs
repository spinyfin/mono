//! `cube change`, `cube stack`, and `cube pr` dispatch, plus the shared
//! `--body-file` resolution used by the PR verbs.

use std::path::{Path, PathBuf};

use console::{Style, style};
use serde_json::json;
use uuid::Uuid;

use crate::cli::{ChangeCommand, PrCommand, StackCommand};
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::metadata::ChangeRecord;
use crate::store::Store;

use crate::app::display::abbreviate_path;
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::jj::{current_change_identity, run_jj};
use crate::app::pr::{ensure_pr_deprecated, pr_create, pr_push, pr_update};
use crate::app::provision::find_workspace_record;
use crate::app::util::current_epoch_s;

pub(super) fn run_change(
    command: ChangeCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    let mut store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        ChangeCommand::Create(args) => {
            if args.workspace.is_some() && args.parent.is_some() {
                return Err(CubeError::InvalidArgument(
                    "change create accepts either --workspace or --parent, not both".to_string(),
                ));
            }
            if args.workspace.is_none() && args.parent.is_none() {
                return Err(CubeError::InvalidArgument(
                    "change create requires --workspace or --parent".to_string(),
                ));
            }

            let (repo, workspace_path, parent_change_id) = if let Some(workspace) = args.workspace {
                let workspace_path = PathBuf::from(&workspace);
                let record = find_workspace_record(&mut store, &workspace_path)?
                    .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
                (record.repo, workspace_path, None)
            } else if let Some(parent_change_id) = args.parent {
                let parent = store
                    .get_change(&parent_change_id)?
                    .ok_or_else(|| CubeError::ChangeNotFound(parent_change_id.clone()))?;
                (parent.repo, parent.workspace_path, Some(parent.change_id))
            } else {
                unreachable!("validated create arguments");
            };

            if let Some(parent_change_id) = parent_change_id.as_deref() {
                let parent = store
                    .get_change(parent_change_id)?
                    .ok_or_else(|| CubeError::ChangeNotFound(parent_change_id.to_string()))?;
                run_jj(
                    runner,
                    database_path,
                    &CommandInvocation {
                        cwd: workspace_path.clone(),
                        program: "jj".to_string(),
                        args: vec![
                            "new".to_string(),
                            parent.jj_change_id,
                            "-m".to_string(),
                            args.title.clone(),
                        ],
                        env: vec![],
                    },
                )?;
            } else {
                run_jj(
                    runner,
                    database_path,
                    &CommandInvocation {
                        cwd: workspace_path.clone(),
                        program: "jj".to_string(),
                        args: vec!["describe".to_string(), "-m".to_string(), args.title.clone()],
                        env: vec![],
                    },
                )?;
            }

            let identity = current_change_identity(runner, database_path, &workspace_path)?;
            let change_id = format!("chg_{}", Uuid::new_v4().simple());
            let record = store.insert_change(&ChangeRecord {
                change_id,
                repo,
                workspace_path,
                parent_change_id,
                title: args.title,
                jj_change_id: identity.jj_change_id,
                head_commit: identity.head_commit,
                created_at_epoch_s: current_epoch_s()?,
            })?;

            RunResult::new(
                format!("Created change `{}`.", record.change_id),
                json!({
                    "change": record,
                }),
            )
        }
        ChangeCommand::Checkout { change } => Err(CubeError::NotImplemented(format!(
            "change command `checkout` is not implemented yet for `{change}`"
        ))),
        ChangeCommand::Info { change } => {
            let record = store
                .get_change(&change)?
                .ok_or_else(|| CubeError::ChangeNotFound(change.clone()))?;
            RunResult::new(
                human_change_detail(&record),
                json!({
                    "change": record,
                }),
            )
        }
    }
}

pub(super) fn run_stack(command: StackCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "stack command `{}` is not implemented yet",
        stack_command_name(&command)
    )))
}

pub(super) fn run_pr(command: PrCommand, runner: &dyn CommandRunner) -> Result<RunResult> {
    match command {
        PrCommand::Create(args) => pr_create(args, runner),
        PrCommand::Update(args) => pr_update(args, runner),
        PrCommand::Ensure(args) => ensure_pr_deprecated(args, runner),
        PrCommand::Push(args) => pr_push(args, runner),
        _ => Err(CubeError::NotImplemented(format!(
            "pr command `{}` is not implemented yet",
            pr_command_name(&command)
        ))),
    }
}

/// Returns true if `path` is a reference to stdin (`/dev/stdin`, `-`, `/dev/fd/0`).
pub(super) fn is_stdin_path(path: &str) -> bool {
    matches!(path, "/dev/stdin" | "-" | "/dev/fd/0")
}

/// Resolve `--body-file <path>` to a concrete filesystem path, materialising
/// stdin and pipe/FIFO sources eagerly.
///
/// Returns `(resolved_path_string, Option<temp_file_path>)`.  When a temp
/// file was created the caller is responsible for deleting it (the `PathBuf`
/// is returned so the caller can `fs::remove_file` it after the subprocess
/// finishes).
///
/// Fails loudly if the body source is empty — an empty description is almost
/// certainly a bug, not intentional.
pub(super) fn resolve_body_file(path: &str) -> Result<(String, Option<PathBuf>)> {
    use std::io::Read;

    // Decide whether we need to slurp the content into memory.
    let is_stdin_like = is_stdin_path(path);

    #[cfg(unix)]
    let is_pipe_or_special = if is_stdin_like {
        true
    } else {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.file_type().is_fifo() || meta.file_type().is_char_device(),
            Err(_) => false,
        }
    };

    #[cfg(not(unix))]
    let is_pipe_or_special = is_stdin_like;

    if is_pipe_or_special {
        // Slurp eagerly before any subprocess can race on the fd.
        let mut content = String::new();
        if is_stdin_like {
            std::io::stdin().read_to_string(&mut content).map_err(CubeError::Io)?;
        } else {
            std::fs::File::open(path)
                .and_then(|mut f| f.read_to_string(&mut content))
                .map_err(CubeError::Io)?;
        }

        if content.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "--body-file `{path}` produced empty content; \
                 refusing to create a PR with no description"
            )));
        }

        // Write to a uniquely-named temp file so gh pr create can open it as
        // a regular file (no race, no /dev/stdin weirdness in the subprocess).
        let tmp_path = std::env::temp_dir().join(format!("cube-pr-body-{}.md", Uuid::new_v4()));
        std::fs::write(&tmp_path, content.as_bytes()).map_err(CubeError::Io)?;
        let tmp_path_str = tmp_path.display().to_string();
        Ok((tmp_path_str, Some(tmp_path)))
    } else {
        // Regular file path — validate it exists and is non-empty.
        let meta =
            std::fs::metadata(path).map_err(|e| CubeError::InvalidArgument(format!("--body-file `{path}`: {e}")))?;
        if meta.len() == 0 {
            return Err(CubeError::InvalidArgument(format!(
                "--body-file `{path}` is empty; \
                 refusing to create a PR with no description"
            )));
        }
        Ok((path.to_string(), None))
    }
}

fn stack_command_name(command: &StackCommand) -> &'static str {
    match command {
        StackCommand::Rebase { .. } => "rebase",
    }
}

fn pr_command_name(command: &PrCommand) -> &'static str {
    match command {
        PrCommand::Create(_) => "create",
        PrCommand::Update(_) => "update",
        PrCommand::Ensure(_) => "ensure",
        PrCommand::Push(_) => "push",
        PrCommand::Sync { .. } => "sync",
        PrCommand::Merge { .. } => "merge",
    }
}

fn human_change_detail(record: &ChangeRecord) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!(
            "{} {}",
            dim.apply_to("change_id:"),
            style(&record.change_id).cyan().bold(),
        ),
        format!("{} {}", dim.apply_to("repo:"), record.repo),
        format!(
            "{} {}",
            dim.apply_to("workspace_path:"),
            abbreviate_path(&record.workspace_path),
        ),
        format!("{} {}", dim.apply_to("title:"), record.title),
        format!(
            "{} {}",
            dim.apply_to("jj_change_id:"),
            dim.apply_to(&record.jj_change_id),
        ),
        format!("{} {}", dim.apply_to("head_commit:"), dim.apply_to(&record.head_commit),),
    ];
    if let Some(parent_change_id) = &record.parent_change_id {
        lines.push(format!("{} {}", dim.apply_to("parent_change_id:"), parent_change_id,));
    }
    lines.push(format!(
        "{} {}",
        dim.apply_to("created_at_epoch_s:"),
        record.created_at_epoch_s,
    ));
    lines.join("\n")
}
