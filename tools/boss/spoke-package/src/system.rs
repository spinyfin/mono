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
use crate::service::{LAUNCHD_LABEL, SYSTEMD_UNIT};

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
            bail!(
                "the service user {name} does not exist. Create the account first (for example with \
                 `sysadminctl -addUser {name} -home {} -shell /bin/zsh`), then run the installer again.",
                home.display()
            );
        }
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
        self.lookup_user(name)?.context("the service user was not created")
    }

    fn register_service(&self, definition: &Path) -> Result<()> {
        if self.platform.is_macos() {
            // Not loaded yet is the normal first-install case.
            let _ = run("launchctl", &["bootout", &format!("system/{LAUNCHD_LABEL}")]);
            succeed("launchctl", &["bootstrap", "system", &definition.display().to_string()])
        } else {
            succeed("systemctl", &["daemon-reload"])?;
            succeed("systemctl", &["enable", SYSTEMD_UNIT])
        }
    }

    fn restart_service(&self) -> Result<()> {
        if self.platform.is_macos() {
            succeed("launchctl", &["kickstart", "-k", &format!("system/{LAUNCHD_LABEL}")])
        } else {
            succeed("systemctl", &["restart", SYSTEMD_UNIT])
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

    fn restart_service(&self) -> Result<()> {
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
    let output = command
        .output()
        .with_context(|| format!("cannot run {}", program.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(RunOutput {
        success: output.status.success(),
        output: text,
    })
}
