//! Fixtures shared by every `conflict_watch` test module.
//!
//! The `pub(super) use` re-exports below let each sibling test module
//! pull the whole test vocabulary in with a single `use super::helpers::*;`.

pub(super) use std::sync::Arc;

pub(super) use async_trait::async_trait;
pub(super) use tempfile::tempdir;
pub(super) use tokio::sync::Mutex;

pub(super) use super::super::*;
pub(super) use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
pub(super) use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
pub(super) use crate::test_support::*;
pub(super) use crate::work::{CreateExecutionInput, ExecutionKind, ExecutionStatus, WorkDb, WorkItem, WorkItemPatch};

pub(super) fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (product.id, chore.id)
}

pub(super) fn chore_status(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => (t.status, t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Read a task row back through the public query path — unlike
/// `WorkDb::get_work_item`, this does not filter out soft-deleted rows, so
/// it can see an archived (tombstoned) revision after
/// `close_resolved_conflict_revision` runs.
pub(super) fn task(db: &WorkDb, id: &str) -> crate::work::Task {
    crate::work::query_task(&db.connect().unwrap(), id).unwrap().unwrap()
}

pub(super) fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id: work_item_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: pr_url.to_owned(),
    }
}

pub(super) fn probe(pr_url: &str, state: PrLifecycleState) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(state)
        .base_ref_oid("abc123")
        .head_ref_oid("head456")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

/// A `PrStateChecker` that reports every PR as `Open`, so the
/// `create_revision` create-time gate passes for the in-review chore
/// fixtures these tests build. A conflicting PR is, by definition,
/// still open — matching what `GhPrStateChecker` returns in production.
pub(super) fn open_checker() -> crate::work::FakePrStateChecker {
    crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
}

pub(super) fn probe_with_labels(pr_url: &str, state: PrLifecycleState, labels: &[&str]) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(state)
        .base_ref_oid("abc123")
        .head_ref_oid("head456")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(labels.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

/// `CubeClient` that records every `release_workspace` call so the
/// retire-path tests can assert the lease release fired without
/// standing up a real cube process.
#[derive(Default)]
pub(super) struct RecordingCubeClient {
    pub(super) releases: Mutex<Vec<String>>,
    pub(super) release_should_fail: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl crate::coordinator::CubeClient for RecordingCubeClient {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<crate::coordinator::CubeRepoHandle> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> anyhow::Result<crate::coordinator::CubeWorkspaceLease> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn create_change(
        &self,
        _: &std::path::Path,
        _: &str,
    ) -> anyhow::Result<crate::coordinator::CubeChangeHandle> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> anyhow::Result<()> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn release_workspace(&self, lease_id: &str) -> anyhow::Result<()> {
        self.releases.lock().await.push(lease_id.to_owned());
        if self.release_should_fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err(anyhow::anyhow!("simulated lease release failure"))
        } else {
            Ok(())
        }
    }
    async fn workspace_status(&self, _: &std::path::Path) -> anyhow::Result<crate::coordinator::CubeWorkspaceStatus> {
        unreachable!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }
    async fn list_repos(&self) -> anyhow::Result<Vec<crate::coordinator::CubeRepoSummary>> {
        Ok(Vec::new())
    }
}

/// Insert a `conflict_resolutions` row in `running` for the given
/// work item and stamp the parent's `blocked_attempt_id`. Mirrors
/// what Phase 3's worker-spawn path will do at runtime; lets the
/// retire-path tests run without standing up the worker pipeline.
pub(super) fn install_running_attempt(
    db: &WorkDb,
    product_id: &str,
    work_item_id: &str,
    pr_url: &str,
    lease_id: &str,
) -> String {
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: work_item_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number: 99,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha".into()),
            head_sha_before: Some("head-sha".into()),
        })
        .unwrap()
        .expect("attempt insert returns Some when no row exists yet");
    db.mark_conflict_resolution_running(&attempt.id, lease_id, "ws-1", "worker-1")
        .unwrap()
        .expect("mark_running must flip the freshly-inserted row");
    attempt.id
}

/// Flip `products.auto_pr_maintenance_enabled` directly on the
/// SQLite file so opt-out tests can drive the gate without
/// exposing a setter that production code doesn't yet need.
pub(super) fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
        rusqlite::params![product_id, if enabled { 1 } else { 0 }],
    )
    .unwrap();
}

/// Re-open the SQLite file and back-date a `conflict_resolutions`
/// row's `created_at` so churn-guard tests can simulate "this
/// attempt is 30 minutes old without sleeping the test for 30
/// minutes." Pure plumbing — production code never touches
/// `created_at` after insert.
pub(super) fn rewind_attempt_created_at(db_path: &std::path::Path, attempt_id: &str, secs_ago: i64) {
    let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let new_ts = (now_secs - secs_ago).to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
        rusqlite::params![attempt_id, new_ts],
    )
    .unwrap();
}

// Helper: build a probe with an explicit head SHA (all other fields match
// the default `probe()` helper).
pub(super) fn probe_with_head(pr_url: &str, state: PrLifecycleState, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe {
        head_ref_oid: Some(head_sha.to_owned()),
        ..probe(pr_url, state)
    }
}

// Helper: build a probe with an explicit head SHA and base SHA.
pub(super) fn probe_with_head_and_base(
    pr_url: &str,
    state: PrLifecycleState,
    head_sha: &str,
    base_sha: &str,
) -> PrLifecycleProbe {
    PrLifecycleProbe {
        head_ref_oid: Some(head_sha.to_owned()),
        base_ref_oid: Some(base_sha.to_owned()),
        ..probe(pr_url, state)
    }
}
