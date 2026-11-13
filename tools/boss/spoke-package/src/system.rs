//! The real [`Host`]: the operating system's account tools and service manager.
//!
//! These commands change the machine they run on, so nothing in the test suite
//! constructs a [`System`]; the installer is tested against a recording host.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::install::{Account, Host, RunOutput};
use crate::manifest::Platform;
use crate::service::{LAUNCHD_LABEL, LAUNCHD_UPDATE_LABEL, SYSTEMD_UNIT, SYSTEMD_UPDATE_PATH, SYSTEMD_UPDATE_SERVICE};

pub struct System {
    pub platform: Platform,
}

fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {program}"))
}

fn succeed(program: &str, args: &[&str]) -> Result<()> {
    let output = run(program, args)?;
    ensure!(
        output.status.success(),
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn id_of(flag: &str, name: &str) -> Result<Option<u32>> {
    let output = run("id", &[flag, name])?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().parse())
        .transpose()?)
}

impl Host for System {
    fn lookup_user(&self, name: &str) -> Result<Option<Account>> {
        Ok(id_of("-u", name)?
            .zip(id_of("-g", name)?)
            .map(|(uid, gid)| Account { uid, gid }))
    }

    fn create_user(&self, name: &str, home: &Path) -> Result<Account> {
        if self.platform.is_macos() {
            // Hidden, non-login service account with a free system UID.
            let listed = run("dscl", &[".", "-list", "/Users", "UniqueID"])?;
            ensure!(listed.status.success(), "cannot enumerate macOS account IDs");
            let uid = free_service_uid(&String::from_utf8(listed.stdout)?)?;
            let record = format!("/Users/{name}");
            succeed("dscl", &[".", "-create", &record])?;
            for (attribute, value) in [
                ("UniqueID", uid.to_string()),
                ("PrimaryGroupID", "20".into()),
                ("UserShell", "/usr/bin/false".into()),
                ("NFSHomeDirectory", home.display().to_string()),
                ("RealName", "Boss spoke worker".into()),
                ("Password", "*".into()),
                ("IsHidden", "1".into()),
            ] {
                succeed("dscl", &[".", "-create", &record, attribute, &value])?;
            }
        } else {
            succeed(
                "useradd",
                &[
                    "--system",
                    "--create-home",
                    "--home-dir",
                    &home.display().to_string(),
                    "--shell",
                    "/bin/bash",
                    "--user-group",
                    name,
                ],
            )?;
        }
        self.lookup_user(name)?.context("the service user was not created")
    }

    fn register_service(&self, definition: &Path) -> Result<()> {
        if self.platform.is_macos() {
            // Not loaded yet is the normal first-install case.
            let _ = run("launchctl", &["bootout", &format!("system/{LAUNCHD_LABEL}")]);
            succeed("launchctl", &["bootstrap", "system", &definition.display().to_string()])?;
            let helper = definition.with_file_name(format!("{LAUNCHD_UPDATE_LABEL}.plist"));
            if helper.is_file() {
                let _ = run("launchctl", &["bootout", &format!("system/{LAUNCHD_UPDATE_LABEL}")]);
                succeed("launchctl", &["bootstrap", "system", &helper.display().to_string()])?;
            }
            Ok(())
        } else {
            succeed("systemctl", &["daemon-reload"])?;
            succeed("systemctl", &["enable", SYSTEMD_UNIT])?;
            succeed("systemctl", &["enable", SYSTEMD_UPDATE_SERVICE])?;
            succeed("systemctl", &["enable", "--now", SYSTEMD_UPDATE_PATH])
        }
    }

    fn service_registered(&self) -> Result<bool> {
        if self.platform.is_macos() {
            Ok(run("launchctl", &["print", &format!("system/{LAUNCHD_LABEL}")])?
                .status
                .success())
        } else {
            Ok(run("systemctl", &["is-enabled", "--quiet", SYSTEMD_UNIT])?
                .status
                .success())
        }
    }

    fn restart_service(&self) -> Result<()> {
        if self.platform.is_macos() {
            // `stop_service` boots the job out so launchd cannot relaunch it
            // under KeepAlive; put it back before starting it.
            let target = format!("system/{LAUNCHD_LABEL}");
            if !run("launchctl", &["print", &target])?.status.success() {
                let definition = format!("/Library/LaunchDaemons/{LAUNCHD_LABEL}.plist");
                succeed("launchctl", &["bootstrap", "system", &definition])?;
            }
            succeed("launchctl", &["kickstart", "-k", &target])
        } else {
            succeed("systemctl", &["restart", SYSTEMD_UNIT])
        }
    }

    fn stop_service(&self) -> Result<()> {
        if self.platform.is_macos() {
            // `launchctl kill` leaves a KeepAlive job free to relaunch the moment
            // the daemon exits. Booting it out removes the job, so nothing can
            // start a writer until `restart_service` bootstraps it again.
            // AbandonProcessGroup keeps the tmux workers alive through it. A job
            // that is not loaded is already stopped. `bootout` can return while
            // the process is still handling SIGTERM; wait for that PID to exit
            // so the journal has no writer.
            let target = format!("system/{LAUNCHD_LABEL}");
            let printed = run("launchctl", &["print", &target])?;
            if printed.status.success() {
                let pid = parse_instance(&String::from_utf8_lossy(&printed.stdout), true).map(|(pid, _)| pid);
                bootout_and_wait_pid(
                    pid,
                    || succeed("launchctl", &["bootout", &target]),
                    Duration::from_secs(120),
                    Duration::from_millis(50),
                    pid_is_alive,
                )?;
            }
            Ok(())
        } else {
            succeed("systemctl", &["stop", SYSTEMD_UNIT])
        }
    }

    fn service_running(&self) -> Result<bool> {
        if self.platform.is_macos() {
            let output = run("launchctl", &["print", &format!("system/{LAUNCHD_LABEL}")])?;
            Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("state = running"))
        } else {
            Ok(run("systemctl", &["is-active", "--quiet", SYSTEMD_UNIT])?
                .status
                .success())
        }
    }

    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        let output = if self.platform.is_macos() {
            run("launchctl", &["print", &format!("system/{LAUNCHD_LABEL}")])?
        } else {
            run("systemctl", &["show", "-p", "MainPID", "-p", "NRestarts", SYSTEMD_UNIT])?
        };
        if !output.status.success() {
            return Ok(None);
        }
        Ok(parse_instance(
            &String::from_utf8(output.stdout)?,
            self.platform.is_macos(),
        ))
    }

    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        run_as(account, env, program, args)
    }
}

/// Stages a release into a directory (`install --root`) for inspection and
/// tests: no account is created and no service manager is touched. The "user"
/// is whoever runs it, and a service that was never started counts as healthy.
#[derive(Default)]
pub struct Staged {
    stopped: std::cell::Cell<bool>,
}

impl Host for Staged {
    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        Ok((!self.stopped.get()).then_some((1, 0)))
    }

    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Ok(Some(Account { uid, gid }))
    }

    fn create_user(&self, name: &str, _: &Path) -> Result<Account> {
        bail!("staging does not create accounts; {name} should have been found")
    }

    fn register_service(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn service_registered(&self) -> Result<bool> {
        Ok(true)
    }

    fn restart_service(&self) -> Result<()> {
        self.stopped.set(false);
        Ok(())
    }

    fn stop_service(&self) -> Result<()> {
        self.stopped.set(true);
        Ok(())
    }

    fn service_running(&self) -> Result<bool> {
        Ok(!self.stopped.get())
    }

    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        run_as(account, env, program, args)
    }
}

/// Runs `program` as `account` with exactly `env`. When the caller is not that
/// user the process switches uid and gid; the standard library drops root's
/// supplementary groups when it does.
pub fn run_as(account: &Account, env: &[(String, String)], program: &Path, args: &[String]) -> Result<RunOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir("/")
        .stdin(Stdio::null());
    // SAFETY: geteuid and getegid have no preconditions and cannot fail.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if account.uid != euid {
        command.uid(account.uid);
    }
    if account.gid != egid {
        command.gid(account.gid);
    }
    // A freshly written executable can be briefly "text file busy" when another
    // thread forked while its write descriptor was still open; that clears as
    // soon as the forked child execs, so retry for a short bounded time.
    let mut attempts = 0;
    let output = loop {
        match command.output() {
            Ok(output) => break output,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot run {}", program.display()));
            }
        }
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(RunOutput {
        success: output.status.success(),
        output: text,
    })
}

/// True while `kill(pid, 0)` does not report ESRCH.
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the pid exists.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Polls until `alive` reports the process is gone, or `deadline` elapses.
pub(crate) fn wait_pid_gone(pid: u32, deadline: Duration, poll: Duration, alive: impl Fn(u32) -> bool) -> Result<()> {
    let started = Instant::now();
    loop {
        if !alive(pid) {
            return Ok(());
        }
        if started.elapsed() >= deadline {
            bail!("stop_failed: process {pid} was still running after {deadline:?}");
        }
        std::thread::sleep(poll);
    }
}

/// Records the PID, runs `bootout`, then waits until that PID is gone. Used by
/// the macOS adapter and by tests whose fake bootout returns while the process
/// is still alive.
pub(crate) fn bootout_and_wait_pid(
    pid: Option<u32>,
    bootout: impl FnOnce() -> Result<()>,
    deadline: Duration,
    poll: Duration,
    alive: impl Fn(u32) -> bool,
) -> Result<()> {
    bootout()?;
    if let Some(pid) = pid {
        wait_pid_gone(pid, deadline, poll, alive)?;
    }
    Ok(())
}

fn parse_instance(text: &str, macos: bool) -> Option<(u32, u64)> {
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            (key.trim() == name).then_some(value.trim())
        })
    };
    let pid = value(if macos { "pid" } else { "MainPID" })?.parse().ok()?;
    let runs = value(if macos { "runs" } else { "NRestarts" })?.parse().ok()?;
    (pid != 0).then_some((pid, runs))
}

fn free_service_uid(listing: &str) -> Result<i64> {
    let used = listing
        .lines()
        .map(|line| {
            line.split_whitespace()
                .last()
                .context("missing macOS account UID")?
                .parse::<i64>()
                .context("invalid macOS account UID")
        })
        .collect::<Result<std::collections::HashSet<_>>>()?;
    (200..=400)
        .find(|uid| !used.contains(uid))
        .context("no free macOS service UID in 200-400")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_instances_require_a_pid_and_launch_count() {
        assert_eq!(parse_instance("MainPID=42\nNRestarts=3\n", false), Some((42, 3)));
        assert_eq!(parse_instance("\tpid = 42\n\truns = 4\n", true), Some((42, 4)));
        for text in ["MainPID=0\nNRestarts=3", "MainPID=42", "MainPID=bad\nNRestarts=3"] {
            assert_eq!(parse_instance(text, false), None);
        }
        assert_eq!(parse_instance("state = waiting\nruns = 2", true), None);
    }

    #[test]
    fn service_uid_skips_occupied_ids_and_accepts_negative_system_ids() {
        assert_eq!(
            free_service_uid("nobody -2\nroot 0\nexisting 200\nother 202").unwrap(),
            201
        );
        assert!(free_service_uid("broken not-a-number").is_err());
        let full = (200..=400)
            .map(|id| format!("user{id} {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(free_service_uid(&full).is_err());
    }

    #[test]
    fn bootout_waits_for_the_recorded_pid_after_the_job_is_unloaded() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let polls = AtomicU32::new(0);
        bootout_and_wait_pid(
            Some(4242),
            || Ok(()),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {
                let n = polls.fetch_add(1, Ordering::SeqCst);
                n < 3
            },
        )
        .unwrap();
        assert!(
            polls.load(Ordering::SeqCst) >= 4,
            "must poll until ESRCH, not return at bootout"
        );
    }

    #[test]
    fn a_pid_that_never_exits_fails_the_quiesce() {
        let error = bootout_and_wait_pid(
            Some(7),
            || Ok(()),
            Duration::from_millis(20),
            Duration::from_millis(1),
            |_| true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("stop_failed"), "{error:#}");
    }
}
