//! A PR's authoritative head: the commit GitHub reports as `head.sha`, made
//! present in the shared jj store.
//!
//! `goto --pr` positions on this commit. The branch name only labels it: on a
//! fork-headed PR the same name on the base remote is an unrelated branch.
//! Cube keeps no per-PR pointer of its own; GitHub and the fetched branch
//! heads are the only records of where a PR is.

use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::Value;

use git_utils::pr_bookmark::NO_PR_HEAD_PHRASE;

use crate::app::errors::{CubeError, Result};
use crate::app::gh_pr;
use crate::app::jj::{network_cmd_total_bound, run_jj, run_jj_network_within};
use crate::command_runner::{CommandRunner, RealCommandRunner};

fn run(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path, args: &[&str]) -> Result<String> {
    run_jj(runner, database_path, &RealCommandRunner::invocation(cwd, "jj", args))
}

/// Import the shared git repo's refs into jj's view.
pub(super) fn import_git_refs(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path) -> Result<()> {
    run(runner, database_path, cwd, &["git", "import"]).map(|_| ())
}

/// The resolved head of a PR.
pub(super) struct PrHead {
    /// `head.sha`, guaranteed present in the shared jj store.
    pub(super) sha: String,
    /// Whether the head branch lives in the PR's base repository. A fork
    /// branch shares its name with nothing here and must never be mapped onto
    /// a local bookmark of that name.
    pub(super) same_repo: bool,
}

pub(super) fn head_repo_full_name(info: &Value) -> Option<&str> {
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
/// work is bounded by `deadline`.
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
        if let Err(error) = remove_remote(runner, database_path, cwd, name) {
            eprintln!("warning: cube could not remove stale temporary remote {name}: {error}");
        }
    }
}

/// Add `remote` at `url` to the shared store under its remote-change lock, so
/// concurrent cube processes queue instead of tripping jj's fail-fast git
/// config lock.
fn add_remote(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    remote: &str,
    url: &str,
) -> Result<()> {
    let _remotes = runner.lock_git_remotes(cwd)?;
    run(runner, database_path, cwd, &["git", "remote", "add", remote, url]).map(|_| ())
}

/// Remove `remote` from the shared store under its remote-change lock.
fn remove_remote(runner: &dyn CommandRunner, database_path: Option<&Path>, cwd: &Path, remote: &str) -> Result<()> {
    let _remotes = runner.lock_git_remotes(cwd)?;
    run(runner, database_path, cwd, &["git", "remote", "remove", remote]).map(|_| ())
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
    add_remote(runner, database_path, cwd, &remote, &url)?;
    let refspec = format!("+refs/pull/{number}/head:refs/remotes/{remote}/pull-{number}");
    // Serialize fetches as well as config changes: git shares shallow.lock.
    let fetch_lock = runner.lock_git_remotes(cwd)?;
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
    drop(fetch_lock);
    let _ = remove_remote(runner, database_path, cwd, &remote);
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
    add_remote(runner, database_path, cwd, &remote, &url)?;
    // Serialize fetches as well as config changes: git shares shallow.lock.
    let fetch_lock = runner.lock_git_remotes(cwd)?;
    let fetched = run_jj_network_within(
        runner,
        database_path,
        &RealCommandRunner::invocation(cwd, "jj", &["git", "fetch", "--remote", &remote, "--branch", branch]),
        deadline,
    );
    // Remove the temporary remote whether or not the fetch worked. The fetched
    // commits stay in the store.
    drop(fetch_lock);
    let _ = remove_remote(runner, database_path, cwd, &remote);
    fetched.map(|_| ())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
