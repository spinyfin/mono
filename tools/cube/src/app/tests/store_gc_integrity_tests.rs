//! Verification failures must be visible to callers and detached upkeep.
use super::store_gc_tests::{
    fresh_index_command, gc_index_command, op_log_command, ps_command, seed_store_files, source_of,
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
