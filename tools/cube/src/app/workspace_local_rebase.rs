//! Rebase preserved work without publishing or collapsing history.
use std::path::Path;

use serde_json::json;

use super::errors::{CubeError, Result, RunResult};
use super::jj::{run_jj, run_jj_network};
use crate::command_runner::{CommandRunner, RealCommandRunner};

pub(super) fn local_rebase(
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    pr: Option<u64>,
    base: Option<&str>,
) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;
    rebase_at(runner, database_path, &cwd, pr, base)
}

pub(super) fn rebase_at(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr: Option<u64>,
    base: Option<&str>,
) -> Result<RunResult> {
    if pr.is_some() == base.is_some() {
        return Err(CubeError::InvalidArgument(
            "specify exactly one of --pr or --base".into(),
        ));
    }
    let (remote, owner_repo) = super::reset::resolve_github_remote_for_workspace(runner, database_path, cwd)?;
    let base = if let Some(pr) = pr {
        runner.run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &[
                "pr",
                "view",
                &pr.to_string(),
                "-R",
                &owner_repo,
                "--json",
                "baseRefName",
                "--jq",
                ".baseRefName",
            ],
        ))?
    } else {
        base.unwrap_or_default().to_owned()
    };
    let base = base.trim();
    if base.is_empty() {
        return Err(CubeError::InvalidArgument(
            "The PR base branch must not be empty".into(),
        ));
    }
    run_jj_network(
        runner,
        database_path,
        &RealCommandRunner::invocation(cwd, "jj", &["git", "fetch", "--remote", &remote]),
    )?;
    // Quote symbols so branch names are never interpreted as revset operators.
    let destination = format!("{}@{}", serde_json::to_string(base)?, serde_json::to_string(&remote)?);
    let rebase_output =
        super::pr_rebase::rebase_preserving_pr_bookmarks(runner, database_path, cwd, "@", &destination)?;
    let conflicts = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                &format!("{destination}..@"),
                "--no-graph",
                "-T",
                "commit_id ++ \" conflicts=\" ++ conflict ++ \"\\n\"",
            ],
        ),
    )?;
    RunResult::new(
        format!("Local rebase onto {base}@{remote}; no push or commit collapse.\n{conflicts}"),
        json!({"base_branch": base, "remote": remote, "destination": destination,
               "rebase_output": rebase_output, "commits": conflicts}),
    )
}
