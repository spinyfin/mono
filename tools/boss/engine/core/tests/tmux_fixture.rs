//! Shared tmux-server lifecycle helpers for engine integration tests.
//!
//! `tmux_recovery_integration` and `tmux_engine_restart_drill_integration`
//! both need the declared host tmux binary, a kill-on-drop private server
//! guard, and a fixture-shell writer. Those used to be copy-pasted into
//! each test file; this testonly library is the single source of truth.
//!
//! Kept as a real `rust_library` (rather than compiled into each test
//! binary via `srcs`) so `pub` items that only one dependent uses are
//! not flagged as dead code.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

/// Resolves the sole host executable intentionally declared as data for
/// the consuming test target. The hermetic test sandbox canonicalizes
/// executable runfiles and only permits precisely declared executables,
/// so this keeps the production tmux binary available without widening
/// every test's PATH.
pub fn declared_tmux_binary() -> Result<PathBuf> {
    let test_srcdir = PathBuf::from(std::env::var("TEST_SRCDIR")?);
    let host_tmux_runfiles = std::fs::read_dir(&test_srcdir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("host_tmux"))
        })
        .ok_or_else(|| anyhow!("Bazel did not provide the declared host tmux runfiles"))?;
    let tmux = host_tmux_runfiles.join("tmux");
    if !tmux.is_file() {
        bail!("the declared tmux binary is unavailable at {}", tmux.display());
    }
    Ok(tmux)
}

/// Write `contents` to `<root>/<filename>` and mark the file executable.
pub fn write_fixture_shell(root: &Path, filename: &str, contents: &str) -> Result<PathBuf> {
    let shell_path = root.join(filename);
    std::fs::write(&shell_path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&shell_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&shell_path, permissions)?;
    }
    Ok(shell_path)
}

/// Kills the private tmux server the fixture started, on drop.
///
/// `prepare_server`'s `exit-empty=off` deliberately stops the server from
/// self-terminating once its last session is killed, so without this
/// guard every run of a consuming test leaks a tmux server process and
/// its unlinked socket. Uses a plain synchronous `Command` rather than
/// the async `Tmux::kill_server` helper so teardown still runs from a
/// panicking assertion's unwind, with no dependency on the tokio runtime
/// still being in a state that can drive an async call.
pub struct TmuxServerGuard {
    program: PathBuf,
    socket: PathBuf,
}

impl TmuxServerGuard {
    pub fn new(program: PathBuf, socket: PathBuf) -> Self {
        Self { program, socket }
    }
}

impl Drop for TmuxServerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.program)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}
