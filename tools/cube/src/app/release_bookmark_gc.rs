//! Release-time sweeps run in one detached process per repo. A dirty bit
//! coalesces releases while a pass is running; the short scheduling lock
//! closes both the spawn and final-unlock races without covering any jj work.

use std::fs::File;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::app::errors::{CubeError, Result, RunResult};
use crate::command_runner::CommandRunner;
use crate::lock::RepoLock;
use crate::{audit, store::Store};

struct Scheduler {
    state: PathBuf,
    running: PathBuf,
    dirty: PathBuf,
}

impl Scheduler {
    fn new(database: Option<&Path>, repo: &str) -> Result<Self> {
        let dir = match database.and_then(Path::parent) {
            Some(dir) => dir.to_path_buf(),
            None => crate::paths::data_dir()?,
        }
        .join("locks");
        Ok(Self {
            state: dir.join(format!("{repo}.release-bookmark-gc-state.lock")),
            running: dir.join(format!("{repo}.release-bookmark-gc.lock")),
            dirty: dir.join(format!("{repo}.release-bookmark-gc-dirty")),
        })
    }

    fn trigger(&self, spawn: impl FnOnce(File) -> Result<()>) -> Result<()> {
        let _state = RepoLock::acquire(&self.state)?;
        std::fs::write(&self.dirty, []).map_err(CubeError::Io)?;
        if let Some(running) = RepoLock::try_acquire(&self.running)? {
            // Inherit the held lock: another release must not spawn a child
            // during the interval before this child begins executing.
            spawn(running.into_file())?;
        }
        Ok(())
    }

    fn drain(&self, running: File, mut sweep: impl FnMut()) -> Result<()> {
        loop {
            let state = RepoLock::acquire(&self.state)?;
            match std::fs::remove_file(&self.dirty) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // Unlock under the scheduling lock so a racing release
                    // either dirties this worker or starts the next one.
                    FileExt::unlock(&running).map_err(CubeError::Io)?;
                    return Ok(());
                }
                Err(err) => return Err(CubeError::Io(err)),
            }
            drop(state);
            sweep();
        }
    }
}

fn failed(database: Option<&Path>, repo: &str, error: &CubeError) {
    eprintln!("cube: error: background release bookmark gc for {repo} failed: {error}");
    audit!(
        database,
        "workspace.release_bookmark_gc_failed",
        repo = repo,
        error = error.to_string()
    );
}

pub(super) fn trigger(database: Option<&Path>, repo: &str) {
    let result = Scheduler::new(database, repo)
        .and_then(|scheduler| scheduler.trigger(|lock| spawn(database, repo, lock).map_err(CubeError::Io)));
    if let Err(error) = result {
        failed(database, repo, &error);
    }
}

#[cfg(not(test))]
fn spawn(database: Option<&Path>, repo: &str, lock: File) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("release-bookmark-gc").arg(repo);
    if let Some(database) = database {
        command.arg("--database").arg(database);
    }
    let dir = match database.and_then(Path::parent) {
        Some(dir) => dir.to_path_buf(),
        None => crate::paths::data_dir().map_err(std::io::Error::other)?,
    };
    let mut child = super::periodic_cleanup::spawn_command_with_input(
        &mut command,
        &dir.join("release-bookmark-gc.log"),
        lock.into(),
    )?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static SPAWN_REQUESTS: std::cell::RefCell<Vec<(String, File)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn spawn(_database: Option<&Path>, repo: &str, lock: File) -> std::io::Result<()> {
    SPAWN_REQUESTS.with(|requests| requests.borrow_mut().push((repo.to_owned(), lock)));
    Ok(())
}

pub(super) fn sweep(runner: &dyn CommandRunner, database: Option<&Path>, repo: &str, source: &Path) {
    if let Err(error) = super::gc::gc_workspace_bookmarks(runner, database, source, false, false) {
        failed(database, repo, &error);
    }
}

pub(super) fn run(database: Option<&Path>, repo: &str, runner: &dyn CommandRunner) -> Result<RunResult> {
    let result = (|| {
        let store = match database {
            Some(path) => Store::open_at(path)?,
            None => Store::open_default()?,
        };
        // The released workspace may already be leased again or removed.
        // Sweep from the stable primary checkout of its shared object store.
        let source = store
            .get_repo(repo)?
            .and_then(|record| record.source)
            .ok_or_else(|| CubeError::InvalidArgument(format!("repo `{repo}` has no source checkout")))?;
        let running = std::io::stdin().as_fd().try_clone_to_owned().map_err(CubeError::Io)?;
        Scheduler::new(database, repo)?.drain(running.into(), || sweep(runner, database, repo, &source))?;
        RunResult::new("Release bookmark gc finished.", serde_json::json!({}))
    })();
    if let Err(error) = &result {
        failed(database, repo, error);
    }
    result
}

#[cfg(test)]
#[path = "release_bookmark_gc_tests.rs"]
mod tests;
