use crate::app::CubeError;
use crate::command_runner::CommandInvocation;
use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::setup_cleanup::read_cleanup_step;
use boss_engine_test_git::jj::JjRepo;
use std::path::Path;
use std::time::Duration;

struct DeclaredJj;
fn declared(invocation: &CommandInvocation) -> CommandInvocation {
    let mut invocation = invocation.clone();
    if invocation.program == "jj" {
        invocation.program = std::fs::canonicalize(JjRepo::binary()).unwrap().display().to_string();
    }
    invocation
}
impl CommandRunner for DeclaredJj {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        RealCommandRunner.run(&declared(invocation))
    }
    fn run_to_file(
        &self,
        invocation: &CommandInvocation,
        destination: &Path,
        timeout: Duration,
    ) -> Result<(), CubeError> {
        RealCommandRunner.run_to_file(&declared(invocation), destination, timeout)
    }
}

#[test]
fn trunk_assets_override_stale_source_and_unmerged_workspace() {
    let _env = super::support::ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let fixture = JjRepo::new(temp.path());
    let root = &fixture.repo;
    std::fs::create_dir_all(root.join(".cube")).unwrap();
    std::fs::write(
        root.join(".cube/setup.yaml"),
        "version: 1\ncleanup:\n  command: 'sh \"$CUBE_CLEANUP_ROOT/.cube/clean.sh\"'\n",
    )
    .unwrap();
    let script = b"#!/bin/sh\ntouch \"$CUBE_WORKSPACE/cleaned.marker\"\n\n";
    std::fs::write(root.join(".cube/clean.sh"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.join(".cube/clean.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    JjRepo::run(root, &["bookmark", "create", "main", "-r", "@"]);
    JjRepo::run(root, &["git", "export"]);
    // A local origin provides a real remote-tracking bookmark without network.
    let source = temp.path().join("source");
    JjRepo::run(
        root,
        &[
            "git",
            "clone",
            root.join(".jj/repo/store/git").to_str().unwrap(),
            source.to_str().unwrap(),
        ],
    );
    JjRepo::run(&source, &["new", "root()", "-m", "stale source working copy"]);
    assert!(!source.join(".cube").exists());
    std::fs::create_dir_all(fixture.worker.join(".cube")).unwrap();
    std::fs::write(
        fixture.worker.join(".cube/setup.yaml"),
        "version: 1\ncleanup:\n  command: false\n",
    )
    .unwrap();

    let resolved = read_cleanup_step(&DeclaredJj, &fixture.worker, Some(&source), "main")
        .unwrap()
        .unwrap();
    let exported = resolved.root.as_ref().unwrap();
    assert_eq!(std::fs::read(exported.join(".cube/clean.sh")).unwrap(), script);
    assert_ne!(
        std::fs::metadata(exported.join(".cube/clean.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    let mut command = RealCommandRunner::invocation(&fixture.worker, "sh", &["-c", &resolved.command]);
    command.env = vec![
        ("CUBE_CLEANUP_ROOT".into(), exported.display().to_string()),
        ("CUBE_WORKSPACE".into(), fixture.worker.display().to_string()),
    ];
    RealCommandRunner.run(&command).unwrap();
    assert!(fixture.worker.join("cleaned.marker").exists());
    let exported = exported.clone();
    drop(resolved);
    assert!(!exported.exists(), "snapshot is removed after use");

    std::fs::remove_dir_all(root.join(".cube")).unwrap();
    JjRepo::run(root, &["status"]);
    JjRepo::run(root, &["git", "export"]);
    JjRepo::run(&source, &["git", "fetch"]);
    assert!(
        read_cleanup_step(&DeclaredJj, &fixture.worker, Some(&source), "main")
            .unwrap()
            .is_none(),
        "removing trunk cleanup must not enable an unmerged declaration"
    );
    assert!(
        read_cleanup_step(&DeclaredJj, &fixture.worker, Some(&source), "missing").is_err(),
        "a trunk lookup failure must not enable an unmerged declaration"
    );
}

#[test]
fn detached_startup_failures_append_to_the_log() {
    let _env = crate::app::tests::support::ENV_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("periodic-cleanup.log");
    std::fs::write(&log, "previous pass\n").unwrap();
    // A database path that is a directory fails before the pass can open.
    let mut command = std::process::Command::new(std::env::var_os("CUBE_TEST_BIN").unwrap());
    command.arg("periodic-cleanup").arg("--database").arg(temp.path());
    let mut child = crate::app::periodic_cleanup::spawn_command(&mut command, &log).unwrap();
    assert!(!child.wait().unwrap().success());
    let contents = std::fs::read_to_string(log).unwrap();
    assert!(contents.starts_with("previous pass\n"));
    assert!(contents.contains("unable to open database"), "{contents}");
}
