use super::*;
use boss_engine_test_git::jj::JjRepo;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Store {
    repo: JjRepo,
    damage_on_fetch: AtomicBool,
    conflicted: bool,
    fail_fetch: AtomicBool,
    damage_after_repair: AtomicBool,
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
        if self.fail_fetch.load(Ordering::SeqCst) && args.starts_with(&["git", "fetch"]) {
            anyhow::bail!("SSH connection reset");
        }
        let result = LocalJj.run(workspace, args).await?;
        if self.damage_after_repair.load(Ordering::SeqCst) && args.starts_with(&["bookmark", "set", "pr/7"]) {
            damage(&self.repo.repo, self.conflicted);
        }
        if args.starts_with(&["git", "fetch"]) && self.damage_on_fetch.swap(false, Ordering::SeqCst) {
            damage(&self.repo.repo, self.conflicted);
        }
        Ok(result)
    }
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
            fail_fetch: false.into(),
            damage_after_repair: false.into(),
        },
        record,
        head,
    )
}

async fn repaired(conflicted: bool, restore: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, head) = scene(dir.path(), conflicted, restore).await;
    let cube = FakeCube::new(&store, &head);
    if restore {
        let preserved = one(&store.repo.repo, &record.head());
        let report = restore_rebased(
            &store,
            &cube,
            &record,
            &store.repo.replacement,
            Some("pr/7"),
            "main",
            "origin",
        )
        .await
        .unwrap();
        assert!(report.pr_bound);
        assert!(JjRepo::run(&store.repo.replacement, &["file", "show", "root:work.txt"]).contains("preserved work"));
        assert_eq!(one(&store.repo.repo, &record.head()), preserved);
        assert_eq!(one(&store.repo.repo, &record.publication()), preserved);
    } else {
        let result = reconcile(&store, &cube, &record, Some("pr/7"), "main")
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

/// Stands in for `cube workspace repair-pr-bookmark`: fetches the head branch
/// and points the alias at the head, through the same store the test damages.
struct FakeCube<'a> {
    store: &'a Store,
    sha: String,
    calls: AtomicUsize,
}

impl<'a> FakeCube<'a> {
    fn new(store: &'a Store, sha: &str) -> Self {
        Self {
            store,
            sha: sha.to_owned(),
            calls: 0.into(),
        }
    }
}

#[async_trait]
impl PrAliasRepair for FakeCube<'_> {
    async fn repair(&self, store: &Path, pr: u64) -> Result<()> {
        assert_eq!(pr, 7);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let jj = self.store;
        jj.run(
            store,
            &["git", "fetch", "--remote", "origin", "--branch", "exact:published"],
        )
        .await?;
        jj.run(
            store,
            &["bookmark", "set", "pr/7", "-r", &self.sha, "--allow-backwards"],
        )
        .await?;
        Ok(())
    }
}

/// Fails the way the production wrappers do: Cube's stderr, classified.
struct FailingCube(&'static str);

#[async_trait]
impl PrAliasRepair for FailingCube {
    async fn repair(&self, _: &Path, _: u64) -> Result<()> {
        Err(classify_repair_error(anyhow::anyhow!(self.0)))
    }
}

#[tokio::test]
async fn lookup_failures_remain_retryable_but_cube_refusals_block() {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, _) = scene(dir.path(), false, false).await;
    let transient = reconcile(
        &store,
        &FailingCube("Cube command failed: HTTP 503 network unavailable"),
        &record,
        Some("pr/7"),
        "main",
    )
    .await
    .unwrap_err();
    assert!(
        !execution_bookmark::is_pointer_integrity_error(&transient),
        "{transient:#}"
    );
    for refusal in [
        "Cube command failed: cannot repair `pr/7`: cube cannot move `pr/7`: cube will not pick a side with unique content",
        "Cube command failed: cannot repair `pr/7`: PR 7 returned no head.sha",
        "Cube command failed: cannot repair `pr/7`: PR 7 returned no head.sha: \"\" is not a full 40-character commit id",
        // The shape `cube --json` writes to stderr, as the local and SSH
        // transports surface it.
        r#"Cube command failed: {"error":"cannot repair `pr/7`: PR 7 returned no head.sha: \"abc123\" is not a full 40-character commit id"}"#,
    ] {
        let blocked = reconcile(&store, &FailingCube(refusal), &record, Some("pr/7"), "main")
            .await
            .unwrap_err();
        assert!(execution_bookmark::is_pointer_integrity_error(&blocked), "{blocked:#}");
    }
}

/// Moves the execution's publication pointer (as Cube's fetch can) and then
/// hangs, so the timeout path has something real to restore.
struct MovesPointerThenHangs<'a>(&'a Store, String);

#[async_trait]
impl PrAliasRepair for MovesPointerThenHangs<'_> {
    async fn repair(&self, store: &Path, _: u64) -> Result<()> {
        self.0
            .run(store, &["bookmark", "set", &self.1, "-r", "main", "--allow-backwards"])
            .await?;
        std::future::pending().await
    }
}

#[tokio::test]
async fn repair_timeout_keeps_prestart_retry_classification_and_restores_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, _) = scene(dir.path(), false, false).await;
    let saved = one(&store.repo.repo, &record.head());
    let published = one(&store.repo.repo, &record.publication());
    assert_ne!(
        one(&store.repo.repo, "main"),
        saved,
        "the stand-in must really move the pointer"
    );
    let stand_in = MovesPointerThenHangs(&store, record.publication().to_string());
    let err = repair_with(&store, &stand_in, &record, 7, Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
    assert!(!execution_bookmark::is_pointer_integrity_error(&err));
    assert_eq!(one(&store.repo.repo, &record.head()), saved);
    assert_eq!(one(&store.repo.repo, &record.publication()), published);
}

#[tokio::test]
async fn ensure_repairs_missing_and_conflicted_alias_once() {
    for conflicted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (store, record, sha) = scene(dir.path(), conflicted, false).await;
        let lookup = FakeCube::new(&store, &sha);
        ensure(&store, &lookup, &record, 7).await.unwrap();
        assert_eq!(one(&store.repo.repo, "pr/7"), lookup.sha);
        ensure(&store, &lookup, &record, 7).await.unwrap();
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn lost_again_after_repair_is_loud_and_not_a_legacy_blocker() {
    for conflicted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (store, record, sha) = scene(dir.path(), conflicted, false).await;
        store.damage_after_repair.store(true, Ordering::SeqCst);
        let lookup = FakeCube::new(&store, &sha);
        let err = reconcile(&store, &lookup, &record, Some("pr/7"), "main")
            .await
            .unwrap_err();
        assert!(execution_bookmark::is_pointer_integrity_error(&err));
        assert!(err.to_string().contains("removed or conflicted again"));
        assert!(!execution_bookmark::is_legacy_pr_anchor_blocker(&err.to_string()));
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn transient_fetch_failure_retries_repair_and_preserves_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let (store, record, sha) = scene(dir.path(), false, false).await;
    let saved = one(&store.repo.repo, &record.head());
    let lookup = FakeCube::new(&store, &sha);
    store.fail_fetch.store(true, Ordering::SeqCst);
    let err = ensure(&store, &lookup, &record, 7).await.unwrap_err();
    assert!(!execution_bookmark::is_pointer_integrity_error(&err));
    assert!(format!("{err:#}").contains("SSH connection reset"));
    assert_eq!(one(&store.repo.repo, &record.head()), saved);
    store.fail_fetch.store(false, Ordering::SeqCst);
    ensure(&store, &lookup, &record, 7).await.unwrap();
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 2);
    assert_eq!(one(&store.repo.repo, "pr/7"), lookup.sha);
}
