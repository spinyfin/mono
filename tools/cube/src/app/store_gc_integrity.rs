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
//! Collection also never touches the live `.jj/repo/index`. That is
//! deliberate and costs nothing: jj 0.26's `util gc` does not prune the
//! default index store (verified against a repo with abandoned operations:
//! the `index/operations` links and segments are identical before and after),
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

/// Environment override for [`OLDEST_SHARED_CLIENT_JJ`], as `major.minor`,
/// set once every client sharing the stores has been upgraded.
pub(super) const OLDEST_CLIENT_ENV: &str = "CUBE_OLDEST_JJ_CLIENT";

#[cfg(test)]
thread_local! {
    pub(super) static OLDEST_CLIENT_OVERRIDE: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}

fn parse_major_minor(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split(['.', '-', '+']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn oldest_shared_client() -> (u64, u64) {
    #[cfg(test)]
    if let Some(oldest) = OLDEST_CLIENT_OVERRIDE.with(std::cell::Cell::get) {
        return oldest;
    }
    std::env::var(OLDEST_CLIENT_ENV)
        .ok()
        .and_then(|value| parse_major_minor(value.trim()))
        .unwrap_or(OLDEST_SHARED_CLIENT_JJ)
}

/// The collector's `jj --version` output, refused when it is newer than the
/// oldest client sharing the stores.
pub(super) fn collector_version(runner: &dyn CommandRunner, source: &Path) -> Result<String> {
    let output = runner.run(&RealCommandRunner::invocation(source, "jj", &["--version"]))?;
    let version = output.trim().to_owned();
    let oldest = oldest_shared_client();
    let found = version
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(parse_major_minor);
    match found {
        Some(found) if found <= oldest => Ok(version),
        _ => Err(CubeError::InvalidArgument(format!(
            "the `jj` on PATH (`{version}`) is not known to be at most {}.{}, the oldest client sharing \
             these stores; collecting with a newer jj can prune history older clients still need. \
             Put the oldest client's jj first on PATH, or set {OLDEST_CLIENT_ENV} once every client \
             has been upgraded",
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
