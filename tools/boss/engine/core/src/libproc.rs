//! Single shared mirror of Darwin's `proc_bsdinfo` (`<sys/proc_info.h>`),
//! read via `libproc`'s `proc_pidinfo(PROC_PIDTBSDINFO, ...)`.
//!
//! [`crate::worker_registry::process_snapshot`] needs `pbi_ppid`, the start
//! time and the process name out of this struct. The pgid/foreground-pgid fields this used to also expose (for
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
#[derive(Clone, Debug)]
pub(crate) struct ProcBsdInfo {
    pub(crate) ppid: libc::pid_t,
    /// Process start time in microseconds since the epoch (`pbi_start_tvsec` /
    /// `pbi_start_tvusec`). Together with the pid it identifies one process
    /// incarnation: a recycled pid always starts later.
    pub(crate) start_micros: u64,
    /// Short process name (`pbi_name`, falling back to `pbi_comm`).
    pub(crate) command: String,
}

const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
const PROC_UID_ONLY: u32 = 4;
const MAXPATHLEN: usize = 1024;

unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

// Darwin `<sys/proc_info.h>` `vnode_info_path`: 152-byte `vnode_info`
// (`vinfo_stat` 136 + type/pad/fsid 16) then `vip_path[MAXPATHLEN]`.
// Opaque prefix because we only read the cwd path.
const VNODE_INFO_SIZE: usize = 152;

#[repr(C)]
struct VnodeInfoPath {
    _vi: [u8; VNODE_INFO_SIZE],
    vip_path: [u8; MAXPATHLEN],
}

impl Default for VnodeInfoPath {
    fn default() -> Self {
        Self {
            _vi: [0; VNODE_INFO_SIZE],
            vip_path: [0; MAXPATHLEN],
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct ProcVnodePathInfo {
    pvi_cdir: VnodeInfoPath,
    pvi_rdir: VnodeInfoPath,
}

/// Current working directory of `pid`, via `PROC_PIDVNODEPATHINFO`.
pub(crate) fn proc_cwd(pid: libc::pid_t) -> std::io::Result<std::path::PathBuf> {
    let mut info = ProcVnodePathInfo::default();
    let info_size = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;
    // SAFETY: `info` is a valid buffer of exactly `info_size` bytes.
    let n = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            info_size,
        )
    };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n != info_size {
        return Err(std::io::Error::other(format!(
            "proc_pidinfo vnodepathinfo returned {n} bytes for pid {pid}; expected {info_size}"
        )));
    }
    let path = c_name(&info.pvi_cdir.vip_path).unwrap_or_default();
    if path.is_empty() {
        return Err(std::io::Error::other(format!("empty cwd for pid {pid}")));
    }
    Ok(std::path::PathBuf::from(path))
}

/// Every pid owned by `uid` (effective uid), via `PROC_UID_ONLY`.
pub(crate) fn list_pids_for_uid(uid: u32) -> std::io::Result<Vec<libc::pid_t>> {
    list_pids_of(PROC_UID_ONLY, uid)
}

fn list_pids_of(kind: u32, info: u32) -> std::io::Result<Vec<libc::pid_t>> {
    // SAFETY: null buffer is the documented size-query form of `proc_listpids`.
    let bytes = unsafe { libc::proc_listpids(kind, info, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0i32; (bytes as usize / std::mem::size_of::<i32>()) + 32];
    let capacity = (buf.len() * std::mem::size_of::<i32>()) as libc::c_int;
    // SAFETY: `buf` is a valid integer buffer of `capacity` bytes.
    let written = unsafe { libc::proc_listpids(kind, info, buf.as_mut_ptr() as *mut c_void, capacity) };
    if written <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let count = (written as usize) / std::mem::size_of::<i32>();
    Ok(buf
        .into_iter()
        .take(count)
        .filter(|pid| *pid > 0)
        .map(|pid| pid as libc::pid_t)
        .collect())
}

/// Fetch `pid`'s `proc_bsdinfo` via `libproc` and pull out `pbi_ppid` and
/// the process start time.
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
        start_micros: info
            .start_time
            .seconds
            .saturating_mul(1_000_000)
            .saturating_add(info.start_time.microseconds),
        command: c_name(&info.names.name)
            .filter(|name| !name.is_empty())
            .or_else(|| c_name(&info.names.command))
            .unwrap_or_default(),
    })
}

/// Decode a NUL-terminated fixed-size C name buffer.
fn c_name(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    std::str::from_utf8(&raw[..end]).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_name_stops_at_the_first_nul() {
        assert_eq!(c_name(b"zsh\0garbage").as_deref(), Some("zsh"));
        assert_eq!(c_name(b"abc").as_deref(), Some("abc"));
        assert_eq!(c_name(b"\0").as_deref(), Some(""));
    }

    #[test]
    fn self_info_has_a_start_time_and_name() {
        let info = proc_bsd_info(std::process::id() as libc::pid_t).unwrap();
        assert!(info.start_micros > 0);
        assert!(!info.command.is_empty());
    }

    #[test]
    fn layout_matches_darwin_abi() {
        assert_eq!(std::mem::size_of::<RawProcBsdInfo>(), 136);
        assert_eq!(std::mem::offset_of!(RawProcBsdInfo, job_control.process_group), 100);
        assert_eq!(
            std::mem::offset_of!(RawProcBsdInfo, job_control.terminal_foreground_group),
            112
        );
        assert_eq!(std::mem::size_of::<VnodeInfoPath>(), VNODE_INFO_SIZE + MAXPATHLEN);
        assert_eq!(
            std::mem::size_of::<ProcVnodePathInfo>(),
            2 * (VNODE_INFO_SIZE + MAXPATHLEN)
        );
    }

    #[test]
    fn self_cwd_matches_env() {
        let via_libproc = proc_cwd(std::process::id() as libc::pid_t).unwrap();
        let via_env = std::env::current_dir().unwrap();
        assert_eq!(via_libproc.canonicalize().unwrap(), via_env.canonicalize().unwrap());
    }

    #[test]
    fn uid_scoped_listing_includes_self() {
        let self_pid = std::process::id() as libc::pid_t;
        // SAFETY: geteuid has no preconditions.
        assert!(
            list_pids_for_uid(unsafe { libc::geteuid() })
                .unwrap()
                .contains(&self_pid)
        );
    }
}
