//! The real [`Host`]: the operating system's account tools and service manager.
//!
//! These commands change the machine they run on, so nothing in the test suite
//! constructs a [`System`]; the installer is tested against a recording host.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

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
            succeed("launchctl", &["kickstart", "-k", &format!("system/{LAUNCHD_LABEL}")])
        } else {
            succeed("systemctl", &["restart", SYSTEMD_UNIT])
        }
    }

    fn stop_service(&self) -> Result<()> {
        if self.platform.is_macos() {
            succeed("launchctl", &["kill", "SIGTERM", &format!("system/{LAUNCHD_LABEL}")])
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
pub struct Staged;

impl Host for Staged {
    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        Ok(Some((1, 0)))
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
        Ok(())
    }

    fn stop_service(&self) -> Result<()> {
        Ok(())
    }

    fn service_running(&self) -> Result<bool> {
        Ok(true)
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
}
