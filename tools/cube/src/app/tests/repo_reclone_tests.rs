use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;

use crate::cli::Cli;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::metadata::WorkspaceCandidate;
use crate::store::Store;

use super::support::{ExpectedCommand, FakeRunner, audit_events, seed_mono_repo, with_database_path, write_setup_yaml};
use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::{CubeError, Result};
use crate::app::repo_reclone::{quarantine_obsolete, stamp_registration, stamp_workspace};

const SETUP: &str = "version: 1\nsteps:\n  - id: secrets\n    command: cp \"$CUBE_BASE_REPO/backend/config-secrets.toml\" .\n    run_when: on-create\n";

struct CloneRunner {
    source: PathBuf,
    busy: bool,
    fail_clone: bool,
    untracked: Vec<String>,
    setup: &'static str,
}

impl CommandRunner for CloneRunner {
    fn run(&self, cmd: &CommandInvocation) -> Result<String> {
        match (cmd.program.as_str(), cmd.args.first().map(String::as_str)) {
            ("lsof", _) => {
                assert!(cmd.args.contains(&"+D".into()));
                if self.busy {
                    Ok("p123\ncworker\nnstore/index\n".into())
                } else {
                    Err(CubeError::CommandFailed {
                        program: "lsof".into(),
                        args: cmd.args.clone(),
                        status: Some(1),
                        stderr: String::new(),
                    })
                }
            }
            ("git", Some("ls-files")) => {
                assert!(cmd.args.contains(&":(exclude).jj".into()));
                assert!(!cmd.args.contains(&"--exclude-standard".into()));
                Ok(self.untracked.join("\0"))
            }
            ("git", Some("ls-remote")) => Ok("ref: refs/heads/main\tHEAD\n".into()),
            ("jj", Some("git")) => {
                assert_eq!(cmd.args[1], "clone");
                assert_eq!(cmd.args[3], "git@github.com:spinyfin/mono.git");
                assert_eq!(Path::new(&cmd.args[4]), self.source);
                fs::create_dir_all(self.source.join(".jj/repo")).unwrap();
                if self.fail_clone {
                    return Err(CubeError::InvalidArgument("clone interrupted".into()));
                }
                if !self.setup.is_empty() {
                    write_setup_yaml(&self.source, self.setup);
                }
                fs::write(self.source.join("fresh"), "fresh clone").unwrap();
                Ok(String::new())
            }
            ("jj", Some("bookmark")) => Ok(String::new()),
            _ => panic!("unexpected recovery command: {cmd:?}"),
        }
    }
}

fn fixture() -> (tempfile::TempDir, PathBuf, Store, CloneRunner, WorkspaceCandidate) {
    let (temp, database) = with_database_path();
    let root = temp.path().join("workspaces");
    seed_mono_repo(&root, &database);
    let mut store = Store::open_at(&database).unwrap();
    let source = store.get_repo("mono").unwrap().unwrap().source.unwrap();
    fs::create_dir_all(source.join(".jj/repo")).unwrap();
    fs::write(source.join(".jj/repo/damaged-index"), "keep me").unwrap();
    write_setup_yaml(&source, SETUP);
    fs::create_dir_all(source.join("backend")).unwrap();
    fs::write(source.join("backend/config-secrets.toml"), "secret").unwrap();
    let workspace = WorkspaceCandidate {
        workspace_id: "mono-agent-001".into(),
        workspace_path: root.join("mono-agent-001"),
    };
    fs::create_dir_all(workspace.workspace_path.join(".jj")).unwrap();
    store.sync_workspaces("mono", std::slice::from_ref(&workspace)).unwrap();
    let runner = CloneRunner {
        source,
        busy: false,
        fail_clone: false,
        untracked: vec!["backend/config-secrets.toml".into()],
        setup: SETUP,
    };
    (temp, database, store, runner, workspace)
}

#[test]
fn reclone_refuses_leased_workspaces_without_touching_store() {
    let (_temp, database, mut store, runner, _) = fixture();
    store
        .claim_workspace("mono", "worker", "work", "lease", 100, None, None)
        .unwrap()
        .unwrap();
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &FakeRunner::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("leased workspaces"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
}

#[test]
fn reclone_refuses_open_store_files() {
    let (_temp, database, _store, mut runner, _) = fixture();
    runner.busy = true;
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("processes are using"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
}

#[test]
fn reclone_keeps_backup_config_secrets_and_audit_and_resets_upkeep() {
    let (temp, database, store, runner, workspace) = fixture();
    let mut before = store.get_repo("mono").unwrap().unwrap();
    before.main_branch = "trunk".into();
    store.upsert_repo(&before).unwrap();
    store.set_pool_metadata_i("store_gc_oldest_jj_client:mono", 34).unwrap();
    store
        .set_pool_metadata_i("store_gc_consecutive_failures:mono", 9)
        .unwrap();
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    let backup = Path::new(result.payload["backup"].as_str().unwrap());
    assert!(
        backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("before-reclone-")
    );
    assert_eq!(
        fs::read_to_string(backup.join(".jj/repo/damaged-index")).unwrap(),
        "keep me"
    );
    assert!(runner.source.join("fresh").exists());
    assert_eq!(
        fs::read_to_string(runner.source.join("backend/config-secrets.toml")).unwrap(),
        "secret"
    );
    assert_eq!(store.get_repo("mono").unwrap().unwrap(), before);
    assert_eq!(
        store.get_pool_metadata_i("store_gc_oldest_jj_client:mono").unwrap(),
        Some(34)
    );
    assert_eq!(
        store.get_pool_metadata_i("store_gc_consecutive_failures:mono").unwrap(),
        None
    );
    assert!(!workspace.workspace_path.exists());
    assert!(store.list_workspaces("mono").unwrap().is_empty());
    assert!(
        audit_events(&temp)
            .iter()
            .any(|e| e.to_string().contains("repo.recloned"))
    );
    assert!(
        crate::setup::read_setup_config(&runner.source).unwrap().unwrap().steps[0]
            .command
            .contains("config-secrets.toml")
    );
}

#[test]
fn missing_setup_file_restores_original_store_and_names_file() {
    let (_temp, database, _store, runner, workspace) = fixture();
    fs::remove_file(runner.source.join("backend/config-secrets.toml")).unwrap();
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("backend/config-secrets.toml"));
    assert!(error.to_string().contains("restore this file"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
    assert!(workspace.workspace_path.exists());
}

#[test]
fn clone_failure_restores_original_and_preserves_failed_clone() {
    let (_temp, database, _store, mut runner, _) = fixture();
    runner.fail_clone = true;
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("clone interrupted"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
    assert!(
        fs::read_dir(runner.source.parent().unwrap()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("failed-reclone"))
    );
}

#[cfg(unix)]
#[test]
fn remove_purges_readonly_build_directories_without_following_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, database, store, _runner, workspace) = fixture();
    let build = workspace.workspace_path.join(".codex/bazel-merge-build/a/b");
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("output"), "build").unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "keep").unwrap();
    symlink(&outside, build.join("link")).unwrap();
    fs::set_permissions(&build, fs::Permissions::from_mode(0o500)).unwrap();
    store
        .set_pool_metadata_i("store_gc_consecutive_failures:mono", 9)
        .unwrap();
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono", "--purge-workspaces"]),
        Some(&database),
        &FakeRunner::default(),
    )
    .unwrap();
    assert!(!workspace.workspace_path.exists());
    assert!(outside.join("keep").exists());
    assert_eq!(
        store.get_pool_metadata_i("store_gc_consecutive_failures:mono").unwrap(),
        None
    );
}

#[test]
fn remove_failure_is_nonzero_and_keeps_registration() {
    let (_temp, database, store, _runner, workspace) = fixture();
    fs::remove_dir_all(&workspace.workspace_path).unwrap();
    fs::write(&workspace.workspace_path, "not a directory").unwrap();
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono", "--purge-workspaces"]),
        Some(&database),
        &FakeRunner::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("mono-agent-001"));
    assert_ne!(error.exit_code(), std::process::ExitCode::SUCCESS);
    assert!(store.get_repo("mono").unwrap().is_some());
}

#[test]
fn rediscovered_workspace_from_previous_registration_is_quarantined() {
    let (_temp, database, store, _runner, workspace) = fixture();
    let repo = store.get_repo("mono").unwrap().unwrap();
    stamp_registration(&repo).unwrap();
    stamp_workspace(&repo, &workspace.workspace_path).unwrap();
    stamp_registration(&repo).unwrap();
    let mut candidates = vec![workspace.clone()];
    quarantine_obsolete(&store, Some(&database), &repo, &mut candidates).unwrap();
    assert!(candidates.is_empty());
    assert!(!workspace.workspace_path.exists());
    assert!(
        fs::read_dir(&repo.workspace_root).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".quarantine-"))
    );
    assert!(store.list_workspaces("mono").unwrap().is_empty());
}

#[test]
fn rediscovered_workspace_pointing_at_another_store_is_quarantined() {
    let (temp, database, store, _runner, workspace) = fixture();
    let repo = store.get_repo("mono").unwrap().unwrap();
    let other = temp.path().join("other/.jj/repo");
    fs::create_dir_all(&other).unwrap();
    fs::write(workspace.workspace_path.join(".jj/repo"), other.display().to_string()).unwrap();
    let mut candidates = vec![workspace];
    quarantine_obsolete(&store, Some(&database), &repo, &mut candidates).unwrap();
    assert!(candidates.is_empty());
}

#[test]
fn remove_ensure_invalidates_old_workspace_and_setup_runs_for_new_registration() {
    let (_temp, database, mut store, runner, workspace) = fixture();
    let repo = store.get_repo("mono").unwrap().unwrap();
    stamp_registration(&repo).unwrap();
    stamp_workspace(&repo, &workspace.workspace_path).unwrap();
    write_setup_yaml(&workspace.workspace_path, SETUP);
    let setup_runner = || {
        FakeRunner::new(vec![ExpectedCommand::ok(
            workspace.workspace_path.clone(),
            "sh",
            &["-c", "cp \"$CUBE_BASE_REPO/backend/config-secrets.toml\" ."],
            "",
        )])
    };
    let first = setup_runner();
    let old_workspace = store.get_workspace_by_path(&workspace.workspace_path).unwrap().unwrap();
    assert_eq!(
        crate::app::provision::run_setup_for_workspace(&store, &first, &old_workspace)
            .unwrap()
            .ran_count(),
        1
    );
    first.assert_exhausted();
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "remove", "mono"]),
        Some(&database),
        &FakeRunner::default(),
    )
    .unwrap();
    let defaults = crate::app::repo::RepoEnsureDefaults {
        repo_root: runner.source.parent().unwrap().to_path_buf(),
        workspace_root: repo.workspace_root.clone(),
    };
    crate::app::dispatch::run_with_context(
        Cli::parse_from(["cube", "repo", "ensure", "--origin", &repo.origin]),
        Some(&database),
        &FakeRunner::default(),
        Some(&defaults),
        None,
    )
    .unwrap();
    let mut candidates = crate::app::provision::discover_workspaces(&repo).unwrap();
    quarantine_obsolete(&store, Some(&database), &repo, &mut candidates).unwrap();
    assert!(candidates.is_empty());
    fs::create_dir_all(workspace.workspace_path.join(".jj")).unwrap();
    stamp_workspace(&repo, &workspace.workspace_path).unwrap();
    write_setup_yaml(&workspace.workspace_path, SETUP);
    store.sync_workspaces("mono", std::slice::from_ref(&workspace)).unwrap();
    let new_workspace = store.get_workspace_by_path(&workspace.workspace_path).unwrap().unwrap();
    let second = setup_runner();
    assert_eq!(
        crate::app::provision::run_setup_for_workspace(&store, &second, &new_workspace)
            .unwrap()
            .ran_count(),
        1
    );
    second.assert_exhausted();
}

#[test]
fn workspace_from_previous_registration_cannot_report_empty_success() {
    let (_temp, _database, store, _runner, workspace) = fixture();
    let repo = store.get_repo("mono").unwrap().unwrap();
    stamp_registration(&repo).unwrap();
    let record = store.get_workspace_by_path(&workspace.workspace_path).unwrap().unwrap();
    let error = crate::app::provision::run_setup_for_workspace(&store, &FakeRunner::default(), &record).unwrap_err();
    assert!(error.to_string().contains("previous registration"));
    assert!(!error.to_string().contains("remove and recreate"));
    // A recovery lease must still reach the workspace holding unpushed work.
    crate::app::provision::run_setup_for_lease(&store, &FakeRunner::default(), &record, false).unwrap();
}

#[test]
fn source_setup_file_absent_from_main_does_not_fail_new_workspaces() {
    let (_temp, database, mut store, mut runner, workspace) = fixture();
    // Fresh main has no setup.yaml, so reclone plants the old one in the source only.
    runner.setup = "";
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    let repo = store.get_repo("mono").unwrap().unwrap();
    assert!(runner.source.join(".cube/setup.yaml").exists());
    fs::create_dir_all(workspace.workspace_path.join(".jj")).unwrap();
    stamp_workspace(&repo, &workspace.workspace_path).unwrap();
    store.sync_workspaces("mono", std::slice::from_ref(&workspace)).unwrap();
    let record = store.get_workspace_by_path(&workspace.workspace_path).unwrap().unwrap();
    let report = crate::app::provision::run_setup_for_workspace(&store, &FakeRunner::default(), &record).unwrap();
    assert_eq!(report.ran_count(), 0);
}

#[test]
fn setup_error_after_claim_releases_the_lease() {
    let (_temp, database, mut store, _runner, workspace) = fixture();
    let claimed = store
        .claim_workspace("mono", "worker", "work", "lease-x", 100, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.workspace_id, workspace.workspace_id);
    let result: Result<()> = crate::app::workspace::release_if_setup_not_run(
        &store,
        Some(&database),
        &claimed,
        "lease-x",
        Err(CubeError::InvalidArgument("setup declaration unreadable".into())),
    );
    assert!(result.is_err());
    assert!(
        store
            .list_workspaces("mono")
            .unwrap()
            .iter()
            .all(|w| w.state != crate::metadata::WorkspaceState::Leased)
    );
    // A successful setup keeps the claim.
    let claimed = store
        .claim_workspace("mono", "worker", "work", "lease-y", 100, None, None)
        .unwrap()
        .unwrap();
    crate::app::workspace::release_if_setup_not_run(&store, Some(&database), &claimed, "lease-y", Ok(())).unwrap();
    assert_eq!(
        store.list_workspaces("mono").unwrap()[0].state,
        crate::metadata::WorkspaceState::Leased
    );
}

#[cfg(unix)]
#[test]
fn incidental_untracked_symlinks_are_skipped_before_the_store_moves() {
    let (_temp, database, _store, mut runner, _) = fixture();
    std::os::unix::fs::symlink("/nonexistent/output", runner.source.join("bazel-bin")).unwrap();
    runner.untracked.push("bazel-bin".into());
    runner.untracked.push(" leading-space".into());
    fs::write(runner.source.join(" leading-space"), "kept").unwrap();
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    assert!(fs::symlink_metadata(runner.source.join("bazel-bin")).is_err());
    assert_eq!(
        fs::read_to_string(runner.source.join(" leading-space")).unwrap(),
        "kept"
    );
}

#[test]
fn pointer_file_source_does_not_quarantine_matching_workspaces() {
    let (temp, database, store, _runner, workspace) = fixture();
    let mut repo = store.get_repo("mono").unwrap().unwrap();
    let primary = temp.path().join("primary/.jj/repo");
    fs::create_dir_all(&primary).unwrap();
    let source = temp.path().join("secondary-source");
    fs::create_dir_all(source.join(".jj")).unwrap();
    fs::write(source.join(".jj/repo"), primary.display().to_string()).unwrap();
    repo.source = Some(source);
    fs::write(workspace.workspace_path.join(".jj/repo"), primary.display().to_string()).unwrap();
    let mut candidates = vec![workspace];
    quarantine_obsolete(&store, Some(&database), &repo, &mut candidates).unwrap();
    assert_eq!(candidates.len(), 1);
}

#[test]
fn dynamic_setup_inputs_fail_before_moving_the_store() {
    let (_temp, database, _store, runner, _) = fixture();
    write_setup_yaml(
        &runner.source,
        "version: 1\nsteps:\n  - id: secrets\n    command: cp $CUBE_BASE_REPO/$SECRET .\n",
    );
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("literal $CUBE_BASE_REPO/file"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
}

#[cfg(unix)]
#[test]
fn setup_input_symlinks_fail_with_actionable_error() {
    let (temp, database, _store, runner, _) = fixture();
    fs::remove_file(runner.source.join("backend/config-secrets.toml")).unwrap();
    let external = temp.path().join("secret");
    fs::write(&external, "external").unwrap();
    std::os::unix::fs::symlink(&external, runner.source.join("backend/config-secrets.toml")).unwrap();
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("replace it with a regular file"));
    assert_eq!(fs::read_to_string(external).unwrap(), "external");
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
}

#[test]
fn reclone_clears_its_old_maintenance_marker_after_replacing_store() {
    let (_temp, database, store, runner, _) = fixture();
    let gate_path = crate::command_runner::JjGate::path(database.parent().unwrap());
    let gate = crate::command_runner::JjGate::exclusive(&gate_path, super::support::EXCLUSIVE_GATE_WAIT).unwrap();
    gate.quarantine(&runner.source).unwrap();
    drop(gate);
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    assert!(!crate::command_runner::JjGate::quarantine_path(&gate_path).exists());
    assert!(store.get_repo("mono").unwrap().is_some());
}

#[test]
fn reclone_can_replace_a_store_with_missing_jj_metadata() {
    let (_temp, database, _store, runner, _) = fixture();
    fs::remove_dir_all(runner.source.join(".jj")).unwrap();
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    assert!(runner.source.join(".jj/repo").is_dir());
    assert!(
        Path::new(result.payload["backup"].as_str().unwrap())
            .join("backend/config-secrets.toml")
            .is_file()
    );
}

#[test]
fn unsupported_shell_expansion_does_not_silently_drop_setup_inputs() {
    let (_temp, database, _store, runner, _) = fixture();
    write_setup_yaml(
        &runner.source,
        "version: 1\nsteps:\n  - id: secrets\n    command: cp ${CUBE_BASE_REPO:?required}/backend/config-secrets.toml .\n",
    );
    let error = run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unsupported base-repo expansion"));
    assert!(runner.source.join(".jj/repo/damaged-index").exists());
}

#[test]
fn expired_obsolete_workspace_is_quarantined_before_health_probe_and_id_is_reserved() {
    let (_temp, database, mut store, clone_runner, workspace) = fixture();
    let repo = store.get_repo("mono").unwrap().unwrap();
    stamp_registration(&repo).unwrap();
    stamp_workspace(&repo, &workspace.workspace_path).unwrap();
    stamp_registration(&repo).unwrap();
    fs::remove_file(clone_runner.source.join(".cube/setup.yaml")).unwrap();
    store
        .claim_workspace("mono", "old-worker", "old work", "old-lease", 1, Some(2), None)
        .unwrap()
        .unwrap();
    let path = repo.workspace_root.join("mono-agent-002");
    let staging = repo.workspace_root.join(".incoming-mono-agent-002");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::workspace_add_mono(&repo.workspace_root, &staging),
        ExpectedCommand::ok(path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "fresh",
        ),
    ]);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "new work"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-002");
    assert!(!workspace.workspace_path.exists());
    runner.assert_exhausted();
}

#[test]
fn untracked_inputs_used_inside_setup_scripts_are_preserved() {
    let (_temp, database, _store, mut runner, _) = fixture();
    const SCRIPT_SETUP: &str = "version: 1\nsteps:\n  - id: secrets\n    command: sh tools/copy-local-config.sh\n";
    write_setup_yaml(&runner.source, SCRIPT_SETUP);
    runner.setup = SCRIPT_SETUP;
    fs::create_dir_all(runner.source.join("tools")).unwrap();
    fs::write(
        runner.source.join("tools/copy-local-config.sh"),
        "cp \"$CUBE_BASE_REPO/backend/private.env\" .",
    )
    .unwrap();
    fs::write(runner.source.join("backend/private.env"), "private setup input").unwrap();
    runner
        .untracked
        .extend(["tools/copy-local-config.sh".into(), "backend/private.env".into()]);
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "reclone", "mono"]),
        Some(&database),
        &runner,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(runner.source.join("backend/private.env")).unwrap(),
        "private setup input"
    );
    assert!(runner.source.join("tools/copy-local-config.sh").is_file());
}

#[test]
fn git_enumeration_includes_ignored_secrets_and_excludes_store_metadata() {
    use crate::command_runner::RealCommandRunner;
    let temp = tempfile::tempdir().unwrap();
    super::support::init_repo_with_branch(temp.path(), "main");
    fs::write(temp.path().join("README.md"), "tracked").unwrap();
    fs::write(temp.path().join(".gitignore"), "backend/private config.env\n").unwrap();
    let runner = RealCommandRunner;
    runner
        .run(&RealCommandRunner::invocation(
            temp.path(),
            "git",
            &["add", "README.md", ".gitignore"],
        ))
        .unwrap();
    fs::create_dir_all(temp.path().join("backend")).unwrap();
    fs::create_dir_all(temp.path().join(".jj/repo/index")).unwrap();
    fs::write(temp.path().join("backend/private config.env"), "ignored secret").unwrap();
    fs::write(temp.path().join(".jj/repo/index/broken"), "damaged index").unwrap();
    let inputs = crate::app::repo_reclone::untracked_inputs(&runner, temp.path()).unwrap();
    assert_eq!(inputs, vec![PathBuf::from("backend/private config.env")]);
}
