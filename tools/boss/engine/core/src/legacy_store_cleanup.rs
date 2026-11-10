//! One-shot cleanup of the per-execution bookmarks earlier engines left in the
//! shared jj stores.
//!
//! The upgrade migration lists every store the engine has dispatched into as
//! pending (`legacy_store_cleanups`). This module cleans them one at a time in
//! the background after boot, so an unreachable host or a slow store never
//! delays startup or dispatch:
//!
//! 1. [`boss_engine_recovery::execution_bookmark::cleanup_legacy_store`] archives
//!    every `boss-base/*` and `boss-recovery/*` head that holds unpublished,
//!    non-empty work, then forgets every legacy name that holds none, in a
//!    handful of jj operations. A name that holds such work is never forgotten.
//! 2. The store is marked completed and never visited again.
//! 3. `cube repo gc` runs on the host, because forgetting bookmarks only
//!    shrinks the view of operations from now on: the old operations, each with
//!    its multi-megabyte view, are collected by cube.
//!
//! A failure leaves the store pending and is retried after [`RETRY_AFTER`]. The
//! cleanup is housekeeping: nothing here ever blocks or delays a dispatch.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use boss_engine_recovery::execution_bookmark::LegacyCleanupReport;

use crate::work::WorkDb;
use boss_execution_contracts::{HostAdapter, HostAdapterProvider};

/// How soon a store whose cleanup failed is tried again.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// Delay after boot before the first pass, so the cleanup does not compete with
/// the engine resuming the workers it found running.
const STARTUP_DELAY: Duration = Duration::from_secs(60);
/// How often the loop looks for stores whose retry is due.
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// What a pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub cleaned: usize,
    pub failed: usize,
    pub archived: usize,
    pub forgotten: usize,
    pub kept: usize,
}

/// The host operations a store cleanup needs.
#[async_trait]
pub(crate) trait StoreCleaner: Send + Sync {
    /// Archive and forget the legacy bookmarks of the store at `repo` on `host_id`.
    async fn cleanup(&self, host_id: &str, repo: &Path, published_heads: &[String]) -> Result<LegacyCleanupReport>;
    /// Run `cube repo gc <repo_id>` on `host_id`.
    async fn collect(&self, host_id: &str, repo_id: &str) -> Result<()>;
}

/// Reaches each store through the host adapter of the host that holds it:
/// the same local and SSH transports dispatch uses.
pub(crate) struct HostCleaner {
    db: Arc<WorkDb>,
    hosts: Arc<dyn HostAdapterProvider>,
}

impl HostCleaner {
    pub(crate) fn new(db: Arc<WorkDb>, hosts: Arc<dyn HostAdapterProvider>) -> Self {
        Self { db, hosts }
    }

    async fn adapter(&self, host_id: &str) -> Result<Arc<dyn HostAdapter>> {
        let host = self
            .db
            .get_host(host_id)?
            .ok_or_else(|| anyhow!("host {host_id} is not registered"))?;
        self.hosts.adapter_for(&host).await
    }
}

#[async_trait]
impl StoreCleaner for HostCleaner {
    async fn cleanup(&self, host_id: &str, repo: &Path, published_heads: &[String]) -> Result<LegacyCleanupReport> {
        self.adapter(host_id)
            .await?
            .cleanup_legacy_store(repo, published_heads)
            .await
    }

    async fn collect(&self, host_id: &str, repo_id: &str) -> Result<()> {
        self.adapter(host_id).await?.gc_cube_repo(repo_id).await
    }
}

/// One pass over the stores still pending whose retry is due. A failure on one
/// store is recorded and never stops the next.
pub(crate) async fn run_one_pass(db: &WorkDb, cleaner: &dyn StoreCleaner) -> Result<PassReport> {
    let pending = db.pending_legacy_store_cleanups(RETRY_AFTER.as_secs() as i64)?;
    let mut report = PassReport::default();
    if pending.is_empty() {
        return Ok(report);
    }
    let published_heads = db.pushed_pr_heads()?;
    for store in pending {
        db.begin_legacy_store_cleanup(&store.host_id, &store.repo_path)?;
        match clean_one(db, cleaner, &store, &published_heads).await {
            Ok(cleaned) => {
                report.cleaned += 1;
                report.archived += cleaned.archived.len();
                report.forgotten += cleaned.forgotten;
                report.kept += cleaned.kept.len();
            }
            Err(err) => {
                report.failed += 1;
                let error = format!("{err:#}");
                tracing::warn!(
                    host = %store.host_id,
                    repo = %store.repo_path.display(),
                    %error,
                    "legacy store cleanup failed; the store stays as it is and is retried later"
                );
                db.fail_legacy_store_cleanup(&store.host_id, &store.repo_path, &error)?;
            }
        }
    }
    Ok(report)
}

async fn clean_one(
    db: &WorkDb,
    cleaner: &dyn StoreCleaner,
    store: &crate::work::LegacyStoreCleanup,
    published_heads: &[String],
) -> Result<LegacyCleanupReport> {
    // A repository that no longer exists on this machine holds nothing to clean.
    let report = if store.host_id == "local" && !store.repo_path.exists() {
        LegacyCleanupReport::default()
    } else {
        cleaner
            .cleanup(&store.host_id, &store.repo_path, published_heads)
            .await?
    };
    db.finish_legacy_store_cleanup(&store.host_id, &store.repo_path, &report)?;
    for archive in &report.archived {
        tracing::info!(
            execution_id = %archive.execution_id,
            source = %archive.source,
            archive = %archive.archive,
            "legacy recovery work archived"
        );
    }
    if report.archived.is_empty() {
        tracing::info!(
            host = %store.host_id,
            repo = %store.repo_path.display(),
            found = report.found,
            forgotten = report.forgotten,
            kept = report.kept.len(),
            operations = report.operations,
            "legacy store cleaned"
        );
    } else {
        tracing::warn!(
            host = %store.host_id,
            repo = %store.repo_path.display(),
            found = report.found,
            archived = report.archived.len(),
            forgotten = report.forgotten,
            kept = report.kept.len(),
            operations = report.operations,
            "legacy store cleaned; heads that held unpublished work were archived under preserved-archive/"
        );
    }

    // Forgetting bookmarks only shrinks the view of later operations; cube
    // collects the old operations. Its failure never undoes the cleanup:
    // cube's own daily pass collects the store as well.
    let gc_error = if report.forgotten == 0 {
        None
    } else if let Some(repo_id) = &store.cube_repo_id {
        cleaner.collect(&store.host_id, repo_id).await.err().map(|err| {
            let error = format!("{err:#}");
            tracing::warn!(host = %store.host_id, repo_id, %error, "cube repo gc after the legacy store cleanup failed");
            error
        })
    } else {
        Some("the cube repo id was never recorded for this store; cube's daily collection covers it".to_owned())
    };
    db.record_legacy_store_gc(&store.host_id, &store.repo_path, gc_error.as_deref())?;
    Ok(report)
}

/// Run passes in the background until every store is cleaned. Returns once none
/// is pending, so a finished migration costs nothing for the rest of the
/// engine's life.
pub fn spawn_loop(db: Arc<WorkDb>, hosts: Arc<dyn HostAdapterProvider>) -> tokio::task::JoinHandle<()> {
    let cleaner = HostCleaner::new(Arc::clone(&db), hosts);
    crate::work::db_diagnostics::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        run_until_clean(&db, &cleaner).await;
    })
}

async fn run_until_clean(db: &WorkDb, cleaner: &dyn StoreCleaner) {
    loop {
        match run_one_pass(db, cleaner).await {
            Ok(report) if report != PassReport::default() => tracing::info!(?report, "legacy store cleanup pass"),
            Ok(_) => {}
            Err(err) => tracing::error!(error = %format!("{err:#}"), "legacy store cleanup pass failed"),
        }
        match db.pending_legacy_store_cleanups(0) {
            Ok(pending) if pending.is_empty() => return,
            Ok(_) => {}
            Err(err) => tracing::error!(error = %format!("{err:#}"), "could not list the stores still to clean"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
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
            vec![format!("preserved-archive/exec_abandoned/{abandoned}").as_str()]
        );
    }
}
