//! Detection-trigger pipeline for merge-conflict handling on
//! `in_review` PRs (`tools/boss/docs/designs/merge-conflict-handling-in-review.md`).
//!
//! Two entry points, both invoked from `merge_poller::sweep_one`:
//!
//!   - [`on_conflict_detected`] — fired when the probe reports a PR
//!     in [`OpenPrMergeability::Conflict`]. Flips the parent
//!     `tasks` row from `in_review` to `blocked: merge_conflict`
//!     unless the auto-rebase flow already owns the slot (design
//!     Q7) or the WHERE-guard misses (human moved the row).
//!
//!   - [`on_resolved`] — fired when the probe reports a previously
//!     conflicting PR back in [`OpenPrMergeability::Clean`]. Flips
//!     the parent back to `in_review`. The WHERE guard ensures we
//!     only undo engine-owned transitions; a human who manually
//!     reclassified the row stays in charge.
//!
//! Both transitions are idempotent: a second call for the same
//! `(work_item, pr_url)` finds the row already in the target state
//! and updates zero rows, so re-firing on every sweep is harmless.
//!
//! Worker spawn and the [`conflict_resolutions`] side table are
//! scoped to Phase 3 of the design (not this module). For now, the
//! parent's `blocked_attempt_id` stays `NULL` after
//! `on_conflict_detected` and is unaffected by `on_resolved`. When
//! Phase 3 lands it will tighten both transitions to also touch the
//! attempt row.
//!
//! [`OpenPrMergeability`]: crate::merge_poller::OpenPrMergeability
//! [`conflict_resolutions`]: https://example.invalid

use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::PrLifecycleProbe;
use crate::work::{PendingMergeCheck, WorkDb};

/// Fire-once flip from `in_review` to `blocked: merge_conflict`.
/// Returns `true` if the row actually transitioned (so the poller's
/// per-sweep counter can record it). All paths that *don't*
/// transition — WHERE-guard miss, auto-rebase owns the slot, DB
/// error — return `false` and log at the appropriate level.
pub async fn on_conflict_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    // Q7: when `auto-rebase-stacked-prs` is already chasing this PR,
    // step aside. Auto-rebase escalation owns the slot until it
    // hits a terminal status; the next conflict-watch sweep will
    // re-evaluate once that resolves.
    match work_db.has_active_rebase_attempt_for_pr(&candidate.pr_url) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: rebase attempt active; deferring conflict flip",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to check rebase attempt; deferring",
            );
            return false;
        }
    }
    let updated = match work_db
        .mark_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(task)) => task,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: WHERE guard missed; row already blocked or manually moved",
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to flip row to blocked: merge_conflict",
            );
            return false;
        }
    };
    publisher
        .publish_work_item_changed(
            &candidate.product_id,
            &updated.id,
            "blocked_merge_conflict",
        )
        .await;
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        base_ref_oid = ?probe.base_ref_oid,
        "conflict_watch: PR conflicts with base; work item flipped to blocked: merge_conflict",
    );
    true
}

/// Symmetric resolution path: flip a `blocked: merge_conflict` row
/// back to `in_review` when the probe says the PR is mergeable
/// again. Returns `true` on transition.
///
/// The function is invoked even on the `in_review` sweep slice (a
/// `Clean` probe for an already-`in_review` row is a no-op via the
/// WHERE guard), so wiring stays simple — every `Clean` result
/// passes through here.
pub async fn on_resolved(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) -> bool {
    let updated = match work_db
        .clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(task)) => task,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to clear blocked: merge_conflict",
            );
            return false;
        }
    };
    publisher
        .publish_work_item_changed(&candidate.product_id, &updated.id, "merge_conflict_resolved")
        .await;
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        "conflict_watch: PR mergeable again; work item returned to in_review",
    );
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::ExecutionPublisher;
    use crate::merge_poller::{OpenPrMergeability, PrLifecycleProbe, PrLifecycleState};
    use crate::work::{
        CreateChoreInput, CreateProductInput, WorkDb, WorkItem, WorkItemPatch,
    };

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String)>>,
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
            self.events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
    }

    fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
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
                priority: None,
                created_via: None,
                repo_remote_url: None,
            })
            .unwrap();
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

    fn chore_status(db: &WorkDb, id: &str) -> (String, Option<String>) {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => (t.status, t.blocked_reason),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: work_item_id.to_owned(),
            product_id: product_id.to_owned(),
            pr_url: pr_url.to_owned(),
        }
    }

    fn probe(pr_url: &str, state: PrLifecycleState) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr_url.to_owned(),
            state,
            base_ref_oid: Some("abc123".into()),
        }
    }

    #[tokio::test]
    async fn detection_flips_in_review_to_blocked_merge_conflict() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/10";
        let (product, chore) = make_in_review(&db, "C-detect", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(transitioned, "first detection must flip the row");

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            (product.clone(), chore.clone(), "blocked_merge_conflict".into())
        );
    }

    #[tokio::test]
    async fn detection_is_idempotent_on_repeated_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/11";
        let (product, chore) = make_in_review(&db, "C-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let first = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(first);
        assert!(!second, "second probe must be a no-op");
        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1, "no second event from idempotent probe");
    }

    #[tokio::test]
    async fn resolution_flips_blocked_back_to_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/12";
        let (product, chore) = make_in_review(&db, "C-resolve", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let resolved = on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await;
        assert!(resolved);

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].2, "merge_conflict_resolved");
    }

    #[tokio::test]
    async fn resolution_is_idempotent_on_repeated_clean_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/13";
        let (product, chore) = make_in_review(&db, "C-clean-noop", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // First call: row is in_review (not blocked), so resolution is
        // a no-op — the WHERE guard misses, no event published.
        let r1 = on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await;
        assert!(!r1);
        assert!(pub_.events.lock().await.is_empty());

        // Drive a full conflict-resolve cycle, then call resolution
        // twice — the second call must also be a no-op.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let r2 = on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await;
        let r3 = on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await;
        assert!(r2);
        assert!(!r3);
    }

    #[tokio::test]
    async fn cycle_flip_resolve_flip() {
        // Integration: conflict → resolve → conflict again — all
        // transitions valid, all events fired, terminal state correct.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/14";
        let (product, chore) = make_in_review(&db, "C-cycle", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
            )
            .await
        );
        assert!(on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await);
        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
            )
            .await
        );

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let reasons: Vec<String> = pub_
            .events
            .lock()
            .await
            .iter()
            .map(|(_, _, r)| r.clone())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "blocked_merge_conflict".to_owned(),
                "merge_conflict_resolved".to_owned(),
                "blocked_merge_conflict".to_owned(),
            ],
        );
    }

    #[tokio::test]
    async fn detection_skipped_when_human_moved_row_off_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/15";
        let (product, chore) = make_in_review(&db, "C-human", pr);
        // Human flipped the row to `active` after PR was opened.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("active".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let pub_ = Arc::new(RecordingPublisher::default());

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(!transitioned, "WHERE guard protects manual moves");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "active");
        assert!(reason.is_none());
        assert!(pub_.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn resolution_skipped_when_human_moved_row_off_blocked() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/16";
        let (product, chore) = make_in_review(&db, "C-human-2", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        // Human dropped the row from `blocked` back to `active` (e.g.
        // pulled the chore out of review themselves while the engine
        // was waiting).
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("active".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let before_count = pub_.events.lock().await.len();
        let r = on_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr)).await;
        assert!(!r);
        assert_eq!(pub_.events.lock().await.len(), before_count);
    }

    #[tokio::test]
    async fn detection_defers_when_rebase_attempt_is_active() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/17";
        let (product, chore) = make_in_review(&db, "C-rebase", pr);
        // Simulate auto-rebase having created its side table and a
        // running attempt for this PR. The table doesn't ship until
        // auto-rebase lands, so the conflict_watch must defer when it
        // does exist + has a non-terminal row.
        let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
        conn.execute(
            "CREATE TABLE rebase_attempts (
                 id                TEXT PRIMARY KEY,
                 dependent_pr_url  TEXT NOT NULL,
                 status            TEXT NOT NULL
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
              VALUES ('reb_1', ?1, 'running')",
            [pr],
        )
        .unwrap();
        drop(conn);

        let pub_ = Arc::new(RecordingPublisher::default());
        let r = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(!r, "rebase-active path must defer");
        let (status, _) = chore_status(&db, &chore);
        assert_eq!(status, "in_review", "row stays where it was");
        assert!(pub_.events.lock().await.is_empty());
    }
}
