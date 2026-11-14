//! Process environment for a scoped Antigravity (`agy`) worker `HOME`.
//!
//! `agy` has no `GROK_AUTH_PATH`-style override. The OAuth token lives in the
//! login keychain and is reached through the shared filesystem bridge in
//! [`crate::scoped_home`]. Host-tool identity (`gh`, Cube, jj, git, ssh) and
//! Bazel cache locations are delegated the same way. The vendor CLI is the
//! only process that may touch the keychain item.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::EnvDirective;
use crate::scoped_home::{HostToolPaths, worker_tool_path_directive};

/// Override for the Boss-owned `agy` executable. Tests and operators pin a
/// specific binary; production copies a qualified host binary into the
/// homes-root `bin/` directory the updater cannot write.
pub const AGY_BIN_ENV: &str = "BOSS_AGY_BIN";

/// When set to `1`/`true`, skip live identity checks (`codesign`, `--version`).
/// Production never sets this; layout tests do.
pub const AGY_SKIP_BINARY_ASSERT_ENV: &str = "BOSS_AGY_SKIP_BINARY_ASSERT";

/// Minimum qualified `agy` version. `auto_approve` was measured on 1.5.7.
pub const MIN_QUALIFIED_AGY_VERSION: &str = "1.5.7";

/// Vendor codesign identity for the macOS `agy` CLI.
pub const AGY_CODESIGN_IDENTIFIER: &str = "cli";
/// Google LLC team id on every measured `agy` binary.
pub const AGY_CODESIGN_TEAM: &str = "EQHXZ8M8AV";

const DISABLE_AUTO_UPDATE_ENV: &str = "AGY_CLI_DISABLE_AUTO_UPDATE";
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
    match std::env::var(AGY_SKIP_BINARY_ASSERT_ENV) {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Locate the Boss-owned `agy` executable without copying one.
///
/// Spawn uses this so a missing binary fails closed instead of installing
/// as a side effect of building the pane command.
pub fn peek_agy_binary(homes_root: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var(AGY_BIN_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let owned = homes_root.join("bin").join("agy");
    if owned.is_file() || skip_binary_assert() {
        return Ok(owned);
    }
    bail!(
        "Boss-owned agy binary {} is missing; provision the workspace first or set {AGY_BIN_ENV}",
        owned.display()
    )
}

/// Resolve the Boss-owned `agy` executable, copying a host binary into a
/// directory the updater cannot write when none is pinned yet.
pub fn resolve_agy_binary(homes_root: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var(AGY_BIN_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            let pinned = PathBuf::from(path);
            verify_agy_binary(&pinned)?;
            return Ok(pinned);
        }
    }
    let owned = homes_root.join("bin").join("agy");
    if owned.is_file() {
        verify_agy_binary(&owned)?;
        return Ok(owned);
    }
    if skip_binary_assert() {
        return Ok(owned);
    }
    let host = find_host_agy().context(
        "no Boss-owned agy binary and none on PATH; set BOSS_AGY_BIN to a qualified \
         Antigravity CLI (1.5.7 or later) that Boss can copy into its homes-root bin directory",
    )?;
    install_boss_owned_agy(&host, &owned)?;
    verify_agy_binary(&owned)?;
    Ok(owned)
}

fn find_host_agy() -> anyhow::Result<PathBuf> {
    let output = Command::new("which")
        .arg("agy")
        .output()
        .context("resolving host agy via which")?;
    if !output.status.success() {
        bail!("`which agy` failed; install Antigravity CLI or set {AGY_BIN_ENV}");
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        bail!("`which agy` returned empty; set {AGY_BIN_ENV}");
    }
    Ok(PathBuf::from(path))
}

fn install_boss_owned_agy(host: &Path, owned: &Path) -> anyhow::Result<()> {
    let bin_dir = owned.parent().context("agy bin directory")?;
    if bin_dir.exists() {
        let mut perms = fs::metadata(bin_dir)
            .with_context(|| format!("stat {}", bin_dir.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(bin_dir, perms)
            .with_context(|| format!("making {} writable to install agy", bin_dir.display()))?;
    } else {
        fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    }
    fs::copy(host, owned)
        .with_context(|| format!("copying host agy {} to Boss-owned {}", host.display(), owned.display()))?;
    let mut file_perms = fs::metadata(owned)?.permissions();
    file_perms.set_mode(0o555);
    fs::set_permissions(owned, file_perms)?;
    let mut dir_perms = fs::metadata(bin_dir)?.permissions();
    dir_perms.set_mode(0o555);
    fs::set_permissions(bin_dir, dir_perms)
        .with_context(|| format!("locking {} against the agy updater", bin_dir.display()))?;
    Ok(())
}

/// Fail closed when `path` is not a qualified Boss-owned `agy` executable.
///
/// On macOS this checks the vendor codesign identity (`cli` / Google team)
/// via `/usr/bin/codesign`, which is not a keychain API. Version is read
/// from `agy --version`. Tests skip both checks with
/// [`AGY_SKIP_BINARY_ASSERT_ENV`].
pub fn verify_agy_binary(path: &Path) -> anyhow::Result<()> {
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
    #[cfg(target_os = "macos")]
    verify_agy_codesign(path)?;
    let version = read_agy_version(path)?;
    if !version_at_least(&version, MIN_QUALIFIED_AGY_VERSION) {
        bail!(
            "agy binary {} reports version {version}, below the qualified minimum {}",
            path.display(),
            MIN_QUALIFIED_AGY_VERSION
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_agy_codesign(path: &Path) -> anyhow::Result<()> {
    let output = Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(path)
        .output()
        .context("running /usr/bin/codesign to verify the Boss-owned agy identity")?;
    // codesign writes identity details to stderr.
    let details = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!("codesign failed for {}: {}", path.display(), details.trim());
    }
    let identifier = codesign_field(&details, "Identifier");
    let team = codesign_field(&details, "TeamIdentifier");
    if identifier != Some(AGY_CODESIGN_IDENTIFIER) || team != Some(AGY_CODESIGN_TEAM) {
        bail!(
            "agy binary {} is not the vendor-signed Antigravity CLI \
             (Identifier={}, TeamIdentifier={}; expected {AGY_CODESIGN_IDENTIFIER} / {AGY_CODESIGN_TEAM})",
            path.display(),
            identifier.unwrap_or("<missing>"),
            team.unwrap_or("<missing>")
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_field<'a>(details: &'a str, key: &str) -> Option<&'a str> {
    details.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(&format!("{key}="))
    })
}

fn read_agy_version(path: &Path) -> anyhow::Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", path.display()))?;
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

fn parse_agy_version(raw: &str) -> anyhow::Result<String> {
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

fn version_at_least(observed: &str, minimum: &str) -> bool {
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
mod tests {
    use super::*;
    use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let lock = SCOPED_HOME_ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut prior = Vec::new();
            for (key, value) in pairs {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: serialised by SCOPED_HOME_ENV_TEST_LOCK.
                unsafe { std::env::set_var(key, value) };
            }
            Self { _lock: lock, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.prior {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[test]
    fn disable_auto_update_value_is_the_measured_true_literal() {
        assert_eq!(DISABLE_AUTO_UPDATE_VALUE, "true");
        assert_ne!(DISABLE_AUTO_UPDATE_VALUE, "1");
    }

    #[test]
    fn version_gate_accepts_qualified_releases() {
        assert!(version_at_least("1.5.7", MIN_QUALIFIED_AGY_VERSION));
        assert!(version_at_least("1.5.9", MIN_QUALIFIED_AGY_VERSION));
        assert!(version_at_least("1.6.0", MIN_QUALIFIED_AGY_VERSION));
        assert!(!version_at_least("1.5.5", MIN_QUALIFIED_AGY_VERSION));
        assert!(!version_at_least("1.1.12", MIN_QUALIFIED_AGY_VERSION));
    }

    #[test]
    fn parse_version_from_plain_and_prefixed_output() {
        assert_eq!(parse_agy_version("1.5.9\n").unwrap(), "1.5.9");
        assert_eq!(parse_agy_version("agy 1.5.9\n").unwrap(), "1.5.9");
    }

    #[test]
    fn directives_scope_home_and_disable_the_updater() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        fs::create_dir_all(&host_home).unwrap();
        let _guard = EnvGuard::set(&[
            ("HOME", host_home.to_str().unwrap()),
            ("BOSS_WORKER_TOOL_PATH", "/opt/boss-worker-tools"),
        ]);
        let env = AgyProcessEnvironment::resolve(
            Path::new("/run/process-home"),
            Path::new("/run/log/agy.log"),
            Path::new("/run/control/session-settings.json"),
            PathBuf::from("/run/bin/agy"),
        )
        .unwrap();
        assert_eq!(env.log_file(), Path::new("/run/log/agy.log"));
        assert_eq!(
            env.session_settings_path(),
            Path::new("/run/control/session-settings.json")
        );
        let directives = env.directives();
        assert!(
            directives
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == "HOME" && v == "/run/process-home"))
        );
        assert!(
            directives
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == DISABLE_AUTO_UPDATE_ENV && v == "true"))
        );
        assert!(
            directives
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == HIDE_ACCOUNT_INFO_ENV && v == "true"))
        );
        assert!(directives.iter().any(
            |d| matches!(d, EnvDirective::Set(k, v) if k == "BOSS_WORKER_TOOL_PATH" && v == "/opt/boss-worker-tools")
        ));
    }

    #[test]
    fn skip_binary_assert_accepts_a_missing_file() {
        let _guard = EnvGuard::set(&[(AGY_SKIP_BINARY_ASSERT_ENV, "1")]);
        verify_agy_binary(Path::new("/definitely/missing/agy")).unwrap();
    }
}
