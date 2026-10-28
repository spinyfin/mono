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

use git_utils::pr_bookmark::{self, NO_PR_HEAD_PHRASE, UNSAFE_REPAIR_PHRASE};

use crate::app::errors::{CubeError, Result};
use crate::app::gh_pr;
use crate::app::jj::{network_cmd_total_bound, run_jj, run_jj_network_within};
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
    let eligible = crate::app::pr_gc::eligible(runner, database_path, cwd, names)?;
    if eligible.len() != names.len() {
        return Err(CubeError::InvalidArgument(
            "refusing to delete PR bookmarks with in-use or unprotected sides".into(),
        ));
    }
    let mut args: Vec<&str> = vec!["bookmark", "delete"];
    args.extend(names.iter().map(String::as_str));
    run(runner, database_path, cwd, &args)?;
    export_git_refs(runner, database_path, cwd)?;
    let refs = names
        .iter()
        .map(|name| format!("present({name}@git)"))
        .collect::<Vec<_>>()
        .join(" | ");
    let remaining = run(
        runner,
        database_path,
        cwd,
        &["log", "--no-graph", "-r", &refs, "-T", r#"commit_id ++ "\n""#],
    )?;
    if !remaining.trim().is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "deleted PR git refs remain: {remaining}"
        )));
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
    ensure_sides_are_obsolete(runner, database_path, cwd, &name, &head, &sides, &policy)
        .map_err(|error| CubeError::InvalidArgument(format!("{error} Requested target came from `{head_ref}`.")))?;

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

/// Read-only proof for the rare predecessor case that ancestry alone cannot
/// classify. GC performs this work outside the store lock, then checks again
/// when repairing. No network is needed for previously confirmed PR content.
pub(super) fn obsolete_at_recorded_head(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    number: u64,
) -> Option<String> {
    let recorded = recorded_heads(cwd, number);
    let head = recorded.last()?;
    let name = pr_bookmark::pr_bookmark_name(number);
    let sides = bookmark_commits(runner, database_path, cwd, &name).ok()?;
    let policy = RepairPolicy {
        recorded_heads: recorded.clone(),
        publication: false,
    };
    ensure_sides_are_obsolete(runner, database_path, cwd, &name, head, &sides, &policy).ok()?;
    Some(head.clone())
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
        // Only fetched trunk refs establish base history, never local main.
        let mut revset = format!(
            "(::{side}) ~ (::{head}) ~ ::(present(main@origin) | present(master@origin) | present(main@github) | present(master@github))"
        );
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
            use git_utils::jj_provenance::{
                COMMIT_ID_LINES_TEMPLATE, candidate_revset, evolution_contains, sole_candidate,
            };
            let scope = if recorded.is_empty() {
                format!("::{head}")
            } else {
                format!("::({head} | {recorded})")
            };
            let candidates = run(
                runner,
                database_path,
                cwd,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &candidate_revset(change, &scope),
                    "-T",
                    COMMIT_ID_LINES_TEMPLATE,
                ],
            )?;
            let proven = if let Some(candidate) = sole_candidate(&candidates) {
                let evolution = run(
                    runner,
                    database_path,
                    cwd,
                    &["evolog", "--no-graph", "-r", candidate, "-T", COMMIT_ID_LINES_TEMPLATE],
                )?;
                evolution_contains(&evolution, commit)
            } else {
                false
            };
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

/// Whether `error` is the deterministic refusal to repair a conflicted
/// bookmark, which no plain retry can get past.
pub(super) fn is_unsafe_repair(error: &CubeError) -> bool {
    error.to_string().contains(UNSAFE_REPAIR_PHRASE)
}

fn unsafe_repair(name: &str, head: &str, side: &str, why: &str) -> CubeError {
    CubeError::InvalidArgument(format!(
        "cube cannot move `{name}` from {side} to requested target {head}: {UNSAFE_REPAIR_PHRASE} \
         with unique content ({why}). Inspect with `jj log -r 'bookmarks(exact:\"{name}\")'`; if {side} \
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

// ── A PR's authoritative head ───────────────────────────────────────────────
//
// The commit GitHub reports as `head.sha`, shared by `goto --pr` and
// lease-time `pr/<n>` repair so the two cannot disagree about which commit is
// the PR head. `resolve_pr_head` makes it present in the shared jj store
// (fetching it from the head repository when the base remote lacks it). The
// *recorded heads* of a PR are only heads confirmed published on GitHub,
// kept in a cube-owned file inside the shared store: a conflicting side
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
/// the head repository's branch through a temporary jj remote, falling back
/// to the base repository's `refs/pull/<n>/head`. The fetched commit must
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
    let sha =
        gh_pr::head_sha(info).ok_or_else(|| CubeError::InvalidArgument(format!("PR {number} {NO_PR_HEAD_PHRASE}")))?;
    // The value is interpolated into a revset, so only a full commit id may
    // reach it; a short or otherwise odd string could resolve to something
    // other than the PR's head. Shares the no-head phrase so callers classify
    // it as the same deterministic refusal.
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CubeError::InvalidArgument(format!(
            "PR {number} {NO_PR_HEAD_PHRASE}: {sha:?} is not a full 40-character commit id"
        )));
    }
    let sha = sha.to_string();
    let head_repo = head_repo_full_name(info);
    let same_repo = head_repo.is_some_and(|name| name.eq_ignore_ascii_case(owner_repo));
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
    // The source branch is gone (deleted on merge) or its fork was deleted, but
    // the base repository keeps `refs/pull/<n>/head` for the life of the PR.
    let pull_error = fetch_pull_ref(runner, database_path, cwd, owner_repo, number, deadline);
    if sha_is_present(runner, database_path, cwd, &sha)? {
        return Ok(head);
    }
    let describe = |result: Result<()>, what: &str| match result {
        Ok(()) => format!("fetching {what} did not bring it in (the PR head moved again)"),
        Err(error) => format!("fetching {what} failed: {error}"),
    };
    Err(CubeError::InvalidArgument(format!(
        "PR {number}'s head {sha} (on {}) is not in the shared jj store; {}; {}",
        head_repo.unwrap_or("an unknown repository"),
        describe(fetch_error, "the head branch"),
        describe(pull_error, "the base repository's pull ref"),
    )))
}

/// Prefix of the temporary jj remotes used to bring a PR head in. The name is
/// `cube-pr-fetch-<n>-<unix seconds>-<uuid>`: unique per call so concurrent
/// fetches never share a remote, and timestamped so a leftover from a killed
/// run can be told apart from one still in use.
const TEMP_REMOTE_PREFIX: &str = "cube-pr-fetch-";

/// Floor for the age at which a temporary remote is presumed abandoned.
const STALE_TEMP_REMOTE_MIN_AGE_SECS: u64 = 15 * 60;

/// Margin added to the longest possible fetch for the work around it.
const STALE_TEMP_REMOTE_MARGIN_SECS: u64 = 5 * 60;

/// A temporary remote older than this cannot belong to a live fetch: it
/// exceeds every attempt and retry at the configured network timeout, so a
/// raised `CUBE_NETWORK_TIMEOUT_SECS` raises it too.
pub(super) fn stale_temp_remote_age_secs() -> u64 {
    (network_cmd_total_bound().as_secs() + STALE_TEMP_REMOTE_MARGIN_SECS).max(STALE_TEMP_REMOTE_MIN_AGE_SECS)
}

fn temp_remote_name(number: u64) -> String {
    let now = unix_now();
    format!("{TEMP_REMOTE_PREFIX}{number}-{now}-{}", uuid::Uuid::new_v4())
}

/// Remove temporary fetch remotes left behind by a run that was killed between
/// `remote add` and `remote remove`. Their remote-tracking bookmarks would
/// otherwise protect commits from collection forever. Run during normal GC
/// sweeps and before adding a temporary remote. A
/// remote younger than [`stale_temp_remote_age_secs`] is left alone because a
/// live fetch may own it.
pub(super) fn prune_stale_temp_remotes(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path) {
    let Ok(listing) = run(runner, database_path, cwd, &["git", "remote", "list"]) else {
        return;
    };
    let now = unix_now();
    let max_age = stale_temp_remote_age_secs();
    for name in listing.lines().filter_map(|line| line.split_whitespace().next()) {
        let Some(rest) = name.strip_prefix(TEMP_REMOTE_PREFIX) else {
            continue;
        };
        // `<n>-<unix seconds>-<uuid>`
        let created = rest.split('-').nth(1).and_then(|ts| ts.parse::<u64>().ok());
        // A name from before timestamps were added has no way to be live.
        let stale = created.is_none_or(|ts| now.saturating_sub(ts) >= max_age);
        if !stale {
            continue;
        }
        if let Err(error) = run(runner, database_path, cwd, &["git", "remote", "remove", name]) {
            eprintln!("warning: cube could not remove stale temporary remote {name}: {error}");
        }
    }
}

/// URL of the matching GitHub repository, ignoring local mirrors and other
/// repositories with similar names; use the canonical URL if none matches.
fn base_repo_url(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path, owner_repo: &str) -> String {
    let listing = run(runner, database_path, cwd, &["git", "remote", "list"]).unwrap_or_default();
    listing
        .lines()
        .find_map(|line| {
            let (_, slug) = git_utils::repo_slug::parse_github_remote(line)?;
            if !slug.eq_ignore_ascii_case(owner_repo) {
                return None;
            }
            line.split_once(char::is_whitespace)
                .map(|(_, url)| url.trim().to_string())
        })
        .unwrap_or_else(|| format!("https://github.com/{owner_repo}.git"))
}

/// The git directory behind the shared jj store.
fn store_git_dir(cwd: &Path) -> Result<PathBuf> {
    let store = crate::lock::shared_repo_dir(cwd)?.join("store");
    let target = std::fs::read_to_string(store.join("git_target")).map_err(CubeError::Io)?;
    std::fs::canonicalize(store.join(target.trim())).map_err(CubeError::Io)
}

/// Fetch `refs/pull/<number>/head` of the base repository into the store.
///
/// jj itself only fetches branches, so the ref is fetched with git into a
/// remote-tracking ref of a temporary jj remote and then imported; removing the
/// remote afterwards leaves just the commits in the store.
fn fetch_pull_ref(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    owner_repo: &str,
    number: u64,
    deadline: Option<Instant>,
) -> Result<()> {
    let url = base_repo_url(runner, database_path, cwd, owner_repo);
    let git_dir = store_git_dir(cwd)?;
    prune_stale_temp_remotes(runner, database_path, cwd);
    let remote = temp_remote_name(number);
    run(runner, database_path, cwd, &["git", "remote", "add", &remote, &url])?;
    let refspec = format!("+refs/pull/{number}/head:refs/remotes/{remote}/pull-{number}");
    let fetched = run_jj_network_within(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "git",
            &[
                "--git-dir",
                &git_dir.display().to_string(),
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                &url,
                &refspec,
            ],
        ),
        deadline,
    );
    let imported = fetched.and_then(|_| import_git_refs(runner, database_path, cwd));
    let _ = run(runner, database_path, cwd, &["git", "remote", "remove", &remote]);
    imported
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
    prune_stale_temp_remotes(runner, database_path, cwd);
    let remote = temp_remote_name(number);
    let jj = |args: &[&str]| run_jj(runner, database_path, &RealCommandRunner::invocation(cwd, "jj", args));
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

/// `<shared jj repo>/cube-pr-heads/published/<n>`, one commit id per line, oldest first.
fn heads_path(cwd: &Path, number: u64) -> Result<PathBuf> {
    Ok(crate::lock::shared_repo_dir(cwd)?
        .join("cube-pr-heads")
        .join("published")
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
    // Legacy records included unpushed tips and cannot prove publication.
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

/// Persist only a head confirmed by GitHub, while holding the bookmark lock.
pub(super) fn record_published_head(cwd: &Path, number: u64, commit: &str) {
    if let Err(error) = record_head(cwd, number, commit) {
        eprintln!("warning: cube could not record published PR {number} head {commit}: {error}");
    }
}

/// How long a PR bookmark whose head could not be resolved is left alone by
/// the sweep before GitHub is asked about it again.
const UNRESOLVED_RETRY_SECS: u64 = 60 * 60;

fn unresolved_path(cwd: &Path, number: u64) -> Result<PathBuf> {
    Ok(crate::lock::shared_repo_dir(cwd)?
        .join("cube-pr-heads")
        .join("unresolved")
        .join(number.to_string()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a sweep recently failed to resolve PR `number`'s head, so asking
/// GitHub again now would only repeat the same round trip.
pub(super) fn recently_unresolved(cwd: &Path, number: u64) -> bool {
    let Ok(path) = unresolved_path(cwd, number) else {
        return false;
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|at| unix_now().saturating_sub(at) < UNRESOLVED_RETRY_SECS)
}

/// Remember (or, with `unresolved == false`, forget) a failed head resolution.
/// Best-effort: losing the memory only costs one extra lookup.
pub(super) fn set_unresolved(cwd: &Path, number: u64, unresolved: bool) {
    let Ok(path) = unresolved_path(cwd, number) else {
        return;
    };
    if unresolved {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{}\n", unix_now()));
    } else {
        let _ = std::fs::remove_file(path);
    }
}
