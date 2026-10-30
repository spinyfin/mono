use super::reconcile_tests::{bookmark, one, revision_on_pr_head};
use super::*;
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn conflicted_and_diverged_targets_are_all_archived_before_release() {
    let scene = revision_on_pr_head("exec_archive_conflict").await;
    let repo = &scene.repo.repo;
    JjRepo::run(repo, &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "main"]);
    let original = one(repo, &revision(&scene.record.head()));
    JjRepo::run(repo, &["new", "main", "-m", "Other unpublished work"]);
    std::fs::write(repo.join("other.txt"), "other work").unwrap();
    JjRepo::run(repo, &["status"]);
    let other = one(repo, "@");
    bookmark(repo, &scene.record.publication(), &other);
    let op = JjRepo::run(repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
    for target in [&other, "main"] {
        JjRepo::run(
            repo,
            &[
                "--at-op",
                op.trim(),
                "bookmark",
                "set",
                &scene.record.head(),
                "-r",
                target,
                "--allow-backwards",
            ],
        );
    }
    assert!(snapshot_pointers(&LocalJj, &scene.record).await.is_err());
    let targets = snapshot::resolve_bookmark_targets(
        &LocalJj,
        repo,
        &[scene.record.base(), scene.record.head(), scene.record.publication()],
    )
    .await
    .unwrap();
    let archived = release_heads(&LocalJj, &scene.record, true).await.unwrap();
    for target in targets.values().flatten() {
        if !JjRepo::run(
            repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("{target} ~ (::main | ::pr/7)"),
                "-T",
                "commit_id",
            ],
        )
        .trim()
        .is_empty()
        {
            assert!(
                archived.iter().any(|(_, commit)| commit == target),
                "target {target} missing from {archived:?}"
            );
        }
    }
    assert!(archived.iter().any(|(_, commit)| commit == &other));
    assert!(
        !archived
            .iter()
            .any(|(_, commit)| commit == scene.record.base_commit.as_ref().unwrap())
    );
    assert!(
        snapshot_pointers(&LocalJj, &scene.record)
            .await
            .unwrap()
            .iter()
            .all(|(_, target)| target.is_none())
    );
    for name in [scene.record.head(), scene.record.publication()] {
        bookmark(repo, &name, &original);
    }
    bookmark(repo, &scene.record.base(), scene.record.base_commit.as_ref().unwrap());
    let outcome = prune(
        &LocalJj,
        &scene.record,
        &[Voucher::commit("published", &original).unwrap()],
        "main",
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PruneOutcome::Published { .. }));
    for (name, commit) in archived {
        assert!(name.starts_with("preserved/exec_archive_conflict/"));
        assert_eq!(one(repo, &revision(&name)), commit);
    }
}

struct ArchiveWriteFailure;

#[tokio::test]
async fn fallback_preserves_publication_tracking_and_allows_another_push() {
    let scene = revision_on_pr_head("exec_archive_push").await;
    let origin_dir = tempfile::tempdir().unwrap();
    let origin = JjRepo::new(origin_dir.path());
    let repo = &scene.repo.repo;
    JjRepo::run(
        repo,
        &[
            "describe",
            "-r",
            &format!(
                "(::{} & description(exact:\"\")) ~ root()",
                revision(&scene.record.publication())
            ),
            "-m",
            "PR positioning",
        ],
    );
    JjRepo::run(
        repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            origin.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(
        repo,
        &[
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            &scene.record.publication(),
            "--allow-new",
        ],
    );
    release_heads(&LocalJj, &scene.record, false).await.unwrap();
    JjRepo::run(
        &scene.repo.worker,
        &["new", &scene.record.publication(), "-m", "Next published work"],
    );
    std::fs::write(scene.repo.worker.join("next.txt"), "next").unwrap();
    JjRepo::run(
        &scene.repo.worker,
        &["bookmark", "set", &scene.record.publication(), "-r", "@"],
    );
    JjRepo::run(
        repo,
        &[
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            &scene.record.publication(),
        ],
    );
    JjRepo::run(&origin.repo, &["git", "import"]);
    assert_eq!(
        one(repo, &revision(&scene.record.publication())),
        one(&origin.repo, &revision(&scene.record.publication()))
    );
    // A self-retry must also retain tracking when it reuses the name.
    release_heads(&LocalJj, &scene.record, true).await.unwrap();
    JjRepo::run(
        &scene.repo.worker,
        &["bookmark", "set", &scene.record.publication(), "-r", "@"],
    );
    JjRepo::run(
        repo,
        &[
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            &scene.record.publication(),
        ],
    );
}

#[tokio::test]
async fn empty_run_needs_no_permanent_archive() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_empty_archive", "local")
        .await
        .unwrap();
    assert!(release_heads(&LocalJj, &record, true).await.unwrap().is_empty());
}

#[async_trait]
impl Jj for ArchiveWriteFailure {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        if args.iter().any(|arg| arg.starts_with("preserved/")) {
            anyhow::bail!("archive storage unavailable");
        }
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[tokio::test]
async fn archive_write_failure_never_releases_original_pointers() {
    let scene = revision_on_pr_head("exec_archive_failure").await;
    let before = snapshot_pointers(&LocalJj, &scene.record).await.unwrap();
    let err = release_heads(&ArchiveWriteFailure, &scene.record, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("archive storage unavailable"));
    assert_eq!(snapshot_pointers(&LocalJj, &scene.record).await.unwrap(), before);
}

#[tokio::test]
async fn another_fresh_start_keeps_earlier_archives() {
    let scene = revision_on_pr_head("exec_archive_again").await;
    let first = release_heads(&LocalJj, &scene.record, true).await.unwrap();
    JjRepo::run(&scene.repo.worker, &["new", "main", "-m", "Next attempt"]);
    let record = create(&LocalJj, &scene.repo.worker, &scene.record.execution_id, "local")
        .await
        .unwrap();
    std::fs::write(scene.repo.worker.join("later.txt"), "later work").unwrap();
    JjRepo::run(&scene.repo.worker, &["status"]);
    let second = release_heads(&LocalJj, &record, true).await.unwrap();
    for (name, commit) in first.iter().chain(&second) {
        assert_eq!(one(&scene.repo.repo, &revision(name)), *commit);
    }
}
