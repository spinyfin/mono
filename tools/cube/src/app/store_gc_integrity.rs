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
//! Collection also never touches the live `.jj/repo/index`. That is
//! deliberate and costs nothing: neither jj 0.26 nor 0.32's `util gc` prunes the
//! default index store (tested against repos with abandoned operations:
//! all index file paths and contents, including operation links and segments,
//! are identical before and after;
//! see `store_gc_default_index_retention_for_both_client_versions`),
//! so running it against the private scratch index leaves the live index
//! exactly as growth-prone as it already was. A rewrite itself orphans the
//! old operation links; reclaiming them is a `jj debug reindex` of the live
//! store, which is a manual step, not part of this pass.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::errors::{CubeError, Result};
use crate::command_runner::{CommandRunner, JJ_GATE_HELD_ENV, RealCommandRunner};

/// The oldest jj (major, minor) that shares the stores cube maintains. The
/// collector must not be newer: see the module doc.
pub(super) const OLDEST_SHARED_CLIENT_JJ: (u64, u64) = (0, 26);

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

pub(super) fn encode_major_minor((major, minor): (u64, u64)) -> i64 {
    (major * 1000 + minor) as i64
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
        .and_then(|value| u64::try_from(value).ok())
        .map_or(OLDEST_SHARED_CLIENT_JJ, |value| (value / 1000, value % 1000)))
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

    /// Run `args` against the private index. `util gc` accepts no
    /// `--at-operation`, so only its caller's head check can bound it.
    pub(super) fn run(&self, runner: &dyn CommandRunner, args: &[&str], timeout: Duration) -> Result<()> {
        self.run_at(runner, None, args, timeout)
    }

    fn run_at(&self, runner: &dyn CommandRunner, head: Option<&str>, args: &[&str], timeout: Duration) -> Result<()> {
        let path = self.scratch.to_string_lossy();
        let mut command = vec!["--ignore-working-copy", "--repository", &path];
        if let Some(head) = head {
            command.extend(["--at-operation", head]);
        }
        command.extend_from_slice(args);
        let mut invocation = RealCommandRunner::invocation(&self.source, "jj", &command);
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

/// Most operations the ancestry walk visits before giving up. A head built on
/// a store's pre-abandon history (tens of thousands of operations on mono)
/// ends here rather than at the root operation.
pub(super) const MAX_ANCESTRY_OPERATIONS: usize = 4096;

/// Wall-clock bound on the whole ancestry walk.
const ANCESTRY_WALK_BUDGET: Duration = Duration::from_secs(60);

/// Require every parent branch to reach the checked head, accepting benign
/// concurrent merges but refusing abandoned ancestry. Bound both work and time;
/// unknown debug output fails closed, so it cannot authorize collection.
pub(super) fn require_safe_progress(
    runner: &dyn CommandRunner,
    source: &Path,
    ancestor: &str,
    head: &str,
) -> Result<()> {
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
        let output = runner.run_with_timeout(
            &RealCommandRunner::invocation(
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
            ),
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
    let Some(failure) = failure else {
        return Ok(());
    };
    Err(CubeError::InvalidArgument(format!(
        "unsafe operation heads in {}: {head} does not descend safely from {ancestor}: {failure}",
        source.display()
    )))
}
