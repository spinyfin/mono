//! Single shared mirror of Darwin's `proc_bsdinfo` (`<sys/proc_info.h>`),
//! read via `libproc`'s `proc_pidinfo(PROC_PIDTBSDINFO, ...)`.
//!
//! [`crate::worker_registry::parent_pid`] needs `pbi_ppid` out of this
//! struct. The pgid/foreground-pgid fields this used to also expose (for
//! [`crate::background_children`]'s process-group probe) were removed along
//! with that probe — see `background_children`'s module doc for why the
//! pgid discriminator was abandoned. The full raw layout mirror stays as-is
//! regardless of which subset is exposed, since `proc_pidinfo` still needs
//! a correctly sized buffer to write into.

use std::os::raw::c_void;

const PROC_PIDTBSDINFO: libc::c_int = 3;

// Layout per <sys/proc_info.h> on Darwin. Grouping adjacent C fields into
// `repr(C)` components preserves the ABI while keeping the public
// `ProcBsdInfo` focused on the fields callers actually read.
#[repr(C)]
#[derive(Default)]
struct Header {
    flags: u32,
    status: u32,
    xstatus: u32,
    pid: u32,
    ppid: u32,
}

#[repr(C)]
#[derive(Default)]
struct Credentials {
    uid: u32,
    gid: u32,
    ruid: u32,
    rgid: u32,
    svuid: u32,
}

#[repr(C)]
#[derive(Default)]
struct Names {
    svgid: u32,
    reserved: u32,
    command: [u8; 16],
    name: [u8; 32],
    open_files: u32,
}

#[repr(C)]
#[derive(Default)]
struct JobControl {
    process_group: u32,
    job_count: u32,
    terminal_device: u32,
    terminal_foreground_group: u32,
    nice: i32,
}

#[repr(C)]
#[derive(Default)]
struct StartTime {
    seconds: u64,
    microseconds: u64,
}

/// Rust layout mirror of Darwin's `proc_bsdinfo`.
#[repr(C)]
#[derive(Default)]
struct RawProcBsdInfo {
    header: Header,
    credentials: Credentials,
    names: Names,
    job_control: JobControl,
    start_time: StartTime,
}

/// The subset of `proc_bsdinfo` this crate's callers need.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcBsdInfo {
    pub(crate) ppid: libc::pid_t,
}

unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

/// Fetch `pid`'s `proc_bsdinfo` via `libproc` and pull out `pbi_ppid`.
pub(crate) fn proc_bsd_info(pid: libc::pid_t) -> std::io::Result<ProcBsdInfo> {
    let mut info = RawProcBsdInfo::default();
    let info_size = std::mem::size_of::<RawProcBsdInfo>() as libc::c_int;
    // SAFETY: `info` is a valid buffer of exactly `info_size` bytes.
    let n = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, info_size) };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n != info_size {
        return Err(std::io::Error::other(format!(
            "proc_pidinfo returned {n} bytes for pid {pid}; expected {info_size}"
        )));
    }
    Ok(ProcBsdInfo {
        ppid: info.header.ppid as libc::pid_t,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_darwin_abi() {
        assert_eq!(std::mem::size_of::<RawProcBsdInfo>(), 136);
        assert_eq!(std::mem::offset_of!(RawProcBsdInfo, job_control.process_group), 100);
        assert_eq!(
            std::mem::offset_of!(RawProcBsdInfo, job_control.terminal_foreground_group),
            112
        );
    }
}
