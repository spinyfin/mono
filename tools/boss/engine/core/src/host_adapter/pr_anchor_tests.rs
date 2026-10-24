use super::*;
use boss_engine_test_git::jj::JjRepo;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

struct Store {
    repo: JjRepo,
    damage_on_fetch: AtomicBool,
    conflicted: bool,
}

fn one(repo: &Path, rev: &str) -> String {
    JjRepo::run(repo, &["log", "--no-graph", "-r", rev, "-T", "commit_id"])
        .trim()
        .to_owned()
}

fn damage(repo: &Path, conflicted: bool) {
    if conflicted {
        JjRepo::run(
            repo,
            &["bookmark", "set", "pr/7", "-r", "published+", "--allow-backwards"],
        );
        let op = JjRepo::run(repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
        for target in ["published", "main"] {
            JjRepo::run(
                repo,
                &[
                    "--at-op",
                    op.trim(),
                    "bookmark",
                    "set",
                    "pr/7",
                    "-r",
                    target,
                    "--allow-backwards",
                ],
            );
        }
        let heads = JjRepo::run(
            repo,
            &[
                "log",
                "--no-graph",
                "-r",
                "bookmarks(exact:\"pr/7\")",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        assert_eq!(heads.lines().count(), 2);
    } else {
        JjRepo::run(repo, &["bookmark", "delete", "pr/7"]);
    }
}

#[async_trait]
impl Jj for Store {
    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }

    async fn run(&self, workspace: &Path, args: &[&str]) -> Result<String> {
        if args == ["git", "remote", "list"] {
            return Ok("origin https://github.com/example/recovery.git\n".into());
        }
        let result = LocalJj.run(workspace, args).await?;
        if args.starts_with(&["git", "fetch"]) && self.damage_on_fetch.swap(false, Ordering::SeqCst) {
            damage(&self.repo.repo, self.conflicted);
        }
        Ok(result)
    }
}

fn fake_gh(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-gh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn scene(dir: &Path, conflicted: bool, restore: bool) -> (Store, ExecutionBookmark, String) {
    let repo = JjRepo::new(dir);
    JjRepo::run(&repo.repo, &["bookmark", "set", "main"]);
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Published work"]);
    std::fs::write(repo.worker.join("work.txt"), "preserved work\n").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", "published", "pr/7"]);
    let head = one(&repo.repo, "published");
    JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
    let record = execution_bookmark::create(&LocalJj, &repo.worker, "exec_adapter", "local")
        .await
        .unwrap();
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain.txt"), "new baseline\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "main"]);
    if !restore {
        // Require reconciliation: the recorded baseline remains authoritative.
        JjRepo::run(
            &repo.repo,
            &["bookmark", "set", &record.base(), "-r", "main", "--allow-backwards"],
        );
    }
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    if !restore {
        damage(&repo.repo, conflicted);
    }
    (
        Store {
            repo,
            damage_on_fetch: AtomicBool::new(restore),
            conflicted,
        },
        record,
        head,
    )
}

async fn repaired(conflicted: bool, restore: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, head) = scene(dir.path(), conflicted, restore).await;
    let gh = fake_gh(
        dir.path(),
        &format!(
            "test \"$1\" = api || exit 9\ntest \"$4\" = repos/example/recovery/pulls/7 || exit 10\nprintf 'published\\n{head}\\n'"
        ),
    );
    if restore {
        let preserved = one(&store.repo.repo, &record.head());
        let report = restore_with_gh(
            &store,
            &record,
            &store.repo.replacement,
            Some("pr/7"),
            "main",
            "origin",
            &gh,
        )
        .await
        .unwrap();
        assert!(report.pr_bound);
        assert!(JjRepo::run(&store.repo.replacement, &["file", "show", "root:work.txt"]).contains("preserved work"));
        assert_eq!(one(&store.repo.repo, &record.head()), preserved);
        assert_eq!(one(&store.repo.repo, &record.publication()), preserved);
    } else {
        let result = reconcile_with_gh(&store, &record, Some("pr/7"), "main", &gh)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.anchor.commit(), head);
    }
    assert_eq!(one(&store.repo.repo, "pr/7"), head);
    assert!(execution_bookmark::diff(&store, &record).await.unwrap().is_empty());
}

#[tokio::test]
async fn reconcile_repairs_missing_alias_through_fake_gh() {
    repaired(false, false).await;
}

#[tokio::test]
async fn reconcile_repairs_conflicted_alias_through_fake_gh() {
    repaired(true, false).await;
}

#[tokio::test]
async fn restore_repairs_alias_deleted_by_fetch() {
    repaired(false, true).await;
}

#[tokio::test]
async fn restore_repairs_alias_conflicted_by_fetch() {
    repaired(true, true).await;
}

#[tokio::test]
async fn lookup_failures_remain_retryable_but_absent_head_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, _) = scene(dir.path(), false, false).await;
    for (body, permanent) in [
        ("echo 'HTTP 503 network unavailable' >&2; exit 1", false),
        ("printf 'null\\nnull\\n'", true),
    ] {
        let gh = fake_gh(dir.path(), body);
        let err = reconcile_with_gh(&store, &record, Some("pr/7"), "main", &gh)
            .await
            .unwrap_err();
        assert_eq!(
            execution_bookmark::is_pointer_integrity_error(&err),
            permanent,
            "{err:#}"
        );
    }
}

struct PendingLookup;

#[async_trait]
impl Jj for PendingLookup {
    async fn shared_repo(&self, workspace: &Path) -> Result<PathBuf> {
        LocalJj.shared_repo(workspace).await
    }

    async fn run(&self, _: &Path, args: &[&str]) -> Result<String> {
        assert_eq!(args, ["git", "remote", "list"]);
        std::future::pending().await
    }
}

#[tokio::test]
async fn lookup_timeout_keeps_prestart_retry_classification() {
    let dir = tempfile::tempdir().unwrap();
    let (_, record, _) = scene(dir.path(), false, false).await;
    tokio::time::pause();
    let err = repair_with_gh(&PendingLookup, &record, 7, Path::new("unused-gh"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
    assert!(!execution_bookmark::is_pointer_integrity_error(&err));
}
