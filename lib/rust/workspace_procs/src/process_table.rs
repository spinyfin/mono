//! Live process identity as the operating system reports it right now.
//!
//! One [`LiveProcess`] per pid carries everything the ownership scan in
//! [`crate::scan`] needs: the parent (for ancestry), the start time (so a
//! recycled pid is never mistaken for the process that was recorded), the
//! working directory (the ownership signal that survives re-parenting to
//! launchd), the argument vector (to recognise a Bazel server).
//!
//! The [`WorkspaceProcessTable`] trait is the seam between the scan and the OS:
//! production uses [`SystemWorkspaceProcessTable`]; consumers' unit tests use
//! a fabricated tree to record the signals the reap would have sent.

use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One live process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: i32,
    /// Parent pid; `1` (launchd / init) once the original parent has exited.
    pub ppid: i32,
    /// Process start time in microseconds since the Unix epoch. Together
    /// with the pid it identifies one process incarnation.
    pub start_micros: u64,
    /// Short process name (`zsh`, `java`, `engine_lib_test`).
    pub command: String,
    /// Full argument vector when readable; empty when the OS refused it
    /// (another user's process, a zombie).
    pub argv: Vec<String>,
    /// Current working directory when readable.
    pub cwd: Option<PathBuf>,
}

impl LiveProcess {
    /// The argument vector joined for a log line, bounded so a long `-c`
    /// script does not flood the log.
    pub fn argv_summary(&self) -> String {
        const MAX: usize = 240;
        let joined = if self.argv.is_empty() {
            self.command.clone()
        } else {
            self.argv.join(" ")
        };
        if joined.chars().count() <= MAX {
            joined
        } else {
            let cut: String = joined.chars().take(MAX).collect();
            format!("{cut}…")
        }
    }
}

/// Which signal a reap pass sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }

    fn raw(self) -> i32 {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

/// Source of live process state. A seam so the scan and the reap ladder are
/// testable against a fabricated process tree without signalling anything.
pub trait WorkspaceProcessTable: Send + Sync {
    /// Every process the caller may inspect.
    fn list(&self) -> io::Result<Vec<LiveProcess>>;

    /// One process, fresh. `Err` with `NotFound` once it has exited.
    fn snapshot(&self, pid: i32) -> io::Result<LiveProcess>;

    /// Deliver `signal` to exactly `pid` (never a group).
    fn signal(&self, pid: i32, signal: Signal) -> io::Result<()>;

    /// The caller's own pid, so the scan can protect it and its ancestors.
    fn own_pid(&self) -> i32;

    /// Current time in microseconds since the Unix epoch, on the same clock
    /// as [`LiveProcess::start_micros`].
    fn now_micros(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }
}

/// The real process table.
#[derive(Default)]
pub struct SystemWorkspaceProcessTable;

impl WorkspaceProcessTable for SystemWorkspaceProcessTable {
    fn list(&self) -> io::Result<Vec<LiveProcess>> {
        sys::list()
    }

    fn snapshot(&self, pid: i32) -> io::Result<LiveProcess> {
        sys::snapshot(pid)
    }

    fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
        if pid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to signal pid {pid}"),
            ));
        }
        // SAFETY: `kill` with a positive pid delivers to that one process;
        // the result is inspected and never assumed.
        let rc = unsafe { libc::kill(pid, signal.raw()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn own_pid(&self) -> i32 {
        std::process::id() as i32
    }
}

/// A missing cwd is not proof of process exit: confirm the identity reader
/// also reports exit before allowing enumeration to discard the process.
fn inspected_cwd(cwd: io::Result<PathBuf>, identity: impl FnOnce() -> io::Result<()>) -> io::Result<Option<PathBuf>> {
    match cwd {
        Ok(path) => Ok(Some(path)),
        Err(error) => {
            identity()?;
            Err(io::Error::other(format!("working directory unreadable: {error}")))
        }
    }
}

/// Ignore only processes that exited during enumeration; every other failure
/// makes verification inconclusive, including an unreadable working directory.
fn collect_snapshots(
    pids: impl IntoIterator<Item = i32>,
    mut snapshot: impl FnMut(i32) -> io::Result<LiveProcess>,
) -> io::Result<Vec<LiveProcess>> {
    let mut processes = Vec::new();
    for pid in pids {
        match snapshot(pid) {
            Ok(process) => processes.push(process),
            Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot inspect pid {pid}: {error}"),
                ));
            }
        }
    }
    Ok(processes)
}

#[cfg(target_os = "macos")]
mod sys {
    use super::LiveProcess;
    use std::io;
    pub(super) fn list() -> io::Result<Vec<LiveProcess>> {
        // SAFETY: geteuid has no preconditions.
        super::collect_snapshots(crate::darwin::list_pids_for_uid(unsafe { libc::geteuid() })?, snapshot)
    }
    pub(super) fn snapshot(pid: i32) -> io::Result<LiveProcess> {
        let info = crate::darwin::proc_bsd_info(pid)?;
        Ok(LiveProcess {
            pid,
            ppid: info.ppid,
            start_micros: info.start_micros,
            command: info.command,
            argv: crate::process_args::arguments(pid as u32).unwrap_or_default(),
            cwd: super::inspected_cwd(crate::darwin::proc_cwd(pid), || {
                crate::darwin::proc_bsd_info(pid).map(|_| ())
            })?,
        })
    }
}

#[cfg(target_os = "linux")]
mod sys {
    //! Linux: `/proc/<pid>/stat`, `/proc/<pid>/cwd` and `/proc/<pid>/cmdline`.

    use std::io;

    use super::{LinuxIdentity, LiveProcess};

    pub(super) fn list() -> io::Result<Vec<LiveProcess>> {
        use std::os::unix::fs::MetadataExt;
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            // Like Darwin, inspect only processes owned by this user.
            // SAFETY: geteuid has no preconditions.
            if metadata.uid() == unsafe { libc::geteuid() } {
                pids.push(pid);
            }
        }
        super::collect_snapshots(pids, snapshot)
    }

    pub(super) fn snapshot(pid: i32) -> io::Result<LiveProcess> {
        let LinuxIdentity {
            ppid,
            start_ticks,
            command,
        } = linux_identity(pid)?;
        let argv = crate::process_args::arguments(pid as u32).unwrap_or_default();
        Ok(LiveProcess {
            pid,
            ppid,
            start_micros: boot_micros().saturating_add(start_ticks.saturating_mul(1_000_000) / clock_ticks()),
            command,
            argv,
            cwd: super::inspected_cwd(std::fs::read_link(format!("/proc/{pid}/cwd")), || {
                linux_identity(pid).map(|_| ())
            })?,
        })
    }

    pub fn linux_identity(pid: i32) -> io::Result<LinuxIdentity> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("/proc/{pid}/stat: {what}"));
        let (head, tail) = stat
            .rsplit_once(')')
            .ok_or_else(|| invalid("missing comm terminator"))?;
        let command = head
            .split_once('(')
            .map(|(_, name)| name)
            .unwrap_or_default()
            .to_owned();
        // After the comm: state, ppid, pgrp, session, tty_nr, ... starttime
        // is field 22 overall, i.e. index 19 after the comm.
        let fields: Vec<&str> = tail.split_whitespace().collect();
        let field = |i: usize| fields.get(i).copied().ok_or_else(|| invalid("too few fields"));
        // Zombies have exited but retain a stat entry until their parent
        // collects them; their missing cwd is not an inspection failure.
        if matches!(field(0)?, "Z" | "X" | "x") {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("pid {pid} has exited")));
        }
        let ppid = field(1)?.parse::<i32>().map_err(|_| invalid("ppid"))?;
        let start_ticks = field(19)?.parse::<u64>().map_err(|_| invalid("starttime"))?;
        Ok(LinuxIdentity {
            ppid,
            start_ticks,
            command,
        })
    }

    fn clock_ticks() -> u64 {
        // SAFETY: `sysconf` reads a constant.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks > 0 { ticks as u64 } else { 100 }
    }

    fn boot_micros() -> u64 {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|stat| {
                stat.lines()
                    .find_map(|line| line.strip_prefix("btime "))
                    .and_then(|v| v.trim().parse::<u64>().ok())
            })
            .map(|secs| secs.saturating_mul(1_000_000))
            .unwrap_or(0)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod sys {
    use std::io;

    use super::LiveProcess;

    pub(super) fn list() -> io::Result<Vec<LiveProcess>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process table unsupported on this platform",
        ))
    }

    pub(super) fn snapshot(_pid: i32) -> io::Result<LiveProcess> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process table unsupported on this platform",
        ))
    }
}

/// Linux stat identity, with start time in kernel clock ticks.
#[cfg(target_os = "linux")]
pub struct LinuxIdentity {
    pub ppid: i32,
    pub start_ticks: u64,
    pub command: String,
}
#[cfg(target_os = "linux")]
pub use sys::linux_identity;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{StaticProcessTable, fake_process};

    #[test]
    fn missing_cwd_requires_confirmed_process_exit() {
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert_ne!(
            inspected_cwd(missing(), || Ok(())).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            inspected_cwd(missing(), || Err(io::Error::from(io::ErrorKind::NotFound)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn reader_only_discards_exited_processes() {
        let processes = collect_snapshots([10, 11], |pid| {
            if pid == 10 {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                Ok(fake_process(pid, 1, "tool", Some("/pool/test")))
            }
        })
        .unwrap();
        assert_eq!(processes.len(), 1);
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidData] {
            let error =
                collect_snapshots([10], |_| Err(io::Error::new(kind, "snapshot or cwd unreadable"))).unwrap_err();
            assert!(error.to_string().contains("pid 10"));
        }
    }

    #[test]
    fn reader_failure_after_term_preserves_signal_audit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Reader {
            inner: StaticProcessTable,
            reads: AtomicUsize,
        }
        impl WorkspaceProcessTable for Reader {
            fn list(&self) -> io::Result<Vec<LiveProcess>> {
                let read = self.reads.fetch_add(1, Ordering::SeqCst);
                collect_snapshots([10], |pid| {
                    if read == 0 {
                        self.inner.snapshot(pid)
                    } else {
                        Err(io::Error::new(io::ErrorKind::PermissionDenied, "cwd unreadable"))
                    }
                })
            }
            fn snapshot(&self, pid: i32) -> io::Result<LiveProcess> {
                self.inner.snapshot(pid)
            }
            fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
                self.inner.signal(pid, signal)
            }
            fn own_pid(&self) -> i32 {
                900
            }
        }
        let table = Reader {
            inner: StaticProcessTable::new(vec![fake_process(10, 1, "tool", Some("/pool/test"))], 900),
            reads: AtomicUsize::new(0),
        };
        let report = crate::reap(
            &table,
            &crate::Scope::for_workspace("/pool/test"),
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert_eq!(report.killed_pids(), vec![10]);
        assert!(!report.is_clean());
        assert!(report.error.unwrap().contains("cwd unreadable"));
    }

    #[test]
    fn self_snapshot_reports_identity_and_working_directory() {
        let table = SystemWorkspaceProcessTable;
        let me = table.snapshot(table.own_pid()).unwrap();
        assert_eq!(me.pid, std::process::id() as i32);
        assert!(me.ppid > 0);
        assert!(me.start_micros > 0);
        assert!(!me.command.is_empty());
        assert!(!me.argv.is_empty(), "own argv must be readable");
        let expected = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let reported = std::fs::canonicalize(me.cwd.expect("own cwd must be readable")).unwrap();
        assert_eq!(reported, expected);
    }

    #[test]
    fn self_start_time_is_in_the_past_on_the_table_clock() {
        let table = SystemWorkspaceProcessTable;
        let me = table.snapshot(table.own_pid()).unwrap();
        let now = table.now_micros();
        assert!(me.start_micros <= now, "start {} after now {now}", me.start_micros);
        // Started within the last day: the clocks agree to within reason.
        assert!(now - me.start_micros < 24 * 3600 * 1_000_000);
    }

    #[test]
    fn list_includes_self() {
        let table = SystemWorkspaceProcessTable;
        let all = table.list().unwrap();
        assert!(all.iter().any(|p| p.pid == table.own_pid()));
    }

    #[test]
    fn missing_pid_is_not_found() {
        // Far past any plausible pid on either platform.
        let err = SystemWorkspaceProcessTable.snapshot(99_999_999 - 1).unwrap_err();
        assert!(
            matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput),
            "{err:?}"
        );
    }

    #[test]
    fn argv_summary_is_bounded() {
        let mut p = fake_process(7, 1, "zsh", None);
        p.argv = vec!["zsh".into(), "-c".into(), "x".repeat(1000)];
        let summary = p.argv_summary();
        assert!(summary.chars().count() <= 241);
        assert!(summary.ends_with('…'));
        p.argv.clear();
        assert_eq!(p.argv_summary(), "zsh");
    }

    #[test]
    fn static_table_kill_removes_and_term_respects_ignores() {
        let table = StaticProcessTable::new(vec![fake_process(10, 1, "a", None), fake_process(11, 1, "b", None)], 1);
        table.ignore_term(11);
        table.signal(10, Signal::Term).unwrap();
        table.signal(11, Signal::Term).unwrap();
        assert!(!table.is_alive(10));
        assert!(table.is_alive(11));
        table.signal(11, Signal::Kill).unwrap();
        assert!(!table.is_alive(11));
        assert_eq!(
            table.signals(),
            vec![(10, Signal::Term), (11, Signal::Term), (11, Signal::Kill)]
        );
        assert!(table.signal(10, Signal::Kill).is_err());
    }

    #[test]
    fn static_table_immortal_survives_every_signal() {
        let table = StaticProcessTable::new(vec![fake_process(12, 1, "c", None)], 1);
        table.immortal(12);
        table.signal(12, Signal::Term).unwrap();
        table.signal(12, Signal::Kill).unwrap();
        assert!(table.is_alive(12));
        assert_eq!(table.signals(), vec![(12, Signal::Term), (12, Signal::Kill)]);
    }
}
