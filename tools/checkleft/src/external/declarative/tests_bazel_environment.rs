use super::*;
use crate::external::environment::prepare;

fn commands(root: &Path, launcher: &Path) -> Vec<Command> {
    let binaries = BTreeMap::new();
    let context = CheckExecutionContext {
        repo_root: root,
        check_id: "lint/rust",
        deadline: CheckDeadline::new(5_000),
        environment: None,
        binaries: Some(&binaries),
    };
    let mut commands = Vec::new();
    for operation in ["query", "build", "cquery"] {
        let mut args = super::super::ci_bazel_startup_flags();
        args.push(operation.into());
        commands.push(invocation_command(context, launcher, &args).unwrap());
    }
    for build in [true, false] {
        commands.push(resolve::bazel_resolve_command(root, launcher, "//:tool", build));
    }
    commands
}

#[test]
fn production_commands_preserve_environment_and_startup_flags() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join(".bazelversion"), "7.4.1\n").unwrap();
    for launcher in ["bazel", "bazelisk"] {
        for mut command in commands(repo.path(), Path::new(launcher)) {
            // Control the child's inherited selector without mutating process globals.
            command.env_remove("USE_BAZEL_VERSION");
            let before: Vec<_> = command.get_args().map(|v| v.to_owned()).collect();
            let flags: Vec<std::ffi::OsString> = super::super::ci_bazel_startup_flags()
                .into_iter()
                .map(Into::into)
                .collect();
            assert_eq!(&before[..flags.len()], flags.as_slice());
            assert!(["query", "build", "cquery"].iter().any(|v| before[flags.len()] == *v));
            let _home = prepare(&mut command).unwrap();
            assert_eq!(command.get_args().collect::<Vec<_>>(), before);
            let env: BTreeMap<_, _> = command.get_envs().collect();
            for key in [
                "PATH",
                "HOME",
                "LC_ALL",
                "LANG",
                "TZ",
                "XDG_CACHE_HOME",
                "BAZELISK_HOME",
            ] {
                assert!(!env.contains_key(std::ffi::OsStr::new(key)), "{key}");
            }
            assert_eq!(env.len(), 1);
            assert_eq!(
                env[std::ffi::OsStr::new("USE_BAZEL_VERSION")],
                Some(std::ffi::OsStr::new("7.4.1"))
            );
        }
    }
}

#[test]
fn conflicting_version_selectors_fail_with_actionable_error() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join(".bazelversion"), "7.4.1").unwrap();
    for version in ["latest", "", "8.0.0"] {
        for mut command in commands(repo.path(), Path::new("bazelisk")) {
            command.env("USE_BAZEL_VERSION", version);
            let error = prepare(&mut command).unwrap_err().to_string();
            assert!(error.contains("conflicts with repository Bazel pin 7.4.1"));
            assert!(error.contains("unset USE_BAZEL_VERSION"));
        }
    }
}

#[cfg(unix)]
#[test]
fn hostile_home_rc_cannot_override_workspace_pin_or_move_cache() {
    use std::os::unix::fs::PermissionsExt;
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".bazeliskrc"), "USE_BAZEL_VERSION=0.0.0-hostile\n").unwrap();
    std::fs::write(repo.path().join(".bazelversion"), "7.4.1\n").unwrap();
    let launcher = repo.path().join("bazelisk");
    // Model Bazelisk's env-over-rc selection and default HOME-based cache.
    std::fs::write(&launcher, "#!/bin/sh\nversion=$USE_BAZEL_VERSION\nif [ -z \"$version\" ]; then . \"$HOME/.bazeliskrc\"; version=$USE_BAZEL_VERSION; fi\nprintf '%s\\n%s\\n' \"$version\" \"$HOME/.cache/bazelisk\"\n").unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let baseline = Command::new(&launcher)
        .env("HOME", home.path())
        .env_remove("USE_BAZEL_VERSION")
        .output()
        .unwrap();
    assert!(baseline.status.success());
    assert_eq!(
        String::from_utf8(baseline.stdout).unwrap(),
        format!("0.0.0-hostile\n{}/.cache/bazelisk\n", home.path().display())
    );
    for pin in ["7.4.1", "8.0.0"] {
        if pin == "8.0.0" {
            std::fs::write(
                repo.path().join(".bazeliskrc"),
                "# workspace wins\nUSE_BAZEL_VERSION=8.0.0\n",
            )
            .unwrap();
        }
        for mut command in commands(repo.path(), &launcher) {
            command.env("HOME", home.path()).env_remove("USE_BAZEL_VERSION");
            let _isolated_home = prepare(&mut command).unwrap();
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{pin}\n{}/.cache/bazelisk\n", home.path().display())
            );
        }
    }
}
