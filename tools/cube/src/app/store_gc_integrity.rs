//! Collect from a freshly reconstructed index, never an inherited cache.
//!
//! jj 0.26 accepts indexes written by newer jj releases that omit legacy
//! predecessors. GC trusts that index, whereas a 0.26 rebuild walks those
//! predecessors. An ordinary `jj log` therefore cannot prove collection safe.
//!
//! The same applies to the collector itself: a `jj` newer than the oldest
//! client sharing the store builds a fresh index that omits those
//! predecessors, prunes them, and passes every verification with the same
//! binary. The collector's version is therefore checked against
//! [`OLDEST_SHARED_CLIENT_JJ`] before anything runs, and recorded in the
//! audit rows.
//!
//! A pinned 200-operation mono sample contained 11 two-parent merges. Parent
//! checks therefore walk the DAG: concurrent merges are safe only when every
//! branch reaches the checked head, without reaching abandoned history.
//!
//! Collection also never rebuilds the live `.jj/repo/index`. That is
//! deliberate and costs nothing: neither jj 0.26 nor 0.32's `util gc` prunes the
//! default index store (tested against repos with abandoned operations:
//! all index file paths and contents, including operation links and segments,
//! are identical before and after;
//! see `store_gc_default_index_retention_for_both_client_versions`).
//! Verification still uses a private scratch index so a damaged live cache
//! cannot hide missing objects. `util gc` itself runs against the live store:
//! the private index is a snapshot of the collection checkpoint, and a lease
//! that publishes a head after that checkpoint would otherwise look
//! unreachable. A rewrite itself orphans the old operation links; reclaiming
//! them is a `jj debug reindex` of the live store, which is a manual step, not
//! part of this pass.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::errors::{CubeError, Result};
use crate::command_runner::{CommandRunner, JJ_GATE_HELD_ENV, RealCommandRunner};

/// The oldest jj (major, minor) that shares the stores cube maintains. The
/// collector must not be newer: see the module doc.
pub(super) const OLDEST_SHARED_CLIENT_JJ: (u64, u64) = (0, 26);

/// Newest collector whose pruning and lease behavior is tested.
pub(super) const NEWEST_COLLECTOR_JJ: (u64, u64) = (0, 46);

pub(super) fn require_supported_collector(version: (u64, u64)) -> Result<()> {
    if version > NEWEST_COLLECTOR_JJ {
        return Err(CubeError::InvalidArgument(format!(
            "jj {}.{} collection is not supported yet; the newest tested collector is {}.{}",
            version.0, version.1, NEWEST_COLLECTOR_JJ.0, NEWEST_COLLECTOR_JJ.1
        )));
    }
    Ok(())
}

/// Whether `version` rewrites the operation log through the prepared, staged
/// path. The in-place abandon holds every reader out for the whole rewrite,
/// which newer jj makes too slow to bound; and only the staged path refreshes
/// the primary working copy afterwards, which newer jj requires before a new
/// workspace can be added. Every collector newer than the oldest shared client
/// (0.26) therefore stages, not just the newest tested one.
pub(super) fn uses_staged_rewrite(version: (u64, u64)) -> bool {
    version > OLDEST_SHARED_CLIENT_JJ
}

#[cfg(test)]
thread_local! {
    pub(super) static OLDEST_CLIENT_OVERRIDE: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

pub(super) fn parse_major_minor(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split(['.', '-', '+']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Pool-metadata key holding the oldest jj (`major * 1000 + minor`) known to
/// share `repo`'s store. It is a fact about the whole fleet, so it is stored
/// with the repo and set by `cube repo declare-oldest-jj`, never read from the
/// environment of whichever process happens to run the collection.
pub(super) fn oldest_client_key(repo: &str) -> String {
    format!("store_gc_oldest_jj_client:{repo}")
}

/// A declared `major.minor`: exactly two all-digit components with a minor
/// below 1000 (the encoding's radix) and a bounded major. Anything else, such
/// as `0.34-rc1` or `0.1000`, is refused rather than encoded into another
/// version.
pub(super) fn parse_declared_version(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.split_once('.')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) || minor.len() > 3 || major.len() > 6 {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?)).filter(|&(_, minor)| minor < 1000)
}

pub(super) fn encode_major_minor((major, minor): (u64, u64)) -> Option<i64> {
    major
        .checked_mul(1000)?
        .checked_add(minor)
        .and_then(|v| i64::try_from(v).ok())
}

/// The `major.minor` a stored oldest-client value encodes, if it is one.
pub(super) fn decode_major_minor(value: i64) -> Option<(u64, u64)> {
    u64::try_from(value).ok().map(|value| (value / 1000, value % 1000))
}

/// The oldest jj client declared for `repo`'s store, or
/// [`OLDEST_SHARED_CLIENT_JJ`] when none was.
pub(super) fn oldest_shared_client(database_path: Option<&Path>, repo: &str) -> Result<(u64, u64)> {
    #[cfg(test)]
    if let Some(oldest) = OLDEST_CLIENT_OVERRIDE.with(std::cell::Cell::get) {
        return Ok(oldest);
    }
    let store = match database_path {
        Some(path) => crate::store::Store::open_at(path)?,
        None => crate::store::Store::open_default()?,
    };
    Ok(store
        .get_pool_metadata_i(&oldest_client_key(repo))?
        .and_then(decode_major_minor)
        .unwrap_or(OLDEST_SHARED_CLIENT_JJ))
}

/// The collector's `jj --version` output, refused when it is newer than the
/// oldest client sharing the stores.
pub(super) fn collector_version(
    runner: &dyn CommandRunner,
    source: &Path,
    repo: &str,
    oldest: (u64, u64),
) -> Result<String> {
    let output = runner.run(&RealCommandRunner::invocation(source, "jj", &["--version"]))?;
    let version = output.trim().to_owned();
    let found = version
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(parse_major_minor);
    if let Some(found) = found {
        require_supported_collector(found)?;
    }
    match found {
        Some(found) if found <= oldest => Ok(version),
        _ => Err(CubeError::InvalidArgument(format!(
            "the `jj` on PATH (`{version}`) is not known to be at most {}.{}, the oldest client sharing \
             the stores of repo `{repo}`; collecting with a newer jj can prune history older clients still need. \
             Put the oldest client's jj first on PATH, or, once every client has been upgraded, record that with \
             `cube repo declare-oldest-jj {repo} <major.minor>`",
            oldest.0, oldest.1
        ))),
    }
}

/// A private index over the real store. The per-store GC lock owns this
/// scratch directory, including cleanup after an interrupted collector.
/// All other metadata remains shared; ignore-working-copy prevents writes
/// to the primary checkout. In particular, failed validation never deletes
/// the live index that may be keeping a damaged store readable.
pub(super) struct GcIndex {
    source: PathBuf,
    scratch: PathBuf,
}

impl GcIndex {
    pub(super) fn new(source: &Path) -> Result<Self> {
        let scratch = source.join(".jj/cube-gc-index");
        match fs::symlink_metadata(&scratch) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&scratch).map_err(CubeError::Io)?,
            Ok(_) => {
                return Err(CubeError::InvalidArgument(format!(
                    "unexpected GC scratch path: {}",
                    scratch.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CubeError::Io(error)),
        }
        let result = Self {
            source: source.to_owned(),
            scratch,
        };
        let repo = result.scratch.join(".jj/repo");
        fs::create_dir_all(repo.join("index")).map_err(CubeError::Io)?;
        fs::copy(source.join(".jj/repo/index/type"), repo.join("index/type")).map_err(CubeError::Io)?;
        for entry in fs::read_dir(source.join(".jj/repo")).map_err(CubeError::Io)? {
            let entry = entry.map_err(CubeError::Io)?;
            if entry.file_name() != "index" {
                std::os::unix::fs::symlink(entry.path(), repo.join(entry.file_name())).map_err(CubeError::Io)?;
            }
        }
        std::os::unix::fs::symlink(source.join(".jj/working_copy"), result.scratch.join(".jj/working_copy"))
            .map_err(CubeError::Io)?;
        Ok(result)
    }

    /// Collect unreferenced files from the live store. `util gc` accepts no
    /// `--at-operation`, so only its caller's head check can bound it, and it
    /// must see the operation heads that exist when it runs: the private index
    /// is a snapshot of the collection checkpoint and would treat commits a
    /// concurrent lease published after that checkpoint as garbage.
    pub(super) fn collect(&self, runner: &dyn CommandRunner, args: &[&str], timeout: Duration) -> Result<()> {
        self.run_repo(runner, &self.source, None, args, timeout)
    }

    fn run_at(&self, runner: &dyn CommandRunner, head: Option<&str>, args: &[&str], timeout: Duration) -> Result<()> {
        self.run_repo(runner, &self.scratch, head, args, timeout)
    }

    fn run_repo(
        &self,
        runner: &dyn CommandRunner,
        repo: &Path,
        head: Option<&str>,
        args: &[&str],
        timeout: Duration,
    ) -> Result<()> {
        let path = repo.to_string_lossy();
        let mut command = vec!["--ignore-working-copy", "--repository", &path];
        if let Some(head) = head {
            command.extend(["--at-operation", head]);
        }
        command.extend_from_slice(args);
        let mut invocation = RealCommandRunner::invocation(repo, "jj", &command);
        if repo == self.scratch {
            // Loading a copied repo can allocate a secure-config directory.
            // Keep both cwd discovery and configuration inside the scratch.
            for name in ["HOME", "XDG_CONFIG_HOME"] {
                invocation.env.push((name.into(), path.to_string()));
            }
        }
        invocation.env.push((JJ_GATE_HELD_ENV.to_owned(), "1".to_owned()));
        runner.run_with_timeout(&invocation, timeout)?;
        Ok(())
    }

    /// Rebuild the index at the operation `head` the caller checked, so a
    /// second head published during the rebuild cannot be merged into it.
    pub(super) fn verify(&self, runner: &dyn CommandRunner, timeout: Duration, phase: &str, head: &str) -> Result<()> {
        self.run_at(runner, Some(head), &["debug", "reindex"], timeout)
            .map_err(|error| {
                CubeError::InvalidArgument(format!(
                    "fresh jj index verification {phase} failed for {}: {error}",
                    self.source.display()
                ))
            })
    }
}

impl Drop for GcIndex {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.scratch) {
            eprintln!(
                "cube: cannot remove GC scratch index {}: {error}",
                self.scratch.display()
            );
        }
    }
}

/// Turn off git's automatic maintenance in the store behind `source`, so that
/// nothing packs refs while `util gc --expire now` runs. Idempotent, and a
/// no-op when the store has no git repository.
///
/// `util gc` spares a commit written after it started by the modification
/// time of that commit's `refs/jj/keep` file, and only a loose ref has one. jj
/// newer than 0.26 fetches through a `git` subprocess, and git 2.54 ends a
/// fetch with maintenance that packs every loose ref. A lease's fetch during
/// collection therefore stripped that protection from every commit written
/// since the collector loaded the store: `util gc` deleted their keep refs,
/// and git pruned those written in the collector's first second, which left
/// the operation log naming a commit that no longer exists.
///
/// The setting is left in place. The store's packing is this pass's own
/// `git gc`, and a pass that cannot run reports that as needing attention.
pub(super) fn disable_git_auto_maintenance(runner: &dyn CommandRunner, source: &Path) -> Result<()> {
    let Some(config) = [source.join(".git/config"), source.join(".jj/repo/store/git/config")]
        .into_iter()
        .find(|config| config.is_file())
    else {
        return Ok(());
    };
    let config = config.to_string_lossy();
    let git = |args: &[&str]| {
        let mut command = vec!["config", "--file", &config];
        command.extend_from_slice(args);
        runner.run(&RealCommandRunner::invocation(source, "git", &command))
    };
    // `--get` fails when the key is unset, which reads as "not off".
    if git(&["--get", "maintenance.auto"]).is_ok_and(|value| value.trim() == "false") {
        return Ok(());
    }
    // jj rewrites this file for every remote change; share its critical section.
    let _remotes = runner.lock_git_remotes(source)?;
    git(&["maintenance.auto", "false"]).map(drop)
}

/// Most operations the ancestry walk visits before giving up. A head built on
/// a store's pre-abandon history (tens of thousands of operations on mono)
/// ends here rather than at the root operation.
pub(super) const MAX_ANCESTRY_OPERATIONS: usize = 4096;

/// Wall-clock bound on the whole ancestry walk.
const ANCESTRY_WALK_BUDGET: Duration = Duration::from_secs(60);

/// Require every parent branch to reach the checked head, accepting benign
/// concurrent merges but refusing abandoned ancestry. Bound both work and time;
/// unknown debug output fails closed, so it cannot authorize collection.
/// Runs its `jj` reads under the shared maintenance gate, so the caller must
/// not hold it exclusively: see [`require_safe_progress_gate_held`].
pub(super) fn require_safe_progress(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
) -> Result<()> {
    require_ancestry(runner, source, ancestor, head, false)
}

/// [`require_safe_progress`] for a caller that already holds the maintenance
/// gate exclusively. The walk's `jj` reads are marked as made under that hold:
/// taking the gate shared through a second file handle would wait on the
/// caller's own exclusive lock until it timed out.
pub(super) fn require_safe_progress_gate_held(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
) -> Result<()> {
    require_ancestry(runner, source, ancestor, head, true)
}

/// Whether `head` descends from `ancestor`, under the same bounds and
/// fail-closed rules as [`require_safe_progress_gate_held`]. A walk that
/// cannot reach `ancestor` is `Ok(false)`; only a failed `jj` read is an error.
pub(super) fn descends_from_gate_held(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
) -> Result<bool> {
    Ok(walk_ancestry(runner, source, ancestor, head, true)?.is_none())
}

fn require_ancestry(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
    gate_held: bool,
) -> Result<()> {
    let Some(failure) = walk_ancestry(runner, source, ancestor, head, gate_held)? else {
        return Ok(());
    };
    Err(CubeError::InvalidArgument(format!(
        "unsafe operation heads in {}: {head} does not descend safely from {ancestor}: {failure}",
        source.display()
    )))
}

/// `None` when every branch from `head` reaches `ancestor`, else why not.
fn walk_ancestry(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
    gate_held: bool,
) -> Result<Option<String>> {
    let mut pending = vec![head.to_owned()];
    let mut seen = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + ANCESTRY_WALK_BUDGET;
    let mut failure = None;
    while let Some(current) = pending.pop() {
        if current == ancestor || !seen.insert(current.clone()) {
            continue;
        }
        if seen.len() > MAX_ANCESTRY_OPERATIONS || std::time::Instant::now() >= deadline {
            failure = Some(format!(
                "the walk exceeded {MAX_ANCESTRY_OPERATIONS} operations / {} seconds without reaching {ancestor}, \
                 so the head is presumed to descend from unverified history",
                ANCESTRY_WALK_BUDGET.as_secs()
            ));
            break;
        }
        let mut invocation = RealCommandRunner::invocation(
            source,
            "jj",
            &[
                "--ignore-working-copy",
                "--at-operation",
                &current,
                "debug",
                "operation",
                "--display",
                "operation",
                &current,
            ],
        );
        if gate_held {
            invocation.env.push((JJ_GATE_HELD_ENV.to_owned(), "1".to_owned()));
        }
        let output = runner.run_with_timeout(
            &invocation,
            deadline.saturating_duration_since(std::time::Instant::now()),
        )?;
        let parents = output
            .split_once("parents: [")
            .and_then(|(_, rest)| rest.split_once(']'));
        let ids: Vec<_> = parents
            .map(|(text, _)| text.split('"').skip(1).step_by(2).collect())
            .unwrap_or_default();
        if ids.is_empty()
            || ids
                .iter()
                .any(|id| id.len() != 128 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            failure = Some(format!(
                "the walk reached an operation with no readable parents without finding {ancestor}; \
                 abandoned or unverified ancestry was found"
            ));
            break;
        }
        pending.extend(ids.into_iter().map(str::to_owned));
    }
    Ok(failure)
}
