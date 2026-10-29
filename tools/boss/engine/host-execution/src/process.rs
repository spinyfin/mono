//! Host process identity shared by local and spoke ingress.
use std::io;

/// One live process as the OS reports it right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSnapshot {
    /// Parent pid; `None` at the root of the tree (parent 0 / launchd).
    pub ppid: Option<libc::pid_t>,
    /// Opaque, platform-specific process start time. Equal values for the
    /// same pid mean the same process incarnation; a recycled pid differs.
    pub start: u64,
    /// Short process name, for diagnostics only.
    pub command: String,
}

/// Read `pid`'s parent pid, start time and command via macOS `proc_pidinfo`.
/// `ppid` is `None` when `pid` is the root of the process tree (parent 0 /
/// launchd).
#[cfg(target_os = "macos")]
pub fn process_snapshot(pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    let info = darwin::proc_bsd_info(pid)?;
    Ok(ProcessSnapshot {
        ppid: if info.ppid == 0 { None } else { Some(info.ppid) },
        start: info.start_micros,
        command: info.command,
    })
}

/// Read `pid`'s parent pid, start time and command from Linux's
/// `/proc/<pid>/stat`.
///
/// The process name field is parenthesized and may itself contain spaces or
/// closing parentheses, so split after its final closing parenthesis before
/// reading the fields that follow it. After the name, field 0 is the state,
/// field 1 the parent pid and field 19 the start time (clock ticks since boot).
#[cfg(target_os = "linux")]
pub fn process_snapshot(pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    let info = workspace_procs::process_table::linux_identity(pid)?;
    Ok(ProcessSnapshot {
        ppid: if info.ppid <= 0 { None } else { Some(info.ppid) },
        start: info.start_ticks,
        command: info.command,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_snapshot(_pid: libc::pid_t) -> io::Result<ProcessSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process_snapshot is only implemented on macOS and Linux",
    ))
}

#[cfg(target_os = "macos")]
pub use workspace_procs::darwin;
