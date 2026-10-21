//! Bounded synchronous process execution shared by command-line tools.
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run a prepared command, capturing its output, and kill it if it does not
/// exit before `timeout`.
///
/// Reader threads keep a command that writes heavily to both pipes from
/// deadlocking. On timeout their output is intentionally discarded: child
/// processes can inherit the pipe descriptors, so joining the readers after
/// killing only the direct child could wait indefinitely for those descendants.
pub fn output_blocking_timeout(command: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    output_bounded(command, timeout, false, |_| {}, |_| {})
}

/// Like [`output_blocking_timeout`], but the child leads its own process group
/// and a timeout kills the whole group, so descendants the child spawned (a
/// `git ls-remote` under `cube workspace status`, say) die with it instead of
/// being orphaned and left holding the output pipes.
///
/// `on_spawn` receives the child's pid (which is also its process-group id) as
/// soon as it exists, so a caller can track the subprocess while it runs and
/// reap it with [`kill_process_group`] if it needs to be abandoned early.
///
/// The deadline also covers draining the output: a child that exits while a
/// descendant still holds its pipe open is treated as timed out rather than
/// waited on indefinitely.
#[cfg(unix)]
pub fn output_blocking_timeout_in_group(
    command: &mut Command,
    timeout: Duration,
    on_spawn: impl FnOnce(u32),
) -> std::io::Result<Output> {
    output_bounded(command, timeout, true, on_spawn, |_| {})
}

/// SIGKILL every process in the group led by `pgid`. Best-effort: a group that
/// has already exited is not an error.
#[cfg(unix)]
pub fn kill_process_group(pgid: u32) {
    if let Ok(pgid) = i32::try_from(pgid)
        && pgid > 0
    {
        // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Execute with an optional isolated group and a polling callback.
pub fn output_bounded(
    command: &mut Command,
    timeout: Duration,
    kill_group: bool,
    on_spawn: impl FnOnce(u32),
    mut on_poll: impl FnMut(Duration),
) -> std::io::Result<Output> {
    #[cfg(unix)]
    if kill_group {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    on_spawn(child.id());
    let kill = |child: &mut std::process::Child| {
        #[cfg(unix)]
        if kill_group {
            kill_process_group(child.id());
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let timed_out = || {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{program} exceeded {timeout:?} timeout"),
        )
    };
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_tx.send(stdout.read_to_end(&mut buf).map(|_| buf));
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_tx.send(stderr.read_to_end(&mut buf).map(|_| buf));
    });
    let started = Instant::now();
    let deadline = started + timeout;
    let status = loop {
        on_poll(started.elapsed());
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill(&mut child);
                return Err(timed_out());
            }
            Err(err) => {
                kill(&mut child);
                return Err(err);
            }
        }
    };
    let mut drain = |rx: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| {
        loop {
            on_poll(started.elapsed());
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => return Err(timed_out()),
            }
        }
    };
    let stdout = drain(&stdout_rx);
    let stderr = drain(&stderr_rx);
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => Ok(Output { status, stdout, stderr }),
        (stdout, stderr) => {
            // A descendant outlived the child holding its pipe; reap the group.
            #[cfg(unix)]
            if kill_group {
                kill_process_group(child.id());
            }
            Err(stdout.err().or(stderr.err()).expect("one side failed"))
        }
    }
}
