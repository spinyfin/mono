//! Owning guards and sleeper fixtures for tests that kill real processes.
//!
//! Lives in its own crate because both `boss-host-execution` and
//! `boss-engine-completion` need it and completion depends on host-execution,
//! so neither can host the shared copy. `completion::test_support` re-exports it.

/// A child that leads its own process group (or session). Dropping it
/// sends `SIGKILL` to the group and waits, so a panic or an early return
/// cannot leave the sleeper behind. The child also exits if the pipe from
/// this process closes, which covers the test binary being killed without
/// running destructors.
pub struct ProcessGroupChild {
    child: std::process::Child,
    lifeline: Option<std::process::ChildStdin>,
}

impl ProcessGroupChild {
    /// Wrap a child that already leads its own process group or session.
    pub fn from_leader(child: std::process::Child) -> Self {
        Self { child, lifeline: None }
    }
}

impl std::ops::Deref for ProcessGroupChild {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for ProcessGroupChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ProcessGroupChild {
    fn drop(&mut self) {
        // A reaped leader's group id may already belong to another process.
        if matches!(self.child.try_wait(), Ok(None)) {
            let pid = self.child.id() as i32;
            // SAFETY: the unreaped child owns this group id; a negative pid
            // signals that group, never the test runner's group.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = self.child.wait();
        }
        drop(self.lifeline.take());
    }
}

/// Spawn a command beneath a group-leading supervisor. Configure arguments,
/// environment, directory and output streams on the supplied command. Stdin
/// is reserved for the parent lifeline; the supervised program sees EOF.
/// Closing the lifeline kills the whole group even without Rust destructors.
pub fn spawn_supervised(
    program: impl AsRef<std::ffi::OsStr>,
    configure: impl FnOnce(&mut std::process::Command),
) -> ProcessGroupChild {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new("python3");
    command
        .args([
            "-I",
            "-c",
            r#"
import os, select, signal, subprocess, sys
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL)
while True:
    if select.select([sys.stdin], [], [], 0.05)[0]:
        os.killpg(os.getpid(), signal.SIGKILL)
    status = child.poll()
    if status is not None:
        sys.exit(status if status >= 0 else 128 - status)
"#,
        ])
        .arg(program);
    configure(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn supervised command");
    // Child::wait closes stdin. Keep the lifeline independent so waiting for
    // a normal exit does not request termination.
    let lifeline = child.stdin.take();
    ProcessGroupChild { child, lifeline }
}

/// Python that leads a new process group, then waits until stdin closes
/// or 300 seconds elapse. Stdin must stay piped from the parent so a
/// killed test still reaps the child.
const GROUP_LEADER_SLEEPER: &str = "import os,select,sys; os.setpgid(0,0); select.select([sys.stdin],[],[],300)";
const SETSID_SLEEPER: &str =
    "import os,select,sys; os.setsid(); print('ready', flush=True); select.select([sys.stdin],[],[],600)";

/// Spawn a long-lived child in its OWN process group and return an owning
/// guard. A reap signals the process *group*, so a test exercising a REAL
/// kill (durable-state teardown, `release_worker_pane`, `retire_pane`'s
/// durable-evidence auto-teardown) must target a process in its own group
/// — never `std::process::id()` — or the signal lands on the test runner's
/// own group and kills the test process itself.
pub fn spawn_group_leader_sleeper() -> ProcessGroupChild {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let child = unsafe {
        Command::new("python3")
            .args(["-u", "-c", GROUP_LEADER_SLEEPER])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                // setpgid(0, 0): become our own process group leader before
                // Python starts, so the window before `os.setpgid` is closed.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .expect("spawn group-leader sleeper")
    };
    ProcessGroupChild::from_leader(child)
}

/// Spawn `python3` that calls `os.setsid()`, prints `ready`, and waits on
/// stdin. Same owning guard as [`spawn_group_leader_sleeper`].
pub fn spawn_setsid_sleeper(
    dir: impl AsRef<std::path::Path>,
    configure: impl FnOnce(&mut std::process::Command),
) -> ProcessGroupChild {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let mut command = Command::new("python3");
    command
        .args(["-u", "-c", SETSID_SLEEPER])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure(&mut command);
    let mut child = command.spawn().expect("spawn setsid sleeper");
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().expect("setsid sleeper stdout"))
        .read_line(&mut ready)
        .expect("read setsid sleeper ready");
    assert_eq!(ready.trim(), "ready", "setsid sleeper must print ready");
    ProcessGroupChild::from_leader(child)
}

#[cfg(test)]
mod process_group_child_tests {
    use super::{spawn_group_leader_sleeper, spawn_setsid_sleeper};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn pid_is_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn assert_exits_on_eof(mut child: super::ProcessGroupChild) {
        drop(child.stdin.take());
        drop(child.lifeline.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "lifeline EOF must stop child");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn group_sleeper_exits_on_stdin_eof() {
        assert_exits_on_eof(spawn_group_leader_sleeper());
    }

    #[test]
    fn session_sleeper_exits_on_stdin_eof() {
        let dir = tempfile::tempdir().unwrap();
        assert_exits_on_eof(spawn_setsid_sleeper(dir.path(), |_| {}));
    }

    #[test]
    fn supervised_command_exits_on_lifeline_eof() {
        use std::io::BufRead;
        let mut child = super::spawn_supervised("python3", |command| {
            command
                .args([
                    "-u",
                    "-c",
                    "import os,time; print(os.getpid(), flush=True); time.sleep(300)",
                ])
                .stdout(std::process::Stdio::piped());
        });
        let mut pid = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut pid)
            .unwrap();
        let pid: u32 = pid.trim().parse().unwrap();
        assert!(pid_is_alive(pid));
        assert_exits_on_eof(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_is_alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "supervised descendant survived EOF"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn supervised_command_preserves_exit_status() {
        let mut child = super::spawn_supervised("python3", |command| {
            command.args(["-c", "import sys; sys.exit(42)"]);
        });
        assert_eq!(child.wait().unwrap().code(), Some(42));
    }

    #[test]
    fn dropping_the_guard_reaps_the_process_group() {
        let child = spawn_group_leader_sleeper();
        let pid = child.id();
        assert!(pid_is_alive(pid), "sleeper must be running before drop");
        drop(child);
        assert!(!pid_is_alive(pid), "drop must kill the process group");
    }

    #[test]
    fn unwinding_through_panic_reaps_the_process_group() {
        let pid = {
            let child = spawn_group_leader_sleeper();
            let pid = child.id();
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                let _child = child;
                panic!("process-group guard must reap on unwind");
            }));
            assert!(panicked.is_err(), "fixture panic must propagate");
            pid
        };
        assert!(!pid_is_alive(pid), "unwind must kill the process group");
    }

    #[test]
    fn dropping_a_setsid_guard_reaps_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let child = spawn_setsid_sleeper(dir.path(), |command| {
            command.env_remove("BOSS_RUN_ID").env_remove("BOSS_SPAWN_TOKEN");
        });
        let pid = child.id();
        assert!(pid_is_alive(pid), "setsid sleeper must be running before drop");
        drop(child);
        assert!(!pid_is_alive(pid), "drop must kill the session");
    }
}
