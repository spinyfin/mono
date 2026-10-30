use super::reconcile_tests::{bookmark, one, revision_on_pr_head};
use super::*;
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn conflicted_and_diverged_targets_are_all_archived_before_release() {
    let scene = revision_on_pr_head("exec_archive_conflict").await;
    let repo = &scene.repo.repo;
    let original = one(repo, &revision(&scene.record.head()));
    JjRepo::run(repo, &["new", "main", "-m", "Other unpublished work"]);
    std::fs::write(repo.join("other.txt"), "other work").unwrap();
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
    let archived = release_heads(&LocalJj, &scene.record).await.unwrap();
    for target in targets.values().flatten() {
        assert!(archived.iter().any(|(_, commit)| commit == target));
    }
    assert!(archived.iter().any(|(_, commit)| commit == &other));
    assert!(
        archived
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
    prune(
        &LocalJj,
        &scene.record,
        &[Voucher::commit("published", &original).unwrap()],
        "main",
    )
    .await
    .unwrap();
    for (name, commit) in archived {
        assert!(name.starts_with("preserved/exec_archive_conflict/"));
        assert_eq!(one(repo, &revision(&name)), commit);
    }
}

struct ArchiveWriteFailure;

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
    let err = release_heads(&ArchiveWriteFailure, &scene.record).await.unwrap_err();
    assert!(err.to_string().contains("archive storage unavailable"));
    assert_eq!(snapshot_pointers(&LocalJj, &scene.record).await.unwrap(), before);
}

#[tokio::test]
async fn another_fresh_start_keeps_earlier_archives() {
    let scene = revision_on_pr_head("exec_archive_again").await;
    let first = release_heads(&LocalJj, &scene.record).await.unwrap();
    JjRepo::run(&scene.repo.worker, &["new", "main", "-m", "Next attempt"]);
    let record = create(&LocalJj, &scene.repo.worker, &scene.record.execution_id, "local")
        .await
        .unwrap();
    std::fs::write(scene.repo.worker.join("later.txt"), "later work").unwrap();
    JjRepo::run(&scene.repo.worker, &["status"]);
    let second = release_heads(&LocalJj, &record).await.unwrap();
    for (name, commit) in first.iter().chain(&second) {
        assert_eq!(one(&scene.repo.repo, &revision(name)), *commit);
    }
}
