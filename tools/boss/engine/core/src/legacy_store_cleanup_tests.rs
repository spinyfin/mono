use super::*;
use crate::test_support::*;
use anyhow::anyhow;
use boss_engine_recovery::execution_bookmark::{LocalJj, cleanup_legacy_store};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Default)]
struct FakeCleaner {
    calls: Mutex<Vec<String>>,
    failing_hosts: Vec<String>,
    gc_fails: bool,
    report: LegacyCleanupReport,
}

#[async_trait]
impl StoreCleaner for FakeCleaner {
    async fn cleanup(&self, host_id: &str, repo: &Path, heads: &[String]) -> Result<LegacyCleanupReport> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("cleanup {host_id} {} heads={}", repo.display(), heads.len()));
        if host_id == "removed" {
            return Err(UnregisteredHost(host_id.to_owned()).into());
        }
        if self.failing_hosts.iter().any(|host| host == host_id) {
            return Err(anyhow!("host {host_id} is unreachable"));
        }
        Ok(self.report.clone())
    }

    async fn collect(&self, host_id: &str, repo_id: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("gc {host_id} {repo_id}"));
        if self.gc_fails {
            return Err(anyhow!("cube is busy"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_removed_host_is_retired_with_a_reason_and_the_loop_finishes() {
    let (_dir, db) = open_db();
    pend(&db, "removed", "/stores/mono", Some("mono"));
    let cleaner = FakeCleaner::default();
    run_until_clean(&db, &cleaner).await;
    let retired = status(&db, "removed", "/stores/mono");
    assert!(retired.completed);
    assert_eq!(retired.attempts, 1);
    assert!(retired.last_error.unwrap().contains("no longer registered"));
    run_until_clean(&db, &cleaner).await;
    assert_eq!(cleaner.calls().len(), 1);
}

impl FakeCleaner {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn pend(db: &WorkDb, host: &str, repo: &str, cube_repo: Option<&str>) {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO legacy_store_cleanups (host_id, repo_path, cube_repo_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![host, repo, cube_repo],
        )
        .unwrap();
}

fn report(forgotten: usize) -> LegacyCleanupReport {
    LegacyCleanupReport {
        found: forgotten,
        forgotten,
        operations: 1,
        ..LegacyCleanupReport::default()
    }
}

fn status(db: &WorkDb, host: &str, repo: &str) -> crate::work::LegacyStoreCleanupStatus {
    db.legacy_store_cleanup_status(host, Path::new(repo)).unwrap().unwrap()
}

#[tokio::test]
async fn cleans_each_store_once_then_collects_and_never_returns() {
    let (_dir, db) = open_db();
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    pend(&db, "local", a.to_str().unwrap(), Some("mono"));
    pend(&db, "local", b.to_str().unwrap(), None);
    let cleaner = FakeCleaner {
        report: report(3),
        ..FakeCleaner::default()
    };

    let first = run_one_pass(&db, &cleaner).await.unwrap();

    assert_eq!(
        first,
        PassReport {
            cleaned: 2,
            forgotten: 6,
            ..PassReport::default()
        }
    );
    assert_eq!(
        cleaner.calls(),
        vec![
            format!("cleanup local {} heads=0", a.display()),
            "gc local mono".to_owned(),
            format!("cleanup local {} heads=0", b.display()),
        ]
    );
    assert!(status(&db, "local", a.to_str().unwrap()).completed);
    assert_eq!(status(&db, "local", a.to_str().unwrap()).gc_error, None);
    let unknown = status(&db, "local", b.to_str().unwrap());
    assert!(unknown.completed, "a missing cube id does not undo the cleanup");
    assert!(unknown.gc_error.unwrap().contains("never recorded"));

    assert_eq!(run_one_pass(&db, &cleaner).await.unwrap(), PassReport::default());
    assert_eq!(cleaner.calls().len(), 3, "a cleaned store is never visited again");
}

#[tokio::test]
async fn a_failing_store_waits_to_be_retried_and_does_not_stop_the_others() {
    let (_dir, db) = open_db();
    pend(&db, "remote", "/stores/mono", Some("mono"));
    let tmp = tempfile::tempdir().unwrap();
    pend(&db, "local", tmp.path().to_str().unwrap(), Some("mono"));
    let cleaner = FakeCleaner {
        failing_hosts: vec!["remote".into()],
        report: report(1),
        ..FakeCleaner::default()
    };

    let first = run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!((first.cleaned, first.failed), (1, 1));
    let failed = status(&db, "remote", "/stores/mono");
    assert!(!failed.completed);
    assert_eq!(failed.attempts, 1);
    assert!(failed.last_error.unwrap().contains("unreachable"));

    let again = run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!(again, PassReport::default(), "not retried before the backoff");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE legacy_store_cleanups SET last_attempt_at = CAST(unixepoch('now') - ?1 AS TEXT)",
            [RETRY_AFTER.as_secs() as i64 + 1],
        )
        .unwrap();
    let retried = run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!((retried.cleaned, retried.failed), (0, 1));
    assert_eq!(status(&db, "remote", "/stores/mono").attempts, 2);
}

#[tokio::test]
async fn a_collection_failure_is_recorded_without_undoing_the_cleanup() {
    let (_dir, db) = open_db();
    let tmp = tempfile::tempdir().unwrap();
    pend(&db, "local", tmp.path().to_str().unwrap(), Some("mono"));
    let cleaner = FakeCleaner {
        gc_fails: true,
        report: report(2),
        ..FakeCleaner::default()
    };
    let pass = run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!((pass.cleaned, pass.failed), (1, 0));
    let status = status(&db, "local", tmp.path().to_str().unwrap());
    assert!(status.completed);
    assert!(status.gc_error.unwrap().contains("busy"));
}

#[tokio::test(start_paused = true)]
async fn the_loop_ends_once_every_store_is_clean() {
    let (_dir, db) = open_db();
    let tmp = tempfile::tempdir().unwrap();
    pend(&db, "local", tmp.path().to_str().unwrap(), Some("mono"));
    let cleaner = FakeCleaner {
        report: report(1),
        ..FakeCleaner::default()
    };
    tokio::time::timeout(POLL_INTERVAL, run_until_clean(&db, &cleaner))
        .await
        .expect("a finished migration leaves nothing for the loop to wait for");
    assert!(db.pending_legacy_store_cleanups(0).unwrap().is_empty());
    run_until_clean(&db, &cleaner).await;
    assert_eq!(cleaner.calls().len(), 2, "the second run found nothing to do");
}

#[tokio::test]
async fn nothing_forgotten_means_nothing_to_collect() {
    let (_dir, db) = open_db();
    let tmp = tempfile::tempdir().unwrap();
    pend(&db, "local", tmp.path().to_str().unwrap(), Some("mono"));
    let cleaner = FakeCleaner::default();
    run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!(cleaner.calls().len(), 1, "no gc: {:?}", cleaner.calls());
}

#[tokio::test]
async fn a_pass_that_only_wrote_operations_still_collects() {
    let (_dir, db) = open_db();
    let tmp = tempfile::tempdir().unwrap();
    pend(&db, "local", tmp.path().to_str().unwrap(), Some("mono"));
    let cleaner = FakeCleaner {
        report: LegacyCleanupReport {
            found: 3,
            operations: 2,
            ..LegacyCleanupReport::default()
        },
        ..FakeCleaner::default()
    };
    run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!(
        cleaner.calls().last().unwrap(),
        "gc local mono",
        "{:?}",
        cleaner.calls()
    );
}

#[tokio::test]
async fn a_local_store_that_is_gone_is_complete_without_asking_jj() {
    let (_dir, db) = open_db();
    pend(&db, "local", "/does/not/exist/anywhere", Some("mono"));
    let cleaner = FakeCleaner {
        failing_hosts: vec!["local".into()],
        ..FakeCleaner::default()
    };
    let pass = run_one_pass(&db, &cleaner).await.unwrap();
    assert_eq!((pass.cleaned, pass.failed), (1, 0));
    assert!(cleaner.calls().is_empty());
    assert!(status(&db, "local", "/does/not/exist/anywhere").completed);
}

/// The real thing against a real jj store: the engine's recorded pushed
/// heads decide what is published, and only unpublished work is archived.
struct LocalStores;

#[async_trait]
impl StoreCleaner for LocalStores {
    async fn cleanup(&self, _host: &str, repo: &Path, heads: &[String]) -> Result<LegacyCleanupReport> {
        cleanup_legacy_store(&LocalJj, repo, heads).await
    }

    async fn collect(&self, _host: &str, _repo_id: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn recorded_pushed_heads_keep_squash_merged_work_out_of_the_archives() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Squash merged");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let tmp = tempfile::tempdir().unwrap();
    let store = boss_engine_test_git::jj::JjRepo::new(tmp.path());
    let run = |args: &[&str]| boss_engine_test_git::jj::JjRepo::run(&store.repo, args);
    let work = |file: &str| -> String {
        boss_engine_test_git::jj::JjRepo::run(&store.worker, &["new", "main", "-m", file]);
        std::fs::write(store.worker.join(file), file).unwrap();
        boss_engine_test_git::jj::JjRepo::snapshot(&store.worker);
        boss_engine_test_git::jj::JjRepo::run(&store.worker, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .trim()
            .to_owned()
    };
    // The PR branch was pushed, then merged by squash and deleted: nothing
    // in the store says so, but the engine recorded the pushed head.
    let merged = work("merged.txt");
    let abandoned = work("abandoned.txt");
    run(&[
        "bookmark",
        "create",
        "boss-recovery/exec_merged",
        "boss/exec_merged",
        "-r",
        &merged,
    ]);
    run(&["bookmark", "create", "boss-recovery/exec_abandoned", "-r", &abandoned]);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![execution.id, merged],
        )
        .unwrap();
    let repo: PathBuf = store.repo.clone();
    pend(&db, "local", repo.to_str().unwrap(), Some("mono"));

    let pass = run_one_pass(&db, &LocalStores).await.unwrap();

    assert_eq!(
        (pass.cleaned, pass.archived, pass.forgotten, pass.kept),
        (1, 1, 3, 0),
        "{pass:?}"
    );
    let names = run(&["bookmark", "list", "-T", "name ++ \"\\n\""]);
    assert_eq!(
        names.lines().filter(|n| *n != "main").collect::<Vec<_>>(),
        vec![format!("preserved-archive/exec_abandoned/legacy/{abandoned}").as_str()]
    );
}
