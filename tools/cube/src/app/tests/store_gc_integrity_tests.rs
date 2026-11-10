//! Verification failures must be visible to callers and detached upkeep.
use super::store_gc_tests::{
    abandon_command, fresh_index_command, gc_index_command, index_command, mono_record, op_log, op_log_command,
    ps_command, seed_store_files, source_of,
};
use super::support::{FakeRunner, audit_events, seed_mono_repo, with_database_path};
use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::app::store_gc::{store_gc_failures_key, store_gc_last_at_key};
use crate::cli::Cli;
use crate::store::Store;
use clap::Parser;

#[test]
fn store_gc_integrity_failure_never_reports_success() {
    for after_collection in [false, true] {
        let (tempdir, database) = with_database_path();
        seed_mono_repo(&tempdir.path().join("workspaces"), &database);
        let source = seed_store_files(&source_of(&database));
        let now = crate::app::util::current_epoch_s().unwrap();
        let mut expected = vec![op_log_command(&source, &format!("{} {now}\n", "a".repeat(64)))];
        if after_collection {
            expected.extend([
                fresh_index_command(&source),
                ps_command(&source, ""),
                fresh_index_command(&source),
                gc_index_command(&source, &["util", "gc", "--expire", "now"]),
            ]);
        }
        let mut failure = fresh_index_command(&source);
        failure.result = Err(CubeError::CommandFailed {
            program: "jj".into(),
            args: failure.args.clone(),
            status: Some(255),
            stderr: "Object abc of type commit not found".into(),
        });
        expected.push(failure);
        let fake = FakeRunner::new(expected);
        let error =
            run_with_dependencies(Cli::parse_from(["cube", "repo", "gc", "mono"]), Some(&database), &fake).unwrap_err();
        fake.assert_exhausted();
        assert_ne!(error.exit_code(), std::process::ExitCode::SUCCESS);
        assert!(
            error.to_string().contains("Object abc of type commit not found"),
            "{error}"
        );
        assert!(
            error.to_string().contains(if after_collection {
                "after collection"
            } else {
                "before abandon"
            }),
            "{error}"
        );
        let store = Store::open_at(&database).unwrap();
        assert_eq!(
            store.get_pool_metadata_i(&store_gc_failures_key("mono")).unwrap(),
            Some(1)
        );
        assert_eq!(store.get_pool_metadata_i(&store_gc_last_at_key("mono")).unwrap(), None);
        let events: Vec<_> = audit_events(&tempdir)
            .into_iter()
            .filter(|row| row["event"] == "repo.store_gc")
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "failed");
        assert!(events[0]["error"].as_str().unwrap().contains("not found"));
        assert!(!source.join(".jj/cube-gc-index").exists());
    }
}

#[test]
fn store_gc_warmup_timeout_and_late_head_are_both_audited_without_collecting() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let mut warmup = index_command(&source);
    warmup.result = Err(CubeError::CommandTimedOut {
        program: "jj".into(),
        args: warmup.args.clone(),
        timeout_secs: 299,
    });
    let fake = FakeRunner::new(vec![
        op_log_command(&source, &op_log(crate::app::util::current_epoch_s().unwrap())),
        fresh_index_command(&source),
        ps_command(&source, ""),
        ps_command(&source, ""),
        abandon_command(&source),
        warmup,
        ps_command(&source, "").creating_dir(source.join(".jj/repo/op_heads/heads/raced")),
    ]);
    let error = run_with_dependencies(Cli::parse_from(["cube", "repo", "gc", "mono"]), Some(&database), &fake)
        .expect_err("a late second head must still refuse collection");
    fake.assert_exhausted();
    assert_ne!(error.exit_code(), std::process::ExitCode::SUCCESS);
    assert!(error.to_string().contains("2 operation heads before the collection"));
    let events = audit_events(&tempdir);
    let warmups: Vec<_> = events
        .iter()
        .filter(|row| row["event"] == "repo.store_gc.index_warmup")
        .collect();
    assert_eq!(warmups.len(), 1);
    assert_eq!(warmups[0]["outcome"], "failed");
    assert_eq!(warmups[0]["timeout_ms"], 300_000);
    assert!(warmups[0]["duration_ms"].is_u64());
    assert!(warmups[0]["error"].as_str().unwrap().contains("299s and was killed"));
    let collections: Vec<_> = events.iter().filter(|row| row["event"] == "repo.store_gc").collect();
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0]["outcome"], "failed");
    assert!(collections[0]["duration_ms"].is_u64());
    assert_eq!(collections[0]["source"], source.to_str().unwrap());
    assert!(collections[0]["error"].as_str().unwrap().contains("2 operation heads"));
    assert_eq!(
        Store::open_at(&database)
            .unwrap()
            .get_pool_metadata_i(&store_gc_failures_key("mono"))
            .unwrap(),
        Some(1)
    );
}

#[test]
fn store_gc_warmup_can_outlast_the_exclusive_rewrite_deadline() {
    use std::time::Duration;

    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    // No colocated refs to clean in this command-runner fixture.
    std::fs::remove_dir(source.join(".git")).unwrap();
    let fake = FakeRunner::new(vec![
        op_log_command(&source, &op_log(crate::app::util::current_epoch_s().unwrap())),
        fresh_index_command(&source),
        ps_command(&source, ""),
        ps_command(&source, ""),
        abandon_command(&source),
        index_command(&source).taking(Duration::from_millis(600)),
        ps_command(&source, ""),
        fresh_index_command(&source),
        gc_index_command(&source, &["util", "gc", "--expire", "now"]),
        fresh_index_command(&source),
    ]);
    let report = crate::app::store_gc::gc_repo_store_with_deadline(
        &fake,
        Some(&database),
        &mono_record(&database),
        crate::app::store_gc::DEFAULT_KEEP,
        crate::app::store_gc::QUIESCENCE_WAIT,
        Duration::from_millis(300),
        false,
    )
    .unwrap();
    fake.assert_exhausted();
    assert_eq!(report.collection, crate::app::store_gc::Collection::Immediate);
    let events = audit_events(&tempdir);
    let warmup = events
        .iter()
        .find(|row| row["event"] == "repo.store_gc.index_warmup")
        .unwrap();
    assert_eq!(warmup["outcome"], "succeeded");
    assert!(warmup["duration_ms"].as_u64().unwrap() >= 600);
    assert!(warmup.get("error").is_none());
    assert!(
        events
            .iter()
            .any(|row| row["event"] == "repo.store_gc" && row["fresh_index_verified"] == true)
    );
}
