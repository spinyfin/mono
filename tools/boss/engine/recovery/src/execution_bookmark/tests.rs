use super::*;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
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
            _root: root,
            repo,
            worker,
            replacement,
        }
    }

    async fn record(&self, id: &str) -> ExecutionBookmark {
        create(&LocalJj, &self.worker, id, "local").await.unwrap()
    }

    async fn operation(&self) -> String {
        LocalJj
            .run(&self.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"])
            .await
            .unwrap()
    }

    async fn bookmark_names(&self) -> String {
        LocalJj
            .run(&self.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
            .await
            .unwrap()
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
    let before = f.operation().await;
    let record = create_from(&LocalJj, &f.worker, "exec_fresh", "local", None, None)
        .await
        .unwrap();
    assert_eq!(f.operation().await, before, "empty dispatch must not mutate the store");
    assert_eq!(
        record.base_commit,
        Some(one_commit(&LocalJj, &f.worker, "trunk()").await.unwrap())
    );
    let names = f.bookmark_names().await;
    assert!(
        !names.lines().any(|name| name.starts_with("boss")),
        "empty dispatch must not write execution bookmarks; got {names}"
    );

    // The worker need not advance any bookmark, even across new changes.
    std::fs::write(f.worker.join("first.txt"), "first edit").unwrap();
    LocalJj.run(&f.worker, &["new", "-m", "Follow-up"]).await.unwrap();
    std::fs::write(f.worker.join("second.txt"), "second edit").unwrap();
    let saved = preserve(&LocalJj, &record, &f.worker, None, None).await.unwrap();
    assert!(saved.tip.is_some());
    assert!(saved.inspect.is_none());
    let names = f.bookmark_names().await;
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
async fn dispatch_leaves_a_leftover_legacy_baseline_name_alone() {
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
    let before = f.operation().await;
    let record = create_from(&LocalJj, &f.worker, "exec_leftover", "local", None, None)
        .await
        .unwrap();
    assert_eq!(f.operation().await, before, "empty dispatch must not mutate the store");
    assert_eq!(record.base_commit.as_deref(), Some(trunk.as_str()));
    assert!(
        f.bookmark_names()
            .await
            .lines()
            .any(|name| name == "boss-base/exec_leftover"),
        "a bookmark an earlier engine created is neither read nor removed"
    );
}

#[tokio::test]
async fn a_successor_inherits_its_predecessors_recorded_floor_without_a_store_operation() {
    let f = Fixture::new().await;
    let prior = f.record("exec_prior").await;
    let before = f.operation().await;
    let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&prior), None)
        .await
        .unwrap();
    assert_eq!(
        f.operation().await,
        before,
        "restored dispatch must not mutate the store"
    );
    assert_eq!(next.base_commit, prior.base_commit);
    let names = f.bookmark_names().await;
    assert!(!names.contains("exec_next"), "{names}");
}

#[tokio::test]
async fn a_predecessor_without_a_usable_baseline_falls_back_to_its_pr_head_before() {
    let f = Fixture::new().await;
    let prior = f.record("exec_pr_head_floor").await;
    let floor = prior.base_commit.clone().unwrap();
    for base_commit in [None, Some("0123456789abcdef0123456789abcdef01234567".to_owned())] {
        let legacy = ExecutionBookmark {
            base_commit,
            pr_head_before: Some(floor.clone()),
            ..prior.clone()
        };
        let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&legacy), None)
            .await
            .unwrap();
        assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
    }
}

#[tokio::test]
async fn a_conflicted_legacy_boss_base_is_ignored_when_inheriting_a_predecessor_floor() {
    let f = Fixture::new().await;
    let prior = f.record("exec_conflicted_boss_base").await;
    let floor = prior.base_commit.clone().unwrap();
    let legacy = ExecutionBookmark {
        base_commit: None,
        pr_head_before: Some(floor.clone()),
        ..prior.clone()
    };
    // Two concurrent operations set a bookmark an earlier engine created to
    // different commits, which leaves it conflicted.
    let op = f.operation().await;
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
                    "boss-base/exec_conflicted_boss_base",
                    "-r",
                    target,
                    "--allow-backwards",
                ],
            )
            .await
            .unwrap();
    }
    let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&legacy), None)
        .await
        .unwrap();
    assert_eq!(next.base_commit.as_deref(), Some(floor.as_str()));
}

#[tokio::test]
async fn create_from_ignores_an_unusable_inherited_base() {
    let f = Fixture::new().await;
    let prior = f.record("exec_unusable_inherited").await;
    let floor = prior.base_commit.clone().unwrap();
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

    let fresh_fixture = Fixture::new().await;
    let expected = one_commit(&LocalJj, &fresh_fixture.worker, "trunk()").await.unwrap();
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
async fn a_predecessor_from_another_store_is_refused() {
    let f = Fixture::new().await;
    let prior = ExecutionBookmark {
        repo_path: PathBuf::from("/nonexistent/other-store"),
        ..f.record("exec_prior").await
    };
    let error = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&prior), None)
        .await
        .unwrap_err();
    assert!(is_pointer_integrity_error(&error), "{error:#}");
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
