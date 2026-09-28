use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn divergent_probe(existing: bool) {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "divergent probe");
    let url = format!("https://github.com/acme/widget/pull/{}", if existing { 81 } else { 82 });
    let mut packet = crate::test_support::source_capture_packet(&url, "rest-base", "head");
    if existing {
        db.persist_pr_review_guide_source_capture(&root, 0, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap();
    }
    packet.probe_base_sha = Some("probe-base".to_owned());
    let calls = Arc::new(AtomicUsize::new(0));
    let metadata_calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let output = packet.clone();
    let collect: PacketCollectFn = Arc::new(move |_, observed, _, metadata| {
        count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(observed.unwrap().base_sha, "probe-base");
        assert_eq!(metadata.unwrap().base_sha, "rest-base");
        let packet = output.clone();
        Box::pin(async move { Ok(packet) })
    });
    let mut collector = SourcePacketCollector::fixture_with_metadata_base(collect, packet, "rest-base".to_owned());
    let metadata = collector.metadata.clone();
    let count = metadata_calls.clone();
    collector.metadata = Arc::new(move |url, branch| {
        count.fetch_add(1, Ordering::SeqCst);
        metadata(url, branch)
    });
    let flags = Arc::new(FeatureFlagsStore::new(_dir.path().join("flags.toml")));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    let request = || {
        SourceCaptureRequest::builder()
            .root_task_id(root.clone())
            .pr_url(url.clone())
            .trigger(PrSourceCaptureTrigger::Poller)
            .observed(PinnedComparison {
                base_sha: "probe-base".to_owned(),
                head_sha: "head".to_owned(),
            })
            .build()
    };
    reconcile_review_guide_source_with_collector(db.clone(), flags.clone(), request(), collector.clone())
        .expect("new probe must resolve REST identity")
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(!existing));
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
    let stored = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert_eq!(stored.packet.observed_base_sha, "rest-base");
    if !existing {
        assert_eq!(stored.packet.probe_base_sha.as_deref(), Some("probe-base"));
    }
    assert_eq!(
        db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_review_guide_source_comparisons", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(reconcile_review_guide_source_with_collector(db, flags, request(), collector).is_none());
    assert_eq!(
        metadata_calls.load(Ordering::SeqCst),
        1,
        "repeat probe must not spawn metadata fetch"
    );
}

#[tokio::test]
async fn divergent_probe_reuses_complete_rest_comparison() {
    divergent_probe(true).await;
}

#[tokio::test]
async fn divergent_probe_collects_and_persists_rest_identity() {
    divergent_probe(false).await;
}
use crate::feature_flags::FeatureFlagsStore;
use crate::test_support::{create_active_chore, create_product, open_db, source_capture_packet};
use crate::work::{FakePrStateChecker, PrOpenState, PublishReviewGuideOutcome};
use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

#[test]
fn unpolled_future_releases_its_claim_and_coalesces_sequences() {
    let key = ("cancel-test".to_owned(), "base".to_owned(), "head".to_owned());
    let guard = InFlightGuard::acquire(key.clone(), 1).unwrap();
    assert!(InFlightGuard::acquire(key.clone(), 4).is_none());
    assert_eq!(*guard.sequence.lock().unwrap(), 4);
    let future = async move {
        let _guard = guard;
    };
    drop(future);
    assert!(InFlightGuard::acquire(key, 5).is_some());
}

#[test]
fn automatic_capture_defaults_off_and_respects_the_rollout_flag() {
    let directory = tempfile::tempdir().unwrap();
    let flags = FeatureFlagsStore::new(directory.path().join("feature-flags.toml"));
    flags.load().unwrap();
    assert!(!source_capture_enabled(&flags));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    assert!(source_capture_enabled(&flags));
}

#[test]
fn disabled_reconciler_does_not_allocate_or_spawn_collection() {
    let (_directory, work_db) = open_db();
    let work_db = Arc::new(work_db);
    let flag_directory = tempfile::tempdir().unwrap();
    let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
    reconcile_review_guide_source_with_collector(
        work_db.clone(),
        flags,
        SourceCaptureRequest::builder()
            .root_task_id("unreachable-root")
            .pr_url("https://github.com/acme/widget/pull/25")
            .trigger(PrSourceCaptureTrigger::Creation)
            .observed(PinnedComparison {
                base_sha: "base".to_owned(),
                head_sha: "head".to_owned(),
            })
            .build(),
        github_source_packet_collector(),
    );
    assert_eq!(
        work_db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
        1
    );
}

#[tokio::test]
async fn execution_reconciler_persists_a_revision_capture_on_its_canonical_root() {
    let (_directory, work_db) = open_db();
    let work_db = Arc::new(work_db);
    let product = create_product(&work_db);
    let root = create_active_chore(&work_db, &product, "root source capture");
    let pr_url = "https://github.com/acme/widget/pull/25";
    work_db
        .update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
    let revision = work_db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root.clone())
                .description("refresh the implementation")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let execution = work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let flag_directory = tempfile::tempdir().unwrap();
    let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    let packet = SourcePacket {
        schema_version: 3,
        canonical_pr_url: pr_url.to_owned(),
        pr_number: 25,
        title: "Captured revision".to_owned(),
        body: None,
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        observed_base_sha: "base".to_owned(),
        probe_base_sha: None,
        merge_base_sha: "merge-base".to_owned(),
        head_sha: "head".to_owned(),
        files: Vec::new(),
        omissions: Vec::new(),
    };
    let fixture_packet = packet.clone();
    let collect: PacketCollectFn = Arc::new(move |url, observed, expected_head_branch, _metadata| {
        let packet = packet.clone();
        Box::pin(async move {
            let Some(observed) = observed else {
                anyhow::bail!("execution reconciler supplied unexpected comparison identity");
            };
            if url != packet.canonical_pr_url || observed.base_sha != "base" || observed.head_sha != "head" {
                anyhow::bail!("execution reconciler supplied unexpected comparison identity");
            }
            if expected_head_branch.is_some() {
                anyhow::bail!("revision capture must not require an implementation branch name");
            }
            Ok(packet)
        })
    });
    let collector = SourcePacketCollector::fixture(collect, fixture_packet);
    let handle = reconcile_review_guide_source_for_execution_with_collector(
        work_db.clone(),
        flags,
        &execution.id,
        pr_url,
        PrSourceCaptureTrigger::Completion,
        Some(PinnedComparison {
            base_sha: "base".to_owned(),
            head_sha: "head".to_owned(),
        }),
        collector,
    )
    .expect("enabled execution reconciliation must start collection");
    handle.await.unwrap();

    let capture = work_db
        .get_latest_pr_review_guide_source_capture(&root)
        .unwrap()
        .expect("capture persisted on canonical root");
    assert_eq!(capture.trigger, "completion");
    assert_eq!(capture.packet.head_sha, "head");
}

#[test]
fn enqueue_is_series_scoped_and_cancels_an_obsolete_attempt() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "enqueue series");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            ["https://github.com/acme/widget.git", root.as_str()],
        )
        .unwrap();
    let packet = crate::test_support::source_capture_packet("https://github.com/acme/widget/pull/9", "base", "head");
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture) = stored else {
        panic!("capture must persist")
    };
    let flags_dir = tempfile::tempdir().unwrap();
    let flags = FeatureFlagsStore::new(flags_dir.path().join("flags.toml"));
    flags.load().unwrap();
    flags.set(REVIEW_GUIDE_GENERATION_FLAG, true).unwrap();

    enqueue_review_guide_generation(&db, &flags, &capture);
    let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
    assert_eq!(live.len(), 1);
    let first_id = live[0].id.clone();
    let first_exec = live[0].execution_id.clone().expect("enqueue must bind an execution");

    enqueue_review_guide_generation(&db, &flags, &capture);
    let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
    assert_eq!(
        live.len(),
        1,
        "duplicate observation of the same comparison must not start a second job"
    );
    assert_eq!(live[0].id, first_id);

    let packet2 = crate::test_support::source_capture_packet("https://github.com/acme/widget/pull/9", "base", "head2");
    let stored2 = db
        .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet2)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture2) = stored2 else {
        panic!("second capture must persist")
    };
    enqueue_review_guide_generation(&db, &flags, &capture2);
    let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
    assert_eq!(live.len(), 1);
    assert_ne!(live[0].id, first_id);
    assert_eq!(live[0].comparison_id, capture2.comparison_id);

    let old_status: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
            [&first_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "superseded");
    let exec = db.get_execution(&first_exec).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Cancelled);
}

#[test]
fn enqueue_without_repository_does_not_create_an_attempt() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "repository missing");
    db.connect()
        .unwrap()
        .execute("UPDATE products SET repo_remote_url = NULL WHERE id = ?1", [&product])
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET repo_remote_url = NULL WHERE id = ?1", [&root])
        .unwrap();
    let packet = crate::test_support::source_capture_packet("https://github.com/acme/widget/pull/9", "base", "head");
    let PrSourceCapturePersistOutcome::Stored(capture) = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap()
    else {
        panic!("capture must persist")
    };
    let flags_dir = tempfile::tempdir().unwrap();
    let flags = FeatureFlagsStore::new(flags_dir.path().join("flags.toml"));
    flags.load().unwrap();
    flags.set(REVIEW_GUIDE_GENERATION_FLAG, true).unwrap();
    enqueue_review_guide_generation(&db, &flags, &capture);
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pr_review_guide_attempts WHERE series_id = ?1",
            [&capture.series_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn enqueue_resolves_product_repository_and_task_override() {
    let product_repo = "https://github.com/acme/widget.git";
    let override_repo = "https://github.com/acme/override.git";
    for task_repo in [None, Some(""), Some(override_repo)] {
        let (_dir, db) = open_db();
        let product =
            crate::test_support::create_test_product_with_repo(&db, "repository resolution", Some(product_repo));
        let root = create_active_chore(&db, &product.id, "enqueue repository resolution");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
                rusqlite::params![task_repo, root],
            )
            .unwrap();
        let packet =
            crate::test_support::source_capture_packet("https://github.com/acme/widget/pull/9", "base", "head");
        let PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap()
        else {
            panic!("capture must persist")
        };
        let flags_dir = tempfile::tempdir().unwrap();
        let flags = FeatureFlagsStore::new(flags_dir.path().join("flags.toml"));
        flags.load().unwrap();
        flags.set(REVIEW_GUIDE_GENERATION_FLAG, true).unwrap();

        enqueue_review_guide_generation(&db, &flags, &capture);

        let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
        assert_eq!(live.len(), 1, "must enqueue with task repository {task_repo:?}");
        let execution = db
            .get_execution(live[0].execution_id.as_deref().expect("must dispatch"))
            .unwrap();
        let expected = task_repo.filter(|repo| !repo.is_empty()).unwrap_or(product_repo);
        assert_eq!(execution.repo_remote_url, expected);
        assert_eq!(execution.kind, ExecutionKind::PrReviewGuide);
        assert_eq!(execution.status, ExecutionStatus::Ready);
    }
}

#[tokio::test]
async fn revision_completion_coalesces_from_published_head_and_failure_keeps_previous_version() {
    for pre_captured in [false, true] {
        let (dir, db) = open_db();
        let db = Arc::new(db);
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "incremental guide");
        let url = "https://github.com/acme/widget/pull/75";
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(url.into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET repo_remote_url = 'https://github.com/acme/widget.git' WHERE id = ?1",
                [&root],
            )
            .unwrap();
        let packet = source_capture_packet(url, "base", "published-head");
        let PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(&root, 0, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap()
        else {
            panic!("capture");
        };
        let first = db
            .create_pr_review_guide_attempt(&capture.series_id, &capture.comparison_id, "test")
            .unwrap();
        let PublishReviewGuideOutcome::Published(version) = db
            .publish_pr_review_guide_version(&first.id, "# Published explanation", "original")
            .unwrap()
        else {
            panic!("publish");
        };
        let flags = Arc::new(FeatureFlagsStore::new(dir.path().join("flags.toml")));
        flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
        flags.set(REVIEW_GUIDE_GENERATION_FLAG, true).unwrap();
        let mut previous_execution: Option<String> = None;
        let mut revision_execution = None;
        for head in ["revision-one", "revision-two"] {
            let revision = db
                .create_revision(
                    CreateRevisionInput::builder()
                        .parent_task_id(&root)
                        .description("fix finding")
                        .build(),
                    &FakePrStateChecker::always(PrOpenState::Open),
                )
                .unwrap();
            let execution = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(&revision.id)
                        .kind(ExecutionKind::RevisionImplementation)
                        .status(ExecutionStatus::Completed)
                        .build(),
                )
                .unwrap();
            revision_execution = Some(execution.id.clone());
            let packet = source_capture_packet(url, "base", head);
            if pre_captured {
                let sequence = db.allocate_pr_review_guide_source_observation_sequence().unwrap();
                db.persist_pr_review_guide_source_capture(&root, sequence, PrSourceCaptureTrigger::Poller, &packet)
                    .unwrap();
            }
            let output = packet.clone();
            let collector = SourcePacketCollector::fixture(
                Arc::new(move |_, _, _, _| {
                    let packet = output.clone();
                    Box::pin(async move { Ok(packet) })
                }),
                packet,
            );
            for _ in 0..2 {
                reconcile_review_guide_source_for_execution_with_collector(
                    db.clone(),
                    flags.clone(),
                    &execution.id,
                    url,
                    PrSourceCaptureTrigger::Completion,
                    None,
                    collector.clone(),
                )
                .unwrap()
                .await
                .unwrap();
            }
            let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
            assert_eq!(live.len(), 1);
            let attempt = &live[0];
            assert_eq!(attempt.prompt_version, boss_review_guide::UPDATE_PROMPT_VERSION);
            let guide_execution = attempt.execution_id.as_deref().unwrap();
            assert_eq!(
                db.review_guide_update_context(guide_execution).unwrap(),
                Some(("published-head".into(), version.markdown.clone()))
            );
            assert_eq!(
                db.get_execution(guide_execution).unwrap().status,
                ExecutionStatus::Ready
            );
            if let Some(previous) = previous_execution {
                assert_eq!(db.get_execution(&previous).unwrap().status, ExecutionStatus::Cancelled);
            }
            previous_execution = Some(guide_execution.to_owned());
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.readable_version_id.as_deref(), Some(version.id.as_str()));
            let findings = db.review_guide_findings(&root, url).unwrap().unwrap();
            assert!(findings.status_text.starts_with("Updating after revision "));
            let item = db.get_work_item(&root).unwrap();
            let boss_protocol::WorkItem::Chore(task) = item else {
                panic!("chore");
            };
            assert_eq!(
                task.review_guide_update_status.as_deref(),
                Some(findings.status_text.as_str())
            );
        }
        let live = db.live_pr_review_guide_attempts_for_series(&capture.series_id).unwrap();
        db.fail_pr_review_guide_attempt(&live[0].id, "invalid new-head source link")
            .unwrap();
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "failed");
        assert_eq!(summary.readable_version_id.as_deref(), Some(version.id.as_str()));
        assert!(
            db.review_guide_findings(&root, url)
                .unwrap()
                .unwrap()
                .status_text
                .contains("failed; previous guide retained")
        );
        assert_eq!(
            db.get_pr_review_guide_version(&version.id).unwrap().unwrap().markdown,
            version.markdown
        );
        let crate::work::RetryReviewGuideOutcome::Created(retry) = db
            .retry_pr_review_guide(&root, None, boss_review_guide::PROMPT_VERSION)
            .unwrap()
        else {
            panic!("retry");
        };
        assert_eq!(
            db.review_guide_update_context(retry.execution_id.as_deref().unwrap())
                .unwrap(),
            Some(("published-head".into(), version.markdown.clone()))
        );
        db.publish_pr_review_guide_version(&retry.id, "# Updated guide", "updated")
            .unwrap();
        let status = db.review_guide_findings(&root, url).unwrap().unwrap().status_text;
        assert!(status.starts_with("Updated for revision "), "{status}");
        assert!(status.ends_with(" at revision-two"), "{status}");
        // Model a poller finishing the guide before completion is recorded.
        // Completion must attach its revision label without generating again.
        db.connect()
            .unwrap()
            .execute(
                "DELETE FROM pr_review_guide_revision_heads WHERE head_sha = 'revision-two'",
                [],
            )
            .unwrap();
        let packet = source_capture_packet(url, "base", "revision-two");
        let output = packet.clone();
        let collector = SourcePacketCollector::fixture(
            Arc::new(move |_, _, _, _| {
                let packet = output.clone();
                Box::pin(async move { Ok(packet) })
            }),
            packet,
        );
        reconcile_review_guide_source_for_execution_with_collector(
            db.clone(),
            flags.clone(),
            revision_execution.as_deref().unwrap(),
            url,
            PrSourceCaptureTrigger::Completion,
            None,
            collector,
        )
        .unwrap()
        .await
        .unwrap();
        assert!(
            db.live_pr_review_guide_attempts_for_series(&capture.series_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.review_guide_findings(&root, url).unwrap().unwrap().status_text,
            status
        );
        assert_eq!(
            db.get_pr_review_guide_version(&version.id).unwrap().unwrap().markdown,
            version.markdown
        );
    }
}

#[test]
fn updated_guide_links_validate_at_the_new_head_and_reject_stale_head_links() {
    use crate::review_guide_workspace::{validate, verify_previous_head};
    use boss_engine_test_git::jj::JjRepo;
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let old = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["new", &old]);
    std::fs::write(repo.repo.join("new.rs"), "fn fixed() {}\n").unwrap();
    JjRepo::run(&repo.repo, &["describe", "-m", "Fix caller"]);
    let head = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let mut packet = crate::test_support::review_guide_source_packet(&old, &head);
    // The previous head is an inspection endpoint, not a valid current-guide link.
    packet.merge_base_sha = "a".repeat(40);
    let markdown = format!(
        "# Guide\n## Changed since the previous version\nThe caller now uses the fix.\n## Problem\n## Fix\n## Walkthrough\n[code](https://github.com/{}/blob/{head}/new.rs#L1)\n## Tests",
        packet.head_repository
    );
    assert!(boss_review_guide::validate_update_section(&markdown).is_ok());
    assert!(validate(&repo.worker, &packet, &markdown).is_ok());
    assert!(validate(&repo.worker, &packet, &markdown.replace(&head, &old)).is_err());
    assert!(verify_previous_head(&repo.worker, &old).is_ok());
    assert!(verify_previous_head(&repo.worker, &"f".repeat(40)).is_err());
}

#[tokio::test]
async fn revision_metadata_failure_keeps_revision_status_and_published_guide() {
    let (dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "revision metadata failure");
    let url = "https://github.com/acme/widget/pull/76";
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let packet = source_capture_packet(url, "base", "published-head");
    let PrSourceCapturePersistOutcome::Stored(capture) = db
        .persist_pr_review_guide_source_capture(&root, 0, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap()
    else {
        panic!("capture");
    };
    let first = db
        .create_pr_review_guide_attempt(&capture.series_id, &capture.comparison_id, "test")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&first.id, "# Published explanation", "original")
        .unwrap()
    else {
        panic!("publish");
    };
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&root)
                .description("fix finding")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let flags = Arc::new(FeatureFlagsStore::new(dir.path().join("flags.toml")));
    flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
    flags.set(REVIEW_GUIDE_GENERATION_FLAG, true).unwrap();
    let mut collector = SourcePacketCollector::fixture(
        Arc::new(|_, _, _, _| panic!("metadata failure must stop source collection")),
        packet,
    );
    collector.metadata = Arc::new(|_, _| Box::pin(async { anyhow::bail!("metadata unavailable") }));
    reconcile_review_guide_source_with_collector(
        db.clone(),
        flags,
        SourceCaptureRequest::builder()
            .root_task_id(&root)
            .pr_url(url)
            .trigger(PrSourceCaptureTrigger::Completion)
            .revision(RevisionCapture {
                task_id: revision.id.clone(),
                head_before: Some("published-head".into()),
                head_after: Some("revised-head".into()),
            })
            .build(),
        collector,
    )
    .unwrap()
    .await
    .unwrap();
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    assert_eq!(summary.readable_version_id.as_deref(), Some(version.id.as_str()));
    let label = boss_protocol::short_id_label(revision.short_id).unwrap_or(revision.id);
    assert_eq!(
        db.review_guide_findings(&root, url).unwrap().unwrap().status_text,
        format!("Update after revision {label} failed; previous guide retained")
    );
    assert_eq!(db.get_pr_review_guide_version(&version.id).unwrap().unwrap(), *version);
    assert!(
        db.live_pr_review_guide_attempts_for_series(&capture.series_id)
            .unwrap()
            .is_empty()
    );
}
