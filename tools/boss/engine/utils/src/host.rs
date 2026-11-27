//! Host identity helpers and local Unix-socket peer checks.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// This machine's hostname (`gethostname(2)`), or `unknown-host`.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of the stated length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown-host".to_owned();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The process's effective user id.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Credentials of the process at the other end of a connected Unix socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixPeerCred {
    pub uid: u32,
    pub pid: Option<libc::pid_t>,
}

/// Why `path` is not a directory owned by this process's user that no one else
/// can enter or list, or `None` if it is one. Does not follow a symbolic link
/// at `path`.
pub fn private_directory_problem(path: &Path) -> io::Result<Option<String>> {
    let metadata = std::fs::symlink_metadata(path)?;
    let shown = path.display();
    Ok(if !metadata.is_dir() {
        Some(format!("{shown} is not a directory"))
    } else if metadata.uid() != effective_uid() {
        Some(format!("{shown} is owned by uid {}", metadata.uid()))
    } else if metadata.mode() & 0o077 != 0 {
        Some(format!("{shown} has mode {:04o}", metadata.mode() & 0o7777))
    } else {
        None
    })
}

/// Peer identity of a connected Unix stream (`LOCAL_PEERPID` on macOS,
/// `SO_PEERCRED` on Linux).
pub fn unix_peer_cred(fd: &impl AsRawFd) -> io::Result<UnixPeerCred> {
    unix_peer_cred_fd(fd.as_raw_fd())
}

/// The peer pid of a connected Unix stream.
pub fn unix_peer_pid(fd: &impl AsRawFd) -> io::Result<libc::pid_t> {
    match unix_peer_cred(fd)?.pid {
        Some(pid) => Ok(pid),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the Unix socket peer pid is unknown",
        )),
    }
}

#[cfg(target_os = "macos")]
fn unix_peer_cred_fd(fd: std::os::fd::RawFd) -> io::Result<UnixPeerCred> {
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is borrowed from the caller; `uid` and `gid` are stack
    // locals passed only to `getpeereid`.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pid: libc::pid_t = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `fd` remains valid; `pid` and `len` are stack locals passed only
    // to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixPeerCred { uid, pid: Some(pid) })
}

#[cfg(target_os = "linux")]
fn unix_peer_cred_fd(fd: std::os::fd::RawFd) -> io::Result<UnixPeerCred> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is borrowed from the caller; `cred` and `len` are stack
    // locals passed only to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixPeerCred {
        uid: cred.uid,
        pid: Some(cred.pid),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unix_peer_cred_fd(_fd: std::os::fd::RawFd) -> io::Result<UnixPeerCred> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix socket peer credentials are not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn host_name_is_non_empty() {
        assert!(!host_name().is_empty());
    }

    #[test]
    fn a_private_directory_is_owned_by_the_engine_user_and_closed_to_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(private_directory_problem(dir.path()).unwrap(), None);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let problem = private_directory_problem(dir.path())
            .unwrap()
            .expect("a group-readable directory");
        assert!(problem.contains("mode 0750"), "{problem}");
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let problem = private_directory_problem(&file)
            .unwrap()
            .expect("a file is not a directory");
        assert!(problem.contains("not a directory"), "{problem}");
    }

    #[test]
    fn a_symlink_to_a_private_directory_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(private_directory_problem(&link).unwrap().is_some());
    }
}
