use super::*;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    repo: PathBuf,
    worker: PathBuf,
    replacement: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let boss_engine_test_git::jj::JjRepo {
            repo,
            worker,
            replacement,
        } = boss_engine_test_git::jj::JjRepo::new(root.path());
        Self {
            root,
            repo,
            worker,
            replacement,
        }
    }

    async fn record(&self, id: &str) -> ExecutionBookmark {
        create(&LocalJj, &self.worker, id, "local").await.unwrap()
    }

    async fn edit(&self, record: &ExecutionBookmark) {
        std::fs::write(self.worker.join("work.txt"), "first change\n").unwrap();
        LocalJj
            .run(&self.worker, &["describe", "-m", "First change"])
            .await
            .unwrap();
        LocalJj
            .run(&self.worker, &["new", "-m", "Second change"])
            .await
            .unwrap();
        LocalJj
            .run(&self.worker, &["bookmark", "set", &record.head(), "-r", "@"])
            .await
            .unwrap();
        std::fs::write(self.worker.join("second.txt"), "second change\n").unwrap();
        LocalJj.run(&self.worker, &["debug", "snapshot"]).await.unwrap();
    }
}

#[tokio::test]
async fn fresh_dispatch_records_the_floor_without_a_store_operation_and_teardown_preserves_edits() {
    let f = Fixture::new().await;
    let trunk = one_commit(&LocalJj, &f.repo, "@").await.unwrap();
    LocalJj
        .run(
            &f.repo,
            &["config", "set", "--repo", "revset-aliases.\"trunk()\"", &trunk],
        )
        .await
        .unwrap();
    let operation = || async {
        LocalJj
            .run(&f.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
            .await
            .unwrap()
    };
    let before = operation().await;
    let record = create_from(&LocalJj, &f.worker, "exec_fresh", "local", None, None)
        .await
        .unwrap();
    assert_eq!(operation().await, before, "empty dispatch must not mutate the store");
    assert_eq!(record.has_recovery_head, Some(false));
    assert_eq!(
        record.base_commit,
        Some(one_commit(&LocalJj, &f.worker, "trunk()").await.unwrap())
    );
    let names = LocalJj
        .run(&f.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
        .await
        .unwrap();
    assert!(
        !names.lines().any(|name| name.starts_with("boss-")),
        "empty dispatch must not write execution bookmarks; got {names}"
    );

    // The worker need not advance any recovery bookmark, even across new changes.
    std::fs::write(f.worker.join("first.txt"), "first edit").unwrap();
    LocalJj.run(&f.worker, &["new", "-m", "Follow-up"]).await.unwrap();
    std::fs::write(f.worker.join("second.txt"), "second edit").unwrap();
    let saved = preserve(&LocalJj, &record, &f.worker, None, None).await.unwrap();
    assert!(saved.tip.is_some());
    assert!(saved.inspect.is_none());
    let names = LocalJj
        .run(&f.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
        .await
        .unwrap();
    let names: Vec<_> = names.lines().filter(|name| *name != "main").collect();
    assert_eq!(names, vec![preserved_name(&record.execution_id).as_str()]);
    for file in ["first.txt", "second.txt"] {
        let content = LocalJj
            .run(
                &f.repo,
                &[
                    "file",
                    "show",
                    "-r",
                    &preserved_name(&record.execution_id),
                    &format!("root:{file}"),
                ],
            )
            .await
            .unwrap();
        assert!(content.contains("edit"));
    }
}

#[tokio::test]
async fn empty_initialization_ignores_a_leftover_legacy_baseline_name() {
    let f = Fixture::new().await;
    let trunk = one_commit(&LocalJj, &f.repo, "@").await.unwrap();
    LocalJj
        .run(
            &f.repo,
            &["config", "set", "--repo", "revset-aliases.\"trunk()\"", &trunk],
        )
        .await
        .unwrap();
    LocalJj
        .run(
            &f.worker,
            &["bookmark", "create", "boss-base/exec_leftover", "-r", "trunk()"],
        )
        .await
        .unwrap();
    let operation = || async {
        LocalJj
            .run(&f.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
            .await
            .unwrap()
    };
    let before = operation().await;
    let record = create_from(&LocalJj, &f.worker, "exec_leftover", "local", None, None)
        .await
        .unwrap();
    assert_eq!(operation().await, before, "empty dispatch must not mutate the store");
    assert_eq!(record.base_commit.as_deref(), Some(trunk.as_str()));
    let names = LocalJj
        .run(&f.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
        .await
        .unwrap();
    assert!(names.lines().any(|name| name == "boss-base/exec_leftover"), "{names}");
    assert!(
        LocalJj
            .run(
                &f.repo,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("bookmarks(exact:{})", record.head()),
                    "-T",
                    "commit_id"
                ],
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn restored_dispatch_creates_no_bookmarks_and_uses_the_recorded_floor() {
    let f = Fixture::new().await;
    let prior = f.record("exec_prior").await;
    f.edit(&prior).await;
    restore(&LocalJj, &prior, &f.replacement).await.unwrap();
    let before = LocalJj
        .run(&f.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
        .await
        .unwrap();
    let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&prior), None)
        .await
        .unwrap();
    let parent = LocalJj
        .run(&f.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
        .await
        .unwrap();
    assert_eq!(parent, before, "restored dispatch must not mutate the store");
    assert_eq!(next.has_recovery_head, Some(false));
    assert_eq!(next.base_commit, prior.base_commit);
    let names = LocalJj
        .run(
            &f.repo,
            &["bookmark", "list", "glob:*exec_next", "-T", "name ++ \"\\n\""],
        )
        .await
        .unwrap();
    assert!(names.trim().is_empty());
    let saved = preserve(&LocalJj, &next, &f.replacement, None, None).await.unwrap();
    assert!(saved.tip.is_some());
    assert_eq!(
        std::fs::read_to_string(f.replacement.join("second.txt")).unwrap(),
        "second change\n"
    );
}

/// The worker's lease ended in `state`; its recorded work is still exported
/// and restored, and nothing a later holder put in the workspace leaks in.
async fn terminated_run_survives_workspace(state: &str) {
    let f = Fixture::new().await;
    let record = f.record("exec_terminated").await;
    f.edit(&record).await;
    match state {
        "removed" => {
            LocalJj.run(&f.repo, &["workspace", "forget", "worker"]).await.unwrap();
            std::fs::remove_dir_all(&f.worker).unwrap();
        }
        "reused" | "leased_elsewhere" => {
            LocalJj
                .run(&f.worker, &["new", "root()", "-m", "Unrelated lease"])
                .await
                .unwrap();
            std::fs::write(f.worker.join("foreign.txt"), "someone else's work\n").unwrap();
        }
        _ => {}
    }
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(
        patch.contains("work.txt") && patch.contains("second.txt"),
        "{state}: {patch}"
    );
    assert!(!patch.contains("foreign.txt"), "{state}: {patch}");
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert_eq!(
        std::fs::read_to_string(f.replacement.join("second.txt")).unwrap(),
        "second change\n"
    );
    if state == "leased_elsewhere" {
        assert_eq!(
            std::fs::read_to_string(f.worker.join("foreign.txt")).unwrap(),
            "someone else's work\n"
        );
    }
}

#[tokio::test]
async fn terminated_run_survives_a_released_workspace() {
    terminated_run_survives_workspace("released").await;
}

#[tokio::test]
async fn terminated_run_survives_a_removed_workspace() {
    terminated_run_survives_workspace("removed").await;
}

#[tokio::test]
async fn terminated_run_survives_a_reused_workspace() {
    terminated_run_survives_workspace("reused").await;
}

#[tokio::test]
async fn terminated_run_survives_a_foreign_leased_workspace() {
    terminated_run_survives_workspace("leased_elsewhere").await;
}

#[tokio::test]
async fn revision_bookmark_recovers_unpushed_work_after_its_workspace_is_removed() {
    let f = Fixture::new().await;
    let record = f.record("exec_revision").await;
    f.edit(&record).await;
    std::fs::remove_dir_all(&f.worker).unwrap();
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert!(f.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn consecutive_recoveries_keep_inherited_work_when_the_next_run_makes_no_edits() {
    let f = Fixture::new().await;
    let prior = f.record("exec_prior").await;
    f.edit(&prior).await;
    assert!(restore(&LocalJj, &prior, &f.replacement).await.unwrap());
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &f.replacement,
        "exec_next",
        "local",
        Some(&prior),
        None,
    )
    .await
    .unwrap();
    std::fs::remove_dir_all(&f.worker).unwrap();
    std::fs::remove_dir_all(&f.replacement).unwrap();
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"));
}

#[tokio::test]
async fn consecutive_recoveries_keep_work_when_the_predecessor_has_no_recorded_baseline() {
    let f = Fixture::new().await;
    let prior = f.record("exec_legacy_null_base").await;
    let floor = prior.base_commit.clone().unwrap();
    f.edit(&prior).await;
    let mut legacy = prior.clone();
    legacy.base_commit = None;
    legacy.pr_head_before = Some(floor.clone());
    assert!(restore(&LocalJj, &legacy, &f.replacement).await.unwrap());
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &f.replacement,
        "exec_legacy_null_next",
        "local",
        Some(&legacy),
        None,
    )
    .await
    .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"), "{patch}");
}

#[tokio::test]
async fn a_conflicted_legacy_boss_base_is_ignored_when_inheriting_a_predecessor_floor() {
    let f = Fixture::new().await;
    let prior = f.record("exec_conflicted_boss_base").await;
    let floor = prior.base_commit.clone().unwrap();
    f.edit(&prior).await;
    let mut legacy = prior.clone();
    legacy.base_commit = None;
    legacy.pr_head_before = Some(floor.clone());
    // Make the predecessor's boss-base/ bookmark conflicted: two concurrent
    // operations set it to different commits.
    let op = LocalJj
        .run(&f.repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"])
        .await
        .unwrap();
    let head = LocalJj
        .run(&f.worker, &["log", "--no-graph", "-r", "@-", "-T", "commit_id"])
        .await
        .unwrap();
    for target in [head.trim(), "root()"] {
        LocalJj
            .run(
                &f.repo,
                &[
                    "--at-op",
                    op.trim(),
                    "bookmark",
                    "set",
                    &prior.base(),
                    "-r",
                    target,
                    "--allow-backwards",
                ],
            )
            .await
            .unwrap();
    }
    assert!(restore(&LocalJj, &legacy, &f.replacement).await.unwrap());
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &f.replacement,
        "exec_conflicted_boss_base_next",
        "local",
        Some(&legacy),
        None,
    )
    .await
    .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"), "{patch}");
}

#[tokio::test]
async fn consecutive_recoveries_keep_work_when_the_predecessors_baseline_hash_is_gone() {
    let f = Fixture::new().await;
    let prior = f.record("exec_missing_hash").await;
    let floor = prior.base_commit.clone().unwrap();
    f.edit(&prior).await;
    let mut gone = prior.clone();
    gone.base_commit = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &prior.base()])
        .await
        .unwrap();
    gone.pr_head_before = Some(floor.clone());
    assert!(restore(&LocalJj, &gone, &f.replacement).await.unwrap());
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &f.replacement,
        "exec_missing_hash_next",
        "local",
        Some(&gone),
        None,
    )
    .await
    .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"), "{patch}");
}

#[tokio::test]
async fn create_from_ignores_an_unusable_inherited_base() {
    let f = Fixture::new().await;
    let prior = f.record("exec_unusable_inherited").await;
    let floor = prior.base_commit.clone().unwrap();
    f.edit(&prior).await;
    assert!(restore(&LocalJj, &prior, &f.replacement).await.unwrap());
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &f.replacement,
        "exec_unusable_inherited_next",
        "local",
        Some(&prior),
        Some(missing),
    )
    .await
    .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"), "{patch}");

    let fresh_fixture = Fixture::new().await;
    let expected = one_commit(&LocalJj, &fresh_fixture.worker, "trunk()").await.unwrap();
    let fresh = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &fresh_fixture.worker,
        "exec_unusable_inherited_fresh",
        "local",
        None,
        Some(missing),
    )
    .await
    .unwrap();
    assert_eq!(fresh.base_commit.as_deref(), Some(expected.as_str()));
}

#[tokio::test]
async fn restoration_uses_pr_head_before_when_base_commit_is_absent_or_unusable() {
    let f = Fixture::new().await;
    let prior = f.record("exec_pr_head_floor").await;
    let floor = prior.base_commit.clone().unwrap();
    f.edit(&prior).await;
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &prior.base()])
        .await
        .unwrap();
    for base_commit in [None, Some("0123456789abcdef0123456789abcdef01234567".to_owned())] {
        let record = ExecutionBookmark {
            base_commit,
            pr_head_before: Some(floor.clone()),
            ..prior.clone()
        };
        let patch = unpublished_diff(&LocalJj, &record).await.unwrap();
        assert!(patch.contains("work.txt") && patch.contains("second.txt"), "{patch}");
    }
    let record = ExecutionBookmark {
        base_commit: None,
        pr_head_before: Some(floor),
        ..prior
    };
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert_eq!(
        std::fs::read_to_string(f.replacement.join("second.txt")).unwrap(),
        "second change\n"
    );
}

#[tokio::test]
async fn publication_cleanup_cannot_remove_the_engine_owned_recovery_pointer() {
    let f = Fixture::new().await;
    let record = f.record("exec_retained").await;
    f.edit(&record).await;
    // Cube forgets consumed boss/exec_* refs. Its existing cleanup never owns
    // the engine's recovery namespace, and no pool coordination is required.
    LocalJj
        .run(&f.repo, &["bookmark", "forget", &record.publication()])
        .await
        .unwrap();
    std::fs::remove_dir_all(&f.worker).unwrap();
    assert!(diff(&LocalJj, &record).await.unwrap().contains("second.txt"));
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
}

#[tokio::test]
async fn published_work_with_an_empty_working_child_is_not_reported_as_abandoned() {
    let f = Fixture::new().await;
    let record = f.record("exec_published").await;
    f.edit(&record).await;
    LocalJj.run(&f.worker, &["git", "export"]).await.unwrap();
    // A local transport fixture supplies actual remote-bookmark metadata
    // without contacting GitHub or publishing a branch.
    let git_store = f.repo.join(".jj/repo/store/git");
    LocalJj
        .run(
            &f.repo,
            &["git", "remote", "add", "origin", git_store.to_str().unwrap()],
        )
        .await
        .unwrap();
    LocalJj.run(&f.repo, &["git", "fetch"]).await.unwrap();
    LocalJj
        .run(&f.worker, &["new", "-m", "Post publication checkpoint"])
        .await
        .unwrap();
    LocalJj
        .run(&f.worker, &["bookmark", "set", &record.head(), "-r", "@"])
        .await
        .unwrap();
    assert!(!diff(&LocalJj, &record).await.unwrap().is_empty());
    assert!(unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
    assert!(!restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert!(!f.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn empty_run_is_distinct_from_a_missing_pointer() {
    let f = Fixture::new().await;
    let record = f.record("exec_empty").await;
    assert!(diff(&LocalJj, &record).await.unwrap().is_empty());
    LocalJj
        .run(
            &f.repo,
            &["bookmark", "set", &record.head(), "-r", "root()", "--allow-backwards"],
        )
        .await
        .unwrap();
    // A head that no longer descends from the baseline is not an integrity
    // failure: lineage is not an invariant.
    assert!(diff(&LocalJj, &record).await.unwrap().is_empty());
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &record.head(), &record.publication()])
        .await
        .unwrap();
    assert!(
        diff(&LocalJj, &record)
            .await
            .unwrap_err()
            .to_string()
            .contains("exactly one")
    );
    assert!(f.root.path().exists());
}

#[tokio::test]
async fn only_missing_heads_fail_without_lineage_or_baseline_bookmark_checks() {
    let f = Fixture::new().await;
    let record = f.record("exec_integrity").await;
    std::fs::write(f.worker.join("recovery-line.txt"), "recovery line\n").unwrap();
    LocalJj.run(&f.worker, &["debug", "snapshot"]).await.unwrap();
    LocalJj
        .run(&f.worker, &["new", "@-", "-m", "Divergent publication"])
        .await
        .unwrap();
    std::fs::write(f.worker.join("publication-line.txt"), "publication line\n").unwrap();
    LocalJj
        .run(
            &f.worker,
            &["bookmark", "set", &record.publication(), "-r", "@", "--allow-backwards"],
        )
        .await
        .unwrap();
    // Heads on different lines are both resumed; no lineage verdict.
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(
        patch.contains("recovery-line.txt") && patch.contains("publication-line.txt"),
        "{patch}"
    );

    // A restore carries both lines' files into the successor workspace.
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert_eq!(
        std::fs::read_to_string(f.replacement.join("recovery-line.txt")).unwrap(),
        "recovery line\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.replacement.join("publication-line.txt")).unwrap(),
        "publication line\n"
    );

    // The baseline bookmark is never read.
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &record.base()])
        .await
        .unwrap();
    diff(&LocalJj, &record).await.unwrap();
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &record.head(), &record.publication()])
        .await
        .unwrap();
    let error = diff(&LocalJj, &record)
        .await
        .unwrap_err()
        .context("inspect predecessor");
    assert!(is_missing_pointer_error(&error));
    assert!(!is_pointer_integrity_error(&error));
    assert!(error.to_string().contains("inspect predecessor"));
    assert!(format!("{error:#}").contains(&record.head()));
    assert!(format!("{error:#}").contains(&record.publication()));
}

async fn commit_ids(f: &Fixture, revset: &str) -> Vec<String> {
    LocalJj
        .run(
            &f.repo,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Rewrite the bookmarked commit twice concurrently, as happens when a PR head
/// is rewritten elsewhere while the execution pointer stays put. That leaves
/// two visible commits sharing one change id (a divergent change) and the
/// pointer bookmark itself conflicted between them. Returns the two twins.
async fn rewrite_head_concurrently(f: &Fixture, record: &ExecutionBookmark) -> Vec<String> {
    std::fs::write(f.worker.join("work.txt"), "first change\n").unwrap();
    LocalJj.run(&f.worker, &["describe", "-m", "Work"]).await.unwrap();
    LocalJj.run(&f.worker, &["new", "-m", "Next"]).await.unwrap();
    LocalJj
        .run(&f.worker, &["bookmark", "set", &record.head(), "-r", "@-"])
        .await
        .unwrap();
    let work = commit_ids(f, &revision(&record.head())).await.remove(0);
    let op = LocalJj
        .run(&f.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
        .await
        .unwrap();
    for message in ["left", "right"] {
        LocalJj
            .run(&f.repo, &["--at-op", op.trim(), "describe", &work, "-m", message])
            .await
            .unwrap();
    }
    let twins = commit_ids(f, "description(substring:\"left\") | description(substring:\"right\")").await;
    assert_eq!(twins.len(), 2, "expected a divergent change: {twins:?}");
    twins
}

#[tokio::test]
async fn divergent_change_id_does_not_fail_the_ancestry_check() {
    let f = Fixture::new().await;
    let record = f.record("exec_divergent").await;
    let twins = rewrite_head_concurrently(&f, &record).await;
    // Both pointers target one twin; the other twin shares its change id.
    for bookmark in [record.head(), record.publication()] {
        LocalJj
            .run(&f.repo, &["bookmark", "set", &bookmark, "-r", &twins[0]])
            .await
            .unwrap();
    }
    let patch = diff(&LocalJj, &record).await.unwrap();
    assert!(patch.contains("work.txt"), "{patch}");
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert!(f.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn a_baseline_bookmark_on_a_divergent_twin_is_ignored() {
    let f = Fixture::new().await;
    let record = f.record("exec_divergent_unrelated").await;
    let twins = rewrite_head_concurrently(&f, &record).await;
    for bookmark in [record.head(), record.publication()] {
        LocalJj
            .run(&f.repo, &["bookmark", "set", &bookmark, "-r", &twins[0]])
            .await
            .unwrap();
    }
    // The other twin is a sibling, not an ancestor. The recorded baseline
    // commit is the floor, so the bookmark's position is irrelevant.
    LocalJj
        .run(&f.repo, &["bookmark", "set", &record.base(), "-r", &twins[1]])
        .await
        .unwrap();
    diff(&LocalJj, &record).await.unwrap();
}

#[tokio::test]
async fn conflicted_pointer_bookmark_is_a_pointer_integrity_error() {
    let f = Fixture::new().await;
    let record = f.record("exec_conflicted").await;
    rewrite_head_concurrently(&f, &record).await;
    // The concurrent rewrites left the pointer itself conflicted: a real
    // error, never something to resolve by silently picking a side.
    let error = diff(&LocalJj, &record).await.unwrap_err();
    assert!(is_pointer_integrity_error(&error), "{error:#}");
    assert!(error.to_string().contains("exactly one"), "{error:#}");
}

#[tokio::test]
async fn initialization_leaves_existing_recovery_work_intact() {
    let f = Fixture::new().await;
    let record = f.record("exec_existing").await;
    f.edit(&record).await;
    let before = test_support::pointer_targets(&LocalJj, &record).await.unwrap();
    let initialized = create_from(&LocalJj, &f.worker, &record.execution_id, "local", None, None)
        .await
        .unwrap();
    assert_eq!(initialized.base_commit, record.base_commit);
    assert_eq!(test_support::pointer_targets(&LocalJj, &record).await.unwrap(), before);
    assert!(diff(&LocalJj, &record).await.unwrap().contains("second.txt"));
}

struct UnavailableInitialization;

#[async_trait]
impl Jj for UnavailableInitialization {
    async fn run(&self, _repo: &Path, _args: &[&str]) -> Result<String> {
        Err(anyhow::anyhow!("SSH unavailable during initialization"))
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        Ok(workspace.to_path_buf())
    }
}

#[tokio::test]
async fn initialization_transport_error_remains_retryable() {
    let f = Fixture::new().await;
    let error = create_from(
        &UnavailableInitialization,
        &f.worker,
        "exec_transport",
        "local",
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<CreationFailure>().is_some());
    assert!(!is_pointer_integrity_error(&error));
    assert!(format!("{error:#}").contains("SSH unavailable"));
}

#[tokio::test]
async fn shallow_history_below_trunk_does_not_affect_baseline_creation() {
    let root = tempfile::tempdir().unwrap();
    let repo = boss_engine_test_git::jj::JjRepo::new(root.path());
    let missing =
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["new", "-m", "Shallow boundary"]);
    let boundary =
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["new", "-m", "Trunk base"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    let base =
        boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["new", "main"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["new", "-m", "Execution"]);
    let git = repo.repo.join(".jj/repo/store/git");
    std::fs::write(git.join("shallow"), format!("{boundary}\n")).unwrap();
    std::fs::remove_file(git.join("objects").join(&missing[..2]).join(&missing[2..])).unwrap();

    let record = create(&LocalJj, &repo.worker, "exec_shallow", "local").await.unwrap();
    assert_eq!(record.base_commit.as_deref(), Some(base.as_str()));

    // The floor is the trunk commit, resolved without walking history below
    // it, so a trunk that advances later is simply the next creation's floor.
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["new", "-m", "Advanced trunk"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    let advanced = one_commit(&LocalJj, &repo.worker, "trunk()").await.unwrap();
    assert_ne!(advanced, base);
    assert_eq!(record.base_commit.as_deref(), Some(base.as_str()));
    let next = create(&LocalJj, &repo.worker, "exec_shallow_next", "local")
        .await
        .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(advanced.as_str()));
}
