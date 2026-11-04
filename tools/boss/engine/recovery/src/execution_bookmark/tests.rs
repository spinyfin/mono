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
        LocalJj.run(&self.worker, &["status"]).await.unwrap();
    }
}

#[tokio::test]
async fn terminated_run_survives_released_removed_reused_and_foreign_leased_workspace() {
    for state in ["released", "removed", "reused", "leased_elsewhere"] {
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
    let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&prior), None)
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
    let next = create_from(
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
    let next = create_from(
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
    let next = create_from(
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
    let next = create_from(
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
    let expected = positioned_base(&LocalJj, &fresh_fixture.worker).await.unwrap();
    let fresh = create_from(
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
    LocalJj
        .run(&f.worker, &["new", "@-", "-m", "Divergent publication"])
        .await
        .unwrap();
    LocalJj
        .run(
            &f.worker,
            &["bookmark", "set", &record.publication(), "-r", "@", "--allow-backwards"],
        )
        .await
        .unwrap();
    // Heads on different lines resume from the recovery head; no verdict.
    diff(&LocalJj, &record).await.unwrap();

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
async fn existing_initialization_names_are_an_integrity_failure() {
    let f = Fixture::new().await;
    let record = f.record("exec_existing").await;
    let error = create(&LocalJj, &f.worker, &record.execution_id, "local")
        .await
        .unwrap_err();
    assert_eq!(pointer_integrity_kind(&error), Some(PointerIntegrityKind::Other));
    assert!(error.downcast_ref::<CreationFailure>().is_some());
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
    let error = create(&UnavailableInitialization, &f.worker, "exec_transport", "local")
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
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "create", "main", "-r", "@"]);
    boss_engine_test_git::jj::JjRepo::run(
        &repo.repo,
        &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "main"],
    );
    let base = boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["new", "main"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.worker, &["new", "-m", "Execution"]);
    let git = repo.repo.join(".jj/repo/store/git");
    std::fs::write(git.join("shallow"), format!("{boundary}\n")).unwrap();
    std::fs::remove_file(git.join("objects").join(&missing[..2]).join(&missing[2..])).unwrap();

    // The unbounded empty() walk needs the missing parent to test whether the
    // boundary is empty.
    let old = LocalJj
        .run(
            &repo.worker,
            &[
                "log",
                "--no-graph",
                "-r",
                "heads((::@-) ~ (empty() & description(exact:\"\")))",
                "-T",
                "commit_id",
            ],
        )
        .await;
    let old_error = format!("{:#}", old.unwrap_err());
    assert!(old_error.contains(&missing), "{old_error}");
    let record = create(&LocalJj, &repo.worker, "exec_shallow", "local").await.unwrap();
    assert_eq!(record.base_commit.as_deref(), Some(base.as_str()));

    // A concurrently advancing trunk must not remove the positioned base
    // from the candidate set, even when that base is an empty commit.
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["new", "-m", "Advanced trunk"]);
    boss_engine_test_git::jj::JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    assert_eq!(positioned_base(&LocalJj, &repo.worker).await.unwrap(), base);

    // If trunk itself cannot bound the missing history, preserve the failure
    // and identify the shallow boundary instead of silently losing provenance.
    boss_engine_test_git::jj::JjRepo::run(
        &repo.repo,
        &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "root()"],
    );
    let error = positioned_base(&LocalJj, &repo.worker).await.unwrap_err();
    assert!(error.to_string().contains("shallow boundaries"), "{error:#}");
    assert!(error.to_string().contains(&boundary), "{error:#}");
}
