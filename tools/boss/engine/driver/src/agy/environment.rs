//! Process environment for a scoped Antigravity (`agy`) worker `HOME`.
//!
//! `agy` has no `GROK_AUTH_PATH`-style override. The OAuth token lives in the
//! login keychain and is reached through the shared filesystem bridge in
//! [`crate::scoped_home`]. Host-tool identity (`gh`, Cube, jj, git, ssh) and
//! Bazel cache locations are delegated the same way. The vendor CLI is the
//! only process that may touch the keychain item.

use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::EnvDirective;
use crate::scoped_home::{HostToolPaths, worker_tool_path_directive};

/// Installation source for the Boss-owned `agy` executable. Tests and deployments select a
/// specific binary; production copies a qualified host binary into the
/// homes-root `bin/` directory the updater cannot write. The first installation
/// pins its bytes; later overrides must match the pinned copy.
pub const AGY_BIN_ENV: &str = "BOSS_AGY_BIN";

/// Host `agy` could not be resolved: `which agy` failed, returned empty, or
/// the [`AGY_BIN_ENV`] path is not a file. The engine maps this to the quota
/// panel's NotInstalled row by downcasting, not by matching the message.
#[derive(Debug)]
pub struct AgyNotInstalled(pub String);

impl std::fmt::Display for AgyNotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AgyNotInstalled {}

/// When set to `1`/`true`, skip live identity checks (`codesign`, `--version`).
/// Production never sets this; layout tests do. It does not skip the `models`
/// posture probe, which then runs whatever is at the Boss-owned binary path;
/// `BOSS_AGY_SKIP_POSTURE_ASSERT` is the switch for that.
pub const AGY_SKIP_BINARY_ASSERT_ENV: &str = "BOSS_AGY_SKIP_BINARY_ASSERT";

/// Minimum qualified `agy` version. `auto_approve` was measured on 1.5.7.
pub const MIN_QUALIFIED_AGY_VERSION: &str = "1.5.7";

/// Vendor codesign identity for the macOS `agy` CLI.
#[cfg(any(target_os = "macos", test))]
pub const AGY_CODESIGN_IDENTIFIER: &str = "cli";
/// Google LLC team id on every measured `agy` binary.
#[cfg(any(target_os = "macos", test))]
pub const AGY_CODESIGN_TEAM: &str = "EQHXZ8M8AV";

/// Wall-clock limit for the short local probes (`agy --version`, `codesign`).
pub const AGY_VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Wall-clock limit for `agy models`, which may reach the network.
pub(crate) const AGY_MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run a vendor subprocess with a timeout, killing its process group on expiry.
/// Shares the crate-wide in-flight registry so engine shutdown reaps it too.
/// A timeout is an error naming the command.
pub fn run_agy_bounded(command: &mut Command, timeout: std::time::Duration) -> anyhow::Result<std::process::Output> {
    crate::bounded_command::run_bounded_as(command, timeout, "Antigravity provisioning failed")
}

pub const DISABLE_AUTO_UPDATE_ENV: &str = "AGY_CLI_DISABLE_AUTO_UPDATE";
const HIDE_ACCOUNT_INFO_ENV: &str = "AGY_CLI_HIDE_ACCOUNT_INFO";

/// Exact value that disables the updater. `=1` is ignored by `agy`.
pub const DISABLE_AUTO_UPDATE_VALUE: &str = "true";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyProcessEnvironment {
    process_home: PathBuf,
    host_tools: HostToolPaths,
    agy_binary: PathBuf,
    log_file: PathBuf,
    session_settings_path: PathBuf,
}

impl AgyProcessEnvironment {
    pub fn resolve(
        process_home: &Path,
        log_file: &Path,
        session_settings_path: &Path,
        agy_binary: PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            process_home: process_home.to_path_buf(),
            host_tools: HostToolPaths::resolve().context("resolving host tool paths for scoped agy HOME")?,
            agy_binary,
            log_file: log_file.to_path_buf(),
            session_settings_path: session_settings_path.to_path_buf(),
        })
    }

    pub fn process_home(&self) -> &Path {
        &self.process_home
    }

    pub fn agy_binary(&self) -> &Path {
        &self.agy_binary
    }

    pub fn log_file(&self) -> &Path {
        &self.log_file
    }

    pub fn session_settings_path(&self) -> &Path {
        &self.session_settings_path
    }

    pub fn provision_home_delegates(&self) -> anyhow::Result<()> {
        self.host_tools.provision_into(&self.process_home)
    }

    /// Arguments shared by model probing and the pane launch composer.
    ///
    /// The rules root is added to the CLI workspace only for a run that has
    /// one: see [`super::home::rules_add_dir`], which reads the working
    /// directory contract stamped when the home was provisioned.
    pub fn launch_args(&self) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec!["--log-file".into(), self.log_file.as_os_str().to_owned()];
        if let Some(rules_root) = super::home::rules_add_dir(&self.process_home) {
            args.push("--add-dir".into());
            args.push(rules_root.into_os_string());
        }
        args
    }

    pub fn directives(&self) -> Vec<EnvDirective> {
        let mut directives = vec![
            EnvDirective::Set("HOME".to_owned(), self.process_home.display().to_string()),
            EnvDirective::Set(DISABLE_AUTO_UPDATE_ENV.to_owned(), DISABLE_AUTO_UPDATE_VALUE.to_owned()),
            EnvDirective::Set(HIDE_ACCOUNT_INFO_ENV.to_owned(), "true".to_owned()),
        ];
        directives.extend(self.host_tools.env_directives());
        if let Some(tool_path) = worker_tool_path_directive() {
            directives.push(tool_path);
        }
        directives
    }

    pub fn apply_to_command(&self, command: &mut Command) {
        for directive in self.directives() {
            match directive {
                EnvDirective::Set(key, value) => {
                    command.env(key, value);
                }
                EnvDirective::Unset(key) => {
                    command.env_remove(key);
                }
            }
        }
    }
}

pub fn skip_binary_assert() -> bool {
    crate::scoped_home::env_flag(AGY_SKIP_BINARY_ASSERT_ENV)
}

/// Locate the Boss-owned `agy` executable without copying one.
///
/// Spawn uses this so a missing binary fails closed instead of installing
/// as a side effect of building the pane command.
pub fn peek_agy_binary(homes_root: &Path) -> anyhow::Result<PathBuf> {
    let owned = homes_root.join("bin").join("agy");
    if owned.is_file() || skip_binary_assert() {
        return Ok(owned);
    }
    bail!(
        "Boss-owned agy binary {} is missing; provision the workspace first",
        owned.display()
    )
}

/// Resolve the Boss-owned `agy` executable, copying a host binary into a
/// directory the updater cannot write when none is pinned yet.
pub fn resolve_agy_binary(homes_root: &Path) -> anyhow::Result<PathBuf> {
    if skip_binary_assert() {
        return Ok(homes_root.join("bin/agy"));
    }
    resolve_agy_binary_with(homes_root, verify_agy_binary)
}

fn resolve_agy_binary_with(
    homes_root: &Path,
    qualify: impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let owned = homes_root.join("bin/agy");
    fs::create_dir_all(homes_root)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(homes_root.join("install.lock"))?;
    lock.lock().context("locking agy installation")?;
    if !owned.is_file() {
        let host = crate::scoped_home::nonempty_env(AGY_BIN_ENV)
            .map(Ok)
            .unwrap_or_else(find_host_agy)?;
        if !host.is_file() {
            return Err(AgyNotInstalled(format!(
                "agy CLI is not installed ({}); install Antigravity CLI or set {AGY_BIN_ENV}",
                host.display()
            ))
            .into());
        }
        install_boss_owned_agy(&host, &owned, &qualify)?;
        assert_protected_binary(&owned)?;
        QUALIFIED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(owned.clone(), binary_identity(&owned)?);
        return Ok(owned);
    }
    assert_protected_binary(&owned)?;
    if let Some(source) = crate::scoped_home::nonempty_env(AGY_BIN_ENV)
        && fs::read(&source).context("reading agy installation override")? != fs::read(&owned)?
    {
        bail!("{AGY_BIN_ENV} override differs from pinned copy {}", owned.display());
    }
    qualify(&owned)?;
    Ok(owned)
}

fn find_host_agy() -> anyhow::Result<PathBuf> {
    let output = Command::new("which")
        .arg("agy")
        .output()
        .context("resolving host agy via which")?;
    if !output.status.success() {
        return Err(AgyNotInstalled(format!(
            "`which agy` failed; install Antigravity CLI or set {AGY_BIN_ENV}"
        ))
        .into());
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(AgyNotInstalled(format!("`which agy` returned empty; set {AGY_BIN_ENV}")).into());
    }
    Ok(PathBuf::from(path))
}

fn install_boss_owned_agy(
    host: &Path,
    owned: &Path,
    qualify: impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let bin_dir = owned.parent().context("agy bin directory")?;
    let temporary = bin_dir.join(format!(
        ".agy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    if bin_dir.is_symlink() {
        bail!("Boss-owned agy bin directory is a symlink");
    }
    fs::create_dir_all(bin_dir)?;
    fs::set_permissions(bin_dir, fs::Permissions::from_mode(0o755))?;
    let result = (|| {
        fs::copy(host, &temporary)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o555))?;
        qualify(&temporary)?;
        fs::rename(&temporary, owned)?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    let cleanup = fs::set_permissions(bin_dir, fs::Permissions::from_mode(0o555));
    cleanup.context("restoring protected agy bin permissions")?;
    result
}

fn assert_protected_binary(path: &Path) -> anyhow::Result<()> {
    for entry in [path, path.parent().context("agy binary parent")?] {
        let meta = fs::symlink_metadata(entry)?;
        if meta.file_type().is_symlink() || meta.mode() & 0o222 != 0 {
            bail!("agy binary or bin directory {} is writable or aliased", entry.display());
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
struct BinaryIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
}

fn binary_identity(path: &Path) -> anyhow::Result<BinaryIdentity> {
    let m = fs::symlink_metadata(path)?;
    Ok(BinaryIdentity {
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
        mode: m.mode(),
    })
}

static QUALIFIED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, BinaryIdentity>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Spawn performs only filesystem reads; vendor subprocesses run during blocking provisioning.
pub fn verify_cached_agy_binary(path: &Path) -> anyhow::Result<()> {
    assert_protected_binary(path)?;
    let identity = binary_identity(path)?;
    if QUALIFIED.lock().unwrap_or_else(|p| p.into_inner()).get(path) != Some(&identity) {
        bail!("agy binary identity changed or was not qualified during provisioning");
    }
    Ok(())
}

/// Fail closed when `path` is not a qualified Boss-owned `agy` executable.
///
/// On macOS this checks the vendor codesign identity (`cli` / Google team)
/// via `/usr/bin/codesign`, which is not a keychain API. Version is read
/// from `agy --version`. Tests skip both checks with
/// [`AGY_SKIP_BINARY_ASSERT_ENV`].
pub fn verify_agy_binary(path: &Path) -> anyhow::Result<()> {
    verify_agy_binary_with(path, |path| {
        #[cfg(target_os = "macos")]
        verify_agy_codesign(path)?;
        #[cfg(not(target_os = "macos"))]
        let _ = path;
        Ok(())
    })
}

pub(super) fn verify_agy_binary_with(
    path: &Path,
    signature: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if skip_binary_assert() {
        return Ok(());
    }
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "agy binary {} does not exist; provision a Boss-owned copy or set {AGY_BIN_ENV}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!("agy binary {} is not a regular file", path.display());
    }
    if metadata.mode() & 0o111 == 0 {
        bail!("agy binary {} is not executable", path.display());
    }
    let identity = binary_identity(path)?;
    signature(path)?;
    let version = read_agy_version(path)?;
    if !version_at_least(&version, MIN_QUALIFIED_AGY_VERSION) {
        bail!(
            "agy binary {} reports version {version}, below the qualified minimum {}",
            path.display(),
            MIN_QUALIFIED_AGY_VERSION
        );
    }
    if binary_identity(path)? != identity {
        bail!("agy binary changed during qualification");
    }
    QUALIFIED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(path.to_path_buf(), identity);
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_agy_codesign(path: &Path) -> anyhow::Result<()> {
    verify_codesign_with(path, |args| {
        let mut command = Command::new("/usr/bin/codesign");
        command.args(args).arg(path);
        run_agy_bounded(&mut command, AGY_VERSION_TIMEOUT).map_err(|err| std::io::Error::other(format!("{err:#}")))
    })
}

#[cfg(any(target_os = "macos", test))]
fn verify_codesign_with(
    path: &Path,
    mut run: impl FnMut(&[&str]) -> std::io::Result<std::process::Output>,
) -> anyhow::Result<()> {
    let requirement = format!(
        "anchor apple generic and identifier \"{AGY_CODESIGN_IDENTIFIER}\" and certificate leaf[subject.OU] = \"{AGY_CODESIGN_TEAM}\""
    );
    let requirement = codesign_literal_requirement(&requirement);
    let verified = run(&codesign_verify_args(&requirement))?;
    if !verified.status.success() {
        bail!(
            "agy signature verification failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&verified.stderr)
        );
    }
    let output = run(&["-dv", "--verbose=4"])?;
    let details = String::from_utf8_lossy(&output.stderr);
    if !output.status.success()
        || codesign_field(&details, "Identifier") != Some(AGY_CODESIGN_IDENTIFIER)
        || codesign_field(&details, "TeamIdentifier") != Some(AGY_CODESIGN_TEAM)
    {
        bail!("agy binary {} has wrong vendor identity: {details}", path.display());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn codesign_literal_requirement(source: &str) -> String {
    format!("={source}")
}

#[cfg(any(target_os = "macos", test))]
fn codesign_verify_args(requirement: &str) -> [&str; 4] {
    ["--verify", "--strict", "-R", requirement]
}

#[cfg(any(target_os = "macos", test))]
fn codesign_field<'a>(details: &'a str, key: &str) -> Option<&'a str> {
    details.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(&format!("{key}="))
    })
}

fn read_agy_version(path: &Path) -> anyhow::Result<String> {
    let probe_home = path
        .parent()
        .and_then(Path::parent)
        .context("agy version probe directory")?
        .join(format!(
            ".version-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
    fs::create_dir(&probe_home)?;
    let mut command = Command::new(path);
    command
        .env("HOME", &probe_home)
        .env(DISABLE_AUTO_UPDATE_ENV, DISABLE_AUTO_UPDATE_VALUE)
        .env(HIDE_ACCOUNT_INFO_ENV, "true")
        .arg("--version");
    let output = run_agy_bounded(&mut command, AGY_VERSION_TIMEOUT);
    let cleanup = fs::remove_dir_all(&probe_home);
    let output = output.with_context(|| format!("running {} --version", path.display()))?;
    cleanup.context("reclaiming agy version-probe HOME")?;
    if !output.status.success() {
        bail!(
            "{} --version failed (status {}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    parse_agy_version(&raw).with_context(|| format!("parsing agy version from {raw:?}"))
}

pub fn parse_agy_version(raw: &str) -> anyhow::Result<String> {
    let token = raw
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(raw.trim());
    let version = token.trim().trim_start_matches('v');
    if version.is_empty() {
        bail!("empty agy version string");
    }
    Ok(version.to_owned())
}

pub fn version_at_least(observed: &str, minimum: &str) -> bool {
    match (parse_semver_prefix(observed), parse_semver_prefix(minimum)) {
        (Some(obs), Some(min)) => obs >= min,
        _ => false,
    }
}

fn parse_semver_prefix(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
