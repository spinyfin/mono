//! Repo commands can run for minutes, so periodic cleanup owns a separate
//! process and process group rather than extending the lease request.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::app::cleanup::{CleanupOptions, CleanupTrigger, run_cleanup_pass};
use crate::app::errors::{Result, RunResult};
use crate::app::gc::POOL_GC_CLEANUP_PROGRESS_KEY;
use crate::command_runner::RealCommandRunner;
use crate::store::Store;

#[cfg(not(test))]
pub(super) fn spawn(database: Option<&Path>) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.arg("periodic-cleanup");
    if let Some(database) = database {
        command.arg("--database").arg(database);
    }
    let state_dir = match database.and_then(Path::parent) {
        Some(path) => path.to_path_buf(),
        None => crate::paths::data_dir().map_err(std::io::Error::other)?,
    };
    let mut child = spawn_command(&mut command, &state_dir.join("periodic-cleanup.log"))?;
    // Reap when embedded in a long-lived caller. A short-lived CLI may exit;
    // the OS then adopts the independent child, which still finishes its pass.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub(super) fn spawn_command(command: &mut Command, log: &Path) -> std::io::Result<Child> {
    spawn_command_with_input(command, log, Stdio::null())
}

pub(super) fn spawn_command_with_input(command: &mut Command, log: &Path, input: Stdio) -> std::io::Result<Child> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    command.stdin(input).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()
}

pub(super) fn run(database: Option<&Path>) -> Result<RunResult> {
    let store = match database {
        Some(path) => Store::open_at(path)?,
        None => Store::open_default()?,
    };
    let report = run_cleanup_pass(
        &RealCommandRunner,
        &store,
        database,
        &CleanupOptions {
            repo: None,
            workspace: None,
            dry_run: false,
            trigger: CleanupTrigger::Periodic,
            deadline: None,
        },
    );
    store.set_pool_metadata_i(POOL_GC_CLEANUP_PROGRESS_KEY, i64::from(report.made_progress()))?;
    eprintln!("cube: auto gc: workspace cleanup: {}", report.summary());
    RunResult::new(report.summary(), serde_json::json!({}))
}

#[cfg(test)]
thread_local! {
    pub(super) static SPAWN_REQUESTS: std::cell::RefCell<Vec<Option<std::path::PathBuf>>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(super) fn spawn(database: Option<&Path>) -> std::io::Result<()> {
    SPAWN_REQUESTS.with(|requests| requests.borrow_mut().push(database.map(Path::to_path_buf)));
    Ok(())
}
