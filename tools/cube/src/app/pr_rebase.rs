//! Keep published PR pointers out of local-only branch rewrites.
use std::path::Path;

use super::errors::{CubeError, Result};
use super::jj::run_jj;
use super::pr_bookmarks::{export_git_refs, import_git_refs};
use crate::command_runner::{CommandRunner, RealCommandRunner};

pub(super) const PR_TARGETS: &str =
    r#"if(!remote && name.starts_with("pr/") && normal_target, name ++ "\t" ++ normal_target.commit_id() ++ "\n")"#;

pub(super) fn rebase_preserving_pr_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    branch: &str,
    destination: &str,
) -> Result<String> {
    let _lock = runner.lock_pr_bookmarks(cwd)?;
    let run = |args: &[&str]| run_jj(runner, database_path, &RealCommandRunner::invocation(cwd, "jj", args));
    import_git_refs(runner, database_path, cwd)?;
    // -b rewrites this whole descendant closure, including other PR stacks.
    let targets = run(&[
        "bookmark",
        "list",
        "-r",
        &format!("({destination}..{branch})::"),
        "-T",
        PR_TARGETS,
    ])?;
    // Ambiguous locals stay attached for positioning to repair; remote rows
    // must never be restored under a bare local name.
    let saved = targets
        .lines()
        .map(|line| {
            let (name, commit) = line
                .split_once('\t')
                .ok_or_else(|| CubeError::InvalidArgument("cannot parse PR bookmark before rebase".into()))?;
            if commit.is_empty() || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(CubeError::InvalidArgument(format!(
                    "cannot rebase ambiguous PR bookmark {name}"
                )));
            }
            Ok((name, commit))
        })
        .collect::<Result<Vec<_>>>()?;
    if saved.is_empty() {
        return run(&["rebase", "-b", branch, "-d", destination, "--ignore-immutable"]);
    }
    // Detach before rewriting, so even an interrupted rebase cannot export an
    // unpublished sibling as a PR head. Delete (never forget) and export retain
    // jj's git tracking semantics. The saved commit ids restore the pointers;
    // an interrupted operation can position an absent pointer from GitHub.
    let mut delete = vec!["bookmark", "delete"];
    delete.extend(saved.iter().map(|(name, _)| *name));
    run(&delete)?;
    let result = export_git_refs(runner, database_path, cwd)
        .and_then(|()| run(&["rebase", "-b", branch, "-d", destination, "--ignore-immutable"]));
    // Restore on both success and failure. Publication advances these only
    // after a verified push; branch and recovery bookmarks keep local work.
    let mut first_error = None;
    let mut failed = Vec::new();
    for (name, commit) in saved {
        if let Err(error) = run(&["bookmark", "set", name, "-r", commit, "--allow-backwards"]) {
            failed.push(name);
            first_error.get_or_insert(error);
        }
    }
    if let Err(error) = export_git_refs(runner, database_path, cwd) {
        first_error.get_or_insert(error);
    }
    if let Some(error) = first_error {
        let detail = format!("PR bookmark restoration failed [{}]: {error}", failed.join(", "));
        eprintln!("{detail}");
        // Keep the primary rebase error, after reporting cleanup failures.
        return result.and_then(|_| Err(CubeError::InvalidArgument(detail)));
    }
    result
}
