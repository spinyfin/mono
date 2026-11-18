use super::*;
use crate::{boss_bin_in, cube_bin_in, write_boss_launcher, write_cube_launcher};
use std::process::Command;

#[test]
fn project_path_prepends_beat_host_tools_in_worker_shells() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("worker bin's");
    let bundle = root.path().join("bundle bin's");
    let host = root.path().join("host");
    let project = root.path().join("project bin's");
    for dir in [&host, &project] {
        write_launcher(dir, "python", "#!/bin/sh\nexit 0\n").unwrap();
        write_launcher(dir, "checkleft", "#!/bin/sh\nexit 92\n").unwrap();
    }
    write_launcher(&bundle, "boss", "#!/bin/sh\nexit 0\n").unwrap();
    write_repo_tool_launcher(&bin, "checkleft", None).unwrap();
    write_shell_environment(&bin).unwrap();
    #[cfg(target_os = "macos")]
    let shells = ["/bin/bash", "/bin/zsh"];
    #[cfg(not(target_os = "macos"))]
    let shells = ["/bin/bash"];
    for shell in shells {
        for bundled in [false, true] {
            let script = format!(
                "export PATH={}:{}:/usr/bin:/bin; {} {} -c {}",
                sh_quote(&bin.to_string_lossy()),
                sh_quote(&host.to_string_lossy()),
                shell_environment_clause(&bin),
                shell,
                sh_quote(&format!(
                    "export PATH={}:\"$PATH\"; command -v python; command -v checkleft; printf '%s\\n' \"$PATH\"; printf '%s\\n' \"$PATH\"",
                    sh_quote(&project.to_string_lossy()),
                )),
            );
            let mut command = Command::new("/bin/sh");
            command.env_remove("BOSS_BIN_DIR");
            if bundled {
                command.env("BOSS_BIN_DIR", &bundle);
            }
            let output = command.args(["-c", &script]).output().unwrap();
            assert!(output.status.success(), "{shell}: {output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<_> = stdout.lines().collect();
            assert_eq!(lines[0], project.join("python").to_str().unwrap(), "{shell}");
            assert_eq!(lines[1], bin.join("checkleft").to_str().unwrap(), "{shell}");
            let prefix = if bundled {
                format!("{}:{}:", bin.display(), bundle.display())
            } else {
                format!("{}:", bin.display())
            };
            assert!(lines[2].starts_with(&format!("{prefix}{}:", project.display())));
            assert_eq!(lines[2], lines[3], "restoring an intact prefix must be idempotent");
            assert_eq!(lines[2].matches(host.to_str().unwrap()).count(), 1);
        }
    }
}

#[test]
fn declarations_use_toml_syntax_and_ignore_string_contents() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    for section in ["tools", "pins"] {
        for declaration in [
            format!("[{section}.\"checkleft\"]\ntarget = '//:tool#name'"),
            format!("[ {section}.checkleft ]\ntarget = '//:tool'"),
            format!("[{section}]\n\"checkleft\" = {{ target = '//:tool' }}"),
            format!("{section}.checkleft.target = '//:tool'"),
            format!("{section} = {{ checkleft = {{ target = '//:tool' }} }}"),
        ] {
            std::fs::write(root.path().join(REPOBIN_CONFIG_NAME), &declaration).unwrap();
            sync_checkleft_launcher(&bin, root.path(), None).unwrap();
            assert!(bin.join("checkleft").is_file(), "{declaration}");
        }
    }
    for text in [
        "description = '''\n[tools.checkleft]\n'''",
        "description = \"\"\"\n[pins.checkleft]\n\"\"\"",
        "# [tools.checkleft]\nversion = 1",
        "[tools.checkleft]\ntarget =",
    ] {
        std::fs::write(root.path().join(REPOBIN_CONFIG_NAME), text).unwrap();
        write_repo_tool_launcher(&bin, "checkleft", None).unwrap();
        sync_checkleft_launcher(&bin, root.path(), None).unwrap();
        assert!(!bin.join("checkleft").exists(), "{text}");
    }
    std::fs::write(root.path().join(REPOBIN_CONFIG_NAME), "[tools.checkleft]").unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    assert!(!workspace_declares_repo_tool(&bin, "checkleft"));
}

/// A decoy `cube` and `boss` earlier on the inherited `PATH` — the shape of the
/// user's `~/bin` repobin shims — must lose to the Boss-bundled binaries in the
/// worker's tool shells, however a driver reaches its shell:
///
/// * `claude`: a shell snapshot re-exports `PATH` after startup files ran;
/// * `codex`: the driver demotes the launcher directory behind the decoys;
/// * `grok`: a login shell under a scoped `HOME` whose profile prepends them.
///
/// The launchers are the real ones the engine writes (`write_boss_launcher` /
/// `write_cube_launcher`), pinned to bundled stand-ins by absolute path. The
/// decoy fails loudly if it ever runs, as the shim's Xcode-pin build did.
#[test]
fn bundled_cube_and_boss_beat_decoys_on_the_inherited_path_for_every_driver_shell() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("worker bin's");
    let bundle = root.path().join("Boss.app/Contents/Resources/bin");
    let decoys = root.path().join("home bin");
    for name in ["boss", "cube"] {
        write_launcher(&bundle, name, &format!("#!/bin/sh\necho bundled-{name}\n")).unwrap();
        write_launcher(
            &decoys,
            name,
            &format!("#!/bin/sh\necho 'repobin: building //tools/{name}:{name}' >&2\nexit 36\n"),
        )
        .unwrap();
    }
    write_boss_launcher(&bin, Some(&bundle.join("boss"))).unwrap();
    write_cube_launcher(&bin, Some(&bundle.join("cube"))).unwrap();
    write_shell_environment(&bin).unwrap();

    let home = root.path().join("scoped home");
    std::fs::create_dir_all(&home).unwrap();
    let profile = format!("export PATH={}:\"$PATH\"\n", sh_quote(&decoys.to_string_lossy()));
    std::fs::write(home.join(".bash_profile"), &profile).unwrap();
    std::fs::write(home.join(".zprofile"), &profile).unwrap();
    let snapshot = root.path().join("snapshot.sh");
    std::fs::write(
        &snapshot,
        format!("export PATH={}:/usr/bin:/bin\n", sh_quote(&decoys.to_string_lossy())),
    )
    .unwrap();

    let probe = "command -v boss; boss; command -v cube; cube; \"$BOSS_BIN\"; \"$CUBE_BIN\"";
    #[cfg(target_os = "macos")]
    let shells = ["/bin/bash", "/bin/zsh"];
    #[cfg(not(target_os = "macos"))]
    let shells = ["/bin/bash"];
    for shell in shells {
        for (driver, tool_shell) in [
            (
                "claude",
                format!(
                    "{shell} -c {}",
                    sh_quote(&format!(
                        "source {} && eval {}",
                        sh_quote(&snapshot.to_string_lossy()),
                        sh_quote(probe)
                    ))
                ),
            ),
            (
                "codex",
                format!(
                    "export PATH={}:{}:\"$PATH\"; {shell} -c {}",
                    sh_quote(&decoys.to_string_lossy()),
                    sh_quote(&bin.to_string_lossy()),
                    sh_quote(probe)
                ),
            ),
            ("grok", format!("{shell} -lc {}", sh_quote(probe))),
        ] {
            let script = format!(
                "export PATH={}:/usr/bin:/bin; export BOSS_BIN={} CUBE_BIN={}; {} {tool_shell}",
                sh_quote(&decoys.to_string_lossy()),
                sh_quote(&boss_bin_in(&bin).to_string_lossy()),
                sh_quote(&cube_bin_in(&bin).to_string_lossy()),
                shell_environment_clause(&bin),
            );
            let output = Command::new("/bin/sh")
                .env("HOME", &home)
                .env("BOSS_BIN_DIR", &bundle)
                .args(["-c", &script])
                .output()
                .unwrap();
            assert!(output.status.success(), "{driver}/{shell}: {output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                stdout.lines().collect::<Vec<_>>(),
                [
                    bin.join("boss").to_str().unwrap(),
                    "bundled-boss",
                    bin.join("cube").to_str().unwrap(),
                    "bundled-cube",
                    "bundled-boss",
                    "bundled-cube",
                ],
                "{driver}/{shell}: bare names must resolve to the worker launchers, never a decoy",
            );
        }
    }
}
