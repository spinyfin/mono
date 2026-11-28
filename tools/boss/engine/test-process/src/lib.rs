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
}

impl ProcessGroupChild {
    /// Wrap a child that already leads its own process group or session.
    pub fn from_leader(child: std::process::Child) -> Self {
        Self { child }
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
        let pid = self.child.id() as i32;
        // SAFETY: `pid` is a process-group (or session) leader this helper
        // spawned; `-pid` targets that group only, never the test runner.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
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

/// How a job left the shell that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    /// `nohup <job> &`, after which the shell exits.
    Nohup,
    /// The job is started in a new session and its launcher exits.
    Setsid,
}

/// Waits until its launcher is gone, so that it has been re-parented before
/// anything inspects it, then publishes its pid and idles. It ends on its own
/// after two minutes so a failed test cannot leave it running for long.
const ESCAPED_JOB: &str = "import os,sys,time\nparent=int(sys.argv[2])\nwhile os.getppid()==parent: time.sleep(0.01)\nopen(sys.argv[1]+'.tmp','w').write(str(os.getpid()))\nos.rename(sys.argv[1]+'.tmp',sys.argv[1])\ntime.sleep(120)";
/// `nohup <job> &`, then the shell exits. The test runtime's `PATH` holds
/// only audited tools, so `nohup` is taken from its standard location, and
/// where there is none the shell does what `nohup` does: ignore `SIGHUP`,
/// then exec the job. `$0` is the job source and `$1` its pid file.
const NOHUP_LAUNCHER: &str = "if [ -x /usr/bin/nohup ]; then /usr/bin/nohup python3 -c \"$0\" \"$1\" $$ >/dev/null 2>&1 </dev/null & \
     else ( trap '' HUP; exec python3 -c \"$0\" \"$1\" $$ >/dev/null 2>&1 </dev/null ) & fi";
const SETSID_LAUNCHER: &str = "import os,subprocess,sys; subprocess.Popen([sys.executable,'-c',sys.argv[1],sys.argv[2],str(os.getpid())], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)";

/// A job that outlived the shell that started it, as a worker's background
/// tool does: re-parented away from this test, sharing no pipe with it, and
/// with a working directory of the caller's choosing. Dropping it kills it.
pub struct EscapedProcess {
    pid: i32,
}

impl EscapedProcess {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(self.pid, 0) == 0 }
    }

    /// Whether the job is gone within `limit`.
    pub fn exited_within(&self, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while self.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        !self.is_alive()
    }
}

impl Drop for EscapedProcess {
    fn drop(&mut self) {
        // SAFETY: the pid of a job this helper started; a no-op once it has exited.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
    }
}

/// Start a job in `dir` that escapes as `escape` describes and return once
/// it has been re-parented. `run_id` is the `BOSS_RUN_ID` it inherits, as a
/// worker's tool shell would pass on; `None` starts it with no run identity.
pub fn spawn_escaped(escape: Escape, dir: impl AsRef<std::path::Path>, run_id: Option<&str>) -> EscapedProcess {
    use std::process::{Command, Stdio};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let dir = dir.as_ref();
    let pid_file = dir.join(format!(
        ".escaped-{}-{}.pid",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut command = match escape {
        Escape::Nohup => {
            let mut command = Command::new("sh");
            command.args(["-c", NOHUP_LAUNCHER, ESCAPED_JOB]);
            command
        }
        Escape::Setsid => {
            let mut command = Command::new("python3");
            command.args(["-c", SETSID_LAUNCHER, ESCAPED_JOB]);
            command
        }
    };
    command
        .arg(&pid_file)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env_remove("BOSS_RUN_ID")
        .env_remove("BOSS_SPAWN_TOKEN");
    if let Some(run_id) = run_id {
        command.env("BOSS_RUN_ID", run_id);
    }
    let status = command.status().expect("start escaped job launcher");
    assert!(status.success(), "escaped job launcher exited {status:?}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            let _ = std::fs::remove_file(&pid_file);
            return EscapedProcess {
                pid: pid.trim().parse().expect("escaped job pid"),
            };
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{escape:?} job never reported that it was re-parented"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod process_group_child_tests {
    use super::{spawn_group_leader_sleeper, spawn_setsid_sleeper};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn pid_is_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid as i32, 0) == 0 }
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
    fn escaped_jobs_are_reparented_into_the_given_directory_and_die_with_their_guard() {
        for escape in [super::Escape::Nohup, super::Escape::Setsid] {
            let dir = tempfile::tempdir().unwrap();
            let job = super::spawn_escaped(escape, dir.path(), Some("run-under-test"));
            assert!(job.is_alive(), "{escape:?} job must be running");
            assert_ne!(job.pid(), std::process::id() as i32);
            let pid = job.pid() as u32;
            drop(job);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while pid_is_alive(pid) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!pid_is_alive(pid), "dropping the guard must kill the {escape:?} job");
        }
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
