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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::EnvDirective;

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
}
