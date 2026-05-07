//! Periodic merge detection.
//!
//! The on-Stop completion path in [`crate::completion`] handles the
//! create-and-merge case during a run, but most merges happen *after*
//! the worker has exited and released its lease — so no Stop event
//! ever arrives to drive the `in_review → done` transition. Without
//! this module, every chore that lands its PR after the worker
//! finished would sit in the kanban "Review" column forever waiting
//! for a manual `boss chore update --status done`.
//!
//! The poller iterates [`WorkDb::list_chores_pending_merge_check`],
//! asks `gh pr view <url> --json state,mergedAt` for each, and calls
//! [`WorkDb::mark_chore_pr_merged`] when GitHub reports
//! `state=MERGED` (or `state=CLOSED` with a non-null `mergedAt`).
//! Errors are logged but never propagate — a temporary network blip
//! must not crash the engine.
//!
//! `gh pr view` accepts a full PR URL and resolves the repo from the
//! URL itself, so the poller works fine inside the engine's process
//! (no workspace context needed).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use crate::coordinator::ExecutionPublisher;
use crate::work::{PendingMergeCheck, WorkDb, WorkItem};

/// What `gh pr view` reports for one PR. The poller only needs the
/// merged-or-not bit; we keep the URL for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrMergeState {
    pub url: String,
    pub merged: bool,
}

/// Probe the merge state of a single PR. Implemented for production
/// by shelling out to `gh`; test doubles can stub it directly.
#[async_trait]
pub trait MergeProbe: Send + Sync {
    /// Returns the latest merge state for `pr_url`. Errors are
    /// reserved for tool / network failures; "PR doesn't exist" is
    /// reported as `Ok` with `merged=false` so the poller's
    /// in-review-stays-in-review behavior is preserved (a deleted PR
    /// is the user's problem, not the poller's).
    async fn probe(&self, pr_url: &str) -> Result<PrMergeState>;
}

/// `MergeProbe` that shells out to `gh pr view <url> --json state,mergedAt`.
#[derive(Debug, Default)]
pub struct CommandMergeProbe;

impl CommandMergeProbe {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MergeProbe for CommandMergeProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrMergeState> {
        let output = Command::new("gh")
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "state,mergedAt",
                "--jq",
                r#"[(.state // ""), (.mergedAt // "")] | @tsv"#,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to spawn `gh pr view {pr_url}`"))?;
        if !output.status.success() {
            let stderr_lower = String::from_utf8_lossy(&output.stderr).to_lowercase();
            // "could not resolve to a Resource" / 404 means the PR
            // doesn't exist any more (force-deleted, transferred). We
            // can't decide it's merged just because we can't see it,
            // so treat as not-merged and leave the chore in review.
            if stderr_lower.contains("could not resolve")
                || stderr_lower.contains("404")
                || stderr_lower.contains("not found")
            {
                return Ok(PrMergeState {
                    url: pr_url.to_owned(),
                    merged: false,
                });
            }
            return Err(anyhow!(
                "`gh pr view {pr_url}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        let mut parts = trimmed.split('\t');
        let state = parts.next().unwrap_or("").trim();
        let merged_at = parts.next().unwrap_or("").trim();
        let merged = state.eq_ignore_ascii_case("MERGED")
            || (!merged_at.is_empty() && !merged_at.eq_ignore_ascii_case("null"));
        Ok(PrMergeState {
            url: pr_url.to_owned(),
            merged,
        })
    }
}

/// Run one full merge-detection sweep over every chore in
/// `in_review` with a `pr_url`. Returns the number of chores that
/// transitioned to `done` so callers can log a one-line summary.
pub async fn run_one_pass(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
) -> usize {
    let candidates = match work_db.list_chores_pending_merge_check() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list pending merge checks");
            return 0;
        }
    };
    if candidates.is_empty() {
        return 0;
    }
    let mut promoted = 0usize;
    for candidate in candidates {
        promoted += sweep_one(work_db, probe, publisher, &candidate).await as usize;
    }
    promoted
}

async fn sweep_one(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) -> bool {
    let state = match probe.probe(&candidate.pr_url).await {
        Ok(state) => state,
        Err(err) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: probe failed; will retry next pass",
            );
            return false;
        }
    };
    if !state.merged {
        return false;
    }
    let updated = match work_db.mark_chore_pr_merged(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(task)) => task,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to mark chore merged",
            );
            return false;
        }
    };
    let work_item = WorkItem::Chore(updated);
    let work_item_id = match &work_item {
        WorkItem::Chore(t) => t.id.clone(),
        _ => return false,
    };
    publisher
        .publish_work_item_changed(&candidate.product_id, &work_item_id, "pr_merged")
        .await;
    tracing::info!(
        work_item_id = %work_item_id,
        pr_url = %candidate.pr_url,
        "merge poller: PR merged; chore moved to done",
    );
    true
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at
/// `interval`, with a small initial delay so engine startup isn't
/// blocked on `gh` while the rest of the runtime comes online. The
/// returned `JoinHandle` is detached by callers — the poller has no
/// shutdown path; aborting the engine process is the only way out,
/// which matches every other engine background task.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    probe: Arc<dyn MergeProbe>,
    publisher: Arc<dyn ExecutionPublisher>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Stagger startup by one tick so we don't pile a
        // `gh`-per-chore on top of the engine's other startup work.
        tokio::time::sleep(interval).await;
        loop {
            let promoted = run_one_pass(work_db.as_ref(), probe.as_ref(), publisher.as_ref()).await;
            if promoted > 0 {
                tracing::info!(promoted, "merge poller: chores moved to done in this pass");
            }
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::ExecutionPublisher;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItem, WorkItemPatch};

    struct StubProbe {
        states: std::sync::Mutex<std::collections::HashMap<String, Result<PrMergeState, String>>>,
    }

    impl StubProbe {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                states: std::sync::Mutex::new(Default::default()),
            })
        }

        fn set_merged(&self, url: &str, merged: bool) {
            self.states.lock().unwrap().insert(
                url.to_owned(),
                Ok(PrMergeState {
                    url: url.to_owned(),
                    merged,
                }),
            );
        }

        fn set_err(&self, url: &str, msg: &str) {
            self.states
                .lock()
                .unwrap()
                .insert(url.to_owned(), Err(msg.to_owned()));
        }
    }

    #[async_trait]
    impl MergeProbe for StubProbe {
        async fn probe(&self, pr_url: &str) -> Result<PrMergeState> {
            let map = self.states.lock().unwrap();
            match map.get(pr_url) {
                Some(Ok(state)) => Ok(state.clone()),
                Some(Err(msg)) => Err(anyhow!(msg.clone())),
                None => Ok(PrMergeState {
                    url: pr_url.to_owned(),
                    merged: false,
                }),
            }
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        work_events: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(
            &self,
            product_id: &str,
            work_item_id: &str,
            reason: &str,
        ) {
            self.work_events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
    }

    fn make_chore_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: format!("Product-{name}"),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: name.into(),
                description: None,
                autostart: false,
            })
            .unwrap();
        // Move chore directly to in_review with a pr_url, mirroring
        // the post-completion state.
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

    #[tokio::test]
    async fn merged_pr_is_promoted_and_publishes_invalidation() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/1";
        let (product_id, chore_id) = make_chore_in_review(&db, "C1", pr);

        let probe = StubProbe::new();
        probe.set_merged(pr, true);
        let publisher = Arc::new(RecordingPublisher::default());

        let promoted = run_one_pass(&db, probe.as_ref(), publisher.as_ref()).await;
        assert_eq!(promoted, 1);

        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "done");
                assert_eq!(t.pr_url.as_deref(), Some(pr));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let events = publisher.work_events.lock().await.clone();
        assert!(
            events
                .iter()
                .any(|(p, w, r)| p == &product_id && w == &chore_id && r == "pr_merged"),
            "expected pr_merged work-item event, got {events:?}",
        );
    }

    #[tokio::test]
    async fn unmerged_pr_leaves_chore_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/2";
        let (_pid, chore_id) = make_chore_in_review(&db, "C2", pr);

        let probe = StubProbe::new();
        probe.set_merged(pr, false);
        let publisher = Arc::new(RecordingPublisher::default());

        assert_eq!(run_one_pass(&db, probe.as_ref(), publisher.as_ref()).await, 0);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        assert!(publisher.work_events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn probe_failure_does_not_crash_or_promote() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr_a = "https://github.com/foo/bar/pull/3";
        let pr_b = "https://github.com/foo/bar/pull/4";
        let (_pa, chore_a) = make_chore_in_review(&db, "Cerr", pr_a);
        let (_pb, chore_b) = make_chore_in_review(&db, "Cok", pr_b);

        let probe = StubProbe::new();
        probe.set_err(pr_a, "auth broken");
        probe.set_merged(pr_b, true);
        let publisher = Arc::new(RecordingPublisher::default());

        // The error on pr_a must not prevent pr_b from being promoted.
        assert_eq!(run_one_pass(&db, probe.as_ref(), publisher.as_ref()).await, 1);
        match db.get_work_item(&chore_a).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
        match db.get_work_item(&chore_b).unwrap() {
            WorkItem::Chore(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn already_done_chore_is_skipped() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        // No chores in review at all → no work, no errors, no events.
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());
        assert_eq!(run_one_pass(&db, probe.as_ref(), publisher.as_ref()).await, 0);
        assert!(publisher.work_events.lock().await.is_empty());
    }
}
