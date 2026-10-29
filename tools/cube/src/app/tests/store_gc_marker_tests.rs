//! The host-wide maintenance marker: ownership across stores, recovery from a
//! killed pass, and the retry and warm-up budgets of the exclusive span.
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::store_gc_tests::{
    abandon_command, index_command, mono_record, op_log, op_log_command, pr_ref_commands, ps_command, seed_store_files,
    source_of,
};
use super::support::{ExpectedCommand, FakeRunner, seed_mono_repo, with_database_path};
use crate::app::errors::CubeError;
use crate::app::store_gc::{Collection, DEFAULT_KEEP, gc_repo_store};
use crate::command_runner::JjGate;

fn marker_path(database: &Path) -> PathBuf {
    JjGate::quarantine_path(&JjGate::path(database.parent().unwrap()))
}

fn write_marker(database: &Path, owner: &Path) {
    std::fs::create_dir_all(marker_path(database).parent().unwrap()).unwrap();
    std::fs::write(marker_path(database), owner.display().to_string()).unwrap();
}

fn collect_command(source: &Path) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
        "jj",
        &["--ignore-working-copy", "util", "gc", "--expire", "now"],
        "",
    )
}

fn run_pass(runner: &FakeRunner, database: &Path, quiescence: Duration) -> Result<Collection, CubeError> {
    gc_repo_store(
        runner,
        Some(database),
        &mono_record(database),
        DEFAULT_KEEP,
        quiescence,
        false,
    )
    .map(|report| report.collection)
}

#[test]
fn a_stale_marker_with_a_single_head_is_cleared_by_the_next_pass() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    write_marker(&database, &source);
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        ps_command(&source, ""),
        op_log_command(&source, &op_log(now)),
        ps_command(&source, ""),
        ps_command(&source, ""),
        abandon_command(&source),
        index_command(&source),
        ps_command(&source, ""),
        collect_command(&source),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let fake = FakeRunner::new(expected);
    assert_eq!(
        run_pass(&fake, &database, Duration::from_secs(1)).unwrap(),
        Collection::Immediate
    );
    fake.assert_exhausted();
    assert!(!marker_path(&database).exists());
    JjGate::shared_for_database(Some(&database)).unwrap();
}

#[test]
fn a_stale_marker_with_two_heads_stays_and_reports_the_heads() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    std::fs::write(source.join(".jj/repo/op_heads/heads/other"), b"").unwrap();
    write_marker(&database, &source);
    let fake = FakeRunner::new(vec![ps_command(&source, "")]);
    let error = run_pass(&fake, &database, Duration::from_secs(1)).unwrap_err();
    assert!(error.to_string().contains("2 operation heads"), "{error}");
    assert!(marker_path(&database).exists());
    assert!(JjGate::shared_for_database(Some(&database)).is_err());
}

#[test]
fn a_pass_neither_replaces_nor_clears_another_stores_marker() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let other = Path::new("/stores/other-repo");
    write_marker(&database, other);
    let fake = FakeRunner::new(vec![]);
    let error = run_pass(&fake, &database, Duration::from_secs(1)).unwrap_err();
    assert!(error.to_string().contains("/stores/other-repo"), "{error}");
    assert_eq!(
        std::fs::read_to_string(marker_path(&database)).unwrap(),
        other.display().to_string()
    );
    // Even a pass that already holds the exclusive gate, having finished its
    // reads before the marker appeared, can neither replace nor clear it.
    let gate = JjGate::exclusive(&JjGate::path(database.parent().unwrap()), Duration::ZERO).unwrap();
    assert!(gate.quarantine(&source).is_err());
    assert!(gate.clear_quarantine(&source).is_err());
    drop(gate);
    assert!(JjGate::shared_for_database(Some(&database)).is_err());
}

#[test]
fn a_racing_reprobe_is_retried_within_the_budget() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let now = crate::app::util::current_epoch_s().unwrap();
    let mut expected = vec![
        op_log_command(&source, &op_log(now)),
        // First attempt: quiet, then a jj appears under the gate.
        ps_command(&source, ""),
        ps_command(&source, "jj"),
        // Second attempt succeeds.
        ps_command(&source, ""),
        ps_command(&source, ""),
        abandon_command(&source),
        index_command(&source),
        ps_command(&source, ""),
        collect_command(&source),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let fake = FakeRunner::new(expected);
    assert_eq!(
        run_pass(&fake, &database, Duration::from_secs(3)).unwrap(),
        Collection::Immediate
    );
    fake.assert_exhausted();
}

#[test]
fn a_failed_index_warm_up_after_a_verified_abandon_still_collects() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let mut warm = index_command(&source);
    warm.result = Err(CubeError::CommandTimedOut {
        program: "jj".into(),
        args: warm.args.clone(),
        timeout_secs: 120,
    });
    let mut expected = vec![
        op_log_command(&source, &op_log(crate::app::util::current_epoch_s().unwrap())),
        ps_command(&source, ""),
        ps_command(&source, ""),
        abandon_command(&source),
        warm,
        ps_command(&source, ""),
        collect_command(&source),
    ];
    expected.extend(pr_ref_commands(&source, "", ""));
    let fake = FakeRunner::new(expected);
    assert_eq!(
        run_pass(&fake, &database, Duration::from_secs(1)).unwrap(),
        Collection::Immediate
    );
    fake.assert_exhausted();
    assert!(fake.observed_timeouts().contains(&Duration::from_secs(20)));
}

#[test]
fn orphaned_jj_keeps_readers_out_until_quiet_recovery() {
    use crate::app::dispatch::run_with_dependencies;
    use crate::cli::Cli;
    use clap::Parser;
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    write_marker(&database, &source);
    let fake = FakeRunner::new(vec![ps_command(&source, "jj")]);
    let error = run_pass(&fake, &database, Duration::ZERO).unwrap_err();
    assert_eq!(error.exit_code(), std::process::ExitCode::from(75));
    fake.assert_exhausted();
    assert!(marker_path(&database).exists());
    let busy = FakeRunner::new(vec![ps_command(&source, "jj")]);
    let lease = || Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "marker recovery"]);
    let error = run_with_dependencies(lease(), Some(&database), &busy).unwrap_err();
    assert_eq!(error.exit_code(), std::process::ExitCode::from(75));
    busy.assert_exhausted();
    assert!(marker_path(&database).exists());
    let workspace = tempdir.path().join("workspaces/mono-agent-001");
    std::fs::create_dir_all(workspace.join(".jj")).unwrap();
    let quiet = super::support::lease_runner_for(&workspace, "abc1234");
    quiet.expectations.borrow_mut().push_front(ps_command(&source, ""));
    run_with_dependencies(lease(), Some(&database), &quiet).unwrap();
    quiet.assert_exhausted();
    assert!(!marker_path(&database).exists());
    JjGate::shared_for_database(Some(&database)).unwrap();
}

#[test]
fn empty_marker_requires_explicit_verified_recovery() {
    let (_tempdir, database) = with_database_path();
    write_marker(&database, Path::new(""));
    let lock = JjGate::path(database.parent().unwrap());
    let fake = FakeRunner::new(vec![]);
    let error = JjGate::shared_with_runner(&lock, Duration::ZERO, &fake).err().unwrap();
    assert_eq!(error.exit_code(), std::process::ExitCode::from(75));
    assert!(error.to_string().contains("unknown owner"));
    let gate = JjGate::exclusive(&lock, Duration::ZERO).unwrap();
    assert!(gate.quarantine(Path::new("/store/owner")).is_err());
    assert_eq!(std::fs::read_to_string(marker_path(&database)).unwrap(), "");
}

#[test]
fn foreign_marker_published_after_listing_stops_the_pass() {
    use crate::command_runner::{CommandInvocation, CommandRunner};
    struct PublishOnProbe {
        fake: FakeRunner,
        database: PathBuf,
    }
    impl CommandRunner for PublishOnProbe {
        fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
            if invocation.program == "ps" {
                write_marker(&self.database, Path::new("/stores/other-repo"));
            }
            self.fake.run(invocation)
        }
    }
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let runner = PublishOnProbe {
        fake: FakeRunner::new(vec![
            op_log_command(&source, &op_log(crate::app::util::current_epoch_s().unwrap())),
            ps_command(&source, ""),
        ]),
        database: database.clone(),
    };
    let error = gc_repo_store(
        &runner,
        Some(&database),
        &mono_record(&database),
        DEFAULT_KEEP,
        Duration::from_secs(1),
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("another store"), "{error}");
    runner.fake.assert_exhausted();
    assert_eq!(
        std::fs::read_to_string(marker_path(&database)).unwrap(),
        "/stores/other-repo"
    );
    assert!(JjGate::shared_for_database(Some(&database)).is_err());
}
