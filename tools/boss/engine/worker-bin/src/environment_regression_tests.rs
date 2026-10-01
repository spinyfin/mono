use super::*;
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
