//! Controlled environment for check subprocesses and provisioning probes.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Resolve a declared launcher once; children never inherit ambient PATH order.
pub(crate) fn locate(program: &Path) -> PathBuf {
    if program.components().count() != 1 {
        return program.to_owned();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| program.to_owned())
}

/// Bazel and Bazelisk launchers; they share one environment policy.
pub(crate) fn is_bazel_launcher(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|name| name == "bazel" || name == "bazelisk")
}

/// Bazel launchers keep the caller's exact environment and arguments so server
/// and cache keys match ordinary builds. Only the Bazelisk version selection is
/// pinned, so `~/.bazeliskrc` cannot override the repository's `.bazelversion`.
fn prepare_bazel(
    command: &mut Command,
    declared: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    home: tempfile::TempDir,
) -> Result<tempfile::TempDir> {
    let pinned = command
        .get_current_dir()
        .map(|dir| dir.join(".bazelversion"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.lines().next().map(|line| line.trim().to_owned()))
        .filter(|version| !version.is_empty());
    if let Some(version) = pinned
        && std::env::var_os("USE_BAZEL_VERSION").is_none()
    {
        command.env("USE_BAZEL_VERSION", version);
    }
    for (key, value) in declared {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    Ok(home)
}

/// Keep the isolated HOME alive until the child exits.
pub(crate) fn prepare(command: &mut Command) -> Result<tempfile::TempDir> {
    let home = tempfile::tempdir().context("create isolated check HOME")?;
    let declared: Vec<_> = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(|value| value.to_owned())))
        .collect();
    let program = Path::new(command.get_program());
    let mut paths = Vec::new();
    if let Some(parent) = program.parent().filter(|path| path.is_absolute()) {
        paths.push(parent.to_owned());
    }
    paths.extend([
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);
    let bazel = is_bazel_launcher(program);
    if bazel {
        return prepare_bazel(command, declared, home);
    }
    let npm = program.file_name().is_some_and(|name| name == "npx");
    command
        .env_clear()
        .env("PATH", std::env::join_paths(paths)?)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC");
    if npm {
        let cache = directories::ProjectDirs::from("", "", "checkleft")
            .context("cannot locate npm cache")?
            .cache_dir()
            .join("npm");
        std::fs::create_dir_all(&cache)?;
        command
            .env("npm_config_cache", cache)
            .env("npm_config_userconfig", home.path().join("npmrc"))
            .env("npm_config_globalconfig", home.path().join("global-npmrc"));
    }
    for key in [
        "RUNFILES_DIR",
        "RUNFILES_MANIFEST_FILE",
        "TEST_SRCDIR",
        "TMPDIR",
        "SYSTEMROOT",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "all_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in declared {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envs(command: &Command) -> std::collections::BTreeMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn bazel_launchers_are_classified_together() {
        assert!(is_bazel_launcher(Path::new("/x/bazel")));
        assert!(is_bazel_launcher(Path::new("bazelisk")));
        assert!(!is_bazel_launcher(Path::new("npx")));
    }

    #[test]
    fn bazel_keeps_caller_env_and_args_and_pins_repo_version() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join(".bazelversion"), "7.4.1\n").unwrap();
        let mut command = Command::new("/usr/bin/bazel");
        command.args(["--max_idle_secs=5", "build"]).current_dir(repo.path());
        let _home = prepare(&mut command).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--max_idle_secs=5", "build"]);
        let envs = envs(&command);
        // Nothing rewritten: PATH, HOME, locale and TZ come from the caller.
        for key in ["PATH", "HOME", "LC_ALL", "LANG", "TZ", "XDG_CACHE_HOME"] {
            assert!(!envs.contains_key(key), "{key} must not be overridden");
        }
        assert!(command.get_envs().count() == 1 || std::env::var_os("USE_BAZEL_VERSION").is_some());
        if std::env::var_os("USE_BAZEL_VERSION").is_none() {
            assert_eq!(envs["USE_BAZEL_VERSION"].as_deref(), Some("7.4.1"));
        }
    }

    #[test]
    fn hostile_bazeliskrc_cannot_override_repo_pin() {
        // The pin is an explicit env var, which Bazelisk ranks above any rc file.
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join(".bazelversion"), "8.0.0").unwrap();
        let mut command = Command::new("bazelisk");
        command.current_dir(repo.path());
        let _home = prepare(&mut command).unwrap();
        if std::env::var_os("USE_BAZEL_VERSION").is_none() {
            assert_eq!(envs(&command)["USE_BAZEL_VERSION"].as_deref(), Some("8.0.0"));
        }
        assert!(!envs(&command).contains_key("HOME"));
    }

    #[test]
    fn other_launchers_inherit_network_transport_vars() {
        let mut command = Command::new("/usr/bin/true");
        let _home = prepare(&mut command).unwrap();
        let envs = envs(&command);
        assert_eq!(envs["LC_ALL"].as_deref(), Some("C"));
        if let Some(v) = std::env::var_os("HTTPS_PROXY") {
            assert_eq!(envs["HTTPS_PROXY"].as_deref(), v.to_str());
        }
    }
}
