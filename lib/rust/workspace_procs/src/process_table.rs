//! Live process identity as the operating system reports it right now.
//!
//! One [`LiveProcess`] per pid carries everything the ownership scan in
//! [`crate::scan`] needs: the parent (for ancestry), the start time (so a
//! recycled pid is never mistaken for the process that was recorded), the
//! working directory (the ownership signal that survives re-parenting to
//! launchd), the argument vector (to recognise a Bazel server) and whether
//! the process has a controlling terminal (to tell an interactive human
//! shell from a detached tool subprocess).
//!
//! The [`ProcessTable`] trait is the seam between the scan and the OS:
//! production uses [`SystemProcessTable`]; consumers' unit tests use
//! [`StaticProcessTable`] to fabricate a tree and record the signals the
//! reap would have sent.

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One live process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: i32,
    /// Parent pid; `1` (launchd / init) once the original parent has exited.
    pub ppid: i32,
    pub pgid: i32,
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
    /// Whether the process has a controlling terminal. A worker's tool
    /// shells are spawned detached and have none; a human's interactive
    /// shell does.
    pub has_controlling_terminal: bool,
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
pub trait ProcessTable: Send + Sync {
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
pub struct SystemProcessTable;

impl ProcessTable for SystemProcessTable {
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

/// An in-memory process table for tests: a fixed set of processes, plus a
/// log of every signal sent. A `Kill` removes the process from the table so
/// a following scan sees it gone; a `Term` does too unless the pid is in
/// `ignores_term`, which models a process that shrugs off SIGTERM. An
/// `immortal` pid survives both, modelling one the caller cannot kill.
pub struct StaticProcessTable {
    inner: Mutex<StaticInner>,
    own_pid: i32,
    now_micros: u64,
}

struct StaticInner {
    processes: Vec<LiveProcess>,
    ignores_term: Vec<i32>,
    immortal: Vec<i32>,
    signals: Vec<(i32, Signal)>,
}

impl StaticProcessTable {
    pub fn new(processes: Vec<LiveProcess>, own_pid: i32) -> Self {
        Self {
            inner: Mutex::new(StaticInner {
                processes,
                ignores_term: Vec::new(),
                immortal: Vec::new(),
                signals: Vec::new(),
            }),
            own_pid,
            now_micros: 1_000_000_000_000_000,
        }
    }

    /// Make `pid` survive SIGTERM (it only disappears on SIGKILL).
    pub fn ignore_term(&self, pid: i32) {
        self.inner.lock().unwrap().ignores_term.push(pid);
    }

    /// Make `pid` survive every signal: the signal is recorded as delivered
    /// but the process stays.
    pub fn immortal(&self, pid: i32) {
        self.inner.lock().unwrap().immortal.push(pid);
    }

    /// Every `(pid, signal)` delivered so far, in order.
    pub fn signals(&self) -> Vec<(i32, Signal)> {
        self.inner.lock().unwrap().signals.clone()
    }

    pub fn is_alive(&self, pid: i32) -> bool {
        self.inner.lock().unwrap().processes.iter().any(|p| p.pid == pid)
    }
}

impl ProcessTable for StaticProcessTable {
    fn list(&self) -> io::Result<Vec<LiveProcess>> {
        Ok(self.inner.lock().unwrap().processes.clone())
    }

    fn snapshot(&self, pid: i32) -> io::Result<LiveProcess> {
        self.inner
            .lock()
            .unwrap()
            .processes
            .iter()
            .find(|p| p.pid == pid)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))
    }

    fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.processes.iter().any(|p| p.pid == pid) {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")));
        }
        inner.signals.push((pid, signal));
        let dies = !inner.immortal.contains(&pid)
            && match signal {
                Signal::Kill => true,
                Signal::Term => !inner.ignores_term.contains(&pid),
            };
        if dies {
            inner.processes.retain(|p| p.pid != pid);
        }
        Ok(())
    }

    fn own_pid(&self) -> i32 {
        self.own_pid
    }

    fn now_micros(&self) -> u64 {
        self.now_micros
    }
}

/// A convenient fabricated process for tests.
pub fn fake_process(pid: i32, ppid: i32, command: &str, cwd: Option<&str>) -> LiveProcess {
    LiveProcess {
        pid,
        ppid,
        pgid: pid,
        start_micros: 999_000_000_000_000,
        command: command.to_owned(),
        argv: vec![command.to_owned()],
        cwd: cwd.map(PathBuf::from),
        has_controlling_terminal: false,
    }
}

#[cfg(target_os = "macos")]
mod sys {
    //! Darwin: `libproc` for the pid list, `proc_bsdinfo` and the vnode
    //! path info (working directory); `sysctl(KERN_PROCARGS2)` for argv.

    use std::io;
    use std::os::raw::c_void;
    use std::path::PathBuf;

    use super::LiveProcess;

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
    /// `NODEV`: the value `proc_bsdinfo.e_tdev` carries for a process
    /// without a controlling terminal.
    const NODEV: u32 = u32::MAX;

    /// `sizeof(struct proc_bsdinfo)`.
    const BSDINFO_SIZE: usize = 136;
    const BSDINFO_PPID: usize = 16;
    const BSDINFO_NAME: usize = 64;
    const BSDINFO_NAME_LEN: usize = 32;
    const BSDINFO_COMM: usize = 48;
    const BSDINFO_COMM_LEN: usize = 16;
    const BSDINFO_PGID: usize = 100;
    const BSDINFO_TDEV: usize = 108;
    const BSDINFO_START_SEC: usize = 120;
    const BSDINFO_START_USEC: usize = 128;

    /// `sizeof(struct proc_vnodepathinfo)`: two `vnode_info_path`, each a
    /// 152-byte `vnode_info` followed by a `MAXPATHLEN` (1024) path. The
    /// current directory is the first; its path starts after its
    /// `vnode_info`.
    const VNODEPATHINFO_SIZE: usize = 2 * (152 + 1024);
    const VNODEPATHINFO_CDIR_PATH: usize = 152;
    const MAXPATHLEN: usize = 1024;

    unsafe extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: libc::c_int) -> libc::c_int;
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    pub(super) fn list() -> io::Result<Vec<LiveProcess>> {
        // SAFETY: a null buffer asks for the byte count needed.
        let needed = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(io::Error::last_os_error());
        }
        // Headroom for processes created between the two calls.
        let capacity = (needed as usize / std::mem::size_of::<libc::pid_t>()) + 64;
        let mut pids = vec![0 as libc::pid_t; capacity];
        let bytes = (capacity * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `pids` holds exactly `bytes` bytes.
        let filled = unsafe { proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr() as *mut c_void, bytes) };
        if filled <= 0 {
            return Err(io::Error::last_os_error());
        }
        let count = filled as usize / std::mem::size_of::<libc::pid_t>();
        let mut out = Vec::with_capacity(count);
        for &pid in pids.iter().take(count) {
            if pid <= 0 {
                continue;
            }
            // A process that exited between listing and inspection, or one
            // we may not inspect, is simply absent from the result.
            if let Ok(process) = snapshot(pid) {
                out.push(process);
            }
        }
        Ok(out)
    }

    pub(super) fn snapshot(pid: i32) -> io::Result<LiveProcess> {
        let mut info = [0u8; BSDINFO_SIZE];
        // SAFETY: `info` is exactly `BSDINFO_SIZE` bytes.
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr() as *mut c_void,
                BSDINFO_SIZE as libc::c_int,
            )
        };
        if n <= 0 {
            let err = io::Error::last_os_error();
            return Err(if err.raw_os_error() == Some(libc::ESRCH) {
                io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}"))
            } else {
                err
            });
        }
        if n as usize != BSDINFO_SIZE {
            return Err(io::Error::other(format!(
                "proc_pidinfo returned {n} bytes for pid {pid}; expected {BSDINFO_SIZE}"
            )));
        }
        let u32_at = |off: usize| u32::from_ne_bytes(info[off..off + 4].try_into().unwrap());
        let u64_at = |off: usize| u64::from_ne_bytes(info[off..off + 8].try_into().unwrap());
        let name = c_name(&info[BSDINFO_NAME..BSDINFO_NAME + BSDINFO_NAME_LEN])
            .filter(|n| !n.is_empty())
            .or_else(|| c_name(&info[BSDINFO_COMM..BSDINFO_COMM + BSDINFO_COMM_LEN]))
            .unwrap_or_default();
        let start_micros = u64_at(BSDINFO_START_SEC)
            .saturating_mul(1_000_000)
            .saturating_add(u64_at(BSDINFO_START_USEC));
        Ok(LiveProcess {
            pid,
            ppid: u32_at(BSDINFO_PPID) as i32,
            pgid: u32_at(BSDINFO_PGID) as i32,
            start_micros,
            command: name,
            argv: argv(pid).unwrap_or_default(),
            cwd: cwd(pid),
            has_controlling_terminal: u32_at(BSDINFO_TDEV) != NODEV,
        })
    }

    fn cwd(pid: i32) -> Option<PathBuf> {
        let mut buf = vec![0u8; VNODEPATHINFO_SIZE];
        // SAFETY: `buf` is exactly `VNODEPATHINFO_SIZE` bytes.
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr() as *mut c_void,
                VNODEPATHINFO_SIZE as libc::c_int,
            )
        };
        if n <= 0 {
            return None;
        }
        let path = c_name(&buf[VNODEPATHINFO_CDIR_PATH..VNODEPATHINFO_CDIR_PATH + MAXPATHLEN])?;
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }

    /// `sysctl(KERN_PROCARGS2)`: a 32-bit argc, the executable path, NUL
    /// padding, then `argc` NUL-terminated strings.
    fn argv(pid: i32) -> Option<Vec<String>> {
        let mut argmax: libc::c_int = 0;
        let mut argmax_len = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: `argmax` is a valid out-parameter of the declared length.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                &mut argmax as *mut _ as *mut c_void,
                &mut argmax_len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || argmax <= 0 {
            return None;
        }
        let mut buf = vec![0u8; argmax as usize];
        let mut len = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `buf` has `len` bytes available.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len < 4 {
            return None;
        }
        buf.truncate(len);
        let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?).max(0) as usize;
        let mut cursor = 4;
        // Skip the executable path.
        while cursor < buf.len() && buf[cursor] != 0 {
            cursor += 1;
        }
        // Skip the NUL padding between the path and argv[0].
        while cursor < buf.len() && buf[cursor] == 0 {
            cursor += 1;
        }
        let mut args = Vec::with_capacity(argc);
        while args.len() < argc && cursor < buf.len() {
            let end = buf[cursor..]
                .iter()
                .position(|b| *b == 0)
                .map(|p| cursor + p)
                .unwrap_or(buf.len());
            args.push(String::from_utf8_lossy(&buf[cursor..end]).into_owned());
            cursor = end + 1;
        }
        Some(args)
    }

    fn c_name(raw: &[u8]) -> Option<String> {
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        std::str::from_utf8(&raw[..end]).ok().map(str::to_owned)
    }
}

#[cfg(target_os = "linux")]
mod sys {
    //! Linux: `/proc/<pid>/stat`, `/proc/<pid>/cwd` and `/proc/<pid>/cmdline`.

    use std::io;
    use std::path::PathBuf;

    use super::LiveProcess;

    pub(super) fn list() -> io::Result<Vec<LiveProcess>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            if let Ok(process) = snapshot(pid) {
                out.push(process);
            }
        }
        Ok(out)
    }

    pub(super) fn snapshot(pid: i32) -> io::Result<LiveProcess> {
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
        let ppid = field(1)?.parse::<i32>().map_err(|_| invalid("ppid"))?;
        let pgid = field(2)?.parse::<i32>().map_err(|_| invalid("pgrp"))?;
        let tty_nr = field(4)?.parse::<i64>().map_err(|_| invalid("tty_nr"))?;
        let start_ticks = field(19)?.parse::<u64>().map_err(|_| invalid("starttime"))?;
        let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|b| *b == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(LiveProcess {
            pid,
            ppid,
            pgid,
            start_micros: boot_micros().saturating_add(start_ticks.saturating_mul(1_000_000) / clock_ticks()),
            command,
            argv,
            cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(PathBuf::from),
            has_controlling_terminal: tty_nr != 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_snapshot_reports_identity_and_working_directory() {
        let table = SystemProcessTable;
        let me = table.snapshot(table.own_pid()).unwrap();
        assert_eq!(me.pid, std::process::id() as i32);
        assert!(me.ppid > 0);
        assert!(me.start_micros > 0);
        assert!(!me.command.is_empty());
        assert!(!me.argv.is_empty(), "own argv must be readable");
        let expected = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let reported = std::fs::canonicalize(me.cwd.expect("own cwd must be readable")).unwrap();
        assert_eq!(reported, expected);
        // Under `bazel test` there is never a controlling terminal.
        assert!(!me.has_controlling_terminal);
    }

    #[test]
    fn self_start_time_is_in_the_past_on_the_table_clock() {
        let table = SystemProcessTable;
        let me = table.snapshot(table.own_pid()).unwrap();
        let now = table.now_micros();
        assert!(me.start_micros <= now, "start {} after now {now}", me.start_micros);
        // Started within the last day: the clocks agree to within reason.
        assert!(now - me.start_micros < 24 * 3600 * 1_000_000);
    }

    #[test]
    fn list_includes_self() {
        let table = SystemProcessTable;
        let all = table.list().unwrap();
        assert!(all.iter().any(|p| p.pid == table.own_pid()));
    }

    #[test]
    fn missing_pid_is_not_found() {
        // Far past any plausible pid on either platform.
        let err = SystemProcessTable.snapshot(99_999_999 - 1).unwrap_err();
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
