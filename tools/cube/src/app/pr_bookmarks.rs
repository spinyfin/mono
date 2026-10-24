//! Keeps cube's local `pr/<n>` bookmarks consistent with the colocated git
//! repo behind the shared jj store.
//!
//! ## The failure this exists to prevent
//!
//! A repo's canonical source checkout is a *colocated* jj repo: it has a real
//! `.git/` next to `.jj/`, and jj mirrors every local bookmark into a
//! `refs/heads/*` ref there. Pool workspaces are *secondary* jj workspaces of
//! that same store and are not colocated, so a bookmark written from one of
//! them is **not** exported to git until some jj command runs at the
//! colocated root. jj remembers what it last exported or imported for every
//! git ref (the `@git` remote-tracking state), and uses that record to tell
//! "the ref is unchanged since I wrote it" from "someone else wrote this ref".
//!
//! `jj bookmark forget` deletes that record along with the bookmark but
//! leaves `refs/heads/pr/<n>` in git. The next command at the colocated root
//! auto-imports the surviving ref as if a stranger had created it. If cube
//! has meanwhile set `pr/<n>` to the PR's current head from a secondary
//! workspace, the imported stale target and the new local target conflict,
//! and `bookmarks(exact:"pr/<n>")` resolves to two commits. Every later
//! positioning attempt then fails, cube forgets the bookmark again, and the
//! cycle repeats.
//!
//! ## The rule
//!
//! `pr/<n>` bookmarks are *only* ever removed with `jj bookmark delete`
//! (never `forget`), so jj keeps the record and the deletion reaches git on
//! export; every cube write to one is followed by an explicit `jj git
//! export`, which works from any workspace because the git repo belongs to
//! the shared store; and every write to one is preceded by `jj git import`,
//! so a stale ref left behind by an older cube is surfaced and resolved
//! *before* cube sets its own target on top of it instead of colliding with
//! it at some later, unrelated moment.
//!
//! The alternative — keeping `pr/*` out of git entirely — is not available:
//! jj has no export filter, and refusing to colocate is exactly the global
//! switch the source repo cannot do without.

use std::collections::BTreeSet;
use std::path::Path;

use git_utils::pr_bookmark;

use crate::app::errors::{CubeError, Result};
use crate::app::jj::run_jj;
use crate::command_runner::{CommandRunner, RealCommandRunner};

/// How many commits unique to one side of a conflicted bookmark the safety
/// check will inspect. A side with more than this is not "an obsolete
/// rewrite of one commit" and is refused rather than traversed.
const MAX_UNIQUE_SIDE_COMMITS: usize = 100;

fn run(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path, args: &[&str]) -> Result<String> {
    run_jj(runner, database_path, &RealCommandRunner::invocation(cwd, "jj", args))
}

/// Import the shared git repo's refs into jj's view.
pub(super) fn import_git_refs(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path) -> Result<()> {
    run(runner, database_path, cwd, &["git", "import"]).map(|_| ())
}

/// Export jj's local bookmarks to the shared git repo, so a bookmark written
/// or deleted from a non-colocated workspace reaches git now rather than at
/// whenever the colocated root next runs a jj command.
pub(super) fn export_git_refs(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path) -> Result<()> {
    run(runner, database_path, cwd, &["git", "export"]).map(|_| ())
}

/// Delete `pr/<n>` bookmarks and propagate the deletion to git.
///
/// Uses `bookmark delete`, not `forget`: see the module docs.
pub(super) fn delete_pr_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    names: &[String],
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut args: Vec<&str> = vec!["bookmark", "delete"];
    args.extend(names.iter().map(String::as_str));
    run(runner, database_path, cwd, &args)?;
    export_git_refs(runner, database_path, cwd)
}

/// Point `pr/<pr_number>` at `head_ref` (the PR's authoritative GitHub head,
/// e.g. `feature@github`) and leave jj and git agreeing about it.
///
/// If the bookmark is already conflicted, it is repaired to the head only
/// when every conflicting side is an ancestor of the head or an obsolete
/// predecessor of commits the head already contains. Otherwise this fails
/// naming both commits; it never picks a side and never forgets-and-retries.
pub(super) fn point_pr_bookmark(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr_number: u64,
    head_ref: &str,
) -> Result<()> {
    let name = pr_bookmark::pr_bookmark_name(pr_number);

    // Surface any stale git ref (left by a past `forget`) as a bookmark/conflict
    // now, while we can see and resolve it.
    import_git_refs(runner, database_path, cwd)?;

    let head = resolve_one_commit(runner, database_path, cwd, head_ref)?;
    let sides = bookmark_commits(runner, database_path, cwd, &name)?;
    if sides.len() > 1 {
        ensure_sides_are_obsolete(runner, database_path, cwd, &name, &head, &sides)?;
    }

    run(
        runner,
        database_path,
        cwd,
        &["bookmark", "set", &name, "-r", head_ref, "--allow-backwards"],
    )?;
    export_git_refs(runner, database_path, cwd)?;

    let after = bookmark_commits(runner, database_path, cwd, &name)?;
    if after != [head.clone()] {
        return Err(CubeError::InvalidArgument(format!(
            "`{name}` does not resolve to the PR head after being set: expected exactly `{head}`, found {} \
             commit(s) [{}]. Cube set it from `{head_ref}` and exported it to git, so something else is \
             writing `{name}` concurrently; refusing to continue on an ambiguous bookmark.",
            after.len(),
            after.join(", "),
        )));
    }
    Ok(())
}

/// Distinct commit ids a local bookmark resolves to: one for a healthy
/// bookmark, several for a conflicted one, none for an absent one.
fn bookmark_commits(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
) -> Result<Vec<String>> {
    let revset = format!("bookmarks(exact:\"{name}\")");
    let out = run(
        runner,
        database_path,
        cwd,
        &["log", "--no-graph", "-r", &revset, "-T", r#"commit_id ++ "\n""#],
    )?;
    Ok(distinct_lines(&out))
}

fn resolve_one_commit(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    revset: &str,
) -> Result<String> {
    let out = run(
        runner,
        database_path,
        cwd,
        &["log", "--no-graph", "-r", revset, "-T", r#"commit_id ++ "\n""#],
    )?;
    let commits = distinct_lines(&out);
    match commits.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(CubeError::InvalidArgument(format!(
            "`{revset}` must resolve to exactly one commit; found {} [{}]",
            commits.len(),
            commits.join(", ")
        ))),
    }
}

fn distinct_lines(out: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| seen.insert((*l).to_string()))
        .map(str::to_owned)
        .collect()
}

/// Refuse to repair a conflicted `name` unless every side that is not the head
/// carries nothing the head lacks.
///
/// A side is safe when it is an ancestor of the head, or when each commit it
/// has beyond the head's history shares a change id with a commit *in* the
/// head's history — i.e. the head already contains a rewrite of it (jj shows
/// such a pair as divergent). Anything else is content only that side has.
fn ensure_sides_are_obsolete(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
    head: &str,
    sides: &[String],
) -> Result<()> {
    for side in sides.iter().filter(|s| s.as_str() != head) {
        let revset = format!("(::{side}) ~ (::{head})");
        let limit = (MAX_UNIQUE_SIDE_COMMITS + 1).to_string();
        let out = run(
            runner,
            database_path,
            cwd,
            &[
                "log",
                "--no-graph",
                "-n",
                &limit,
                "-r",
                &revset,
                "-T",
                r#"change_id ++ "\t" ++ commit_id ++ "\n""#,
            ],
        )?;
        let unique: Vec<(&str, &str)> = out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(change, commit)| (change.trim(), commit.trim()))
            .collect();
        if unique.is_empty() {
            continue; // ancestor of the head
        }
        if unique.len() > MAX_UNIQUE_SIDE_COMMITS {
            return Err(unsafe_repair(name, head, side, "more than 100 commits the head lacks"));
        }

        // Which of those change ids already have a commit in the head's
        // history? A change id as a revset resolves to every visible commit
        // carrying it, so intersecting with the head's ancestry yields the
        // head's rewrite of each one.
        let changes: BTreeSet<&str> = unique.iter().map(|(change, _)| *change).collect();
        let union = changes.iter().copied().collect::<Vec<_>>().join(" | ");
        let rewritten = run(
            runner,
            database_path,
            cwd,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("(::{head}) & ({union})"),
                "-T",
                r#"change_id ++ "\n""#,
            ],
        )?;
        let superseded: BTreeSet<String> = distinct_lines(&rewritten).into_iter().collect();
        if let Some((_, commit)) = unique.iter().find(|(change, _)| !superseded.contains(*change)) {
            return Err(unsafe_repair(
                name,
                head,
                side,
                &format!("commit {commit} is not in the head's history and has no rewrite there"),
            ));
        }
    }
    Ok(())
}

fn unsafe_repair(name: &str, head: &str, side: &str, why: &str) -> CubeError {
    CubeError::InvalidArgument(format!(
        "`{name}` is conflicted and cube will not pick a side: the GitHub head is {head} but the other side \
         {side} has unique content ({why}). Inspect with `jj log -r 'bookmarks(exact:\"{name}\")'`; if {side} \
         is really obsolete, resolve with `jj bookmark set {name} -r {head} --allow-backwards`."
    ))
}
