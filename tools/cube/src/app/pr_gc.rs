//! Batched local PR retention checks and repair of legacy stale refs.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::app::errors::Result;
use crate::app::{gh_pr, jj::run_jj, pr_bookmarks, reset};
use crate::command_runner::{CommandRunner, RealCommandRunner};

/// Check every candidate in one jj invocation. Protection is specific to each
/// PR's confirmed published heads; local @git refs are never protection.
pub(super) fn eligible(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    names: &[String],
) -> Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let unsafe_revset = names
        .iter()
        .map(|name| {
            let heads = name[3..]
                .parse()
                .ok()
                .map(|number| pr_bookmarks::recorded_heads(cwd, number))
                .unwrap_or_default();
            let mut protection = "remote_bookmarks()".to_string();
            for head in heads {
                protection.push_str(&format!(" | present({head})"));
            }
            format!(
                r#"(bookmarks(exact:"{name}") & ::working_copies()) | (bookmarks(exact:"{name}") ~ ::({protection}))"#
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let unsafe_names = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                &unsafe_revset,
                "-T",
                super::gc::BOOKMARK_NAMES_TEMPLATE,
            ],
        ),
    )?;
    Ok(names
        .iter()
        .filter(|name| !unsafe_names.lines().any(|line| line.trim() == name.as_str()))
        .cloned()
        .collect())
}

pub(super) struct Resolved {
    number: u64,
    sha: String,
    closed: bool,
}

/// Reserve a rotating batch under the store lock, then release it before
/// network work. Advancing even when repair is unsafe prevents starvation.
fn next_batch(runner: &dyn CommandRunner, cwd: &Path, names: &[String]) -> Result<Vec<String>> {
    let _lock = runner.lock_pr_bookmarks(cwd)?;
    let path = crate::lock::shared_repo_dir(cwd)?.join("cube-pr-gc-cursor");
    let cursor = std::fs::read_to_string(&path).unwrap_or_default();
    let mut ordered = names.to_vec();
    ordered.sort();
    let start = ordered.partition_point(|name| name <= &cursor);
    let count = ordered.len();
    ordered.rotate_left(start % count);
    ordered.truncate(MAX_REPAIRS_PER_SWEEP);
    if let Some(last) = ordered.last() {
        std::fs::write(path, last).map_err(super::errors::CubeError::Io)?;
    }
    Ok(ordered)
}

/// Network resolution runs without the store lock. Failures preserve the refs
/// and identify what could not be checked rather than silently claiming success.
pub(super) fn resolve_retained(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    names: &[String],
) -> Vec<Resolved> {
    if names.is_empty() {
        return Vec::new();
    }
    let names = match next_batch(runner, cwd, names) {
        Ok(names) => names,
        Err(error) => {
            eprintln!("warning: cube could not schedule PR repairs: {error}");
            return Vec::new();
        }
    };
    let mut resolved = Vec::new();
    let mut unknown = Vec::new();
    for name in &names {
        let Ok(number) = name[3..].parse() else {
            eprintln!("warning: cube retained {name}: PR number is out of range");
            continue;
        };
        if let Some(sha) = pr_bookmarks::obsolete_at_recorded_head(runner, database_path, cwd, number) {
            // Like the batched ancestry case, a proven predecessor of this
            // PR's published content can be collected without a state lookup.
            resolved.push(Resolved {
                number,
                sha,
                closed: true,
            });
        } else {
            unknown.push(name);
        }
    }
    if unknown.is_empty() {
        return resolved;
    }
    let result = reset::resolve_github_remote_for_workspace(runner, database_path, cwd);
    let (_, repo) = match result {
        Ok(repo) => repo,
        Err(error) => {
            eprintln!("warning: cube retained PR bookmarks {unknown:?}: {error}");
            return resolved;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    for name in unknown {
        if Instant::now() >= deadline {
            eprintln!("warning: cube retained {name}: PR lookup deadline exhausted");
            continue;
        }
        let Ok(number) = name[3..].parse() else {
            eprintln!("warning: cube retained {name}: PR number is out of range");
            continue;
        };
        if pr_bookmarks::recently_unresolved(cwd, number) {
            continue;
        }
        let resolve = || -> Result<Resolved> {
            let info = gh_pr::fetch_pr_json_with_timeout(
                runner,
                cwd,
                &repo,
                number,
                deadline.saturating_duration_since(Instant::now()),
            )?;
            let head = pr_bookmarks::resolve_pr_head(runner, database_path, cwd, &repo, number, &info, Some(deadline))?;
            Ok(Resolved {
                number,
                sha: head.sha,
                closed: matches!(gh_pr::state(&info).as_str(), "CLOSED" | "MERGED"),
            })
        };
        match resolve() {
            Ok(head) => {
                pr_bookmarks::set_unresolved(cwd, number, false);
                resolved.push(head);
            }
            Err(error) => {
                eprintln!("warning: cube retained {name}: {error}");
                pr_bookmarks::set_unresolved(cwd, number, true);
            }
        }
    }
    resolved
}

/// Most bookmarks repaired per lock acquisition. The lock blocks `goto`,
/// `pr push` and lease repair with no timeout, and each repair runs several jj
/// commands, so a large legacy backlog is worked off over successive sweeps
/// instead of in one long critical section.
const MAX_REPAIRS_PER_SWEEP: usize = 20;

/// Called under the lock; re-read sides because goto may have run meanwhile.
/// Open PRs are repaired but kept during this sweep, as are bookmarks beyond
/// [`MAX_REPAIRS_PER_SWEEP`], which a later sweep picks up.
pub(super) fn repair_retained(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    cwd: &Path,
    resolved: &[Resolved],
) -> Vec<String> {
    let mut keep = Vec::new();
    let (batch, deferred) = resolved.split_at(resolved.len().min(MAX_REPAIRS_PER_SWEEP));
    keep.extend(deferred.iter().map(|head| format!("pr/{}", head.number)));
    if batch.is_empty() {
        return keep;
    }
    // One jj call finds every bookmark a workspace is positioned on.
    let revset = batch
        .iter()
        .map(|head| format!(r#"bookmarks(exact:"pr/{}")"#, head.number))
        .collect::<Vec<_>>()
        .join(" | ");
    let in_use = match run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("({revset}) & ::working_copies()"),
                "-T",
                super::gc::BOOKMARK_NAMES_TEMPLATE,
            ],
        ),
    ) {
        Ok(out) => out,
        Err(error) => {
            eprintln!("warning: cube retained PR bookmarks: {error}");
            keep.extend(batch.iter().map(|head| format!("pr/{}", head.number)));
            return keep;
        }
    };
    for head in batch {
        let name = format!("pr/{}", head.number);
        let repair = || -> Result<()> {
            if in_use.lines().any(|line| line.trim() == name) {
                return Err(super::errors::CubeError::InvalidArgument(format!("{name} is in use")));
            }
            pr_bookmarks::record_published_head(cwd, head.number, &head.sha);
            pr_bookmarks::point_pr_bookmark(runner, database_path, cwd, head.number, &head.sha)
        };
        if let Err(error) = repair() {
            eprintln!("warning: cube retained {name}: {error}");
            keep.push(name);
        } else if !head.closed {
            keep.push(name);
        }
    }
    keep
}
