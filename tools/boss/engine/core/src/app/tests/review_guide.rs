use super::*;
use crate::test_support::{create_active_chore, create_product, source_capture_packet};
use crate::work::{PrSourceCaptureTrigger, PublishReviewGuideOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};

const PR_URL: &str = "https://github.com/acme/widget/pull/91";

struct Fixture {
    state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    root: String,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    fn new(fail_capture: bool) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let packet = source_capture_packet(PR_URL, "base", "head");
        let output = packet.clone();
        let collect: crate::review_guide_capture::PacketCollectFn = Arc::new(move |url, observed, branch, _| {
            assert_eq!(url, PR_URL);
            assert!(observed.is_none());
            assert!(branch.is_none(), "manual capture must not require a live head branch");
            count.fetch_add(1, Ordering::SeqCst);
            let packet = output.clone();
            Box::pin(async move {
                anyhow::ensure!(!fail_capture, "source access refused");
                Ok(packet)
            })
        });
        let collector = crate::review_guide_capture::SourcePacketCollector::fixture(collect, packet);
        let (state, dir) = test_server_state_with_source_collector(collector);
        let db = &state.work_db;
        let product = create_product(db);
        let root = create_active_chore(db, &product, "Existing PR");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?1, repo_remote_url = 'https://github.com/acme/widget' WHERE id = ?2",
                rusqlite::params![PR_URL, root],
            )
            .unwrap();
        for flag in ["review_guide_source_capture", "review_guide_generation"] {
            assert!(!state.feature_flags.is_enabled(flag));
        }
        Self {
            state,
            _dir: dir,
            root,
            calls,
        }
    }

    fn capture_idle(&self) {
        let db = &self.state.work_db;
        db.persist_pr_review_guide_source_capture(
            &self.root,
            db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
            PrSourceCaptureTrigger::Creation,
            &source_capture_packet(PR_URL, "base", "head"),
        )
        .unwrap();
        assert_eq!(
            db.get_pr_review_guide_summary_for_root(&self.root)
                .unwrap()
                .unwrap()
                .lifecycle,
            "idle"
        );
    }

    async fn request(&self, task_id: &str, token: &str) -> FrontendEvent {
        let sink = make_session_sink();
        let ctx = Dispatch::builder()
            .server_state(self.state.clone())
            .work_db(self.state.work_db.clone())
            .sink(sink.clone())
            .session_id("session")
            .request_id("request")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build();
        crate::app::review_guide::handle_generate_review_guide(
            ctx,
            FrontendRequest::GenerateReviewGuide {
                root_task_id: task_id.to_owned(),
                idempotency_token: Some(token.to_owned()),
            },
        )
        .await;
        sink.close();
        let reply = sink.next().await.unwrap();
        assert_eq!(reply.request_id.as_deref(), Some("request"));
        assert!(sink.next().await.is_none());
        reply.payload
    }

    async fn generate(&self, token: &str, already: bool) -> boss_protocol::ReviewGuideAttempt {
        let reply = self.request(&self.root, token).await;
        let FrontendEvent::ReviewGuideRetryQueued {
            root_task_id,
            attempt,
            already_requested,
        } = reply
        else {
            panic!("expected queued reply: {reply:?}")
        };
        assert_eq!(root_task_id, self.root);
        assert_eq!(already_requested, already);
        attempt
    }
}

#[tokio::test]
async fn manual_generation_captures_and_dispatches_with_both_flags_off() {
    let f = Fixture::new(false);
    let attempt = f.generate("initial", false).await;
    let db = &f.state.work_db;
    let capture = db.get_latest_pr_review_guide_source_capture(&f.root).unwrap().unwrap();
    assert_eq!(capture.trigger, "manual");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(attempt.status, "running");
    assert_eq!(
        db.get_pr_review_guide_summary_for_root(&f.root)
            .unwrap()
            .unwrap()
            .lifecycle,
        "generating"
    );
    assert_eq!(f.generate("initial", true).await.id, attempt.id);
    assert_eq!(f.generate("second-click", true).await.id, attempt.id);
    db.fail_pr_review_guide_attempt(&attempt.id, "fixture failure").unwrap();
    assert_eq!(f.generate("second-click", true).await.id, attempt.id);
    assert_eq!(f.generate("initial", true).await.id, attempt.id);
}

#[tokio::test]
async fn existing_idle_comparison_is_generated_without_recapture() {
    let f = Fixture::new(false);
    f.capture_idle();
    f.generate("idle", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn existing_unselected_packet_is_selected_and_generated() {
    let f = Fixture::new(false);
    f.capture_idle();
    f.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_guide_source_series SET selected_comparison_id = NULL WHERE root_task_id = ?1",
            [&f.root],
        )
        .unwrap();
    f.generate("existing", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn regenerate_ready_guide_keeps_old_version_and_advances_epoch() {
    let f = Fixture::new(false);
    let first = f.generate("first", false).await;
    let db = &f.state.work_db;
    let PublishReviewGuideOutcome::Published(old) =
        db.publish_pr_review_guide_version(&first.id, "# Old", "old").unwrap()
    else {
        panic!("expected publication")
    };
    let next = f.generate("next", false).await;
    assert!(next.request_epoch > first.request_epoch);
    assert_ne!(next.id, first.id);
    assert_eq!(
        db.get_pr_review_guide_summary_for_root(&f.root)
            .unwrap()
            .unwrap()
            .readable_version_id
            .as_deref(),
        Some(old.id.as_str())
    );
    let PublishReviewGuideOutcome::Published(new) =
        db.publish_pr_review_guide_version(&next.id, "# New", "new").unwrap()
    else {
        panic!("expected publication")
    };
    assert_ne!(old.id, new.id);
    assert_eq!(db.get_pr_review_guide_version(&old.id).unwrap().unwrap(), *old);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn merged_and_closed_prs_can_generate_readable_guides() {
    for pr_state in ["merged", "closed"] {
        let f = Fixture::new(false);
        let db = &f.state.work_db;
        if pr_state == "merged" {
            db.mark_chore_pr_merged(&f.root, PR_URL).unwrap().unwrap();
        } else {
            db.mark_chore_pr_closed_unmerged(&f.root, PR_URL).unwrap().unwrap();
        }
        let attempt = f.generate("done", false).await;
        assert!(matches!(
            db.publish_pr_review_guide_version(&attempt.id, "# Context", "context")
                .unwrap(),
            PublishReviewGuideOutcome::Published(_)
        ));
        assert_eq!(
            db.get_pr_review_guide_summary_for_root(&f.root)
                .unwrap()
                .unwrap()
                .lifecycle,
            "ready"
        );
    }
}

#[tokio::test]
async fn manual_backfill_on_old_sources_survives_retention_during_and_after_generation() {
    let f = Fixture::new(false);
    f.capture_idle();
    let db = &f.state.work_db;
    db.mark_chore_pr_merged(&f.root, PR_URL).unwrap().unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
        .unwrap();
    let attempt = f.generate("backfill", false).await;
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(f.generate("backfill", true).await.id, attempt.id);
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Backfill", "raw")
        .unwrap()
    else {
        panic!("expected publication")
    };
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(db.get_pr_review_guide_version(&version.id).unwrap().unwrap(), *version);
    assert_eq!(
        db.get_pr_review_guide_summary_for_root(&f.root)
            .unwrap()
            .unwrap()
            .readable_version_id,
        Some(version.id)
    );
}

#[tokio::test]
async fn missing_pr_or_repository_is_an_explicit_error_before_capture() {
    for (column, expected) in [("pr_url", "no PR URL"), ("repo_remote_url", "no repository remote")] {
        for value in [None, Some(" ")] {
            let f = Fixture::new(false);
            f.state
                .work_db
                .connect()
                .unwrap()
                .execute(
                    &format!("UPDATE tasks SET {column} = ?1 WHERE id = ?2"),
                    rusqlite::params![value, f.root],
                )
                .unwrap();
            let FrontendEvent::WorkError { message } = f.request(&f.root, "missing").await else {
                panic!("expected error")
            };
            assert!(message.contains(expected), "{message}");
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn revision_request_resolves_chain_root_and_echoes_clicked_card() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    let product = create_product(db);
    let revision = create_active_chore(db, &product, "Revision");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?1 WHERE id = ?2",
            rusqlite::params![f.root, revision],
        )
        .unwrap();
    let FrontendEvent::ReviewGuideRetryQueued { root_task_id, .. } = f.request(&revision, "revision").await else {
        panic!("expected queued reply")
    };
    assert_eq!(root_task_id, revision);
    assert!(db.get_latest_pr_review_guide_source_capture(&f.root).unwrap().is_some());
    assert!(db.get_pr_review_guide_summary_for_root(&revision).unwrap().is_none());
}

#[tokio::test]
async fn capture_failure_is_reported_and_does_not_enqueue() {
    let f = Fixture::new(true);
    let FrontendEvent::WorkError { message } = f.request(&f.root, "failure").await else {
        panic!("expected error")
    };
    assert!(message.contains("source access refused"), "{message}");
    let summary = f
        .state
        .work_db
        .get_pr_review_guide_summary_for_root(&f.root)
        .unwrap()
        .unwrap();
    assert!(summary.selected_comparison_id.is_none());
    assert_eq!(summary.request_epoch, 0);
}
