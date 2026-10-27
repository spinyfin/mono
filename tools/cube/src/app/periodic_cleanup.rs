//! Repo commands can run for minutes, so periodic cleanup owns a separate
//! process and process group rather than extending the lease request.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::app::cleanup::{CleanupOptions, CleanupTrigger, run_cleanup_pass};
use crate::app::errors::{Result, RunResult};
use crate::app::gc::POOL_GC_CLEANUP_PROGRESS_KEY;
use crate::command_runner::RealCommandRunner;
use crate::store::Store;

pub(super) fn spawn(database: Option<&Path>) -> std::io::Result<()> {
    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = std::path::PathBuf::from(std::env::var_os("CUBE_TEST_BIN").expect("Bazel cube data dependency"));
    let mut command = Command::new(executable);
    command.arg("periodic-cleanup");
    if let Some(database) = database {
        command.arg("--database").arg(database);
    }
    let mut child = spawn_command(&mut command)?;
    // Reap when embedded in a long-lived caller. A short-lived CLI may exit;
    // the OS then adopts the independent child, which still finishes its pass.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub(super) fn spawn_command(command: &mut Command) -> std::io::Result<Child> {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
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
    RunResult::new(report.summary(), serde_json::json!({}))
}
