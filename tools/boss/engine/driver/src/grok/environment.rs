//! Exact process environment shared by Grok provisioning probes and pane spawn.
//!
//! Grok needs a scoped process `HOME` to quarantine user-global Claude
//! permission settings, but worker tools still need the operator's Cube, gh,
//! jj, and git state. This module delegates those stores through both each
//! tool's native environment variable and symlinks at its scoped-HOME default
//! path. The filesystem form is load-bearing: Grok's tool sandbox strips
//! several parent-process environment variables. Grok OAuth is likewise
//! delegated to one shared credential path so every concurrent worker also
//! shares the CLI-managed auth lock.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fs4::fs_std::FileExt;

use crate::EnvDirective;

/// Grok's OAuth refresh lock lives next to the credential (`auth.json.lock`).
/// Shared flock on this file waits out in-flight refreshes and blocks a new
/// exclusive refresh from starting while Boss probes `grok models`.
const AUTH_LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH_LOCK_POLL: Duration = Duration::from_millis(50);

pub const GROK_AUTH_PATH_ENV: &str = "GROK_AUTH_PATH";
const GROK_DISABLE_API_KEY_AUTH_ENV: &str = "GROK_DISABLE_API_KEY_AUTH";
const GROK_AMBIENT_AUTH_ENV: &str = "GROK_AUTH";
const XAI_API_KEY_ENV: &str = "XAI_API_KEY";

const GH_DEFAULT_CONFIG_LEAF: &str = ".config/gh";
const CUBE_DEFAULT_DATA_LEAF: &str = ".local/share/cube";
const CUBE_DEFAULT_CONFIG_LEAF: &str = ".config/cube";
const LOGIN_KEYCHAIN_LEAF: &str = "Library/Keychains/login.keychain-db";

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
struct HostToolPaths {
    gh_config_dir: PathBuf,
    cube_data_dir: PathBuf,
    cube_config_dir: PathBuf,
    login_keychain: Option<PathBuf>,
    vcs: HostVcsPaths,
    caches: HostCachePaths,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostVcsPaths {
    jj_config: Option<PathBuf>,
    git_config_global: Option<PathBuf>,
    ssh_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostCachePaths {
    xdg_cache_home: PathBuf,
    repobin_cache_dir: PathBuf,
    bazelisk_home: PathBuf,
    bazel_output_root: PathBuf,
}

/// One resolved environment contract, applied identically to live preflight
/// commands and the worker pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokProcessEnvironment {
    grok_home: PathBuf,
    process_home: PathBuf,
    auth_path: PathBuf,
    host_tools: HostToolPaths,
}

impl GrokProcessEnvironment {
    pub fn resolve(grok_home: &Path, process_home: &Path, auth_path: &Path) -> anyhow::Result<Self> {
        let host_tools = HostToolPaths::builder()
            .gh_config_dir(resolve_gh_config_dir().context(
                "resolving host gh config for scoped Grok HOME; set GH_CONFIG_DIR or ensure HOME is available",
            )?)
            .cube_data_dir(resolve_cube_data_dir().context(
                "resolving host Cube data for scoped Grok HOME; set CUBE_DATA_DIR or ensure HOME is available",
            )?)
            .cube_config_dir(resolve_cube_config_dir().context(
                "resolving host Cube config for scoped Grok HOME; set CUBE_CONFIG_DIR or ensure HOME is available",
            )?)
            .maybe_login_keychain(resolve_login_keychain_source())
            .vcs(HostVcsPaths {
                jj_config: resolve_jj_config(),
                git_config_global: resolve_git_config_global(),
                ssh_dir: resolve_ssh_dir(),
            })
            .caches(
                resolve_host_cache_paths()
                    .context("resolving host tool caches for scoped Grok HOME; ensure HOME is available")?,
            )
            .build();
        Ok(Self {
            grok_home: grok_home.to_path_buf(),
            process_home: process_home.to_path_buf(),
            auth_path: auth_path.to_path_buf(),
            host_tools,
        })
    }

    /// The per-run `GROK_HOME` this environment points Grok at. Preflight
    /// needs it to probe the session store that lives under it.
    pub fn grok_home(&self) -> &Path {
        &self.grok_home
    }

    /// Acquire Grok's `auth.json.lock` in shared mode and re-verify the
    /// credential is a non-empty JSON object under that lock.
    ///
    /// Grok refreshes OAuth by taking an exclusive flock on
    /// `{GROK_AUTH_PATH}.lock` and rewriting `auth.json`. That rewrite is not
    /// always atomic: a concurrent `grok models` that opens the path during
    /// the empty window prints `You are not authenticated` even though login
    /// is healthy (reproduced with a truncate-then-rewrite mutator). Holding
    /// a **shared** flock for the OAuth preflight:
    /// - waits if a refresh already holds exclusive
    /// - prevents a new exclusive refresh from starting mid-probe
    /// - re-checks the file under the lock so we never probe an empty body
    ///
    /// This is lock coordination with Grok's own protocol, not a
    /// retry-until-pass loop on the preflight itself. Genuine logout / cleared
    /// credentials still fail closed once the lock is held.
    pub fn lock_auth_for_oauth_probe(&self) -> anyhow::Result<GrokAuthSharedLock> {
        GrokAuthSharedLock::acquire(&self.auth_path)
    }

    pub fn directives(&self) -> Vec<EnvDirective> {
        let mut directives = vec![
            EnvDirective::Set("GROK_HOME".to_owned(), self.grok_home.display().to_string()),
            EnvDirective::Set(GROK_AUTH_PATH_ENV.to_owned(), self.auth_path.display().to_string()),
            EnvDirective::Set("HOME".to_owned(), self.process_home.display().to_string()),
            EnvDirective::Set(
                "GH_CONFIG_DIR".to_owned(),
                self.host_tools.gh_config_dir.display().to_string(),
            ),
            EnvDirective::Set(
                "CUBE_DATA_DIR".to_owned(),
                self.host_tools.cube_data_dir.display().to_string(),
            ),
            EnvDirective::Set(
                "CUBE_CONFIG_DIR".to_owned(),
                self.host_tools.cube_config_dir.display().to_string(),
            ),
        ];
        if let Some(path) = &self.host_tools.vcs.jj_config {
            directives.push(EnvDirective::Set("JJ_CONFIG".to_owned(), path.display().to_string()));
        }
        if let Some(path) = &self.host_tools.vcs.git_config_global {
            directives.push(EnvDirective::Set(
                "GIT_CONFIG_GLOBAL".to_owned(),
                path.display().to_string(),
            ));
        }
        directives.extend([
            EnvDirective::Set(
                "XDG_CACHE_HOME".to_owned(),
                self.host_tools.caches.xdg_cache_home.display().to_string(),
            ),
            EnvDirective::Set(
                "REPOBIN_CACHE_DIR".to_owned(),
                self.host_tools.caches.repobin_cache_dir.display().to_string(),
            ),
            EnvDirective::Set(
                "BAZELISK_HOME".to_owned(),
                self.host_tools.caches.bazelisk_home.display().to_string(),
            ),
            // Bazel uses TEST_TMPDIR as its explicit output_user_root. This
            // preserves the same server/cache tree non-Grok workers use.
            EnvDirective::Set(
                "TEST_TMPDIR".to_owned(),
                self.host_tools.caches.bazel_output_root.display().to_string(),
            ),
        ]);
        directives.extend([
            // Boss's Grok driver is OAuth-only. Without both belts below, a
            // permanently failed refresh silently falls through to an
            // inherited XAI_API_KEY and changes endpoint/credential families.
            EnvDirective::Set(GROK_DISABLE_API_KEY_AUTH_ENV.to_owned(), "1".to_owned()),
            EnvDirective::Unset(XAI_API_KEY_ENV.to_owned()),
            EnvDirective::Unset(GROK_AMBIENT_AUTH_ENV.to_owned()),
            // A host value of 0 ungates project hooks/MCP.
            EnvDirective::Unset("GROK_FOLDER_TRUST".to_owned()),
        ]);
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

    /// Install native-path delegates in the scoped HOME. Grok retains HOME
    /// for terminal tools but strips tool-specific parent variables such as
    /// GH_CONFIG_DIR and CUBE_DATA_DIR, so startup checks and real tool calls
    /// must also work through these default paths.
    pub fn provision_home_delegates(&self) -> anyhow::Result<()> {
        optional_delegate(
            &self.host_tools.gh_config_dir,
            &self.process_home.join(GH_DEFAULT_CONFIG_LEAF),
            "gh configuration",
        )?;
        optional_delegate(
            &self.host_tools.cube_data_dir,
            &self.process_home.join(CUBE_DEFAULT_DATA_LEAF),
            "Cube workspace registry",
        )?;
        optional_delegate(
            &self.host_tools.cube_config_dir,
            &self.process_home.join(CUBE_DEFAULT_CONFIG_LEAF),
            "Cube configuration",
        )?;
        if let Some(source) = &self.host_tools.login_keychain {
            require_delegate(
                source,
                &self.process_home.join(LOGIN_KEYCHAIN_LEAF),
                "macOS login keychain",
            )?;
        }
        if let Some(source) = &self.host_tools.vcs.jj_config {
            let destination = if cfg!(target_os = "macos") {
                self.process_home.join("Library/Application Support/jj/config.toml")
            } else {
                self.process_home.join(".config/jj/config.toml")
            };
            require_delegate(source, &destination, "jj user configuration")?;
        }
        if let Some(source) = &self.host_tools.vcs.git_config_global {
            require_delegate(source, &self.process_home.join(".gitconfig"), "git user configuration")?;
        }
        if let Some(source) = &self.host_tools.vcs.ssh_dir {
            require_delegate(source, &self.process_home.join(".ssh"), "SSH configuration")?;
        }
        optional_delegate(
            &self.host_tools.caches.repobin_cache_dir,
            &self.process_home.join(".cache/repobin"),
            "repobin cache",
        )?;
        let bazelisk_destination = if cfg!(target_os = "macos") {
            self.process_home.join("Library/Caches/bazelisk")
        } else {
            self.process_home.join(".cache/bazelisk")
        };
        optional_delegate(
            &self.host_tools.caches.bazelisk_home,
            &bazelisk_destination,
            "Bazelisk cache",
        )?;
        let bazel_destination = if cfg!(target_os = "macos") {
            self.process_home.join("Library/Caches/bazel")
        } else {
            self.process_home.join(".cache/bazel")
        };
        optional_delegate(
            &self.host_tools.caches.bazel_output_root,
            &bazel_destination,
            "Bazel output root",
        )?;
        Ok(())
    }

    /// Host paths that Grok and its terminal tools must be able to mutate
    /// while running under Boss's macOS Seatbelt profile.
    ///
    /// The process and Grok homes hold per-run state. Cube owns the jj/git
    /// object store for secondary workspaces, and the cache roots are needed
    /// by repobin/Bazel. OAuth refresh is atomic and locks beside
    /// `GROK_AUTH_PATH`, so its parent must be writable by every concurrent
    /// worker that shares that credential.
    pub fn macos_writable_roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut roots = vec![
            self.grok_home.clone(),
            self.process_home.clone(),
            workspace.to_path_buf(),
            self.host_tools.cube_data_dir.clone(),
            self.host_tools.cube_config_dir.clone(),
            self.host_tools.caches.xdg_cache_home.clone(),
            self.host_tools.caches.repobin_cache_dir.clone(),
            self.host_tools.caches.bazelisk_home.clone(),
            self.host_tools.caches.bazel_output_root.clone(),
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
            PathBuf::from("/private/tmp"),
        ];
        if let Some(parent) = self.auth_path.parent() {
            roots.push(parent.to_path_buf());
        }

        let mut unique = Vec::new();
        for root in roots {
            let root = fs::canonicalize(&root).unwrap_or(root);
            if !unique.contains(&root) {
                unique.push(root);
            }
        }
        unique
    }

    /// Reproduce the environment visible to Grok terminal tools: HOME stays
    /// scoped, while the parent-only delegation variables are absent. The
    /// startup preflight uses this for Cube/gh/jj so it cannot go green on a
    /// capability the actual worker sandbox will silently discard.
    pub fn apply_tool_sandbox_environment(&self, command: &mut Command) {
        self.apply_to_command(command);
        for key in [
            "GH_CONFIG_DIR",
            "CUBE_DATA_DIR",
            "CUBE_CONFIG_DIR",
            "JJ_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "XDG_CACHE_HOME",
            "REPOBIN_CACHE_DIR",
            "BAZELISK_HOME",
            "TEST_TMPDIR",
        ] {
            command.env_remove(key);
        }
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self {
            grok_home: PathBuf::from("/tmp/grok-home"),
            process_home: PathBuf::from("/tmp/process-home"),
            auth_path: PathBuf::from("/host/auth.json"),
            host_tools: HostToolPaths {
                gh_config_dir: PathBuf::from("/host/gh"),
                cube_data_dir: PathBuf::from("/host/cube"),
                cube_config_dir: PathBuf::from("/host/cube-config"),
                login_keychain: None,
                vcs: HostVcsPaths {
                    jj_config: None,
                    git_config_global: None,
                    ssh_dir: None,
                },
                caches: HostCachePaths {
                    xdg_cache_home: PathBuf::from("/host/cache"),
                    repobin_cache_dir: PathBuf::from("/host/cache/repobin"),
                    bazelisk_home: PathBuf::from("/host/cache/bazelisk"),
                    bazel_output_root: PathBuf::from("/host/cache/bazel"),
                },
            },
        }
    }
}

/// Shared flock guard on Grok's `auth.json.lock`. Dropping releases the lock.
#[derive(Debug)]
pub struct GrokAuthSharedLock {
    _file: File,
}

impl GrokAuthSharedLock {
    /// See [`GrokProcessEnvironment::lock_auth_for_oauth_probe`].
    pub fn acquire(auth_path: &Path) -> anyhow::Result<Self> {
        let lock_path = grok_auth_lock_path(auth_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent for Grok auth lock {}", parent.display()))?;
        }
        // Open read/write so flock works on all Unixes; do not truncate an
        // existing lock body (Grok stores pid:timestamp there).
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening Grok auth lock {}", lock_path.display()))?;

        let deadline = Instant::now() + AUTH_LOCK_ACQUIRE_TIMEOUT;
        loop {
            // fs4: Ok(true) acquired, Ok(false) would-block, Err(io).
            match FileExt::try_lock_shared(&file) {
                Ok(true) => break,
                Ok(false) if Instant::now() < deadline => {
                    tracing::debug!(
                        lock = %lock_path.display(),
                        "Grok auth.json.lock shared flock busy; waiting for in-flight refresh"
                    );
                    std::thread::sleep(AUTH_LOCK_POLL);
                }
                Ok(false) => {
                    bail!(
                        "could not acquire shared lock on Grok OAuth lock {} within {}s; \
                         a sibling Grok process may be mid-refresh",
                        lock_path.display(),
                        AUTH_LOCK_ACQUIRE_TIMEOUT.as_secs()
                    );
                }
                Err(err) if Instant::now() < deadline => {
                    tracing::debug!(
                        lock = %lock_path.display(),
                        error = %err,
                        "Grok auth.json.lock shared flock error; retrying until deadline"
                    );
                    std::thread::sleep(AUTH_LOCK_POLL);
                }
                Err(err) => {
                    bail!(
                        "could not acquire shared lock on Grok OAuth lock {} within {}s; \
                         a sibling Grok process may be mid-refresh. Last error: {err}",
                        lock_path.display(),
                        AUTH_LOCK_ACQUIRE_TIMEOUT.as_secs()
                    );
                }
            }
        }

        // TOCTOU: re-validate the credential only after the shared lock is held
        // so we observe a post-refresh stable body, not a truncated mid-write.
        ensure_auth_file_ready_for_probe(auth_path)
            .with_context(|| format!("after acquiring shared lock on {}", lock_path.display()))?;

        Ok(Self { _file: file })
    }
}

/// Path of Grok's CLI-managed refresh lock: `{auth_path}.lock`.
pub fn grok_auth_lock_path(auth_path: &Path) -> PathBuf {
    let mut path = auth_path.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Fail closed when the shared OAuth credential is missing, empty, or not JSON.
///
/// Does not log credential bytes. Used under the shared auth lock before
/// `grok models` so a mid-refresh empty body cannot be mistaken for a healthy
/// unauthenticated state that should pass after a soft retry.
pub fn ensure_auth_file_ready_for_probe(auth_path: &Path) -> anyhow::Result<()> {
    let metadata = fs::metadata(auth_path).with_context(|| {
        format!(
            "Grok OAuth credential {} is missing; log in once with interactive `grok` \
             or wait for an in-flight refresh to rewrite the file",
            auth_path.display()
        )
    })?;
    if !metadata.is_file() {
        bail!("Grok OAuth credential {} is not a regular file", auth_path.display());
    }
    if metadata.len() == 0 {
        bail!(
            "Grok OAuth credential {} is empty (mid-refresh truncate or cleared credentials); \
             refusing OAuth preflight until a non-empty credential is present",
            auth_path.display()
        );
    }
    let bytes =
        fs::read(auth_path).with_context(|| format!("reading Grok OAuth credential {}", auth_path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Grok OAuth credential {} is not valid JSON (mid-refresh or corrupt)",
            auth_path.display()
        )
    })?;
    if !value.is_object() {
        bail!("Grok OAuth credential {} must be a JSON object", auth_path.display());
    }
    Ok(())
}

fn require_delegate(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    if !source.exists() {
        bail!(
            "cannot delegate {capability} into scoped Grok HOME: source {} does not exist",
            source.display()
        );
    }
    replace_with_symlink(source, destination, capability)
}

fn optional_delegate(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    replace_with_symlink(source, destination, capability)
}

fn replace_with_symlink(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .with_context(|| format!("resolving parent for {} delegate", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating delegate parent {}", parent.display()))?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(destination)
            .with_context(|| format!("replacing prior {capability} delegate {}", destination.display()))?,
        Ok(_) => bail!(
            "refusing to replace non-symlink {capability} path in scoped Grok HOME: {}",
            destination.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", destination.display())),
    }
    std::os::unix::fs::symlink(source, destination).with_context(|| {
        format!(
            "symlinking {capability} from {} to {}",
            source.display(),
            destination.display()
        )
    })
}

/// Resolve gh's non-secret configuration using gh's documented precedence.
pub fn resolve_gh_config_dir() -> Option<PathBuf> {
    if let Some(dir) = nonempty_env("GH_CONFIG_DIR") {
        return Some(dir);
    }
    if let Some(dir) = nonempty_env("XDG_CONFIG_HOME") {
        return Some(dir.join("gh"));
    }
    nonempty_env("HOME").map(|home| home.join(GH_DEFAULT_CONFIG_LEAF))
}

/// Resolve Cube's machine-global workspace registry using Cube's own
/// precedence from `tools/cube/src/paths.rs`.
pub fn resolve_cube_data_dir() -> Option<PathBuf> {
    if let Some(dir) = nonempty_env("CUBE_DATA_DIR") {
        return Some(dir);
    }
    if let Some(dir) = nonempty_env("XDG_DATA_HOME") {
        return Some(dir.join("cube"));
    }
    nonempty_env("HOME").map(|home| home.join(CUBE_DEFAULT_DATA_LEAF))
}

/// Resolve Cube's user configuration using Cube's own precedence from
/// `tools/cube/src/config.rs`.
pub fn resolve_cube_config_dir() -> Option<PathBuf> {
    if let Some(dir) = nonempty_env("CUBE_CONFIG_DIR") {
        return Some(dir);
    }
    if let Some(dir) = nonempty_env("XDG_CONFIG_HOME") {
        return Some(dir.join("cube"));
    }
    nonempty_env("HOME").map(|home| home.join(CUBE_DEFAULT_CONFIG_LEAF))
}

fn resolve_jj_config() -> Option<PathBuf> {
    if let Some(path) = nonempty_env("JJ_CONFIG") {
        return Some(path);
    }
    let home = nonempty_env("HOME")?;
    let legacy = home.join(".jjconfig.toml");
    if legacy.is_file() {
        return Some(legacy);
    }
    let platform = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/jj/config.toml")
    } else if let Some(xdg) = nonempty_env("XDG_CONFIG_HOME") {
        xdg.join("jj/config.toml")
    } else {
        home.join(".config/jj/config.toml")
    };
    platform.is_file().then_some(platform)
}

fn resolve_git_config_global() -> Option<PathBuf> {
    if let Some(path) = nonempty_env("GIT_CONFIG_GLOBAL") {
        return Some(path);
    }
    let home = nonempty_env("HOME")?;
    let home_config = home.join(".gitconfig");
    if home_config.is_file() {
        return Some(home_config);
    }
    let xdg_config = nonempty_env("XDG_CONFIG_HOME")?.join("git/config");
    xdg_config.is_file().then_some(xdg_config)
}

fn resolve_ssh_dir() -> Option<PathBuf> {
    let path = nonempty_env("HOME")?.join(".ssh");
    path.is_dir().then_some(path)
}

fn resolve_host_cache_paths() -> Option<HostCachePaths> {
    let home = nonempty_env("HOME")?;
    let xdg_cache_home = nonempty_env("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
    let repobin_cache_dir = nonempty_env("REPOBIN_CACHE_DIR").unwrap_or_else(|| xdg_cache_home.join("repobin"));
    let bazelisk_home = nonempty_env("BAZELISK_HOME").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            home.join("Library/Caches/bazelisk")
        } else {
            xdg_cache_home.join("bazelisk")
        }
    });
    let bazel_output_root = nonempty_env("TEST_TMPDIR").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            home.join("Library/Caches/bazel")
        } else {
            xdg_cache_home.join("bazel")
        }
    });
    Some(HostCachePaths {
        xdg_cache_home,
        repobin_cache_dir,
        bazelisk_home,
        bazel_output_root,
    })
}

/// Resolve the host login keychain file bridged into the scoped HOME for gh.
pub fn resolve_login_keychain_source() -> Option<PathBuf> {
    let path = nonempty_env("HOME")?.join(LOGIN_KEYCHAIN_LEAF);
    path.exists().then_some(path)
}

fn nonempty_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok::GROK_HOMES_ENV_TEST_LOCK;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set_and_clear(pairs: &[(&'static str, &Path)], clear: &[&'static str]) -> Self {
            let lock = GROK_HOMES_ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut prior = Vec::new();
            for (key, value) in pairs {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: all Grok env-mutating tests share the lock above.
                unsafe { std::env::set_var(key, value) };
            }
            for key in clear {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: all Grok env-mutating tests share the lock above.
                unsafe { std::env::remove_var(key) };
            }
            Self { _lock: lock, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.prior {
                match value {
                    // SAFETY: this guard still owns the shared environment lock.
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[test]
    fn directives_delegate_host_state_and_force_oauth() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        let jj_config = if cfg!(target_os = "macos") {
            host_home.join("Library/Application Support/jj/config.toml")
        } else {
            host_home.join(".config/jj/config.toml")
        };
        std::fs::create_dir_all(jj_config.parent().unwrap()).unwrap();
        std::fs::write(&jj_config, "[user]\n").unwrap();
        std::fs::write(host_home.join(".gitconfig"), "[user]\n").unwrap();
        let _guard = EnvGuard::set_and_clear(
            &[("HOME", &host_home)],
            &[
                "GH_CONFIG_DIR",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "CUBE_DATA_DIR",
                "CUBE_CONFIG_DIR",
                "JJ_CONFIG",
                "GIT_CONFIG_GLOBAL",
                "REPOBIN_CACHE_DIR",
                "BAZELISK_HOME",
                "TEST_TMPDIR",
            ],
        );

        let env = GrokProcessEnvironment::resolve(
            Path::new("/run/grok-home"),
            Path::new("/run/process-home"),
            Path::new("/host/auth.json"),
        )
        .unwrap();
        let directives = env.directives();

        for (key, expected) in [
            ("GH_CONFIG_DIR", host_home.join(".config/gh")),
            ("CUBE_DATA_DIR", host_home.join(".local/share/cube")),
            ("CUBE_CONFIG_DIR", host_home.join(".config/cube")),
            ("JJ_CONFIG", jj_config),
            ("GIT_CONFIG_GLOBAL", host_home.join(".gitconfig")),
            ("XDG_CACHE_HOME", host_home.join(".cache")),
            ("REPOBIN_CACHE_DIR", host_home.join(".cache/repobin")),
            (
                "BAZELISK_HOME",
                if cfg!(target_os = "macos") {
                    host_home.join("Library/Caches/bazelisk")
                } else {
                    host_home.join(".cache/bazelisk")
                },
            ),
            (
                "TEST_TMPDIR",
                if cfg!(target_os = "macos") {
                    host_home.join("Library/Caches/bazel")
                } else {
                    host_home.join(".cache/bazel")
                },
            ),
        ] {
            assert!(directives.iter().any(
                |directive| matches!(directive, EnvDirective::Set(actual_key, value) if actual_key == key && value == &expected.display().to_string())
            ));
        }
        assert!(directives.iter().any(
            |directive| matches!(directive, EnvDirective::Set(key, value) if key == GROK_AUTH_PATH_ENV && value == "/host/auth.json")
        ));
        assert!(directives.iter().any(
            |directive| matches!(directive, EnvDirective::Set(key, value) if key == GROK_DISABLE_API_KEY_AUTH_ENV && value == "1")
        ));
        assert!(
            directives
                .iter()
                .any(|directive| matches!(directive, EnvDirective::Unset(key) if key == XAI_API_KEY_ENV))
        );
    }

    #[test]
    fn scoped_home_delegates_survive_tool_environment_stripping() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        let process_home = tmp.path().join("process-home");
        for directory in [
            host_home.join(".config/gh"),
            host_home.join(".config/cube"),
            host_home.join(".local/share/cube"),
            host_home.join("Library/Application Support/jj"),
            host_home.join("Library/Keychains"),
            host_home.join(".ssh"),
            host_home.join(".cache/repobin"),
            host_home.join("Library/Caches/bazelisk"),
            host_home.join("Library/Caches/bazel"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(host_home.join("Library/Application Support/jj/config.toml"), "[user]\n").unwrap();
        std::fs::write(host_home.join("Library/Keychains/login.keychain-db"), "keychain\n").unwrap();
        std::fs::write(host_home.join(".gitconfig"), "[user]\n").unwrap();
        let _guard = EnvGuard::set_and_clear(
            &[("HOME", &host_home)],
            &[
                "GH_CONFIG_DIR",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "CUBE_DATA_DIR",
                "CUBE_CONFIG_DIR",
                "JJ_CONFIG",
                "GIT_CONFIG_GLOBAL",
                "REPOBIN_CACHE_DIR",
                "BAZELISK_HOME",
                "TEST_TMPDIR",
            ],
        );

        let environment =
            GrokProcessEnvironment::resolve(Path::new("/run/grok-home"), &process_home, Path::new("/host/auth.json"))
                .unwrap();
        environment.provision_home_delegates().unwrap();

        for (destination, source) in [
            (process_home.join(".config/gh"), host_home.join(".config/gh")),
            (
                process_home.join(".local/share/cube"),
                host_home.join(".local/share/cube"),
            ),
            (process_home.join(".gitconfig"), host_home.join(".gitconfig")),
            (process_home.join(".ssh"), host_home.join(".ssh")),
            (
                process_home.join("Library/Keychains/login.keychain-db"),
                host_home.join("Library/Keychains/login.keychain-db"),
            ),
        ] {
            assert_eq!(std::fs::read_link(destination).unwrap(), source);
        }
    }

    #[test]
    fn cube_and_gh_resolvers_honor_explicit_overrides() {
        let _guard = EnvGuard::set_and_clear(
            &[
                ("HOME", Path::new("/host-home")),
                ("GH_CONFIG_DIR", Path::new("/explicit/gh")),
                ("CUBE_DATA_DIR", Path::new("/explicit/cube-data")),
                ("CUBE_CONFIG_DIR", Path::new("/explicit/cube-config")),
            ],
            &["XDG_CONFIG_HOME", "XDG_DATA_HOME"],
        );
        assert_eq!(resolve_gh_config_dir(), Some(PathBuf::from("/explicit/gh")));
        assert_eq!(resolve_cube_data_dir(), Some(PathBuf::from("/explicit/cube-data")));
        assert_eq!(resolve_cube_config_dir(), Some(PathBuf::from("/explicit/cube-config")));
    }

    #[test]
    fn grok_auth_lock_path_is_sibling_dot_lock() {
        assert_eq!(
            grok_auth_lock_path(Path::new("/Users/op/.grok/auth.json")),
            PathBuf::from("/Users/op/.grok/auth.json.lock")
        );
    }

    #[test]
    fn ensure_auth_file_ready_rejects_empty_and_non_json() {
        let tmp = TempDir::new().unwrap();
        let auth = tmp.path().join("auth.json");

        fs::write(&auth, b"").unwrap();
        let err = ensure_auth_file_ready_for_probe(&auth).unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");

        fs::write(&auth, b"not-json").unwrap();
        let err = ensure_auth_file_ready_for_probe(&auth).unwrap_err().to_string();
        assert!(err.contains("not valid JSON"), "{err}");

        fs::write(&auth, br#"{"accessToken":"x"}"#).unwrap();
        ensure_auth_file_ready_for_probe(&auth).expect("object body must pass");
    }

    /// Deterministic stand-in for the production race: a writer holds Grok's
    /// exclusive `auth.json.lock` while truncating then rewriting auth.json.
    /// Without a shared lock, a concurrent probe can observe the empty window
    /// and report "not authenticated". With the shared lock, the probe waits
    /// for the writer and then sees a stable non-empty body.
    #[test]
    fn shared_auth_lock_waits_out_exclusive_refresh_rewrite() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let auth = tmp.path().join("auth.json");
        let lock_path = grok_auth_lock_path(&auth);
        let good = br#"{"accessToken":"stable","refreshToken":"r"}"#;
        fs::write(&auth, good).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let auth_for_writer = auth.clone();
        let lock_for_writer = lock_path.clone();
        let barrier_writer = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_for_writer)
                .unwrap();
            FileExt::lock_exclusive(&file).unwrap();
            // Signal the reader that exclusive is held, then simulate the
            // non-atomic truncate + rewrite window Grok's refresh can expose.
            barrier_writer.wait();
            fs::write(&auth_for_writer, b"").unwrap();
            thread::sleep(Duration::from_millis(150));
            fs::write(&auth_for_writer, good).unwrap();
            // Unlock by dropping the file after the rewrite completes.
            drop(file);
        });

        barrier.wait();
        // Reader must not observe the empty body: shared acquire blocks until
        // the exclusive holder finishes, then re-validates the file.
        let started = Instant::now();
        let guard = GrokAuthSharedLock::acquire(&auth).expect("shared lock + ready auth");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "shared lock should have waited for the exclusive refresh rewrite"
        );
        ensure_auth_file_ready_for_probe(&auth).unwrap();
        drop(guard);
        writer.join().unwrap();
        assert_eq!(fs::read(&auth).unwrap(), good);
    }

    #[test]
    fn shared_auth_lock_fails_closed_when_credential_empty_after_lock() {
        let tmp = TempDir::new().unwrap();
        let auth = tmp.path().join("auth.json");
        fs::write(&auth, b"").unwrap();
        // Lock file can be created; the post-lock readiness check must fail.
        let err = GrokAuthSharedLock::acquire(&auth).unwrap_err();
        let full = format!("{err:#}");
        assert!(
            full.contains("empty") || full.contains("mid-refresh"),
            "expected empty-credential fail-closed, got {full}"
        );
    }
}
