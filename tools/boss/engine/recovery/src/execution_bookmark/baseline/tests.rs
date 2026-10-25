use super::*;
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn shallow_history_below_trunk_does_not_affect_baseline_creation() {
    let root = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(root.path());
    let missing = JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", "-m", "Shallow boundary"]);
    let boundary = JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", "-m", "Trunk base"]);
    JjRepo::run(&repo.repo, &["bookmark", "create", "main", "-r", "@"]);
    JjRepo::run(
        &repo.repo,
        &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "main"],
    );
    let base = JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.worker, &["new", "main"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Execution"]);
    let git = repo.repo.join(".jj/repo/store/git");
    std::fs::write(git.join("shallow"), format!("{boundary}\n")).unwrap();
    std::fs::remove_file(git.join("objects").join(&missing[..2]).join(&missing[2..])).unwrap();

    // The old query needs the missing parent to test whether the boundary is empty.
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
    JjRepo::run(&repo.repo, &["new", "-m", "Advanced trunk"]);
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    assert_eq!(durable_baseline(&LocalJj, &repo.worker).await.unwrap(), base);

    // If trunk itself cannot bound the missing history, preserve the failure
    // and identify the shallow boundary instead of silently losing provenance.
    JjRepo::run(
        &repo.repo,
        &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "root()"],
    );
    let error = durable_baseline(&LocalJj, &repo.worker).await.unwrap_err();
    assert!(error.to_string().contains("shallow boundaries"), "{error:#}");
    assert!(error.to_string().contains(&boundary), "{error:#}");
}
