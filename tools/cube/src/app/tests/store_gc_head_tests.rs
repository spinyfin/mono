//! Phase-accurate head and ancestry-walk failures in the audited GC pass.
use super::store_gc_tests::{
    gc_index_command, mono_record, op_log, op_log_command, ps_command, seed_store_files, source_of,
};
use super::support::{ExpectedCommand, FakeRunner, audit_events, seed_mono_repo, with_database_path};
use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::app::store_gc::Collection;
use crate::cli::Cli;
use clap::Parser;

fn operation_command(source: &std::path::Path, head: &str, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        source.to_path_buf(),
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
        output,
    )
}

fn parent_of(parent: &str) -> String {
    format!("Operation {{ parents: [OperationId(\"{parent}\")], metadata: {{}} }}")
}

#[test]
fn the_ancestry_walk_reports_budget_exhaustion_differently_from_reaching_the_root() {
    use crate::app::store_gc_integrity::{MAX_ANCESTRY_OPERATIONS, require_safe_progress};

    let source = std::path::Path::new("/stores/mono");
    let id = |n: usize| format!("{n:0128x}");
    let ancestor = "f".repeat(128);

    // A long linear chain that never contains the ancestor, like a head built
    // on mono's pre-abandon history, runs out of budget.
    let chain: Vec<_> = (0..MAX_ANCESTRY_OPERATIONS)
        .map(|n| operation_command(source, &id(n), &parent_of(&id(n + 1))))
        .collect();
    let fake = FakeRunner::new(chain);
    let error = require_safe_progress(&fake, source, &ancestor, &id(0)).unwrap_err();
    // Exactly the budgeted number of lookups ran, then the walk stopped.
    fake.assert_exhausted();
    let text = error.to_string();
    assert!(text.contains("unsafe operation heads"), "{text}");
    assert!(
        text.contains(&format!("exceeded {MAX_ANCESTRY_OPERATIONS} operations / 60 seconds")),
        "{text}"
    );
    assert!(!text.contains("no readable parents"), "{text}");

    // A short chain that ends at the root operation says so instead.
    let fake = FakeRunner::new(vec![
        operation_command(source, &id(0), &parent_of(&id(1))),
        operation_command(source, &id(1), "Operation { parents: [], metadata: {} }"),
    ]);
    let text = require_safe_progress(&fake, source, &ancestor, &id(0))
        .unwrap_err()
        .to_string();
    fake.assert_exhausted();
    assert!(text.contains("no readable parents"), "{text}");
    assert!(!text.contains("exceeded"), "{text}");
}

#[test]
fn two_heads_before_any_rewrite_are_a_retryable_concurrency_not_a_repair_request() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    std::fs::write(source.join(".jj/repo/op_heads/heads/raced"), b"").unwrap();
    let verify = |head: &str| gc_index_command(&source, &["--at-operation", head, "debug", "reindex"]);
    // Nothing is old enough to abandon, so the pass reaches the collection
    // rebuild with both heads still there. Each head is verified first.
    let now = crate::app::util::current_epoch_s().unwrap();
    let fake = FakeRunner::new(vec![
        op_log_command(&source, &format!("{} {now}\n", "a".repeat(64))),
        verify("head"),
        verify("raced"),
    ]);
    let error = run_with_dependencies(Cli::parse_from(["cube", "repo", "gc", "mono"]), Some(&database), &fake)
        .expect_err("concurrent heads stop the pass");
    fake.assert_exhausted();
    let audited = audit_events(&tempdir)
        .iter()
        .find(|row| row["event"] == "repo.store_gc")
        .unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned();
    for text in [error.to_string(), audited] {
        assert!(
            text.contains("2 operation heads before the collection rebuild"),
            "{text}"
        );
        assert!(text.contains("concurrent jj operation is in flight"), "{text}");
        assert!(text.contains("Nothing was abandoned or collected"), "{text}");
        assert!(!text.contains("Repair"), "{text}");
    }
}

/// A pass whose collection is followed by a second head published by a
/// concurrent worker. `descends` is whether that head builds on the head the
/// collection ran against.
fn collect_then_publish_a_head(descends: bool) -> (tempfile::TempDir, Result<Collection, CubeError>) {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    std::fs::remove_dir(source.join(".git")).unwrap();
    let heads = source.join(".jj/repo/op_heads/heads");
    let original = "b".repeat(128);
    let raced = "c".repeat(128);
    std::fs::rename(heads.join("head"), heads.join(&original)).unwrap();
    let verify = |head: &str| gc_index_command(&source, &["--at-operation", head, "debug", "reindex"]);
    let now = crate::app::util::current_epoch_s().unwrap();
    let parent_output = if descends {
        parent_of(&original)
    } else {
        "Operation { parents: [], metadata: {} }".to_owned()
    };
    let mut expected = vec![
        op_log_command(&source, &format!("{original} {now}\n")),
        verify(&original),
        verify(&original),
        ps_command(&source, ""),
        gc_index_command(&source, &["util", "gc", "--expire", "now"]).creating_dir(heads.join(&raced)),
        verify(&original),
        operation_command(&source, &raced, &parent_output),
    ];
    if descends {
        expected.push(verify(&raced));
    }
    let fake = FakeRunner::new(expected);
    let result = crate::app::store_gc::gc_repo_store(
        &fake,
        Some(&database),
        &mono_record(&database),
        crate::app::store_gc::DEFAULT_KEEP,
        std::time::Duration::from_millis(200),
        false,
    )
    .map(|report| report.collection);
    fake.assert_exhausted();
    (tempdir, result)
}

#[test]
fn a_benign_head_published_after_the_collection_is_verified_not_refused() {
    let (tempdir, result) = collect_then_publish_a_head(true);
    assert_eq!(result.unwrap(), Collection::Immediate);
    assert!(
        !audit_events(&tempdir)
            .iter()
            .any(|row| row["event"] == "repo.store_gc" && row["outcome"] == "failed")
    );
}

#[test]
fn an_unsafe_head_published_after_the_collection_says_collection_completed() {
    let (tempdir, result) = collect_then_publish_a_head(false);
    let text = result.unwrap_err().to_string();
    assert!(text.contains("collection completed"), "{text}");
    assert!(text.contains("fresh-index verification was not performed"), "{text}");
    assert!(!text.contains("nothing was collected"), "{text}");
    let row = audit_events(&tempdir)
        .into_iter()
        .find(|row| row["event"] == "repo.store_gc" && row["outcome"] == "failed")
        .unwrap();
    let audited = row["error"].as_str().unwrap();
    assert!(audited.contains("collection completed"), "{audited}");
    assert!(!audited.contains("nothing was collected"), "{audited}");
}

#[test]
fn an_abandon_raced_by_a_second_head_still_asks_for_repair() {
    // The after-abandon failure keeps its repair advice: abandoned ancestry
    // may be merged by the next jj command.
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let heads = source.join(".jj/repo/op_heads/heads");
    let now = crate::app::util::current_epoch_s().unwrap();
    let fake = FakeRunner::new(vec![
        op_log_command(&source, &op_log(now)),
        gc_index_command(&source, &["--at-operation", "head", "debug", "reindex"]),
        ps_command(&source, ""),
        ps_command(&source, ""),
        super::store_gc_tests::abandon_command(&source).creating_dir(heads.join("interrupted")),
    ]);
    let error = run_with_dependencies(Cli::parse_from(["cube", "repo", "gc", "mono"]), Some(&database), &fake)
        .expect_err("raced abandon");
    fake.assert_exhausted();
    let text = error.to_string();
    assert!(text.contains("2 operation heads after the abandon"), "{text}");
    assert!(text.contains("Repair the operation heads"), "{text}");
}

#[test]
fn the_oldest_client_declaration_is_stored_per_repo() {
    let (tempdir, database) = with_database_path();
    seed_mono_repo(&tempdir.path().join("workspaces"), &database);
    let source = seed_store_files(&source_of(&database));
    let version = |text: &str| ExpectedCommand::ok(source.clone(), "jj", &["--version"], text);
    let run_gc = |fake: &FakeRunner| {
        run_with_dependencies(Cli::parse_from(["cube", "repo", "gc", "mono"]), Some(&database), fake)
    };

    // Without a declaration a newer jj is refused, and the message says where to declare.
    let fake = FakeRunner::new(vec![version("jj 0.34.0\n")]);
    let error = run_gc(&fake).expect_err("undeclared newer collector");
    fake.assert_exhausted();
    assert!(
        error.to_string().contains("cube repo declare-oldest-jj mono"),
        "{error}"
    );

    // A malformed declaration is rejected and changes nothing.
    let fake = FakeRunner::default();
    for bad in ["0", "0.34.1", "latest"] {
        run_with_dependencies(
            Cli::parse_from(["cube", "repo", "declare-oldest-jj", "mono", bad]),
            Some(&database),
            &fake,
        )
        .expect_err("malformed version");
    }

    // The explicit, audited, per-repo declaration is what lets a newer jj collect.
    run_with_dependencies(
        Cli::parse_from(["cube", "repo", "declare-oldest-jj", "mono", "0.34"]),
        Some(&database),
        &fake,
    )
    .unwrap();
    assert!(
        audit_events(&tempdir)
            .iter()
            .any(|row| row["event"] == "repo.oldest_jj_declared" && row["oldest_jj"] == "0.34")
    );
    let mut op_log = op_log_command(&source, "");
    op_log.result = Err(CubeError::InvalidArgument("op log unreadable".into()));
    let fake = FakeRunner::new(vec![version("jj 0.34.0\n"), op_log]);
    run_gc(&fake).expect_err("op log failure");
    fake.assert_exhausted();
    assert!(
        audit_events(&tempdir)
            .iter()
            .any(|row| row["outcome"] == "failed" && row["collector_jj_version"] == "jj 0.34.0")
    );
}
