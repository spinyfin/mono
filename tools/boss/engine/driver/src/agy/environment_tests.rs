use super::*;
use std::time::Duration;
use tempfile::TempDir;

fn output(success: bool, stderr: &str) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn signature_integrity_and_vendor_requirement_precede_display() {
    let path = Path::new("/candidate/agy");
    let mut calls = 0;
    assert!(
        verify_codesign_with(path, |args| {
            calls += 1;
            assert_eq!(&args[..2], &["--verify", "--strict"]);
            assert!(args[3].starts_with("=anchor apple generic"));
            assert!(args[3].contains("EQHXZ8M8AV"));
            Ok(output(false, "invalid signature"))
        })
        .is_err()
    );
    assert_eq!(calls, 1);
    for details in ["Identifier=cli\nTeamIdentifier=OTHER", "TeamIdentifier=EQHXZ8M8AV"] {
        assert!(
            verify_codesign_with(path, |args| Ok(output(
                true,
                if args[0] == "--verify" { "" } else { details }
            )))
            .is_err()
        );
    }
    let captured =
        "Executable=/Applications/agy\nIdentifier=cli\nFormat=Mach-O thin (arm64)\nTeamIdentifier=EQHXZ8M8AV\n";
    assert_eq!(codesign_field(captured, "Identifier"), Some("cli"));
    assert_eq!(codesign_field(captured, "TeamIdentifier"), Some("EQHXZ8M8AV"));
    assert_eq!(codesign_field("TeamIdentifier=OTHER", "Identifier"), None);
    verify_codesign_with(path, |_| Ok(output(true, captured))).unwrap();
}

#[test]
fn concurrent_resolution_publishes_one_protected_complete_binary() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, "#!/bin/sh\ntest \"$AGY_CLI_DISABLE_AUTO_UPDATE\" = true || exit 9\ntest \"$HOME\" != \"$HOST_HOME\" || exit 8\necho 1.5.7\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let _guard = EnvGuard::set(&[
        (AGY_SKIP_BINARY_ASSERT_ENV, "0"),
        (AGY_BIN_ENV, source.to_str().unwrap()),
        ("HOST_HOME", std::env::var("HOME").unwrap().as_str()),
    ]);
    let root = tmp.path().join("homes");
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| resolve_agy_binary_with(&root, |path| verify_agy_binary_with(path, |_| Ok(()))).unwrap())
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), root.join("bin/agy"));
        }
    });
    let owned = root.join("bin/agy");
    assert_eq!(fs::metadata(&owned).unwrap().mode() & 0o777, 0o555);
    assert_eq!(fs::metadata(root.join("bin")).unwrap().mode() & 0o777, 0o555);
    assert_eq!(fs::read(&owned).unwrap(), fs::read(&source).unwrap());
    verify_cached_agy_binary(&owned).unwrap();
    fs::set_permissions(&owned, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&owned, "#!/bin/sh\necho 1.5.5\n").unwrap();
    fs::set_permissions(&owned, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(verify_cached_agy_binary(&owned).is_err());
    let error = verify_agy_binary_with(&owned, |_| Ok(())).unwrap_err();
    assert!(error.to_string().contains("below the qualified minimum"), "{error:#}");
    fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn failed_install_restores_directory_permissions() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, "dummy").unwrap();
    let bin = tmp.path().join("bin");
    assert!(install_boss_owned_agy(&source, &bin.join("agy"), |_| bail!("invalid signature")).is_err());
    assert_eq!(fs::metadata(&bin).unwrap().mode() & 0o777, 0o555);
    assert_eq!(fs::read_dir(&bin).unwrap().count(), 0);
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
}

use super::super::test_environment::EnvGuard;

#[test]
fn disable_auto_update_value_is_the_measured_true_literal() {
    assert_eq!(DISABLE_AUTO_UPDATE_VALUE, "true");
    assert_ne!(DISABLE_AUTO_UPDATE_VALUE, "1");
}

#[test]
fn version_gate_accepts_qualified_releases() {
    assert!(version_at_least("1.5.7", MIN_QUALIFIED_AGY_VERSION));
    assert!(version_at_least("1.5.9", MIN_QUALIFIED_AGY_VERSION));
    assert!(version_at_least("1.6.0", MIN_QUALIFIED_AGY_VERSION));
    assert!(!version_at_least("1.5.5", MIN_QUALIFIED_AGY_VERSION));
    assert!(!version_at_least("1.1.12", MIN_QUALIFIED_AGY_VERSION));
}

#[test]
fn parse_version_from_plain_and_prefixed_output() {
    assert_eq!(parse_agy_version("1.5.9\n").unwrap(), "1.5.9");
    assert_eq!(parse_agy_version("agy 1.5.9\n").unwrap(), "1.5.9");
}

#[test]
fn directives_scope_home_and_disable_the_updater() {
    let tmp = TempDir::new().unwrap();
    let host_home = tmp.path().join("host-home");
    fs::create_dir_all(&host_home).unwrap();
    let _guard = EnvGuard::set(&[
        ("HOME", host_home.to_str().unwrap()),
        ("BOSS_WORKER_TOOL_PATH", "/opt/boss-worker-tools"),
    ]);
    let env = AgyProcessEnvironment::resolve(
        Path::new("/run/process-home"),
        Path::new("/run/log/agy.log"),
        Path::new("/run/control/session-settings.json"),
        PathBuf::from("/run/bin/agy"),
    )
    .unwrap();
    assert_eq!(env.log_file(), Path::new("/run/log/agy.log"));
    assert_eq!(
        env.session_settings_path(),
        Path::new("/run/control/session-settings.json")
    );
    let directives = env.directives();
    assert_eq!(
        env.launch_args(),
        vec![
            std::ffi::OsString::from("--log-file"),
            "/run/log/agy.log".into(),
            "--add-dir".into(),
            "/run/rules-root".into(),
        ]
    );
    assert!(
        directives
            .iter()
            .any(|d| matches!(d, EnvDirective::Set(k, v) if k == "HOME" && v == "/run/process-home"))
    );
    assert!(
        directives
            .iter()
            .any(|d| matches!(d, EnvDirective::Set(k, v) if k == DISABLE_AUTO_UPDATE_ENV && v == "true"))
    );
    assert!(
        directives
            .iter()
            .any(|d| matches!(d, EnvDirective::Set(k, v) if k == HIDE_ACCOUNT_INFO_ENV && v == "true"))
    );
    assert!(directives.iter().any(
        |d| matches!(d, EnvDirective::Set(k, v) if k == "BOSS_WORKER_TOOL_PATH" && v == "/opt/boss-worker-tools")
    ));
}

#[test]
fn skip_binary_assert_accepts_a_missing_file() {
    let _guard = EnvGuard::set(&[(AGY_SKIP_BINARY_ASSERT_ENV, "1")]);
    verify_agy_binary(Path::new("/definitely/missing/agy")).unwrap();
}

#[test]
fn run_agy_bounded_timeout_names_the_command() {
    let mut command = Command::new("/bin/sleep");
    command.arg("60");
    let error = run_agy_bounded(&mut command, Duration::from_millis(300))
        .unwrap_err()
        .to_string();
    assert!(error.contains("Antigravity provisioning failed"), "{error}");
    assert!(error.contains("`/bin/sleep 60`"), "{error}");
    assert!(error.contains("did not complete within"), "{error}");
}

#[cfg(target_os = "macos")]
#[test]
fn real_codesign_accepts_literal_requirement() {
    let requirement = codesign_literal_requirement("anchor apple");
    let result = Command::new("/usr/bin/codesign")
        .args(codesign_verify_args(&requirement))
        .arg("/usr/bin/true")
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
}

#[test]
fn changed_override_is_rejected_after_installation() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, "original").unwrap();
    let _guard = EnvGuard::set(&[(AGY_BIN_ENV, source.to_str().unwrap())]);
    let root = tmp.path().join("homes");
    let owned = resolve_agy_binary_with(&root, |_| Ok(())).unwrap();
    resolve_agy_binary_with(&root, |_| Ok(())).unwrap();
    fs::write(&source, "replacement").unwrap();
    let error = resolve_agy_binary_with(&root, |_| Ok(())).unwrap_err();
    assert!(error.to_string().contains("override differs from pinned copy"));
    assert_eq!(fs::read(&owned).unwrap(), b"original");
    fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn missing_host_binary_is_typed_not_installed() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("no-agy");
    let _guard = EnvGuard::set(&[(AGY_BIN_ENV, missing.to_str().unwrap())]);
    let error = resolve_agy_binary_with(&tmp.path().join("homes"), |_| Ok(())).unwrap_err();
    assert!(error.downcast_ref::<AgyNotInstalled>().is_some(), "{error:#}");
}
