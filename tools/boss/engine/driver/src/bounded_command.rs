//! Bounded subprocess helper shared by Grok preflight and Antigravity probes.
//!
//! Every subprocess here is bounded: a wall-clock timeout kills the child and
//! its whole process group, and a timeout is a loud failure naming the command
//! — never a silent pass. Engine shutdown reaps whatever is still registered.

use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::Duration;

/// Subprocesses currently running, keyed by pid (which is also the
/// process-group id), so an engine shutdown can reap and report them.
#[derive(Default)]
pub(crate) struct InFlight(Mutex<BTreeMap<u32, String>>);

impl InFlight {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, String>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Kill every registered process group and return the command lines.
    fn abandon(&self) -> Vec<String> {
        std::mem::take(&mut *self.map())
            .into_iter()
            .map(|(pgid, command)| {
                boss_command_runner::kill_process_group(pgid);
                command
            })
            .collect()
    }
}

static IN_FLIGHT: InFlight = InFlight(Mutex::new(BTreeMap::new()));

/// Kill every bounded subprocess still running and return their command
/// lines. Called when the engine gives up waiting for blocking tasks at
/// shutdown, so abandoned probes do not outlive the engine as orphans.
pub fn abandon_in_flight_commands() -> Vec<String> {
    IN_FLIGHT.abandon()
}

/// Human-readable `program arg arg` for errors and the in-flight registry.
pub(crate) fn render_command(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `command` capturing stdout/stderr, killing it and its process group if
/// it has not finished within `timeout`.
///
/// A timeout is an error that names the command; it is never mapped to an
/// empty success. Output is drained concurrently with the wait (see
/// [`boss_command_runner::output_blocking_timeout_in_group`]) so a chatty child
/// cannot deadlock against a non-reading parent. `failure_prefix` names the
/// caller in the timeout error.
pub(crate) fn run_bounded_as(command: &mut Command, timeout: Duration, failure_prefix: &str) -> anyhow::Result<Output> {
    run_bounded_in(&IN_FLIGHT, command, timeout, failure_prefix)
}

pub(crate) fn run_bounded_in(
    in_flight: &InFlight,
    command: &mut Command,
    timeout: Duration,
    failure_prefix: &str,
) -> anyhow::Result<Output> {
    let rendered = render_command(command);
    let result = boss_command_runner::retry_on_etxtbsy(|| {
        let registered = std::cell::Cell::new(None);
        let result = boss_command_runner::output_blocking_timeout_in_group(command, timeout, |pid| {
            in_flight.map().insert(pid, rendered.clone());
            registered.set(Some(pid));
        });
        if let Some(pid) = registered.get() {
            in_flight.map().remove(&pid);
        }
        result
    });
    result.map_err(|err| {
        if err.kind() == std::io::ErrorKind::TimedOut {
            anyhow::anyhow!(
                "{failure_prefix}: `{rendered}` did not complete within {}s and was killed \
                 along with its process group",
                timeout.as_secs()
            )
        } else {
            anyhow::Error::new(err).context(format!("{failure_prefix}: running `{rendered}`"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bounded_as_captures_stdout() {
        let mut command = Command::new("/bin/echo");
        command.arg("bounded-ok");
        let output = run_bounded_as(&mut command, Duration::from_secs(5), "bounded test").unwrap();
        assert!(output.status.success(), "status={}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("bounded-ok"),
            "stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Linux refuses `execve` while any fd still has the file open for write.
    /// Holding that writer on another thread, then closing it mid-retry, is
    /// the race `agy --version` hits after a test overwrites a just-run binary.
    #[test]
    fn etxtbsy_from_an_open_writer_is_retried() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("busy.sh");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&script)
                .unwrap();
            file.write_all(b"#!/bin/sh\necho ready\n").unwrap();
            file.flush().unwrap();
        }
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let holder = OpenOptions::new().write(true).open(&script).unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(holder);
        });
        let mut command = Command::new(&script);
        let output = run_bounded_as(&mut command, Duration::from_secs(5), "etxtbsy test").unwrap();
        closer.join().unwrap();
        assert!(output.status.success(), "status={}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ready"),
            "stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// A hung command is killed at the timeout and surfaces as a failure that
    /// names the command — it is not a silent pass.
    #[test]
    fn a_hung_command_is_killed_and_names_the_command() {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let started = std::time::Instant::now();
        let registry = InFlight::default();
        let error = run_bounded_in(&registry, &mut command, Duration::from_millis(300), "bounded test")
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timeout did not bound the call"
        );
        assert!(error.contains("bounded test"), "{error}");
        assert!(error.contains("`/bin/sleep 60`"), "{error}");
        assert!(error.contains("did not complete within"), "{error}");
        assert!(registry.map().is_empty(), "timed-out command must leave the registry");
    }

    /// The real hang: a child wedged on a grandchild must not leave that
    /// grandchild behind.
    #[test]
    fn a_timeout_kills_the_grandchild_too() {
        let pid_file = std::env::temp_dir().join(format!("boss-bounded-grandchild-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pid_file.display()));
        run_bounded_as(&mut command, Duration::from_millis(500), "bounded test").unwrap_err();
        let grandchild: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let alive = || unsafe { libc::kill(grandchild, 0) == 0 };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "grandchild {grandchild} survived the bounded timeout");
    }

    #[test]
    fn abandoning_in_flight_commands_kills_and_reports_them() {
        let mut command = Command::new("/bin/sleep");
        command.arg("61");
        let registry = std::sync::Arc::new(InFlight::default());
        let runner = {
            let registry = registry.clone();
            std::thread::spawn(move || {
                run_bounded_in(&registry, &mut command, Duration::from_secs(120), "bounded test")
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while registry.map().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let abandoned = registry.abandon();
        assert!(abandoned.iter().any(|c| c == "/bin/sleep 61"), "{abandoned:?}");
        let output = runner.join().unwrap().unwrap();
        assert!(!output.status.success());
    }

    #[test]
    fn run_bounded_as_drains_large_stdout() {
        // ~200KiB exceeds the typical 64KiB pipe buffer; without concurrent drain
        // this would deadlock the wait. Prefer pure shell so the Bazel sandbox
        // need not grant /dev/zero or PATH-resolved helpers.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            // 4000 * 50 = 200_000 bytes of stdout.
            "i=0; while [ \"$i\" -lt 4000 ]; do printf '%050d' \"$i\"; i=$((i + 1)); done",
        ]);
        let output = run_bounded_as(&mut command, Duration::from_secs(30), "bounded test").unwrap();
        assert!(
            output.status.success(),
            "status={} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.len() > 100_000,
            "expected large captured stdout, got {} bytes",
            output.stdout.len()
        );
    }
}
