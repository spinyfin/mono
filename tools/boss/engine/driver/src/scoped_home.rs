//! Shared host-tool and login-keychain delegation for scoped worker `HOME`.
//!
//! Grok and Antigravity both isolate CLI state under a Boss-owned per-run
//! `HOME` while still needing the host login home's gh, Cube, jj, git, ssh, and
//! Bazel configuration and caches. Credential access for tools that use the macOS login
//! keychain is a filesystem symlink of that one keychain file — never a
//! keychain API, never the macOS security CLI, and never a copied token.
//!
//! Grok and Antigravity both call [`HostToolPaths::provision_into`], so their
//! login-keychain bridge and host-tool delegates are identical.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::EnvDirective;

/// Relative path of the login keychain file under `$HOME`.
pub const LOGIN_KEYCHAIN_LEAF: &str = "Library/Keychains/login.keychain-db";

/// Process-global lock for tests that mutate `HOME` or per-driver homes-root
/// overrides. Grok and Antigravity tests share this so they cannot race.
pub static SCOPED_HOME_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const GH_DEFAULT_CONFIG_LEAF: &str = ".config/gh";
const CUBE_DEFAULT_DATA_LEAF: &str = ".local/share/cube";
const CUBE_DEFAULT_CONFIG_LEAF: &str = ".config/cube";
const BAZEL_DISK_CACHE_LEAF: &str = ".cache/bazelcache";
const JJ_XDG_CONFIG_LEAF: &str = ".config/jj";
const JJ_MACOS_LEGACY_CONFIG_LEAF: &str = "Library/Application Support/jj/config.toml";

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct HostToolPaths {
    pub(crate) gh_config_dir: PathBuf,
    pub(crate) cube_data_dir: PathBuf,
    pub(crate) cube_config_dir: PathBuf,
    pub(crate) login_keychain: Option<PathBuf>,
    pub(crate) bazelrc: Option<PathBuf>,
    pub(crate) vcs: HostVcsPaths,
    pub(crate) caches: HostCachePaths,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostVcsPaths {
    pub(crate) jj_config: Option<PathBuf>,
    pub(crate) git_config_global: Option<PathBuf>,
    pub(crate) ssh_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostCachePaths {
    pub(crate) xdg_cache_home: PathBuf,
    pub(crate) repobin_cache_dir: PathBuf,
    pub(crate) bazelisk_home: PathBuf,
    pub(crate) bazel_output_root: PathBuf,
    /// Host path of `.bazelrc`'s `--disk_cache=~/.cache/bazelcache`. Tilde
    /// expansion follows `HOME`, so a scoped home without this delegate puts
    /// the disk cache inside the per-run tree.
    pub(crate) bazel_disk_cache: PathBuf,
}

impl HostToolPaths {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        Ok(Self::builder()
            .gh_config_dir(
                resolve_gh_config_dir().context(
                    "resolving host gh config for scoped HOME; set GH_CONFIG_DIR or ensure HOME is available",
                )?,
            )
            .cube_data_dir(
                resolve_cube_data_dir().context(
                    "resolving host Cube data for scoped HOME; set CUBE_DATA_DIR or ensure HOME is available",
                )?,
            )
            .cube_config_dir(resolve_cube_config_dir().context(
                "resolving host Cube config for scoped HOME; set CUBE_CONFIG_DIR or ensure HOME is available",
            )?)
            .maybe_login_keychain(resolve_login_keychain_source())
            .maybe_bazelrc(nonempty_env("HOME").map(|home| home.join(".bazelrc")))
            .vcs(HostVcsPaths {
                jj_config: resolve_jj_config(),
                git_config_global: resolve_git_config_global(),
                ssh_dir: resolve_ssh_dir(),
            })
            .caches(
                resolve_host_cache_paths()
                    .context("resolving host tool caches for scoped HOME; ensure HOME is available")?,
            )
            .build())
    }

    /// Native-path delegates in the scoped HOME. The login-keychain bridge is
    /// a symlink of the keychain *file*; this helper never opens the file or
    /// talks to a keychain API.
    pub(crate) fn provision_into(&self, process_home: &Path) -> anyhow::Result<()> {
        optional_delegate(
            &self.gh_config_dir,
            &process_home.join(GH_DEFAULT_CONFIG_LEAF),
            "gh configuration",
        )?;
        optional_delegate(
            &self.cube_data_dir,
            &process_home.join(CUBE_DEFAULT_DATA_LEAF),
            "Cube workspace registry",
        )?;
        optional_delegate(
            &self.cube_config_dir,
            &process_home.join(CUBE_DEFAULT_CONFIG_LEAF),
            "Cube configuration",
        )?;
        if let Some(source) = &self.login_keychain {
            require_delegate(source, &process_home.join(LOGIN_KEYCHAIN_LEAF), "macOS login keychain")?;
        }
        if let Some(source) = &self.vcs.jj_config {
            // jj 0.36 and newer read the XDG location on every platform; older
            // jj on macOS reads only the platform one. Grok strips `JJ_CONFIG`
            // from tool shells, so whichever jj runs must find it through HOME.
            require_delegate(
                source,
                &process_home.join(JJ_XDG_CONFIG_LEAF).join("config.toml"),
                "jj user configuration",
            )?;
            if cfg!(target_os = "macos") {
                require_delegate(
                    source,
                    &process_home.join(JJ_MACOS_LEGACY_CONFIG_LEAF),
                    "jj user configuration",
                )?;
            }
        }
        // From 0.38 jj keeps every repository's configuration beside the user
        // configuration it finds through HOME, whatever `JJ_CONFIG` says, and
        // moves a repository's configuration there on first contact. Without
        // this delegate a scoped HOME reads an empty repository configuration,
        // and the first jj to touch a store would move its configuration into
        // a per-run home that is later deleted. Older jj ignores the directory.
        if let Some(source) = resolve_jj_repo_config_dir() {
            fs::create_dir_all(&source)
                .with_context(|| format!("creating jj repository configuration directory {}", source.display()))?;
            require_delegate(
                &source,
                &process_home.join(JJ_XDG_CONFIG_LEAF).join("repos"),
                "jj repository configuration",
            )?;
        }
        if let Some(source) = &self.vcs.git_config_global {
            require_delegate(source, &process_home.join(".gitconfig"), "git user configuration")?;
        }
        if let Some(source) = &self.vcs.ssh_dir {
            require_delegate(source, &process_home.join(".ssh"), "SSH configuration")?;
        }
        // Bazel discovers machine configuration through HOME, independently of
        // its cache paths. Keep the native rc lookup for every repository: the
        // scoped rc imports the host one, so endpoint changes still apply
        // without copying stale settings, and it pins the output root that
        // `TEST_TMPDIR` used to (see `provision_bazelrc`).
        provision_bazelrc(
            self.bazelrc.as_deref(),
            &self.caches.bazel_output_root,
            bazel_user_name().as_deref(),
            &process_home.join(".bazelrc"),
        )?;
        optional_delegate(
            &self.caches.repobin_cache_dir,
            &process_home.join(".cache/repobin"),
            "repobin cache",
        )?;
        let bazelisk_destination = if cfg!(target_os = "macos") {
            process_home.join("Library/Caches/bazelisk")
        } else {
            process_home.join(".cache/bazelisk")
        };
        optional_delegate(&self.caches.bazelisk_home, &bazelisk_destination, "Bazelisk cache")?;
        let bazel_destination = if cfg!(target_os = "macos") {
            process_home.join("Library/Caches/bazel")
        } else {
            process_home.join(".cache/bazel")
        };
        optional_delegate(&self.caches.bazel_output_root, &bazel_destination, "Bazel output root")?;
        optional_delegate(
            &self.caches.bazel_disk_cache,
            &process_home.join(BAZEL_DISK_CACHE_LEAF),
            "Bazel disk cache",
        )?;
        Ok(())
    }

    pub(crate) fn env_directives(&self) -> Vec<EnvDirective> {
        let mut directives = vec![
            EnvDirective::Set("GH_CONFIG_DIR".to_owned(), self.gh_config_dir.display().to_string()),
            EnvDirective::Set("CUBE_DATA_DIR".to_owned(), self.cube_data_dir.display().to_string()),
            EnvDirective::Set("CUBE_CONFIG_DIR".to_owned(), self.cube_config_dir.display().to_string()),
        ];
        if let Some(path) = &self.vcs.jj_config {
            directives.push(EnvDirective::Set("JJ_CONFIG".to_owned(), path.display().to_string()));
        }
        if let Some(path) = &self.vcs.git_config_global {
            directives.push(EnvDirective::Set(
                "GIT_CONFIG_GLOBAL".to_owned(),
                path.display().to_string(),
            ));
        }
        directives.extend([
            EnvDirective::Set(
                "XDG_CACHE_HOME".to_owned(),
                self.caches.xdg_cache_home.display().to_string(),
            ),
            EnvDirective::Set(
                "REPOBIN_CACHE_DIR".to_owned(),
                self.caches.repobin_cache_dir.display().to_string(),
            ),
            EnvDirective::Set(
                "BAZELISK_HOME".to_owned(),
                self.caches.bazelisk_home.display().to_string(),
            ),
            // Deliberately no `TEST_TMPDIR`: the Bazel client treats any value
            // of it as "running inside a test" (15 s server idle timeout,
            // output root under it). The output root is pinned by the scoped
            // `.bazelrc` instead.
        ]);
        directives
    }
}

/// First line of the `.bazelrc` this module generates in a scoped HOME. A
/// pre-existing regular file is only replaced when it starts with this.
const SCOPED_BAZELRC_MARKER: &str = "# Generated by Boss for a scoped worker HOME.";

/// Write the scoped HOME's user `.bazelrc`.
///
/// Bazel's client treats any set `TEST_TMPDIR` as "running inside a test"
/// (`startup_options.cc`: `max_idle_secs` drops from 3 h to 15 s, and the
/// output user root moves under it), so the output root is pinned with a
/// `startup --output_user_root` line instead. The pinned root is the same
/// `<bazel_output_root>/_bazel_<user>` the env var produced, so existing
/// output bases and install bases stay valid.
///
/// The host rc is imported rather than copied or edited: its settings (remote
/// endpoint, `dev-remote` configs, ...) and later edits keep applying, and
/// setup never writes to the host file. The import comes after the startup
/// line so a host rc that sets its own `output_user_root` still wins, as an
/// explicit flag did over `TEST_TMPDIR`.
pub(crate) fn provision_bazelrc(
    host_rc: Option<&Path>,
    bazel_output_root: &Path,
    user: Option<&str>,
    destination: &Path,
) -> anyhow::Result<()> {
    let mut contents = format!(
        "{SCOPED_BAZELRC_MARKER}\n# Rewritten each time the home is provisioned; edit the host ~/.bazelrc instead.\n"
    );
    if let Some(user) = user {
        let root = bazel_output_root.join(format!("_bazel_{user}"));
        contents.push_str(&format!(
            "startup --output_user_root={}\n",
            rc_token(&root.display().to_string())?
        ));
    }
    if let Some(host_rc) = host_rc.filter(|host_rc| *host_rc != destination) {
        contents.push_str(&format!("try-import {}\n", rc_token(&host_rc.display().to_string())?));
    }

    let parent = destination
        .parent()
        .with_context(|| format!("resolving parent for {} delegate", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating delegate parent {}", parent.display()))?;
    match fs::symlink_metadata(destination) {
        // A link from an earlier provisioning points at the host file; it is
        // replaced, never written through.
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(metadata) if metadata.is_file() && fs::read_to_string(destination)?.starts_with(SCOPED_BAZELRC_MARKER) => {}
        Ok(_) => bail!(
            "refusing to replace non-generated Bazel user configuration path in scoped HOME: {}",
            destination.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", destination.display())),
    }
    let staging = destination.with_file_name(".bazelrc.boss-new");
    fs::write(&staging, contents).with_context(|| format!("writing Bazel user configuration {}", staging.display()))?;
    fs::rename(&staging, destination)
        .with_context(|| format!("installing Bazel user configuration {}", destination.display()))
}

/// Bazel names its per-user output directory after `$USER`, falling back to
/// the passwd entry. Only the environment is consulted here; with neither set
/// the output root is not pinned and Bazel uses its own default.
fn bazel_user_name() -> Option<String> {
    ["USER", "LOGNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|user| !user.is_empty() && !user.contains('/') && user != "." && user != "..")
}

/// One bazelrc token. Plain paths are written bare; anything the rc lexer
/// would split or treat as a comment is double-quoted with `\` and `"` escaped.
fn rc_token(value: &str) -> anyhow::Result<String> {
    if value.contains(['\n', '\r']) {
        bail!("cannot write {value:?} into a bazelrc: it contains a line break");
    }
    if value.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '#')) {
        Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
    } else {
        Ok(value.to_owned())
    }
}

/// Re-export `BOSS_WORKER_TOOL_PATH` when the parent process has one, so a
/// fully-scoped `HOME` cannot drop the worker tool dir.
pub(crate) fn worker_tool_path_directive() -> Option<EnvDirective> {
    nonempty_env("BOSS_WORKER_TOOL_PATH")
        .map(|path| EnvDirective::Set("BOSS_WORKER_TOOL_PATH".to_owned(), path.display().to_string()))
}

pub(crate) fn require_delegate(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    if !source.exists() {
        bail!(
            "cannot delegate {capability} into scoped HOME: source {} does not exist",
            source.display()
        );
    }
    replace_with_symlink(source, destination, capability)
}

pub(crate) fn optional_delegate(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    replace_with_symlink(source, destination, capability)
}

pub(crate) fn replace_with_symlink(source: &Path, destination: &Path, capability: &str) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .with_context(|| format!("resolving parent for {} delegate", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating delegate parent {}", parent.display()))?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(destination)
            .with_context(|| format!("replacing prior {capability} delegate {}", destination.display()))?,
        Ok(_) => bail!(
            "refusing to replace non-symlink {capability} path in scoped HOME: {}",
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
    // Every jj from 0.29 reads the XDG location, and from 0.36 it is the only
    // platform location jj reads on macOS, so it wins once it exists. Before
    // that, jj on macOS reads only `~/Library/Application Support/jj`.
    let xdg = jj_xdg_config_dir(&home).join("config.toml");
    if xdg.is_file() {
        return Some(xdg);
    }
    let macos = home.join(JJ_MACOS_LEGACY_CONFIG_LEAF);
    (cfg!(target_os = "macos") && macos.is_file()).then_some(macos)
}

fn jj_xdg_config_dir(home: &Path) -> PathBuf {
    nonempty_env("XDG_CONFIG_HOME")
        .map(|xdg| xdg.join("jj"))
        .unwrap_or_else(|| home.join(JJ_XDG_CONFIG_LEAF))
}

/// Where jj 0.38 and newer keep per-repository configuration on this host.
/// Read at provisioning time: it follows the same HOME as the paths above.
fn resolve_jj_repo_config_dir() -> Option<PathBuf> {
    nonempty_env("HOME").map(|home| jj_xdg_config_dir(&home).join("repos"))
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
    let bazel_output_root = bazel_support::cache_root(
        std::env::var("TEST_TMPDIR").ok().as_deref(),
        home.to_str(),
        xdg_cache_home.to_str(),
        cfg!(target_os = "macos"),
    )?;
    let bazel_disk_cache = home.join(BAZEL_DISK_CACHE_LEAF);
    Some(HostCachePaths {
        xdg_cache_home,
        repobin_cache_dir,
        bazelisk_home,
        bazel_output_root,
        bazel_disk_cache,
    })
}

/// Resolve the host login keychain file bridged into the scoped HOME.
///
/// Existence check only. Never opens the file, never invokes a keychain API.
pub fn resolve_login_keychain_source() -> Option<PathBuf> {
    let path = nonempty_env("HOME")?.join(LOGIN_KEYCHAIN_LEAF);
    path.exists().then_some(path)
}

pub(crate) fn nonempty_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn trust_path_variants(workspace: &Path) -> Vec<String> {
    let mut paths = BTreeSet::new();
    push_path_and_symlink_forms(&mut paths, workspace);
    if let Ok(canon) = fs::canonicalize(workspace) {
        push_path_and_symlink_forms(&mut paths, &canon);
    }
    paths.into_iter().collect()
}

fn push_path_and_symlink_forms(paths: &mut BTreeSet<String>, path: &Path) {
    let s = path.display().to_string();
    if s.is_empty() {
        return;
    }
    paths.insert(s.clone());
    if let Some(rest) = s.strip_prefix("/tmp") {
        paths.insert(format!("/private/tmp{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/tmp") {
        paths.insert(format!("/tmp{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/var/") {
        paths.insert(format!("/private/var/{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/var/") {
        paths.insert(format!("/var/{rest}"));
    }
}

pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set_and_clear(pairs: &[(&'static str, &Path)], clear: &[&'static str]) -> Self {
            let lock = SCOPED_HOME_ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Bind Bazel's isolated root before clearing TEST_TMPDIR. Registry
            // and transcript tests resolve it concurrently without this lock.
            boss_log_files::default_state_root().expect("Bazel supplies an isolated state root");
            let mut prior = Vec::new();
            for (key, value) in pairs {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: all scoped-home env-mutating tests share the lock above.
                unsafe { std::env::set_var(key, value) };
            }
            for key in clear {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: all scoped-home env-mutating tests share the lock above.
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

    fn host_clear() -> [&'static str; 11] {
        [
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
        ]
    }

    #[test]
    fn registry_resolves_state_root_while_host_environment_is_cleared() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::set_and_clear(&[("HOME", tmp.path())], &host_clear());
        let wrapper_root = _guard
            .prior
            .iter()
            .find_map(|(key, value)| (*key == "TEST_TMPDIR").then_some(value))
            .unwrap()
            .as_ref()
            .unwrap();
        assert!(std::env::var_os("TEST_TMPDIR").is_none());
        // Registration probes Codex progress wiring, which resolves the durable
        // transcript store beneath the isolated Boss state root.
        let registry = crate::registry::DriverRegistry::default();
        assert!(registry.require("codex").is_ok());
        assert_eq!(
            boss_log_files::default_state_root().unwrap(),
            PathBuf::from(wrapper_root)
        );
    }

    #[test]
    fn login_keychain_bridge_is_a_symlink_of_a_dummy_file() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        let process_home = tmp.path().join("process-home");
        let keychain_dir = host_home.join("Library/Keychains");
        fs::create_dir_all(&keychain_dir).unwrap();
        let source = keychain_dir.join("login.keychain-db");
        fs::write(&source, b"dummy-keychain-bytes").unwrap();
        let _guard = EnvGuard::set_and_clear(&[("HOME", &host_home)], &host_clear());

        let tools = HostToolPaths::resolve().unwrap();
        tools.provision_into(&process_home).unwrap();

        let bridged = process_home.join(LOGIN_KEYCHAIN_LEAF);
        let meta = fs::symlink_metadata(&bridged).unwrap();
        assert!(meta.file_type().is_symlink(), "bridge must be a symlink, never a copy");
        assert_eq!(fs::read_link(&bridged).unwrap(), source);
        assert_eq!(fs::read(&bridged).unwrap(), b"dummy-keychain-bytes");
    }

    #[test]
    fn replace_with_symlink_refuses_a_regular_destination() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        fs::write(&source, b"src").unwrap();
        fs::write(&dest, b"existing").unwrap();
        let err = replace_with_symlink(&source, &dest, "probe").unwrap_err().to_string();
        assert!(err.contains("refusing to replace non-symlink"), "{err}");
        assert_eq!(fs::read(&dest).unwrap(), b"existing");
    }

    #[test]
    fn jj_configuration_resolves_for_old_and_new_jj() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        let process_home = tmp.path().join("process-home");
        let xdg = host_home.join(".config/jj/config.toml");
        let macos = host_home.join(JJ_MACOS_LEGACY_CONFIG_LEAF);
        for path in [&xdg, &macos] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        let _guard = EnvGuard::set_and_clear(&[("HOME", &host_home)], &host_clear());

        // Before the host configuration is moved, only old jj on macOS has one.
        fs::write(&macos, "[user]\n").unwrap();
        assert_eq!(resolve_jj_config(), cfg!(target_os = "macos").then(|| macos.clone()));
        // Once the XDG file exists it is the one every newer jj reads.
        fs::write(&xdg, "[user]\n").unwrap();
        assert_eq!(resolve_jj_config(), Some(xdg.clone()));

        HostToolPaths::resolve().unwrap().provision_into(&process_home).unwrap();
        assert_eq!(fs::read_link(process_home.join(".config/jj/config.toml")).unwrap(), xdg);
        if cfg!(target_os = "macos") {
            assert_eq!(
                fs::read_link(process_home.join(JJ_MACOS_LEGACY_CONFIG_LEAF)).unwrap(),
                xdg
            );
        }
        // Repository configuration written through the scoped home lands in
        // the host directory, which is created if no jj has made it yet.
        let repos = host_home.join(".config/jj/repos");
        assert_eq!(fs::read_link(process_home.join(".config/jj/repos")).unwrap(), repos);
        fs::create_dir_all(process_home.join(".config/jj/repos/0123")).unwrap();
        assert!(repos.join("0123").is_dir());
    }

    #[test]
    fn optional_delegate_skips_missing_source() {
        let tmp = TempDir::new().unwrap();
        optional_delegate(
            &tmp.path().join("missing"),
            &tmp.path().join("dest"),
            "missing capability",
        )
        .unwrap();
        assert!(!tmp.path().join("dest").exists());
    }

    #[test]
    fn require_delegate_fails_when_source_is_missing() {
        let tmp = TempDir::new().unwrap();
        let err = require_delegate(
            &tmp.path().join("missing"),
            &tmp.path().join("dest"),
            "macOS login keychain",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("macOS login keychain"), "{err}");
    }

    #[test]
    fn bazel_disk_cache_is_delegated_when_present() {
        let tmp = TempDir::new().unwrap();
        let host_home = tmp.path().join("host-home");
        let process_home = tmp.path().join("process-home");
        let disk_cache = host_home.join(BAZEL_DISK_CACHE_LEAF);
        fs::create_dir_all(&disk_cache).unwrap();
        let _guard = EnvGuard::set_and_clear(&[("HOME", &host_home)], &host_clear());

        HostToolPaths::resolve().unwrap().provision_into(&process_home).unwrap();
        assert_eq!(
            fs::read_link(process_home.join(BAZEL_DISK_CACHE_LEAF)).unwrap(),
            disk_cache
        );
    }

    /// Parse a generated rc the way Bazel's lexer would: logical lines split
    /// into tokens, comments dropped.
    fn rc_lines(path: &Path) -> Vec<Vec<String>> {
        bazel_support::logical_bazelrc_lines(&fs::read_to_string(path).unwrap())
            .unwrap()
            .into_iter()
            .map(|(_, line)| bazel_support::shell_split(&line).unwrap())
            .filter(|tokens| !tokens.is_empty())
            .collect()
    }

    fn output_root_lines(path: &Path) -> Vec<Vec<String>> {
        rc_lines(path)
            .into_iter()
            .filter(|tokens| {
                tokens.first().map(String::as_str) == Some("startup")
                    && tokens.iter().any(|token| token.starts_with("--output_user_root"))
            })
            .collect()
    }

    #[test]
    fn env_directives_never_export_test_tmpdir() {
        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard::set_and_clear(&[("HOME", tmp.path())], &host_clear());
        let directives = HostToolPaths::resolve().unwrap().env_directives();
        assert!(!directives.is_empty());
        for directive in directives {
            let (EnvDirective::Set(key, _) | EnvDirective::Unset(key)) = directive;
            assert_ne!(key, "TEST_TMPDIR", "the Bazel client treats it as test mode");
        }
    }

    #[test]
    fn scoped_bazelrc_pins_the_output_root_and_imports_the_host_rc() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        let source = host.join(".bazelrc");
        let process_home = tmp.path().join("process-home");
        let _guard = EnvGuard::set_and_clear(&[("HOME", &host), ("USER", Path::new("someone"))], &host_clear());
        let tools = HostToolPaths::resolve().unwrap();
        let destination = process_home.join(".bazelrc");
        let expected_root = tools
            .caches
            .bazel_output_root
            .join("_bazel_someone")
            .display()
            .to_string();

        // A machine without user configuration still gets the pin, and
        // Bazel's try-import tolerates the missing host file.
        tools.provision_into(&process_home).unwrap();
        assert_eq!(
            output_root_lines(&destination),
            [vec![
                "startup".to_owned(),
                format!("--output_user_root={expected_root}")
            ]]
        );

        // Provisioning twice never repeats the line, and the host file is
        // reached through the import: edits and atomic replacement of it are
        // visible to workers, and setup never writes to it.
        let host_rc = "build:dev-remote --remote_executor=grpc://example.test:1\nbuild --config=dev-remote\n";
        fs::write(&source, host_rc).unwrap();
        tools.provision_into(&process_home).unwrap();
        tools.provision_into(&process_home).unwrap();
        assert!(!fs::symlink_metadata(&destination).unwrap().file_type().is_symlink());
        assert_eq!(output_root_lines(&destination).len(), 1);
        assert!(rc_lines(&destination).contains(&vec!["try-import".to_owned(), source.display().to_string()]));
        assert_eq!(fs::read_to_string(&source).unwrap(), host_rc);
        let replacement = host.join("new.bazelrc");
        fs::write(&replacement, "build --jobs=4\n").unwrap();
        fs::rename(replacement, &source).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "build --jobs=4\n");
    }

    #[test]
    fn scoped_bazelrc_replaces_a_prior_link_without_writing_through_it() {
        let tmp = TempDir::new().unwrap();
        let host_rc = tmp.path().join("host.bazelrc");
        fs::write(&host_rc, "build --announce_rc\n").unwrap();
        let destination = tmp.path().join("home/.bazelrc");
        // The shape earlier provisioning left behind.
        replace_with_symlink(&host_rc, &destination, "Bazel user configuration").unwrap();

        provision_bazelrc(Some(&host_rc), Path::new("/cache/bazel"), Some("u"), &destination).unwrap();

        assert_eq!(fs::read_to_string(&host_rc).unwrap(), "build --announce_rc\n");
        assert!(!fs::symlink_metadata(&destination).unwrap().file_type().is_symlink());
        assert_eq!(
            output_root_lines(&destination),
            [vec![
                "startup".to_owned(),
                "--output_user_root=/cache/bazel/_bazel_u".to_owned()
            ]]
        );
    }

    #[test]
    fn scoped_bazelrc_refuses_to_replace_a_hand_written_file() {
        let tmp = TempDir::new().unwrap();
        let destination = tmp.path().join(".bazelrc");
        fs::write(&destination, "build --jobs=4\n").unwrap();
        let err = provision_bazelrc(None, Path::new("/cache"), Some("u"), &destination)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to replace"), "{err}");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "build --jobs=4\n");
    }

    #[test]
    fn scoped_bazelrc_quotes_paths_the_rc_lexer_would_split() {
        let tmp = TempDir::new().unwrap();
        let host_rc = tmp.path().join("My Host/.bazelrc");
        let destination = tmp.path().join(".bazelrc");
        provision_bazelrc(
            Some(&host_rc),
            Path::new("/Users/A B/Library/Caches/bazel"),
            Some("a b"),
            &destination,
        )
        .unwrap();
        assert_eq!(
            output_root_lines(&destination),
            [vec![
                "startup".to_owned(),
                "--output_user_root=/Users/A B/Library/Caches/bazel/_bazel_a b".to_owned()
            ]]
        );
        assert!(rc_lines(&destination).contains(&vec!["try-import".to_owned(), host_rc.display().to_string()]));
        assert!(provision_bazelrc(None, Path::new("/a\nb"), Some("u"), &destination).is_err());
    }

    #[test]
    fn scoped_bazelrc_without_a_user_name_leaves_the_output_root_to_bazel() {
        let tmp = TempDir::new().unwrap();
        let destination = tmp.path().join(".bazelrc");
        provision_bazelrc(
            Some(Path::new("/host/.bazelrc")),
            Path::new("/cache"),
            None,
            &destination,
        )
        .unwrap();
        assert!(output_root_lines(&destination).is_empty());
        assert!(rc_lines(&destination).contains(&vec!["try-import".to_owned(), "/host/.bazelrc".to_owned()]));
    }

    #[test]
    fn codex_and_claude_keep_the_inherited_bazelrc() {
        use crate::{AgentDriver, SpawnRequest, claude::ClaudeDriver, codex::CodexDriver};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".bazelrc"), "build --announce_rc\n").unwrap();
        let _codex = crate::test_support::codex_homes_override(&tmp.path().join("codex-homes"));
        for driver in [&ClaudeDriver as &dyn AgentDriver, &CodexDriver::default()] {
            let plan = driver.spawn_invocation(
                SpawnRequest::builder()
                    .model("test-model")
                    .non_opus_auto_mode(false)
                    .run_id("bazelrc-test")
                    .build(),
            );
            let mut home = Some(tmp.path().to_owned());
            for directive in plan.env {
                match directive {
                    EnvDirective::Set(key, value) if key == "HOME" => home = Some(value.into()),
                    EnvDirective::Unset(key) if key == "HOME" => home = None,
                    _ => {}
                }
            }
            assert_eq!(home.as_deref(), Some(tmp.path()), "{}", driver.descriptor().name);
            assert_eq!(
                fs::read_to_string(home.unwrap().join(".bazelrc")).unwrap(),
                "build --announce_rc\n"
            );
        }
    }

    #[test]
    fn worker_tool_path_directive_preserves_a_parent_value() {
        let tmp = TempDir::new().unwrap();
        let tool_path = tmp.path().join("worker-tools");
        fs::create_dir_all(&tool_path).unwrap();
        let _guard = EnvGuard::set_and_clear(&[("BOSS_WORKER_TOOL_PATH", &tool_path)], &[]);
        let directive = worker_tool_path_directive().expect("parent value must be re-exported");
        assert!(matches!(
            directive,
            EnvDirective::Set(key, value)
                if key == "BOSS_WORKER_TOOL_PATH" && value == tool_path.display().to_string()
        ));
    }

    #[test]
    fn provisioning_source_does_not_invoke_keychain_or_security_cli() {
        // Concatenate so this test's own source does not contain the needles.
        let forbidden = [
            ["/usr/bin/", "security"].concat(),
            ["Command::new(\"", "security", "\")"].concat(),
            ["Sec", "Item"].concat(),
            ["Security", ".framework"].concat(),
            ["unlock-", "keychain"].concat(),
            ["find-generic-", "password"].concat(),
            ["dump-", "keychain"].concat(),
        ];
        let sources = [
            include_str!("scoped_home.rs"),
            include_str!("agy.rs"),
            include_str!("agy/home.rs"),
            include_str!("agy/environment.rs"),
        ];
        for (name, source) in [
            ("scoped_home.rs", sources[0]),
            ("agy.rs", sources[1]),
            ("agy/home.rs", sources[2]),
            ("agy/environment.rs", sources[3]),
        ] {
            for needle in &forbidden {
                assert!(
                    !source.contains(needle.as_str()),
                    "{name} must not contain {needle:?}; provisioning is filesystem-only"
                );
            }
        }
    }
}
