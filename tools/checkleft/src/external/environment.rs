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
    let bazel = program
        .file_name()
        .is_some_and(|name| name == "bazel" || name == "bazelisk");
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
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if bazel {
        // Preserve Bazel's existing cache namespace; callers disable home rc.
        for key in ["HOME", "USER", "XDG_CACHE_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
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
