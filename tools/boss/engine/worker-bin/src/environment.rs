//! Worker-owned repository tools and shell environment, applied after pane startup.
//!
//! The initial login shell discovers host tools. Once composed, that PATH has
//! priority in tool shells: a second login/profile pass cannot shadow its tools.

use std::io;
use std::path::{Path, PathBuf};

use crate::{sh_quote, write_launcher};

/// Install a repository tool using the engine's repobin dispatcher. Unlike the
/// Boss CLIs, repository tools must track the leased checkout, not the engine
/// release. repobin already implements config discovery, Bazel builds and caching.
/// Missing dispatchers fail closed instead of falling through to a host binary.
pub fn write_repo_tool_launcher(dir: &Path, name: &str, repobin: Option<&Path>) -> io::Result<PathBuf> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid repository tool name",
        ));
    }
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

/// Materialize shell startup files next to (not inside) the executable directory.
/// Bash reads BASH_ENV for noninteractive commands, including `bash -lc`;
/// zsh reads .zshenv even for noninteractive commands. Disabling further zsh
/// startup files prevents path_helper and user profiles from undoing the seal.
pub fn write_shell_environment(bin_dir: &Path) -> io::Result<PathBuf> {
    let dir = environment_dir(bin_dir);
    std::fs::create_dir_all(&dir)?;
    // Drivers may add private helper binaries after spawn (e.g. Codex's arg0
    // directory). Preserve those additions behind the composed toolchain.
    let restore = "case \"$PATH\" in\n\
        \"$BOSS_WORKER_PATH\"|\"$BOSS_WORKER_PATH\":*) ;;\n\
        *) export PATH=\"$BOSS_WORKER_PATH:$PATH\" ;;\n\
        esac\n";
    write_launcher(&dir, "bash-env", restore)?;
    write_launcher(&dir, ".zshenv", &format!("{restore}unsetopt RCS GLOBAL_RCS\n"))?;
    Ok(dir)
}

/// Apply the same composed environment to every local driver. Capture PATH only
/// after the pane's login setup and engine-owned launcher prepends have finished.
/// Existing driver auth/environment directives must be applied before this seal.
pub fn shell_environment_clause(bin_dir: &Path) -> String {
    let dir = environment_dir(bin_dir);
    format!(
        "export BOSS_WORKER_PATH=\"$PATH\"; export BASH_ENV={}; export ZDOTDIR={}; ",
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
}
