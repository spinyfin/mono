//! Rewritten history must remain readable without a cached commit index.
use super::*;
use crate::app::store_gc::{Collection, gc_repo_store};
use crate::metadata::RepoRecord;
use git_utils::jj_provenance::COMMIT_ID_LINES_TEMPLATE;
use std::time::Duration;

struct QuietRunner<'a>(&'a DeclaredJj);

impl CommandRunner for QuietRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        // Only this fixture uses the store; unrelated host jj processes must
        // not skip the collection.
        if invocation.program == "ps" {
            Ok(String::new())
        } else {
            self.0.run(invocation)
        }
    }
}

fn fixture() -> Fixture {
    let f = Fixture::new();
    // jj 0.26 passes --expire now to its own collector but runs plain git gc.
    // Expire Git objects and reflogs now too, modeling old objects without a
    // two-week sleep. This configuration belongs only to the temporary repo.
    for setting in ["gc.pruneExpire", "gc.reflogExpire"] {
        f.runner
            .run(&RealCommandRunner::invocation(
                &f.root,
                "git",
                &["config", setting, "now"],
            ))
            .unwrap();
    }
    for description in ["first rewrite", "second rewrite", "third rewrite"] {
        f.jj(&f.ws, &["describe", "-m", description]);
    }
    assert!(evolution(&f).lines().count() >= 4);
    f
}

fn evolution(f: &Fixture) -> String {
    f.jj(
        &f.ws,
        &[
            "--ignore-working-copy",
            "evolog",
            "--no-graph",
            "-T",
            COMMIT_ID_LINES_TEMPLATE,
        ],
    )
}

fn newer(f: &Fixture, args: &[&str]) -> String {
    DeclaredJj {
        binary: std::fs::canonicalize(std::env::var("CUBE_NEWER_JJ_BIN").unwrap()).unwrap(),
        config: f.runner.config.clone(),
    }
    .run(&RealCommandRunner::invocation(&f.root, "jj", args))
    .unwrap()
}

fn inherit_newer_index(f: &Fixture) {
    // Since 0.32, jj intentionally truncates legacy predecessor history.
    // The on-disk index is still readable by 0.26, which trusts it during GC.
    newer(f, &["--ignore-working-copy", "op", "abandon", "..@-"]);
    newer(f, &["--ignore-working-copy", "debug", "reindex"]);
}

fn collect(f: &Fixture, keep: Duration) -> Result<crate::app::store_gc::StoreGcReport> {
    collect_with_runner(f, keep, &QuietRunner(&f.runner))
}

fn collect_with_runner(
    f: &Fixture,
    keep: Duration,
    runner: &dyn CommandRunner,
) -> Result<crate::app::store_gc::StoreGcReport> {
    let record = RepoRecord {
        repo: "scratch".into(),
        origin: f.root.display().to_string(),
        main_branch: "main".into(),
        workspace_root: f._dir.path().to_owned(),
        workspace_prefix: "ws".into(),
        source: Some(f.root.clone()),
        clone_command: None,
    };
    gc_repo_store(
        runner,
        Some(&f._dir.path().join("state.db")),
        &record,
        keep,
        Duration::from_millis(100),
        false,
    )
}

fn race_with_head(during_rebuild: bool) {
    let f = fixture();
    let original_head = f.jj(
        &f.root,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "id",
        ],
    );
    struct RacingHead<'a> {
        fixture: &'a Fixture,
        original_head: String,
        during_rebuild: bool,
        reindexes: std::cell::Cell<usize>,
    }
    impl CommandRunner for RacingHead<'_> {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            if invocation.program == "ps" {
                return Ok(String::new());
            }
            let reindex = invocation.args.iter().any(|arg| arg == "reindex");
            if reindex {
                self.reindexes.set(self.reindexes.get() + 1);
            }
            // The second reindex is the pre-collection rebuild: pinned to the
            // checked head, it must not merge the head published under it.
            let publish = if self.during_rebuild {
                reindex && self.reindexes.get() == 2
            } else {
                invocation.args.iter().any(|arg| arg == "root()")
            };
            if publish {
                // A writer loaded the old head before abandon and publishes
                // it while the warm-up is starting. An unpinned read would
                // merge it, hiding the race from the collector's head check.
                std::fs::write(
                    self.fixture
                        .root
                        .join(".jj/repo/op_heads/heads")
                        .join(self.original_head.trim()),
                    b"",
                )
                .unwrap();
            }
            let output = self.fixture.runner.run(invocation)?;
            if invocation
                .args
                .iter()
                .any(|arg| arg == crate::app::store_gc::OP_LOG_TEMPLATE)
            {
                // Age only the cut selection; keep the actual operation DAG.
                return Ok(output
                    .lines()
                    .map(|line| format!("{} 1\n", line.split_once(' ').unwrap().0))
                    .collect());
            }
            Ok(output)
        }
    }
    let error = collect_with_runner(
        &f,
        Duration::ZERO,
        &RacingHead {
            fixture: &f,
            original_head,
            during_rebuild,
            reindexes: Default::default(),
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("2 operation heads before the collection"),
        "{error}"
    );
    assert_eq!(crate::app::store_gc::count_op_heads(&f.root).unwrap(), 2);
}

#[test]
fn store_gc_predecessor_warmup_does_not_merge_a_racing_head() {
    race_with_head(false);
}

#[test]
fn store_gc_predecessor_pre_collection_rebuild_does_not_merge_a_racing_head() {
    race_with_head(true);
}

fn cached_read(f: &Fixture) {
    f.jj(
        &f.root,
        &[
            "--ignore-working-copy",
            "log",
            "-r",
            "root()",
            "--no-graph",
            "-T",
            "commit_id",
        ],
    );
}

fn reindex(f: &Fixture) -> Result<String> {
    f.runner.run(&RealCommandRunner::invocation(
        &f.root,
        "jj",
        &["--ignore-working-copy", "debug", "reindex"],
    ))
}

fn unsafe_collect(f: &Fixture) {
    f.jj(&f.root, &["--ignore-working-copy", "util", "gc", "--expire", "now"]);
}

#[test]
fn store_gc_predecessor_mixed_version_repro() {
    let f = fixture();
    inherit_newer_index(&f);
    unsafe_collect(&f);
    // A cached read passes despite deleted predecessors.
    cached_read(&f);
    let error = reindex(&f).unwrap_err();
    eprintln!("jj 0.26 inherited-index GC, fresh rebuild: {error}");
    assert!(
        error.to_string().contains("Failed to index commits at operation"),
        "{error}"
    );
    assert!(error.to_string().contains("of type commit not found"), "{error}");
    // Newer jj can rebuild, but its different retention semantics do not
    // make the store readable to the 0.26 clients sharing it.
    newer(&f, &["--ignore-working-copy", "debug", "reindex"]);
}

#[test]
fn store_gc_predecessor_all_026_history_survives() {
    let f = fixture();
    let before = evolution(&f);
    f.jj(&f.root, &["--ignore-working-copy", "op", "abandon", "..@-"]);
    cached_read(&f);
    unsafe_collect(&f);
    reindex(&f).unwrap();
    assert_eq!(evolution(&f), before);
}

#[test]
fn store_gc_predecessor_rebuilds_inherited_index_before_collecting() {
    let f = fixture();
    let before = evolution(&f);
    inherit_newer_index(&f);
    for keep in [Duration::from_secs(86400), Duration::ZERO] {
        let report = collect(&f, keep).unwrap();
        assert_eq!(report.collection, Collection::Immediate);
        reindex(&f).unwrap();
        assert_eq!(evolution(&f), before);
        // Both the primary store and its secondary workspace still load.
        f.jj(&f.ws, &["status"]);
        assert!(!f.root.join(".jj/cube-gc-index").exists());
    }
}

#[test]
fn store_gc_predecessor_preflight_preserves_readable_cache_on_damaged_store() {
    let f = fixture();
    inherit_newer_index(&f);
    unsafe_collect(&f);
    cached_read(&f);
    let head = f.jj(
        &f.root,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--limit",
            "1",
            "--no-graph",
            "-T",
            "id",
        ],
    );
    let index_path = f.root.join(".jj/repo/index/operations").join(&head);
    let index = std::fs::read(&index_path).unwrap();
    let error = collect(&f, Duration::ZERO).unwrap_err();
    assert!(error.to_string().contains("before abandon"), "{error}");
    assert!(error.to_string().contains("of type commit not found"), "{error}");
    assert_eq!(std::fs::read(index_path).unwrap(), index);
    assert!(f.root.join(".jj/repo/op_heads/heads").join(&head).exists());
    cached_read(&f);
    assert!(!f.root.join(".jj/cube-gc-index").exists());
}

#[test]
fn store_gc_predecessor_unreferenced_commits_are_still_collected() {
    let f = fixture();
    f.jj(&f.root, &["new", "root()", "-m", "discarded branch"]);
    let discarded = f.jj(&f.root, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    f.jj(&f.root, &["abandon", &discarded]);
    let report = collect(&f, Duration::ZERO).unwrap();
    assert_eq!(report.collection, Collection::Immediate);
    assert!(report.before.operations > report.after.operations);
    let error = f
        .runner
        .run(&RealCommandRunner::invocation(
            &f.root,
            "git",
            &["cat-file", "-t", &discarded],
        ))
        .unwrap_err();
    assert!(matches!(error, CubeError::CommandFailed { .. }));
    reindex(&f).unwrap();
}

#[test]
fn store_gc_predecessor_squash_history_survives() {
    let f = fixture();
    f.jj(&f.ws, &["new", "-m", "middle"]);
    std::fs::write(f.ws.join("middle"), "middle\n").unwrap();
    f.jj(&f.ws, &["new", "-m", "top"]);
    std::fs::write(f.ws.join("top"), "top\n").unwrap();
    f.jj(&f.ws, &["squash", "--from", "@", "--into", "@--", "-m", "squashed"]);
    collect(&f, Duration::ZERO).unwrap();
    reindex(&f).unwrap();
}

#[test]
fn store_gc_predecessor_recovery_with_newer_clients_preserves_visible_work() {
    let mut f = fixture();
    let visible = |f: &Fixture| {
        let mut ids: Vec<_> = f
            .jj(
                &f.root,
                &[
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    "all()",
                    "--no-graph",
                    "-T",
                    COMMIT_ID_LINES_TEMPLATE,
                ],
            )
            .lines()
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids
    };
    inherit_newer_index(&f);
    unsafe_collect(&f);
    let before = visible(&f);
    assert!(
        reindex(&f)
            .unwrap_err()
            .to_string()
            .contains("of type commit not found")
    );

    // Recovery requires switching every client, including the collector.
    // No predecessor metadata is edited and no operations are restored.
    f.runner.binary = std::fs::canonicalize(std::env::var("CUBE_NEWER_JJ_BIN").unwrap()).unwrap();
    // Every client has been upgraded, which the collector is told by declaration.
    crate::app::store_gc_integrity::OLDEST_CLIENT_OVERRIDE.with(|cell| cell.set(Some((0, 34))));
    reindex(&f).unwrap();
    assert_eq!(visible(&f), before);
    for keep in [Duration::from_secs(86400), Duration::ZERO] {
        assert_eq!(collect(&f, keep).unwrap().collection, Collection::Immediate);
        reindex(&f).unwrap();
        assert_eq!(visible(&f), before);
        assert_eq!(f.jj(&f.root, &["file", "show", "b.txt", "-r", "feat"]), "b");
        f.jj(&f.ws, &["status"]);
    }
}
