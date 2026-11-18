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
//! 3. `cube repo gc` runs on the host whenever the pass wrote operations, because
//!    forgetting bookmarks or hiding heads only
//!    shrinks the view of operations from now on: the old operations, each with
//!    its multi-megabyte view, are collected by cube.
//!
//! A failure leaves the store pending and is retried after [`RETRY_AFTER`]. The
//! third failure files one inspection attention per store. Missing stores complete
//! after a probe on their owning host. Persistent failures keep the loop alive
//! and retrying until cleanup succeeds. Nothing here blocks or delays dispatch.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
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

#[derive(Debug)]
struct UnregisteredHost(String);

impl std::fmt::Display for UnregisteredHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "host {} is no longer registered; legacy store cleanup retired",
            self.0
        )
    }
}

impl std::error::Error for UnregisteredHost {}

impl HostCleaner {
    pub(crate) fn new(db: Arc<WorkDb>, hosts: Arc<dyn HostAdapterProvider>) -> Self {
        Self { db, hosts }
    }

    async fn adapter(&self, host_id: &str) -> Result<Arc<dyn HostAdapter>> {
        let host = self
            .db
            .get_host(host_id)?
            .ok_or_else(|| UnregisteredHost(host_id.to_owned()))?;
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
                if err.is::<UnregisteredHost>() {
                    db.retire_legacy_store_cleanup(&store.host_id, &store.repo_path, &err.to_string())?;
                    tracing::info!(host = %store.host_id, error = %err, "legacy store cleanup retired");
                    report.cleaned += 1;
                    continue;
                }
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
    let gc_error = if report.operations == 0 {
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
#[path = "legacy_store_cleanup_tests.rs"]
mod tests;
