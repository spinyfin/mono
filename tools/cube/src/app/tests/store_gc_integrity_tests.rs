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
    for phase in 0..4 {
        let (tempdir, database) = with_database_path();
        seed_mono_repo(&tempdir.path().join("workspaces"), &database);
        let source = seed_store_files(&source_of(&database));
        let now = crate::app::util::current_epoch_s().unwrap();
        let log = if phase == 3 {
            op_log(now)
        } else {
            format!("{} {now}\n", "a".repeat(64))
        };
        let mut expected = vec![op_log_command(&source, &log)];
        if phase >= 1 {
            expected.push(fresh_index_command(&source));
        }
        if phase == 3 {
            expected.extend([
                ps_command(&source, ""),
                ps_command(&source, ""),
                abandon_command(&source),
                index_command(&source),
            ]);
        }
        if phase == 2 {
            expected.extend([
                fresh_index_command(&source),
                ps_command(&source, ""),
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
            error.to_string().contains(
                [
                    "before abandon",
                    "before collection",
                    "after collection",
                    "before collection"
                ][phase]
            ),
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
        use crate::command_runner::JjGate;
        assert!(!JjGate::quarantine_path(&JjGate::path(database.parent().unwrap())).exists());
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
        fresh_index_command(&source),
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
        fresh_index_command(&source),
        ps_command(&source, ""),
        gc_index_command(&source, &["util", "gc", "--expire", "now"]),
        fresh_index_command(&source),
    ]);
    let report = crate::app::store_gc::gc_repo_store_with_deadline(
        &fake,
        Some(&database),
        &mono_record(&database),
        crate::app::store_gc::DEFAULT_KEEP,
        crate::app::store_gc::QUIESCENCE_WAIT,
        crate::app::store_gc::Collector {
            exclusive_timeout: Duration::from_millis(300),
            jj_version: Some("jj 0.26.0"),
        },
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

#[test]
fn a_head_published_during_the_pre_collection_rebuild_stops_both_collection_paths() {
    use crate::app::store_gc::{count_op_heads, gc_repo_store};
    use std::time::Duration;

    for probe_output in ["", "/opt/homebrew/bin/jj\n"] {
        let (tempdir, database) = with_database_path();
        seed_mono_repo(&tempdir.path().join("workspaces"), &database);
        let source = seed_store_files(&source_of(&database));
        // The writer loaded the pre-abandon head and publishes it while the
        // slow fresh-index rebuild runs; the pass sees it only afterwards.
        let racing = fresh_index_command(&source).creating_dir(source.join(".jj/repo/op_heads/heads/raced"));
        let fake = FakeRunner::new(vec![
            op_log_command(&source, &op_log(crate::app::util::current_epoch_s().unwrap())),
            fresh_index_command(&source),
            ps_command(&source, ""),
            ps_command(&source, ""),
            abandon_command(&source),
            index_command(&source),
            racing,
            ps_command(&source, probe_output),
        ]);
        let error = gc_repo_store(
            &fake,
            Some(&database),
            &mono_record(&database),
            crate::app::store_gc::DEFAULT_KEEP,
            Duration::from_millis(200),
            false,
        )
        .expect_err("a head published during the rebuild must refuse collection");
        // No `util gc` was scripted, so none was run.
        fake.assert_exhausted();
        assert!(
            error.to_string().contains("2 operation heads before the collection"),
            "{error}"
        );
        assert_eq!(count_op_heads(&source).unwrap(), 2);
        let failed: Vec<_> = audit_events(&tempdir)
            .into_iter()
            .filter(|row| row["event"] == "repo.store_gc" && row["outcome"] == "failed")
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0]["error"].as_str().unwrap().contains("2 operation heads"));
    }
}

#[test]
fn collection_checks_linear_progress_and_refuses_a_merge_with_abandoned_history() {
    for stale_merge in [false, true] {
        use super::support::ExpectedCommand;
        use crate::command_runner::{CommandInvocation, CommandRunner};
        use std::cell::Cell;

        let (tempdir, database) = with_database_path();
        seed_mono_repo(&tempdir.path().join("workspaces"), &database);
        let source = seed_store_files(&source_of(&database));
        std::fs::remove_dir(source.join(".git")).unwrap();
        let heads = source.join(".jj/repo/op_heads/heads");
        let original = "b".repeat(128);
        let forward = "c".repeat(128);
        let collected = "d".repeat(128);
        std::fs::rename(heads.join("head"), heads.join(&original)).unwrap();
        let verify = |head: &str| gc_index_command(&source, &["--at-operation", head, "debug", "reindex"]);
        let parents = |head: &str, parent: &str| {
            ExpectedCommand::ok(
                source.clone(),
                "jj",
                &[
                    "--ignore-working-copy",
                    "--at-operation",
                    head,
                    "debug",
                    "operation",
                    "--display",
                    "operation",
                    head,
                ],
                &format!("Operation {{ parents: [OperationId(\"{parent}\")], metadata: {{}} }}"),
            )
        };
        let now = crate::app::util::current_epoch_s().unwrap();
        let mut expected = vec![
            op_log_command(&source, &format!("{original} {now}\n")),
            verify(&original),
            verify(&original),
            ps_command(&source, ""),
            parents(&forward, &original),
            gc_index_command(&source, &["util", "gc", "--expire", "now"]),
        ];
        if stale_merge {
            let root = "0".repeat(128);
            let mut merged = parents(&collected, &forward);
            merged.result = Ok(format!(
                "Operation {{ parents: [OperationId(\"{forward}\"), OperationId(\"{root}\")], metadata: {{}} }}"
            ));
            let mut root_op = parents(&root, &root);
            root_op.result = Ok("Operation { parents: [], metadata: {} }".into());
            expected.extend([merged, root_op]);
        } else {
            expected.extend([parents(&collected, &forward), verify(&collected)]);
        }
        let fake = FakeRunner::new(expected);
        struct Forward<'a> {
            fake: &'a FakeRunner,
            heads: std::path::PathBuf,
            ids: [&'a str; 3],
            reindexes: Cell<usize>,
        }
        impl CommandRunner for Forward<'_> {
            fn run(&self, invocation: &CommandInvocation) -> crate::app::errors::Result<String> {
                let result = self.fake.run(invocation)?;
                let transition = if invocation.args.iter().any(|arg| arg == "reindex") {
                    self.reindexes.set(self.reindexes.get() + 1);
                    (self.reindexes.get() == 2).then_some(0)
                } else if invocation.args.iter().any(|arg| arg == "gc") {
                    Some(1)
                } else {
                    None
                };
                if let Some(i) = transition {
                    std::fs::rename(self.heads.join(self.ids[i]), self.heads.join(self.ids[i + 1])).unwrap();
                }
                Ok(result)
            }
        }
        let runner = Forward {
            fake: &fake,
            heads,
            ids: [&original, &forward, &collected],
            reindexes: Cell::new(0),
        };
        let report = crate::app::store_gc::gc_repo_store(
            &runner,
            Some(&database),
            &mono_record(&database),
            crate::app::store_gc::DEFAULT_KEEP,
            std::time::Duration::from_millis(200),
            false,
        );
        fake.assert_exhausted();
        if stale_merge {
            assert!(report.unwrap_err().to_string().contains("does not descend safely"));
            let events: Vec<_> = audit_events(&tempdir)
                .into_iter()
                .filter(|row| row["event"] == "repo.store_gc")
                .collect();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["outcome"], "failed");
        } else {
            assert_eq!(report.unwrap().collection, crate::app::store_gc::Collection::Immediate);
        }
    }
}

#[test]
fn a_collector_newer_than_the_oldest_client_is_refused_and_its_version_audited() {
    use super::support::ExpectedCommand;
    use crate::app::store_gc::gc_repo_store;
    use std::time::Duration;

    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let version = |text: &str| ExpectedCommand::ok(source.clone(), "jj", &["--version"], text);
    let run = |fake: &FakeRunner| {
        gc_repo_store(
            fake,
            Some(&database),
            &mono_record(&database),
            crate::app::store_gc::DEFAULT_KEEP,
            Duration::from_millis(200),
            false,
        )
    };
    let fake = FakeRunner::new(vec![version("jj 0.34.0\n")]);
    let error = run(&fake).expect_err("newer collector");
    fake.assert_exhausted();
    assert!(error.to_string().contains("jj 0.34.0"), "{error}");
    assert!(error.to_string().contains("oldest client"), "{error}");

    // An accepted collector's version lands on the failed row of a pass that
    // then fails for another reason.
    let mut op_log = op_log_command(&source, "");
    op_log.result = Err(CubeError::InvalidArgument("op log unreadable".into()));
    let fake = FakeRunner::new(vec![version("jj 0.26.1\n"), op_log]);
    run(&fake).expect_err("op log failure");
    fake.assert_exhausted();
    assert!(
        audit_events(&tempdir)
            .iter()
            .any(|row| row["outcome"] == "failed" && row["collector_jj_version"] == "jj 0.26.1")
    );
}
