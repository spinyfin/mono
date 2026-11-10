//! Collect from a freshly reconstructed index, never an inherited cache.
//!
//! jj 0.26 accepts indexes written by newer jj releases that omit legacy
//! predecessors. GC trusts that index, whereas a 0.26 rebuild walks those
//! predecessors. An ordinary `jj log` therefore cannot prove collection safe.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::errors::{CubeError, Result};
use crate::command_runner::{CommandRunner, JJ_GATE_HELD_ENV, RealCommandRunner};

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

    pub(super) fn run(&self, runner: &dyn CommandRunner, args: &[&str], timeout: Duration) -> Result<()> {
        let path = self.scratch.to_string_lossy();
        let mut command = vec!["--ignore-working-copy", "--repository", &path];
        command.extend_from_slice(args);
        let mut invocation = RealCommandRunner::invocation(&self.source, "jj", &command);
        invocation.env.push((JJ_GATE_HELD_ENV.to_owned(), "1".to_owned()));
        runner.run_with_timeout(&invocation, timeout)?;
        Ok(())
    }

    pub(super) fn verify(&self, runner: &dyn CommandRunner, timeout: Duration, phase: &str) -> Result<()> {
        self.run(runner, &["debug", "reindex"], timeout).map_err(|error| {
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
