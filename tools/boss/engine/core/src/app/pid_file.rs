//! Engine process identity: the pid file plus the exclusive instance lock.
//!
//! The pid file is how the macOS app and the CLI decide whether an engine is
//! already running. Historically it was written only after `WorkDb::open` and
//! the frontend-socket bind, so a slow database open (WAL recovery after
//! SIGTERM, schema init) left the process invisible. The app supervisor then
//! treated "no pid file" as "engine exited" and launched a second copy that
//! lost the SQLite busy-timeout and died with `error:database is locked`.
//!
//! [`PidFileGuard::acquire`] takes a non-blocking exclusive `flock` on the
//! pid file and writes this process's pid while holding it. The flock is
//! released when the guard (and its fd) is dropped. A second engine that
//! reaches this path while the first is still opening the database fails
//! immediately with [`AcquireError::AlreadyHeld`] instead of racing SQLite.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// RAII guard that holds the instance lock and removes the pid file on drop
/// when the file still names this process.
#[derive(Debug)]
pub(super) struct PidFileGuard {
    pub(super) path: String,
    pub(super) pid: u32,
    /// Kept open so the exclusive flock lives for the process lifetime.
    _file: File,
}

/// Why [`PidFileGuard::acquire`] refused to take the pid file.
#[derive(Debug)]
pub(super) enum AcquireError {
    /// Another live engine already holds the exclusive flock.
    AlreadyHeld {
        path: PathBuf,
        holder_pid: Option<u32>,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld {
                path,
                holder_pid: Some(pid),
            } => {
                write!(f, "instance lock held by pid {pid} ({})", path.display())
            }
            Self::AlreadyHeld { path, holder_pid: None } => {
                write!(f, "instance lock held ({})", path.display())
            }
            Self::Io { path, source } => {
                write!(f, "failed to acquire instance lock {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyHeld { .. } => None,
        }
    }
}

impl PidFileGuard {
    /// Open `path`, take a non-blocking exclusive flock, and write this
    /// process's pid. Fails immediately if another process already holds
    /// the lock — no wait, no retry.
    pub(super) fn acquire(path: &Path) -> Result<Self, AcquireError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| AcquireError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }

        // Do not truncate on open: a loser of the flock must still be able
        // to read the winner's pid, and truncating before `flock` would
        // wipe it. The winner truncates explicitly after it holds the lock.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| AcquireError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        // SAFETY: `file` is a valid open fd; `flock` does not take ownership.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let holder_pid = read_holder_pid(path);
            return Err(AcquireError::AlreadyHeld {
                path: path.to_path_buf(),
                holder_pid,
            });
        }

        let pid = std::process::id();
        file.set_len(0).map_err(|source| AcquireError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        file.seek(SeekFrom::Start(0)).map_err(|source| AcquireError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        file.write_all(format!("{pid}\n").as_bytes())
            .map_err(|source| AcquireError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        file.sync_all().map_err(|source| AcquireError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            pid,
            _file: file,
        })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return,
        };
        let parsed = content.trim().parse::<u32>().ok();
        if parsed == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_holder_pid(path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    content.trim().parse().ok().filter(|&pid| pid > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn acquire_writes_this_process_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.pid");
        let guard = PidFileGuard::acquire(&path).expect("acquire");
        let written: u32 = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
        assert_eq!(written, std::process::id());
        assert_eq!(guard.pid, std::process::id());
        drop(guard);
        assert!(!path.exists(), "drop must unlink the pid file we still own");
    }

    #[test]
    fn drop_leaves_file_if_contents_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.pid");
        let guard = PidFileGuard::acquire(&path).expect("acquire");
        // Simulate a replacement engine that reclaimed the path after we
        // crashed without Drop — we must not delete its pid.
        std::fs::write(&path, "99999\n").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "99999");
    }

    #[test]
    fn acquire_reclaims_a_stale_unlocked_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.pid");
        std::fs::write(&path, "1\n").unwrap();
        let guard = PidFileGuard::acquire(&path).expect("stale pid file must be reclaimable");
        let written: u32 = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
        assert_eq!(written, std::process::id());
        drop(guard);
    }

    /// Same-process second acquire. On kernels that associate flock with the
    /// fd (macOS) this returns `AlreadyHeld`. On kernels that associate it
    /// with the process (Linux) the second call succeeds — cross-process
    /// exclusion still holds, and [`second_process_loses_held_lock`] covers
    /// that case.
    #[test]
    fn same_process_second_acquire_is_already_held_or_is_the_same_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.pid");
        let first = PidFileGuard::acquire(&path).expect("first acquire");
        match PidFileGuard::acquire(&path) {
            Err(AcquireError::AlreadyHeld { holder_pid, .. }) => {
                assert_eq!(holder_pid, Some(std::process::id()));
            }
            Ok(second) => {
                assert_eq!(second.pid, first.pid);
                drop(second);
            }
            Err(other) => panic!("unexpected acquire error: {other}"),
        }
        drop(first);
    }

    /// A foreign process holding the flock must make `acquire` fail
    /// immediately with `AlreadyHeld`, never wait on SQLite. Uses a
    /// short Python (or Perl) holder so the test is two real processes
    /// rather than two fds in this one.
    #[test]
    fn second_process_loses_held_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.pid");
        let ready = dir.path().join("ready");
        let Some(mut child) = spawn_lock_holder(&path, &ready) else {
            eprintln!("skipping: no python3/perl available to hold a foreign flock");
            return;
        };
        let err = PidFileGuard::acquire(&path).expect_err("foreign holder must win the flock");
        match err {
            AcquireError::AlreadyHeld { holder_pid, .. } => {
                assert!(
                    holder_pid.is_none() || holder_pid != Some(std::process::id()),
                    "holder pid must not be this test process, got {holder_pid:?}"
                );
            }
            other => panic!("expected AlreadyHeld, got {other}"),
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn spawn_lock_holder(path: &Path, ready: &Path) -> Option<std::process::Child> {
        let path_str = path.to_string_lossy().into_owned();
        let ready_str = ready.to_string_lossy().into_owned();
        for (program, args) in lock_holder_commands(&path_str, &ready_str) {
            let mut cmd = Command::new(&program);
            cmd.args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let Ok(mut child) = cmd.spawn() else {
                continue;
            };
            if wait_for_ready_file(ready, Duration::from_secs(3)) {
                return Some(child);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }

    fn lock_holder_commands(path: &str, ready: &str) -> Vec<(String, Vec<String>)> {
        let py = "import fcntl, os, sys, time\n\
                  p, r = sys.argv[1], sys.argv[2]\n\
                  f = open(p, 'a+')\n\
                  fcntl.flock(f, fcntl.LOCK_EX)\n\
                  f.seek(0); f.truncate(); f.write('%d\\n' % os.getpid()); f.flush()\n\
                  open(r, 'w').write('1')\n\
                  time.sleep(60)\n";
        let pl = "use Fcntl qw(:flock SEEK_SET);\n\
                  my ($p, $r) = @ARGV;\n\
                  open(my $fh, '+>>', $p) or die;\n\
                  flock($fh, LOCK_EX) or die;\n\
                  seek($fh, 0, SEEK_SET); truncate($fh, 0);\n\
                  print $fh \"$$\\n\"; $fh->flush;\n\
                  open(my $ready, '>', $r) or die; print $ready \"1\\n\"; close $ready;\n\
                  sleep 60;\n";
        vec![
            (
                "python3".into(),
                vec!["-c".into(), py.to_owned(), path.to_owned(), ready.to_owned()],
            ),
            (
                "perl".into(),
                vec![
                    "-e".into(),
                    pl.to_owned(),
                    "--".into(),
                    path.to_owned(),
                    ready.to_owned(),
                ],
            ),
        ]
    }

    fn wait_for_ready_file(ready: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ready.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}
