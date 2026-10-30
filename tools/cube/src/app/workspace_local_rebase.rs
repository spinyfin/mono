//! Rebase preserved revision work without publishing or collapsing history.
use std::path::Path;

use serde_json::json;

use super::errors::{CubeError, Result, RunResult};
use super::jj::{run_jj, run_jj_network};
use crate::command_runner::{CommandRunner, RealCommandRunner};

pub(super) fn local_rebase(database_path: Option<&Path>, runner: &dyn CommandRunner, pr: u64) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;
    rebase_at(runner, database_path, &cwd, pr)
}

pub(super) fn rebase_at(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr: u64,
) -> Result<RunResult> {
    let (remote, owner_repo) = super::reset::resolve_github_remote_for_workspace(runner, database_path, cwd)?;
    let base = runner.run(&RealCommandRunner::invocation(
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
    ))?;
    let base = base.trim();
    if base.is_empty() {
        return Err(CubeError::InvalidArgument(
            "GitHub returned an empty PR base branch".into(),
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
