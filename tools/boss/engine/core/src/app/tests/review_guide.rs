use super::*;
use crate::test_support::{create_active_chore, create_product, source_capture_packet};
use crate::work::{PrSourceCaptureTrigger, PublishReviewGuideOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};

const PR_URL: &str = "https://github.com/acme/widget/pull/91";
const REPORTED_EVIDENCE: &str = "`bazel test //widget:layout_test` passed (12 tests).";

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
        let mut packet = source_capture_packet(PR_URL, "base", "head");
        packet.body = Some(format!("## Validation\n\n{REPORTED_EVIDENCE}"));
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
        // This test drives the manual capture path, which never consults
        // the branch verifier; any verifier satisfies the builder.
        let (state, dir) =
            test_server_state_with_source_collector(collector, Arc::new(SettableHeadRefVerifier::default()));
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

    async fn request_summary(&self, series_id: Option<&str>) -> Option<boss_protocol::ReviewGuideSummary> {
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
        crate::app::review_guide::handle_get_review_guide_summary(
            ctx,
            FrontendRequest::GetReviewGuideSummary {
                root_task_id: self.root.clone(),
                series_id: series_id.map(str::to_owned),
            },
        )
        .await;
        sink.close();
        let reply = sink.next().await.unwrap();
        assert!(sink.next().await.is_none());
        let FrontendEvent::ReviewGuideSummary { summary } = reply.payload else {
            panic!("expected summary reply: {:?}", reply.payload)
        };
        summary
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

    async fn prompt(&self, attempt_id: &str) -> String {
        let db = &self.state.work_db;
        let attempt = db.dispatch_pr_review_guide_attempt(attempt_id, &self.root).unwrap();
        let execution = db.get_execution(attempt.execution_id.as_deref().unwrap()).unwrap();
        let item = db.get_work_item(&self.root).unwrap();
        crate::runner::compose_worker_spawn(
            db,
            "guide-fixture",
            &execution,
            &item,
            self._dir.path(),
            None,
            crate::runner::WorkerSpawnOpts::default(),
        )
        .await
        .unwrap()
        .prompt_text
    }

    fn screenshot(&self, task: &str, caption: &str) -> String {
        let db = &self.state.work_db;
        let execution = crate::test_support::create_ready_chore_execution(db, task);
        let image = boss_engine_attachments::IngestedImage {
            content_digest: format!("fixture-{}", execution.id),
            media_type: boss_protocol::AttachmentMediaType::Png,
            pixel_width: 800,
            pixel_height: 600,
            size_bytes: 256,
            source_name: "private-capture.png".to_owned(),
        };
        db.submit_work_attachment(crate::work::SubmitAttachmentInput {
            execution_id: &execution.id,
            work_item_id: task,
            image: &image,
            caption,
        })
        .unwrap()
        .unwrap()
        .attachment
        .id
    }

    fn publish_fixture_guide(&self, attempt: &boss_protocol::ReviewGuideAttempt, notice: &str) {
        // Deterministic driver response: tests the contract/publication seam,
        // not whether a live model follows the prose instructions.
        let markdown = format!(
            "# Layout review\n\n## Problem\nThe layout needs verification.\n\n\
             ## Core fix\nThe author reports a layout adjustment.\n\n\
             ## Walkthrough\nThis fixture supplies no source files.\n\n\
             ## Tests\nThe PR describes a layout test run.\n\n\
             ## Evidence\nAuthor-reported: {REPORTED_EVIDENCE}\n\n\
             Source verification is limited: this fixture has no source files; pinned source is needed to assess coverage against the intent.\n\n\
             {notice}\n"
        );
        let db = &self.state.work_db;
        let capture = db
            .get_pr_review_guide_comparison_by_id(&attempt.comparison_id)
            .unwrap()
            .unwrap();
        let guide = boss_review_guide::validate_guide_output_with_resolver(&markdown, &capture.packet, |_, _, _, _| {
            panic!("fixture has no source references")
        })
        .unwrap();
        let PublishReviewGuideOutcome::Published(version) = db
            .publish_pr_review_guide_version(&attempt.id, &guide.markdown, &markdown)
            .unwrap()
        else {
            panic!("expected fixture publication")
        };
        let stored = db.get_pr_review_guide_version(&version.id).unwrap().unwrap();
        assert_eq!(stored.markdown, markdown.trim());
        println!("Fixture-generated guide:\n{}", stored.markdown);
    }
}

#[tokio::test]
async fn guide_evidence_prompt_and_fixture_report_no_registered_screenshots() {
    let f = Fixture::new(false);
    let attempt = f.generate("no-screenshots", false).await;
    let prompt = f.prompt(&attempt.id).await;
    assert!(prompt.contains(&format!("### PR description\n\n## Validation\n\n{REPORTED_EVIDENCE}")));
    assert!(
        prompt.contains("### Registered screenshots\n\nNo screenshots are registered for this task or its revisions.")
    );
    assert!(prompt.contains("section headed exactly `## Evidence`"));
    f.publish_fixture_guide(
        &attempt,
        "No screenshots are registered for this task or its revisions.",
    );
}

#[tokio::test]
async fn guide_evidence_prompt_includes_root_and_revision_screenshots_without_urls() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    let product = create_product(db);
    let revision = create_active_chore(db, &product, "Layout revision");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?1, status = 'in_review' WHERE id = ?2",
            rusqlite::params![f.root, revision],
        )
        .unwrap();
    let root_image = f.screenshot(&f.root, "Wide layout before the fix");
    let revision_image = f.screenshot(&revision, "Wide layout after the fix");
    assert_eq!(
        db.mark_attachments_reclaimed(std::slice::from_ref(&root_image))
            .unwrap(),
        1
    );
    let unrelated = create_active_chore(db, &product, "Another change");
    let unrelated_image = f.screenshot(&unrelated, "Unrelated evidence");
    let attempt = f.generate("screenshots", false).await;
    let prompt = f.prompt(&attempt.id).await;
    let manifest_text = prompt.split("### Registered screenshots").last().unwrap();
    let manifest_start = manifest_text.find('[').unwrap();
    let manifest = serde_json::Deserializer::from_str(&manifest_text[manifest_start..])
        .into_iter::<serde_json::Value>()
        .next()
        .unwrap()
        .unwrap();
    let rows = manifest.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (id, caption, available) in [
        (&root_image, "Wide layout before the fix", false),
        (&revision_image, "Wide layout after the fix", true),
    ] {
        let row = rows.iter().find(|row| row["id"] == *id).unwrap();
        assert_eq!(row["caption"], caption);
        assert_eq!(row["pixel_width"], 800);
        assert_eq!(row["pixel_height"], 600);
        assert_eq!(row["image_available"], available);
        assert_eq!(row.as_object().unwrap().len(), 5);
    }
    for absent in [
        &unrelated_image,
        "Unrelated evidence",
        "private-capture.png",
        "localhost",
        "127.0.0.1",
    ] {
        assert!(!manifest.to_string().contains(absent));
    }
    f.publish_fixture_guide(&attempt, "Screenshots are attached to the task: Wide layout before the fix; Wide layout after the fix (800 × 600 each, described by author captions; images not inspected).");

    // An incremental update must refresh metadata rather than relying on the
    // screenshot notice embedded in the previous guide.
    let new_image = f.screenshot(&revision, "Narrow layout after revision");
    let sequence = db.allocate_pr_review_guide_source_observation_sequence().unwrap();
    let packet = source_capture_packet(PR_URL, "base", "revised-head");
    db.persist_pr_review_guide_source_capture(&f.root, sequence, PrSourceCaptureTrigger::Completion, &packet)
        .unwrap();
    let capture = db.get_latest_pr_review_guide_source_capture(&f.root).unwrap().unwrap();
    db.record_review_guide_revision_head(&f.root, PR_URL, "revised-head", &revision, sequence)
        .unwrap();
    let update = db
        .create_automatic_pr_review_guide_attempt(&capture.series_id, &capture.comparison_id)
        .unwrap();
    let update = db.dispatch_pr_review_guide_attempt(&update.id, &f.root).unwrap();
    assert_eq!(update.prompt_version, boss_review_guide::UPDATE_PROMPT_VERSION);
    let prompt = f.prompt(&update.id).await;
    assert!(prompt.contains("including a refreshed `## Evidence` section"));
    assert!(prompt.contains(&new_image));
    assert!(prompt.contains("Narrow layout after revision"));
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
async fn manual_generation_still_dispatches_for_design_root() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET kind = 'design' WHERE id = ?1", [f.root.as_str()])
        .unwrap();
    let attempt = f.generate("design", false).await;
    assert_eq!(attempt.status, "running");
    let capture = db.get_latest_pr_review_guide_source_capture(&f.root).unwrap().unwrap();
    assert_eq!(capture.trigger, "manual");
    let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, attempt.id);
    assert!(live[0].execution_id.is_some());
}

#[tokio::test]
async fn manual_generation_uses_product_repository_without_task_override() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE products SET repo_remote_url = 'https://github.com/acme/widget' WHERE id = (SELECT product_id FROM tasks WHERE id = ?1)",
            [&f.root],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET repo_remote_url = NULL WHERE id = ?1", [&f.root])
            .unwrap();
    }

    let attempt = f.generate("product-repo", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(attempt.status, "running");
    let capture = db.get_latest_pr_review_guide_source_capture(&f.root).unwrap().unwrap();
    assert_eq!(capture.trigger, "manual");
}

#[tokio::test]
async fn existing_idle_comparison_is_refreshed_and_generated() {
    let f = Fixture::new(false);
    f.capture_idle();
    f.generate("idle", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
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
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
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
                    "UPDATE products SET repo_remote_url = NULL WHERE id = (SELECT product_id FROM tasks WHERE id = ?1)",
                    [&f.root],
                )
                .unwrap();
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

#[tokio::test]
async fn replaced_pr_captures_and_admits_the_current_pr_series() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    let retired_url = "https://github.com/acme/widget/pull/90";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![retired_url, f.root],
        )
        .unwrap();
    db.persist_pr_review_guide_source_capture(
        &f.root,
        db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
        PrSourceCaptureTrigger::Creation,
        &source_capture_packet(retired_url, "base", "retired"),
    )
    .unwrap();
    let retired = db.get_pr_review_guide_summary_for_root(&f.root).unwrap().unwrap();
    assert!(retired.selected_comparison_id.is_some());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![PR_URL, f.root],
        )
        .unwrap();
    let attempt = f.generate("replacement", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    let current = db.get_pr_review_guide_summary_for_root(&f.root).unwrap().unwrap();
    assert_eq!(current.canonical_pr_url, PR_URL);
    assert_eq!(attempt.series_id, current.series_id);
    assert_ne!(attempt.series_id, retired.series_id);
    assert_eq!(Some(attempt.comparison_id), current.selected_comparison_id);
}

#[tokio::test]
async fn get_summary_scoped_to_series_survives_a_pr_replacement() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    let retired_url = "https://github.com/acme/widget/pull/90";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![retired_url, f.root],
        )
        .unwrap();
    db.persist_pr_review_guide_source_capture(
        &f.root,
        db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
        PrSourceCaptureTrigger::Creation,
        &source_capture_packet(retired_url, "base", "retired"),
    )
    .unwrap();
    let retired = db.get_pr_review_guide_summary_for_root(&f.root).unwrap().unwrap();

    // A viewer opens the retired PR's guide, then the root gets a
    // replacement PR while that guide stays on screen.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![PR_URL, f.root],
        )
        .unwrap();
    f.generate("replacement", false).await;
    let current = db.get_pr_review_guide_summary_for_root(&f.root).unwrap().unwrap();
    assert_ne!(current.series_id, retired.series_id);

    // Unscoped (root-only) lookup now returns the *new* series — this is the
    // behaviour a poll keyed only on root_task_id would silently pick up.
    let unscoped = f.request_summary(None).await.unwrap();
    assert_eq!(unscoped.series_id, current.series_id);

    // Scoped to the retired series id, the RPC must keep returning that PR's
    // own summary rather than switching to the replacement's.
    let scoped = f.request_summary(Some(&retired.series_id)).await.unwrap();
    assert_eq!(scoped.series_id, retired.series_id);
    assert_eq!(scoped.canonical_pr_url, retired_url);

    // A series id from a different root's guide must never leak this root's
    // findings back to it.
    assert!(f.request_summary(Some("not-a-real-series")).await.is_none());
}

#[tokio::test]
async fn regeneration_refreshes_a_changed_head() {
    let f = Fixture::new(false);
    let db = &f.state.work_db;
    db.persist_pr_review_guide_source_capture(
        &f.root,
        db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
        PrSourceCaptureTrigger::Creation,
        &source_capture_packet(PR_URL, "base", "old-head"),
    )
    .unwrap();
    let old = db.get_pr_review_guide_summary_for_root(&f.root).unwrap().unwrap();
    let attempt = f.generate("fresh-head", false).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_ne!(Some(attempt.comparison_id), old.selected_comparison_id);
    assert_eq!(attempt.series_id, old.series_id);
}
