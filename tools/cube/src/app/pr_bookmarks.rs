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
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use git_utils::pr_bookmark;

use crate::app::errors::{CubeError, Result};
use crate::app::gh_pr;
use crate::app::jj::{run_jj, run_jj_network_within};
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
    for name in names {
        if !can_delete(runner, database_path, cwd, name)? {
            return Err(CubeError::InvalidArgument(format!(
                "refusing to delete {name}: a side is in use or has no remote protection"
            )));
        }
    }
    let mut args: Vec<&str> = vec!["bookmark", "delete"];
    args.extend(names.iter().map(String::as_str));
    run(runner, database_path, cwd, &args)?;
    export_git_refs(runner, database_path, cwd)?;
    for name in names {
        verify_git_ref(runner, database_path, cwd, name, &[])?;
    }
    Ok(())
}

/// Point `pr/<pr_number>` at `head_ref` (the PR's authoritative GitHub head,
/// e.g. `feature@github`) and leave jj and git agreeing about it.
///
/// Every existing target, including a singleton stale ref, is replaced only
/// when every conflicting side is an ancestor of the head or recorded PR
/// history, or a proven predecessor of commits the head contains. Otherwise this fails
/// naming both commits; it never picks a side and never forgets-and-retries.
/// Only `point_published_pr_bookmark` enables conflicted-side rewrite acceptance.
pub(super) fn point_pr_bookmark(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr_number: u64,
    head_ref: &str,
) -> Result<()> {
    point_pr_bookmark_with_mode(runner, database_path, cwd, pr_number, head_ref, false)
}

/// Publication alone may discard conflicted rewrites of published PR history.
pub(super) fn point_published_pr_bookmark(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr_number: u64,
    head_ref: &str,
) -> Result<()> {
    point_pr_bookmark_with_mode(runner, database_path, cwd, pr_number, head_ref, true)
}

fn point_pr_bookmark_with_mode(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    pr_number: u64,
    head_ref: &str,
    publication: bool,
) -> Result<()> {
    let name = pr_bookmark::pr_bookmark_name(pr_number);

    // Surface any stale git ref (left by a past `forget`) as a bookmark/conflict
    // now, while we can see and resolve it.
    import_git_refs(runner, database_path, cwd)?;

    let head = resolve_one_commit(runner, database_path, cwd, head_ref)?;
    let sides = bookmark_commits(runner, database_path, cwd, &name)?;
    let policy = RepairPolicy {
        recorded_heads: recorded_heads(cwd, pr_number),
        publication,
    };
    ensure_sides_are_obsolete(runner, database_path, cwd, &name, &head, &sides, &policy)?;

    run(
        runner,
        database_path,
        cwd,
        &["bookmark", "set", &name, "-r", head_ref, "--allow-backwards"],
    )?;
    export_git_refs(runner, database_path, cwd)?;

    let mut after = bookmark_commits(runner, database_path, cwd, &name)?;
    if after != [head.clone()] {
        ensure_sides_are_obsolete(runner, database_path, cwd, &name, &head, &after, &policy)?;
        run(
            runner,
            database_path,
            cwd,
            &["bookmark", "set", &name, "-r", &head, "--allow-backwards"],
        )?;
        export_git_refs(runner, database_path, cwd)?;
        after = bookmark_commits(runner, database_path, cwd, &name)?;
    }
    if after != [head.clone()] {
        return Err(CubeError::InvalidArgument(format!(
            "`{name}` does not resolve to the PR head after being set: expected exactly `{head}`, found {} \
             commit(s) [{}]. Cube set it from `{head_ref}` and exported it to git, so something else is \
             writing `{name}` concurrently; refusing to continue on an ambiguous bookmark.",
            after.len(),
            after.join(", "),
        )));
    }
    verify_git_ref(runner, database_path, cwd, &name, std::slice::from_ref(&head))?;
    // Callers hold the PR-bookmark lock. A store that cannot hold the record
    // loses only the durable proof, never the correctly written bookmark.
    if let Err(error) = record_head(cwd, pr_number, &head) {
        eprintln!("warning: cube could not record `{head}` as a head of PR {pr_number}: {error}");
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

/// [`bookmark_commits`] rendered for a diagnostic: never fails.
pub(super) fn bookmark_commits_best_effort(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
) -> String {
    match bookmark_commits(runner, database_path, cwd, name) {
        Ok(commits) if !commits.is_empty() => commits.join(", "),
        Ok(_) => "none".to_string(),
        Err(error) => format!("unreadable: {error}"),
    }
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

/// Recorded PR heads provide provenance; publication enables conflicted-side repair.
struct RepairPolicy {
    recorded_heads: Vec<String>,
    publication: bool,
}

/// Accept ancestors of the head or recorded PR heads, and proven predecessors
/// of the head's history. Publication additionally accepts a conflicted side
/// when every unique commit evolves from published PR history outside trunk.
/// This permits clean rebased intermediates, but preserves clean unpublished
/// amends. Matching change ids alone never establish provenance.
/// If trunk resolves to root, the publication-only rule fails closed.
/// Base-only siblings also qualify when every authored commit has an empty
/// interdiff against its unique same-change counterpart in the head.
fn ensure_sides_are_obsolete(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
    head: &str,
    sides: &[String],
    policy: &RepairPolicy,
) -> Result<()> {
    let recorded = policy
        .recorded_heads
        .iter()
        .map(|h| format!("present({h})"))
        .collect::<Vec<_>>()
        .join(" | ");
    for side in sides.iter().filter(|s| s.as_str() != head) {
        let mut revset = format!("(::{side}) ~ (::{head})");
        if !recorded.is_empty() {
            revset = format!("({revset}) ~ ::({recorded})");
        }
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
        if unique.len() != out.lines().filter(|line| !line.trim().is_empty()).count() {
            return Err(unsafe_repair(
                name,
                head,
                side,
                "could not parse the unique commit history",
            ));
        }
        if unique.len() > MAX_UNIQUE_SIDE_COMMITS {
            return Err(unsafe_repair(name, head, side, "more than 100 commits the head lacks"));
        }

        for (change, commit) in unique {
            let mut candidates = String::new();
            let proven = git_utils::jj_provenance::proven_successor(
                |args| {
                    let output = run(runner, database_path, cwd, args)?;
                    if args[0] == "log" {
                        candidates.clone_from(&output);
                    }
                    Ok::<_, CubeError>(output)
                },
                change,
                &format!("::{head}"),
                commit,
            )
            .map_err(|error| unsafe_repair(name, head, side, &format!("evolution unavailable: {error}")))?
            .is_some();
            if proven {
                continue;
            }
            let conflicted = policy.publication
                && !recorded.is_empty()
                && !run(
                    runner,
                    database_path,
                    cwd,
                    &[
                        "log",
                        "--no-graph",
                        "-r",
                        &format!("{side} & conflicts()"),
                        "-T",
                        "commit_id",
                    ],
                )?
                .trim()
                .is_empty();
            let has_trunk = conflicted
                && !run(
                    runner,
                    database_path,
                    cwd,
                    &["log", "--no-graph", "-r", "trunk() ~ root()", "-T", "commit_id"],
                )?
                .trim()
                .is_empty();
            let rewrite = has_trunk
                && git_utils::jj_provenance::is_rewrite_of_history(
                    |args| run(runner, database_path, cwd, args),
                    commit,
                    &format!("::({recorded}) ~ ::trunk()"),
                )
                .map_err(|error| unsafe_repair(name, head, side, &format!("evolution unavailable: {error}")))?;
            let equivalent = rewrite
                || equivalent_sibling(runner, database_path, cwd, commit, &candidates)
                    .map_err(|error| unsafe_repair(name, head, side, &format!("interdiff unavailable: {error}")))?;
            if !equivalent {
                let publication_rule = if policy.publication {
                    ", nor part of a conflicted side rewritten from published PR history with a known trunk"
                } else {
                    ""
                };
                return Err(unsafe_repair(
                    name,
                    head,
                    side,
                    &format!(
                        "commit {commit} is neither an ancestor of a head recorded for this PR \
                         nor a recorded predecessor of the head's history, nor a content-equivalent same-change sibling{publication_rule}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Compare patches on the same parents, so base-only sibling rewrites are safe.
fn equivalent_sibling(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    commit: &str,
    candidates: &str,
) -> Result<bool> {
    let Some(candidate) = git_utils::jj_provenance::sole_candidate(candidates) else {
        return Ok(false);
    };
    let diff = run(
        runner,
        database_path,
        cwd,
        &["interdiff", "--from", commit, "--to", candidate, "--summary"],
    )?;
    if !diff.trim().is_empty() {
        return Ok(false);
    }
    eprintln!("cube: obsolete sibling {commit} of {candidate}: same change id and empty interdiff");
    crate::audit!(
        database_path,
        "workspace.pr_obsolete_sibling",
        obsolete_commit = commit,
        published_commit = candidate,
        reason = "same change id and empty interdiff",
    );
    Ok(true)
}

/// Wording shared by [`unsafe_repair`] and [`is_unsafe_repair`].
const UNSAFE_REPAIR_PHRASE: &str = "cube will not pick a side";

/// Whether `error` is the deterministic refusal to repair a conflicted
/// bookmark, which no plain retry can get past.
pub(super) fn is_unsafe_repair(error: &CubeError) -> bool {
    error.to_string().contains(UNSAFE_REPAIR_PHRASE)
}

fn unsafe_repair(name: &str, head: &str, side: &str, why: &str) -> CubeError {
    CubeError::InvalidArgument(format!(
        "`{name}` is conflicted and {UNSAFE_REPAIR_PHRASE}: the GitHub head is {head} but the other side \
         {side} has unique content ({why}). Inspect with `jj log -r 'bookmarks(exact:\"{name}\")'`; if {side} \
         is really obsolete, resolve with `jj bookmark set {name} -r {head} --allow-backwards`."
    ))
}

/// Export may exit successfully while warning that a ref could not be moved.
/// The @git target changes only after a successful export of that ref.
fn verify_git_ref(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
    expected: &[String],
) -> Result<()> {
    let actual = run(
        runner,
        database_path,
        cwd,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("present({name}@git)"),
            "-T",
            r#"commit_id ++ "\n""#,
        ],
    )?;
    if distinct_lines(&actual) != expected {
        return Err(CubeError::InvalidArgument(format!(
            "failed to export refs/heads/{name}: expected {expected:?}, @git is {actual:?}"
        )));
    }
    Ok(())
}

/// A bookmark can be collected only if all its sides are remotely protected
/// and none is an ancestor of a working copy. @git is local, not protection.
pub(super) fn can_delete(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    name: &str,
) -> Result<bool> {
    let unsafe_sides = run(
        runner,
        database_path,
        cwd,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!(
                r#"(bookmarks(exact:"{name}") & ::working_copies()) | (bookmarks(exact:"{name}") ~ ::remote_bookmarks())"#
            ),
            "-T",
            r#"commit_id ++ "\n""#,
        ],
    )?;
    Ok(unsafe_sides.trim().is_empty())
}

// ── A PR's authoritative head ───────────────────────────────────────────────
//
// The commit GitHub reports as `head.sha`, shared by `goto --pr` and
// lease-time `pr/<n>` repair so the two cannot disagree about which commit is
// the PR head. `resolve_pr_head` makes it present in the shared jj store
// (fetching it from the head repository when the base remote lacks it). The
// *recorded heads* of a PR are every commit cube successfully pointed `pr/<n>`
// at, kept in a cube-owned file inside the shared store: a conflicting side
// whose unique commits are all ancestors of a recorded head was a head of
// *this PR* and lost nothing when GitHub's head replaced it, however long ago
// and whatever became of any branch it was pushed to.

/// How many distinct recorded heads are kept per PR (oldest dropped first).
const MAX_RECORDED_HEADS: usize = 64;

/// The resolved head of a PR.
pub(super) struct PrHead {
    /// `head.sha`, guaranteed present in the shared jj store.
    pub(super) sha: String,
    /// Whether the head branch lives in the PR's base repository. A fork
    /// branch shares its name with nothing here and must never be mapped onto
    /// a local bookmark of that name.
    pub(super) same_repo: bool,
}

fn head_repo_full_name(info: &Value) -> Option<&str> {
    info.get("head")?.get("repo")?.get("full_name")?.as_str()
}

fn sha_is_present(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path, sha: &str) -> Result<bool> {
    let out = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({sha})"),
                "-T",
                r#"commit_id ++ "\n""#,
            ],
        ),
    )?;
    Ok(!out.trim().is_empty())
}

/// Resolve the head of the PR described by `info` (its REST JSON) and make it
/// present in the shared store.
///
/// The caller has normally just fetched the base remote, so the common case is
/// a single local lookup. When the commit is still missing, it is fetched from
/// the head repository's branch through a temporary jj remote, and must then
/// equal `head.sha` exactly; a head that cannot be had is an error. Network
/// work is bounded by `deadline` and runs before any PR-bookmark lock is taken.
pub(super) fn resolve_pr_head(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    owner_repo: &str,
    number: u64,
    info: &Value,
    deadline: Option<Instant>,
) -> Result<PrHead> {
    let sha = gh_pr::head_sha(info)
        .ok_or_else(|| CubeError::InvalidArgument(format!("PR {number} returned no head.sha")))?
        .to_string();
    let head_repo = head_repo_full_name(info);
    let same_repo = head_repo.is_none_or(|name| name.eq_ignore_ascii_case(owner_repo));
    let head = PrHead {
        sha: sha.clone(),
        same_repo,
    };
    if sha_is_present(runner, database_path, cwd, &sha)? {
        return Ok(head);
    }

    let fetch_error = fetch_head_from_head_repo(runner, database_path, cwd, number, info, deadline);
    if sha_is_present(runner, database_path, cwd, &sha)? {
        return Ok(head);
    }
    Err(CubeError::InvalidArgument(format!(
        "PR {number}'s head {sha} (on {}) is not in the shared jj store{}",
        head_repo.unwrap_or("an unknown repository"),
        match fetch_error {
            Ok(()) => " and fetching the head branch did not bring it in (the branch moved again)".to_string(),
            Err(error) => format!(" and could not be fetched: {error}"),
        }
    )))
}

fn fetch_head_from_head_repo(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    number: u64,
    info: &Value,
    deadline: Option<Instant>,
) -> Result<()> {
    let head = info.get("head");
    let branch =
        gh_pr::head_ref(info).ok_or_else(|| CubeError::InvalidArgument(format!("PR {number} returned no head.ref")))?;
    let url = head
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("clone_url"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| head_repo_full_name(info).map(|name| format!("https://github.com/{name}.git")))
        .ok_or_else(|| CubeError::InvalidArgument(format!("PR {number} reports no head repository")))?;
    let remote = format!("cube-pr-fetch-{number}");
    let jj = |args: &[&str]| run_jj(runner, database_path, &RealCommandRunner::invocation(cwd, "jj", args));
    // A leftover from an interrupted run; absent is the normal case.
    let _ = jj(&["git", "remote", "remove", &remote]);
    jj(&["git", "remote", "add", &remote, &url])?;
    let fetched = run_jj_network_within(
        runner,
        database_path,
        &RealCommandRunner::invocation(cwd, "jj", &["git", "fetch", "--remote", &remote, "--branch", branch]),
        deadline,
    );
    // Remove the temporary remote whether or not the fetch worked. The fetched
    // commits stay in the store.
    let _ = jj(&["git", "remote", "remove", &remote]);
    fetched.map(|_| ())
}

/// `<shared jj repo>/cube-pr-heads/<n>`, one commit id per line, oldest first.
fn heads_path(cwd: &Path, number: u64) -> Result<PathBuf> {
    Ok(crate::lock::shared_repo_dir(cwd)?
        .join("cube-pr-heads")
        .join(number.to_string()))
}

/// Every commit cube recorded as a head of PR `number`. Empty when none was
/// recorded or the store cannot be located.
pub(super) fn recorded_heads(cwd: &Path, number: u64) -> Vec<String> {
    let Ok(path) = heads_path(cwd, number) else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Record `commit` as a head of PR `number`. Callers hold the PR-bookmark lock.
pub(super) fn record_head(cwd: &Path, number: u64, commit: &str) -> Result<()> {
    let path = heads_path(cwd, number)?;
    let mut heads = recorded_heads(cwd, number);
    heads.retain(|h| h != commit);
    heads.push(commit.to_string());
    if heads.len() > MAX_RECORDED_HEADS {
        heads.drain(..heads.len() - MAX_RECORDED_HEADS);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(CubeError::Io)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, heads.join("\n") + "\n").map_err(CubeError::Io)?;
    std::fs::rename(&tmp, &path).map_err(CubeError::Io)
}
