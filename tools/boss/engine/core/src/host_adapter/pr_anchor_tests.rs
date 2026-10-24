use super::*;
use boss_engine_test_git::jj::JjRepo;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

struct Store {
    repo: JjRepo,
    damage_on_fetch: AtomicBool,
    conflicted: bool,
    fail_fetch: bool,
    damage_after_repair: bool,
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
        if self.fail_fetch && args.starts_with(&["git", "fetch"]) {
            anyhow::bail!("SSH connection reset");
        }
        let result = LocalJj.run(workspace, args).await?;
        if self.damage_after_repair && args.starts_with(&["bookmark", "set", "pr/7"]) {
            damage(&self.repo.repo, self.conflicted);
        }
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
            fail_fetch: false,
            damage_after_repair: false,
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
    let err = repair_with_gh(&PendingLookup, &record, 7, &PathBuf::from("unused-gh"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
    assert!(!execution_bookmark::is_pointer_integrity_error(&err));
}

struct FakeHead {
    sha: String,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl PrHeadLookup for FakeHead {
    async fn fetch(&self, slug: &str, pr: u64) -> Result<(String, String)> {
        assert_eq!((slug, pr), ("example/recovery", 7));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(("published".into(), self.sha.clone()))
    }
}

#[tokio::test]
async fn ensure_repairs_missing_and_conflicted_alias_once() {
    for conflicted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (store, record, sha) = scene(dir.path(), conflicted, false).await;
        let lookup = FakeHead { sha, calls: 0.into() };
        ensure_with_gh(&store, &record, 7, &lookup).await.unwrap();
        assert_eq!(one(&store.repo.repo, "pr/7"), lookup.sha);
        ensure_with_gh(&store, &record, 7, &lookup).await.unwrap();
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn lost_again_after_repair_is_loud_and_not_a_legacy_blocker() {
    for conflicted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, record, sha) = scene(dir.path(), conflicted, false).await;
        store.damage_after_repair = true;
        let lookup = FakeHead { sha, calls: 0.into() };
        let err = reconcile_with_gh(&store, &record, Some("pr/7"), "main", &lookup)
            .await
            .unwrap_err();
        assert!(execution_bookmark::is_pointer_integrity_error(&err));
        assert!(err.to_string().contains("removed or conflicted again"));
        assert!(err.to_string().contains(&lookup.sha));
        assert!(!execution_bookmark::is_legacy_pr_anchor_blocker(&err.to_string()));
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn transient_fetch_failure_retries_repair_and_preserves_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, record, sha) = scene(dir.path(), false, false).await;
    let saved = one(&store.repo.repo, &record.head());
    let lookup = FakeHead { sha, calls: 0.into() };
    store.fail_fetch = true;
    let err = ensure_with_gh(&store, &record, 7, &lookup).await.unwrap_err();
    assert!(!execution_bookmark::is_pointer_integrity_error(&err));
    assert!(format!("{err:#}").contains("SSH connection reset"));
    use crate::work::{CreateExecutionInput, ExecutionKind, WorkDb};
    let db = WorkDb::open_in_memory().unwrap();
    let product = crate::test_support::create_test_product(&db);
    let task = crate::test_support::create_test_chore(&db, &product.id, "retry recovery");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .build(),
        )
        .unwrap();
    assert!(!crate::execution_bookmark_recovery::block_unrecoverable(&db, &execution, &err).unwrap());
    let crate::work::WorkItem::Chore(current) = db.get_work_item(&task.id).unwrap() else {
        panic!("expected chore")
    };
    assert_eq!(current.status, task.status);
    assert_eq!(current.autostart, task.autostart);
    assert!(current.blocked_reason.is_none());
    assert!(
        db.list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
            .unwrap()
            .is_empty()
    );
    assert_eq!(one(&store.repo.repo, &record.head()), saved);
    store.fail_fetch = false;
    ensure_with_gh(&store, &record, 7, &lookup).await.unwrap();
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 2);
    assert_eq!(one(&store.repo.repo, "pr/7"), lookup.sha);
}

#[tokio::test]
async fn invalid_authoritative_sha_is_an_integrity_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, _) = scene(dir.path(), false, false).await;
    let lookup = FakeHead {
        sha: "not-a-sha".into(),
        calls: 0.into(),
    };
    let err = ensure_with_gh(&store, &record, 7, &lookup).await.unwrap_err();
    assert!(execution_bookmark::is_pointer_integrity_error(&err));
    assert!(format!("{err:#}").contains("invalid or absent"));
}
