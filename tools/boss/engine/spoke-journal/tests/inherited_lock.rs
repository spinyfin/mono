use boss_spoke_journal::{Journal, Limits};
use std::os::{fd::AsRawFd, unix::net::UnixStream};

// Model a child paused between fork and exec, when CLOEXEC has not yet closed
// its inherited journal descriptor. Only async-signal-safe calls run there.
struct Child {
    pid: libc::pid_t,
    release: UnixStream,
}

impl Drop for Child {
    fn drop(&mut self) {
        use std::io::Write;
        self.release.write_all(&[1]).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(self.pid, &mut status, 0) }, self.pid);
        assert_eq!(status, 0);
    }
}

#[test]
fn inherited_descriptor_does_not_extend_journal_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let journal = Journal::open(&root, "host", Limits::default()).unwrap();
    let (release, child) = UnixStream::pair().unwrap();
    let fd = child.as_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let mut byte = 0u8;
        unsafe {
            let count = libc::read(fd, (&mut byte as *mut u8).cast(), 1);
            libc::_exit(if count == 1 { 0 } else { 1 });
        }
    }
    let _child = Child { pid, release };
    drop(child);
    assert_eq!(
        Journal::open(&root, "host", Limits::default())
            .err()
            .unwrap()
            .to_string(),
        "spoke daemon lock is held"
    );
    drop(journal);
    let reopened = Journal::open(&root, "host", Limits::default()).unwrap();
    assert_eq!(
        Journal::open(&root, "host", Limits::default())
            .err()
            .unwrap()
            .to_string(),
        "spoke daemon lock is held"
    );
    drop(reopened);
}
