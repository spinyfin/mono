//! Worker-owned repository tools and shell environment, applied after pane startup.
//!
//! The initial login shell discovers host tools. Only the worker-owned prefix
//! has priority in tool shells; project toolchains can still precede host tools.
//! A DEBUG trap repeats the restore before later commands, so a driver that
//! sources an `export PATH=...` snapshot after startup (Claude Code's Bash tool)
//! cannot demote the launcher directory.

use std::io;
use std::path::{Path, PathBuf};

use crate::{sh_quote, write_launcher};

const REPOBIN_CONFIG_NAME: &str = "REPOBIN.toml";

/// Restore snippet shared by bash-env and .zshenv. Drivers may add private
/// helper directories after spawn (e.g. Codex's arg0 directory); those stay
/// reachable behind the worker-owned tools.
const PATH_RESTORE: &str = "case \"$PATH\" in\n\
    \"$BOSS_WORKER_TOOL_PATH\"|\"$BOSS_WORKER_TOOL_PATH\":*) ;;\n\
    *) export PATH=\"$BOSS_WORKER_TOOL_PATH:$PATH\" ;;\n\
    esac\n";

/// Re-run the PATH restore before each subsequent command. Bash DEBUG fires
/// before the command; zsh needs `DEBUG_BEFORE_CMD` for the same timing.
/// Single-quoted so `$PATH` expands when the trap fires, not when it is set.
const PATH_DEBUG_TRAP: &str = "trap 'case \"$PATH\" in\n\
    \"$BOSS_WORKER_TOOL_PATH\"|\"$BOSS_WORKER_TOOL_PATH\":*) ;;\n\
    *) export PATH=\"$BOSS_WORKER_TOOL_PATH:$PATH\" ;;\n\
    esac' DEBUG\n";

/// Install a repository tool using the engine's repobin dispatcher. Unlike the
/// Boss CLIs, repository tools must track the leased checkout, not the engine
/// release. repobin already implements config discovery, Bazel builds and caching.
/// Missing dispatchers fail closed instead of falling through to a host binary.
pub fn write_repo_tool_launcher(dir: &Path, name: &str, repobin: Option<&Path>) -> io::Result<PathBuf> {
    validate_repo_tool_name(name)?;
    let script = match repobin {
        Some(binary) => format!(
            "#!/bin/sh\nexec {} exec {} \"$@\"\n",
            sh_quote(&binary.to_string_lossy()),
            sh_quote(name),
        ),
        None => format!(
            "#!/bin/sh\nprintf '%s\\n' {} >&2\nexit 127\n",
            sh_quote(&format!(
                "{name}: engine-owned repobin unavailable; refusing a host PATH fallback"
            )),
        ),
    };
    write_launcher(dir, name, &script)
}

/// Drop a previously written repository-tool launcher. Missing files are fine:
/// a workspace that never declared the tool must not fail spawn.
pub fn remove_repo_tool_launcher(dir: &Path, name: &str) -> io::Result<()> {
    validate_repo_tool_name(name)?;
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Write `checkleft` only when the workspace root's `REPOBIN.toml` declares it
/// as a tool or pin. Otherwise remove a leftover launcher so host lookup
/// remains available in repos that have no checkleft entry (or no config).
pub fn sync_checkleft_launcher(dir: &Path, workspace: &Path, repobin: Option<&Path>) -> io::Result<()> {
    if workspace_declares_repo_tool(workspace, "checkleft") {
        write_repo_tool_launcher(dir, "checkleft", repobin).map(|_| ())
    } else {
        remove_repo_tool_launcher(dir, "checkleft")
    }
}

/// True when `<workspace>/REPOBIN.toml` declares `name` under `[tools]` or
/// `[pins]`. The search is the workspace root only: cube leases a checkout
/// root, and walking ancestors would pick up an unrelated config from a
/// Bazel execroot during tests.
pub fn workspace_declares_repo_tool(workspace: &Path, name: &str) -> bool {
    let path = workspace.join(REPOBIN_CONFIG_NAME);
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|text| toml_declares_tool(&text, name))
}

fn validate_repo_tool_name(name: &str) -> io::Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid repository tool name",
        ));
    }
    Ok(())
}

fn toml_declares_tool(text: &str, name: &str) -> bool {
    if validate_repo_tool_name(name).is_err() {
        return false;
    }
    #[derive(serde::Deserialize)]
    struct ToolDeclarations {
        #[serde(default)]
        tools: std::collections::BTreeMap<String, toml::Value>,
        #[serde(default)]
        pins: std::collections::BTreeMap<String, toml::Value>,
    }
    toml::from_str::<ToolDeclarations>(text)
        .is_ok_and(|config| config.tools.contains_key(name) || config.pins.contains_key(name))
}

/// Materialize shell startup files next to (not inside) the executable directory.
/// Bash reads BASH_ENV for noninteractive commands, including `bash -lc`;
/// zsh reads .zshenv even for noninteractive commands. Disabling further zsh
/// startup files prevents path_helper and user profiles from undoing the seal.
/// The DEBUG trap repeats the PATH restore after a later snapshot `export PATH`.
pub fn write_shell_environment(bin_dir: &Path) -> io::Result<PathBuf> {
    let dir = environment_dir(bin_dir);
    std::fs::create_dir_all(&dir)?;
    write_launcher(&dir, "bash-env", &format!("{PATH_RESTORE}{PATH_DEBUG_TRAP}"))?;
    write_launcher(
        &dir,
        ".zshenv",
        &format!("{PATH_RESTORE}unsetopt RCS GLOBAL_RCS\nsetopt DEBUG_BEFORE_CMD\n{PATH_DEBUG_TRAP}"),
    )?;
    Ok(dir)
}

/// Seal only the worker launcher and optional bundle directories for every driver.
/// Host PATH entries remain behind project toolchain prepends.
/// Existing driver auth/environment directives must be applied before this seal.
pub fn shell_environment_clause(bin_dir: &Path) -> String {
    let dir = environment_dir(bin_dir);
    format!(
        "export BOSS_WORKER_TOOL_PATH={}\"${{BOSS_BIN_DIR:+:$BOSS_BIN_DIR}}\"; export BASH_ENV={}; export ZDOTDIR={}; ",
        sh_quote(&bin_dir.to_string_lossy()),
        sh_quote(&dir.join("bash-env").to_string_lossy()),
        sh_quote(&dir.to_string_lossy()),
    )
}

fn environment_dir(bin_dir: &Path) -> PathBuf {
    let mut name = bin_dir.as_os_str().to_os_string();
    name.push(".environment");
    PathBuf::from(name)
}

#[cfg(test)]
#[path = "environment_regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn tool_shells_restore_the_composed_path_and_propagate_failures() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("worker bin's");
        let stale = root.path().join("cargo/bin");
        let helpers = root.path().join("driver-helpers");
        write_launcher(&helpers, "driver-helper", "#!/bin/sh\nexit 0\n").unwrap();
        let dispatcher = write_launcher(
            root.path(),
            "repobin",
            "#!/bin/sh\n[ \"$1/$2/$3\" = exec/checkleft/run ] || exit 91\nexit 37\n",
        )
        .unwrap();
        write_launcher(&stale, "checkleft", "#!/bin/sh\nexit 92\n").unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let profile = format!("export PATH={}:\"$PATH\"\n", sh_quote(&stale.to_string_lossy()));
        std::fs::write(home.join(".bash_profile"), &profile).unwrap();
        std::fs::write(home.join(".zprofile"), &profile).unwrap();
        write_repo_tool_launcher(&bin, "checkleft", Some(&dispatcher)).unwrap();
        write_shell_environment(&bin).unwrap();
        #[cfg(target_os = "macos")]
        let shells = ["/bin/bash", "/bin/zsh"];
        #[cfg(not(target_os = "macos"))]
        let shells = ["/bin/bash"];
        for shell in shells {
            // Model a driver demoting our launcher before starting its tool
            // shell. The shell must recover the composed PATH before lookup.
            let script = format!(
                "export PATH={}:{}:/usr/bin:/bin; {} export PATH={}:{}:\"$PATH\"; {} -lc 'driver-helper || exit 93; command -v checkleft; checkleft run'",
                sh_quote(&bin.to_string_lossy()),
                sh_quote(&stale.to_string_lossy()),
                shell_environment_clause(&bin),
                sh_quote(&stale.to_string_lossy()),
                sh_quote(&helpers.to_string_lossy()),
                shell,
            );
            let result = Command::new("/bin/sh")
                .env("HOME", &home)
                .args(["-c", &script])
                .output()
                .unwrap();
            assert_eq!(result.status.code(), Some(37), "{shell}: {result:?}");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap().trim(),
                bin.join("checkleft").display().to_string(),
                "{shell} must run the workspace launcher",
            );
        }
    }

    #[test]
    fn missing_dispatcher_never_runs_a_host_checkleft() {
        let root = tempfile::tempdir().unwrap();
        let launcher = write_repo_tool_launcher(root.path(), "checkleft", None).unwrap();
        let result = Command::new(launcher).arg("run").output().unwrap();
        assert_eq!(result.status.code(), Some(127));
        assert!(
            String::from_utf8(result.stderr)
                .unwrap()
                .contains("refusing a host PATH fallback")
        );
        assert!(write_repo_tool_launcher(root.path(), "../escape", None).is_err());
    }

    #[test]
    fn tool_shells_restore_path_after_a_snapshot_export() {
        // Claude Code's Bash tool sources a snapshot that re-exports PATH
        // after BASH_ENV / .zshenv have already run:
        //   zsh -c "source <snapshot> 2>/dev/null || true && eval '<cmd>'"
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("worker bin's");
        let stale = root.path().join("cargo/bin");
        let helpers = root.path().join("driver-helpers");
        write_launcher(&helpers, "driver-helper", "#!/bin/sh\nexit 0\n").unwrap();
        let dispatcher = write_launcher(
            root.path(),
            "repobin",
            "#!/bin/sh\n[ \"$1/$2/$3\" = exec/checkleft/run ] || exit 91\nexit 37\n",
        )
        .unwrap();
        write_launcher(&stale, "checkleft", "#!/bin/sh\nexit 92\n").unwrap();
        write_repo_tool_launcher(&bin, "checkleft", Some(&dispatcher)).unwrap();
        write_shell_environment(&bin).unwrap();
        let snapshot = root.path().join("snapshot.sh");
        std::fs::write(
            &snapshot,
            format!(
                "export PATH={}:{}\n",
                sh_quote(&stale.to_string_lossy()),
                sh_quote(&helpers.to_string_lossy()),
            ),
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        let shells = ["/bin/bash", "/bin/zsh"];
        #[cfg(not(target_os = "macos"))]
        let shells = ["/bin/bash"];
        for shell in shells {
            let script = format!(
                "export PATH={}:{}:/usr/bin:/bin; {} {} -c {}",
                sh_quote(&bin.to_string_lossy()),
                sh_quote(&stale.to_string_lossy()),
                shell_environment_clause(&bin),
                shell,
                sh_quote(&format!(
                    "source {} 2>/dev/null || true && eval 'driver-helper || exit 93; command -v checkleft; checkleft run'",
                    sh_quote(&snapshot.to_string_lossy()),
                )),
            );
            let result = Command::new("/bin/sh").args(["-c", &script]).output().unwrap();
            assert_eq!(result.status.code(), Some(37), "{shell}: {result:?}");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap().trim(),
                bin.join("checkleft").display().to_string(),
                "{shell} must run the workspace launcher after a snapshot PATH export",
            );
        }
    }

    #[test]
    fn workspace_declares_checkleft_from_tools_or_pins_tables() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path();
        assert!(!workspace_declares_repo_tool(workspace, "checkleft"));

        std::fs::write(
            workspace.join("REPOBIN.toml"),
            "version = 1\n[tools.buildifier]\ntarget = \"//:buildifier\"\n",
        )
        .unwrap();
        assert!(!workspace_declares_repo_tool(workspace, "checkleft"));

        std::fs::write(
            workspace.join("REPOBIN.toml"),
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
        )
        .unwrap();
        assert!(workspace_declares_repo_tool(workspace, "checkleft"));

        std::fs::write(
            workspace.join("REPOBIN.toml"),
            "version = 1\n[pins.checkleft]\nrepo = \"https://example.invalid/mono.git\"\ntag = \"v1\"\n",
        )
        .unwrap();
        assert!(workspace_declares_repo_tool(workspace, "checkleft"));

        std::fs::write(
            workspace.join("REPOBIN.toml"),
            "version = 1\n[tools]\ncheckleft = { target = \"//tools/checkleft:checkleft\" }\n",
        )
        .unwrap();
        assert!(workspace_declares_repo_tool(workspace, "checkleft"));

        std::fs::write(workspace.join("REPOBIN.toml"), "# [tools.checkleft]\nversion = 1\n").unwrap();
        assert!(!workspace_declares_repo_tool(workspace, "checkleft"));

        sync_checkleft_launcher(workspace, workspace, None).unwrap();
        assert!(
            !workspace.join("checkleft").exists(),
            "a leftover launcher must be removed when the workspace does not declare checkleft"
        );
        std::fs::write(
            workspace.join("REPOBIN.toml"),
            "version = 1\n[tools.checkleft]\ntarget = \"//tools/checkleft:checkleft\"\n",
        )
        .unwrap();
        sync_checkleft_launcher(workspace, workspace, None).unwrap();
        assert!(workspace.join("checkleft").is_file());
    }
}
